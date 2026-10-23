//! Driver tests for the Linux `systemd --user` backend.
//!
//! Lifted out of the inline `#[cfg(test)] mod tests` in `systemd_user.rs`
//! when that file outgrew the 500-LOC cap. `use super::*` resolves to the
//! parent `systemd_user` module, which gives these tests the
//! [`SystemdUser`] driver plus the builder functions it re-exports
//! (`build_unit_file`, `build_target_unit`, `validate_service_name`). The
//! pure-builder/validator tests live alongside their code in the sibling
//! `builder.rs`.
//!
//! These exercise the file-writing half of `install`/`uninstall`/
//! `install_target`/`uninstall_target` against a custom units dir, without
//! touching the live `systemctl --user` manager, plus the #896 composition of
//! the stop readers with `remove_after_stop`. They run on any host with a
//! writable /tmp.
//!
//! Not covered here: that `uninstall` hands its stop result to
//! `remove_after_stop` at all — a custom dir sends no stop, so only the live
//! smoke tests reach that line (`stop_and_disable_with` is pinned in
//! `systemctl/tests.rs`).

use super::*;
use super::systemctl::tests::{backend, NOT_LOADED, NO_BUS, UNIT};
use super::systemctl::{disable_outcome, stop_outcome};
use std::sync::atomic::{AtomicU64, Ordering};

/// Minimal spec used as a starting point in driver tests.
fn minimal_spec(name: &str) -> ServiceSpec {
    ServiceSpec {
        name: name.into(),
        program: PathBuf::from("/usr/bin/true"),
        args: vec![],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: None,
        restart_backoff: None,
        environment_files: Vec::new(),
    }
}

/// Tempdir helper mirroring `core::workspace::tests::TestRoot`:
/// unique per process+test+call, removed on drop.
struct TestRoot(PathBuf);
impl TestRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kastellan-supervisor-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test root");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------- driver tests using a custom units dir ----------

#[test]
fn install_writes_unit_file_with_expected_content() {
    let dir = TestRoot::new("install-content");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let spec = minimal_spec("kastellan-test");
    sup.install(&spec).expect("install");

    let path = sup.unit_path("kastellan-test");
    assert!(path.exists(), "unit file not written: {}", path.display());
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("[Unit]"), "{body}");
    assert!(body.contains("ExecStart=/usr/bin/true"), "{body}");
}

#[test]
fn install_rejects_relative_program_path() {
    let dir = TestRoot::new("rel-program");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.program = PathBuf::from("relative/foo");
    let err = sup.install(&spec).expect_err("relative program");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
}

#[test]
fn install_rejects_newline_in_path_field() {
    // Audit finding #10: a newline in a path field would inject a unit-file
    // directive (path fields are written verbatim via Display). Must fail
    // closed before any file is written.
    let dir = TestRoot::new("newline-path");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.working_dir = Some(PathBuf::from("/tmp\nExecStartPre=/evil"));
    let err = sup.install(&spec).expect_err("newline working_dir must be rejected");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(
        !sup.unit_path("svc").exists(),
        "no unit file may be written when a path field is rejected"
    );
}

#[test]
fn install_rejects_a_control_character_in_an_environment_file_path() {
    // The Linux half of the guarantee #530 relocated. `EnvironmentFile=` is now
    // emitted BARE (a quoted path is one systemd drops), so `quote_if_needed`
    // no longer stands between a newline and the rendered unit — the
    // `validate_env_file_path` call in `write_unit_file` does, and this is what
    // fails if that call is deleted.
    //
    // The sibling `install_rejects_newline_in_path_field` cannot cover it: it
    // uses `working_dir`, which is still escaped at the emission seam and would
    // stay safe either way.
    let dir = TestRoot::new("env-file-ctrl");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.environment_files = vec![crate::EnvFileRef {
        path: PathBuf::from("/tmp/x\nExecStartPre=/evil"),
        optional: true,
    }];
    let err = sup.install(&spec).expect_err("a newline in an env-file path must be refused");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(
        !sup.unit_path("svc").exists(),
        "no unit file may be written when an env-file path is rejected"
    );
}

#[test]
fn install_rejects_a_relative_environment_file_path() {
    // Fail-OPEN without the check: systemd drops a relative `EnvironmentFile=`
    // with a journal warning and loads the unit anyway, so the daemon starts
    // with no environment at all — #458's failure in its total form.
    let dir = TestRoot::new("env-file-rel");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.environment_files =
        vec![crate::EnvFileRef { path: PathBuf::from("kastellan.env"), optional: true }];
    let err = sup.install(&spec).expect_err("a relative env-file path must be refused");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(!sup.unit_path("svc").exists(), "no unit file may be written");
}

#[test]
fn install_rejects_invalid_name() {
    let dir = TestRoot::new("bad-name");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.name = "../traversal".into();
    let err = sup.install(&spec).expect_err("traversal name");
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err}");
}

#[test]
fn install_creates_units_dir_if_missing() {
    let dir = TestRoot::new("nested-dir");
    let nested = dir.path().join("a").join("b").join("c");
    let sup = SystemdUser::with_units_dir(nested.clone());
    sup.install(&minimal_spec("svc")).expect("install");
    assert!(nested.is_dir(), "nested units dir should be created");
    assert!(nested.join("svc.service").is_file());
}

#[test]
fn uninstall_removes_unit_file() {
    let dir = TestRoot::new("uninstall");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.unit_path("svc");
    assert!(path.exists());
    sup.uninstall("svc").expect("uninstall");
    assert!(!path.exists(), "unit file still present after uninstall");
}

#[test]
fn uninstall_is_idempotent_when_nothing_installed() {
    let dir = TestRoot::new("idempotent");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.uninstall("nonexistent")
        .expect("uninstall must be idempotent");
}

/// #896: a stop or disable that did not give one of systemd's benign answers
/// keeps the unit file — the unit may still run, or start at the next boot —
/// and the error says the file was kept so `uninstall` can be retried.
#[test]
fn a_failed_stop_or_disable_keeps_the_unit_file() {
    let dir = TestRoot::new("kept-unit");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.unit_path("svc");
    for failed in [
        stop_outcome(UNIT, backend(NO_BUS)),
        disable_outcome(UNIT, backend(NO_BUS)),
        stop_outcome(UNIT, Err(SupervisorError::Io("run systemctl: Broken pipe".into()))),
    ] {
        let err = remove_after_stop(failed, &path).expect_err("the unit may still be live");
        assert!(path.exists(), "unit file removed although the unit may still run");
        assert!(err.to_string().contains("kept"), "{err}");
    }
}

/// The whole "already stopped" chain with systemd's real wording: a unit the
/// manager never loaded still has its file removed.
#[test]
fn a_not_loaded_stop_removes_the_unit_file() {
    let dir = TestRoot::new("not-loaded-unit");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.unit_path("svc");
    remove_after_stop(stop_outcome(UNIT, backend(NOT_LOADED)), &path).expect("removed");
    assert!(!path.exists());
    remove_after_stop(Ok(()), &path).expect("already absent is success");
}

/// A custom-dir `uninstall_target` (no systemctl call) removes every member
/// and the `.target` file — and is idempotent.
#[test]
fn uninstall_target_removes_members_and_target_file() {
    let dir = TestRoot::new("uninstall-target");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let target = TargetSpec { name: "kastellan".into(), members: vec!["a".into(), "b".into()] };
    sup.install_target(&target, &[minimal_spec("a"), minimal_spec("b")]).expect("install");
    sup.uninstall_target(&target).expect("uninstall_target");
    assert_eq!(dir_names(dir.path()), Vec::<String>::new());
    sup.uninstall_target(&target).expect("idempotent");
}

/// #896 fail-fast: a member that cannot be uninstalled stops the teardown
/// there, keeping the `.target` file and the members not yet reached, so a
/// retry finds everything it needs.
#[test]
fn uninstall_target_stops_at_a_member_it_cannot_remove() {
    let dir = TestRoot::new("uninstall-target-fail-fast");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let target = TargetSpec { name: "kastellan".into(), members: vec!["a".into(), "b".into()] };
    sup.install_target(&target, &[minimal_spec("a"), minimal_spec("b")]).expect("install");
    // Members go in reverse, so `b` is first; a directory cannot be removed
    // as a file.
    fs::remove_file(sup.unit_path("b")).expect("rm b");
    fs::create_dir(sup.unit_path("b")).expect("mkdir in b's place");
    let err = sup.uninstall_target(&target).expect_err("b cannot be removed");
    assert!(err.to_string().contains("b.service"), "{err}");
    assert!(sup.target_path("kastellan").exists(), "target file removed after a failed member");
    assert!(sup.unit_path("a").exists(), "a torn down after b failed");
}

/// Every member name is checked before anything is torn down — a bad one
/// found mid-teardown would wedge every retry on it.
#[test]
fn uninstall_target_rejects_a_bad_member_before_touching_anything() {
    let dir = TestRoot::new("uninstall-target-bad-member");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let good = TargetSpec { name: "kastellan".into(), members: vec!["a".into()] };
    sup.install_target(&good, &[minimal_spec("a")]).expect("install");
    let bad = TargetSpec { name: "kastellan".into(), members: vec!["../x".into(), "a".into()] };
    let err = sup.uninstall_target(&bad).expect_err("bad member name");
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err}");
    assert!(sup.unit_path("a").exists() && sup.target_path("kastellan").exists());
}

#[test]
fn status_returns_not_installed_when_unit_absent() {
    let dir = TestRoot::new("status-absent");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let s = sup.status("never-installed").expect("status");
    assert_eq!(s, ServiceStatus::NotInstalled);
    let with_word = sup.status_with_word("never-installed").expect("status_with_word");
    assert_eq!(with_word, (ServiceStatus::NotInstalled, None), "no manager was asked, so no word");
}

/// A units dir that cannot be searched is an error naming the stat, not
/// `NotInstalled` — the launchd twin's #898 test. Never reaches systemctl.
#[test]
fn status_reports_an_unreadable_units_dir() {
    use std::os::unix::fs::PermissionsExt;
    let root = TestRoot::new("unreadable");
    let locked = root.path().join("locked");
    fs::create_dir_all(locked.join("units")).expect("mkdir");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    let bites = locked.join("units").try_exists().is_err();
    let sup = SystemdUser::with_units_dir(locked.join("units"));
    let status = sup.status("svc");
    // Restore before asserting, so a failure still lets TestRoot clean up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod 700");
    if !bites {
        eprintln!("[SKIP] directory permissions do not apply to this user (root)");
        return;
    }
    assert!(matches!(&status, Err(e) if e.to_string().contains("stat ")), "{status:?}");
}

// ---------- ordering-field injection rejection tests ----------

#[test]
fn install_rejects_after_entry_with_injection() {
    let dir = TestRoot::new("inject-after");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("kastellan-core");
    spec.program = std::path::PathBuf::from("/bin/true");
    spec.after = vec!["pg\n[Service]\nExecStart=/bin/evil".into()];
    let err = sup.install(&spec).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    // No unit file should have been written.
    assert!(!dir.path().join("kastellan-core.service").exists());
}

#[test]
fn install_rejects_part_of_with_injection() {
    let dir = TestRoot::new("inject-partof");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("kastellan-core");
    spec.program = std::path::PathBuf::from("/bin/true");
    spec.part_of = Some("kastellan\nWantedBy=evil.target".into());
    let err = sup.install(&spec).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    assert!(!dir.path().join("kastellan-core.service").exists());
}

#[test]
fn install_target_rejects_member_with_injection() {
    let dir = TestRoot::new("inject-member");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let target = TargetSpec {
        name: "kastellan".into(),
        members: vec!["pg\nExecStart=/bin/evil".into()],
    };
    // members slice can be empty here — the target-name/members validation
    // must fire before any member install.
    let err = sup.install_target(&target, &[]).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    assert!(!dir.path().join("kastellan.target").exists());
}

#[test]
fn install_target_writes_target_unit_and_members_into_units_dir() {
    let dir = std::env::temp_dir().join(format!(
        "kastellan-target-unit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let sup = SystemdUser::with_units_dir(dir.clone());

    let mut pg = minimal_spec("kastellan-postgres");
    pg.program = std::path::PathBuf::from("/usr/lib/postgresql/18/bin/postgres");
    pg.part_of = Some("kastellan".into());
    let mut core = minimal_spec("kastellan-core");
    core.program = std::path::PathBuf::from("/opt/kastellan/kastellan");
    core.after = vec!["kastellan-postgres".into()];
    core.part_of = Some("kastellan".into());

    let target = TargetSpec {
        name: "kastellan".into(),
        members: vec!["kastellan-postgres".into(), "kastellan-core".into()],
    };
    sup.install_target(&target, &[pg, core]).expect("install_target");

    // Target unit written with Wants= of both members.
    let target_body =
        std::fs::read_to_string(dir.join("kastellan.target")).expect("target file");
    assert!(
        target_body.contains("Wants=kastellan-postgres.service kastellan-core.service\n"),
        "{target_body}"
    );
    // Member units written, core ordered After= postgres.
    assert!(dir.join("kastellan-postgres.service").exists());
    let core_body =
        std::fs::read_to_string(dir.join("kastellan-core.service")).expect("core file");
    assert!(core_body.contains("After=kastellan-postgres.service\n"), "{core_body}");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------- atomic-write staging (#509 review) ----------
//
// The staging contract itself (unique-per-writer names, cleanup on the
// error paths, `.target` not collapsing onto `.service`) lives with the
// shared helper in `crate::atomic_write` and runs on both hosts. What is
// left here is the wiring: that the driver publishes through it, leaving
// the units dir holding exactly the units and nothing else.

/// Every name in `dir`, sorted — so a leftover staging file fails the
/// assertion by being present, not by being looked for.
fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read units dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn install_publishes_the_unit_and_leaves_no_staging_file_behind() {
    let dir = TestRoot::new("staging-clean");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("kastellan-test")).expect("install");

    assert_eq!(dir_names(dir.path()), vec!["kastellan-test.service"]);
}

#[test]
fn install_target_publishes_the_target_unit_without_staging_leftovers() {
    // `install_target` writes N member units and then the `.target`
    // through the same helper; the `.target` is the path that used to be
    // staged as `<name>.service.tmp`, colliding with a like-named
    // `.service`.
    let dir = TestRoot::new("staging-target");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let target = TargetSpec {
        name: "kastellan-test".into(),
        members: vec!["kastellan-test-member".into()],
    };
    sup.install_target(&target, &[minimal_spec("kastellan-test-member")]).expect("install_target");

    assert_eq!(
        dir_names(dir.path()),
        vec!["kastellan-test-member.service", "kastellan-test.target"]
    );
}
