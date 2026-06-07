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
//! `install_target` against a custom units dir, without touching the live
//! `systemctl --user` manager. They run on any host with a writable /tmp.

use super::*;
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
            "hhagent-supervisor-test-{}-{}-{}",
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
    let spec = minimal_spec("hhagent-test");
    sup.install(&spec).expect("install");

    let path = sup.unit_path("hhagent-test");
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

#[test]
fn status_returns_not_installed_when_unit_absent() {
    let dir = TestRoot::new("status-absent");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let s = sup.status("never-installed").expect("status");
    assert_eq!(s, ServiceStatus::NotInstalled);
}

// ---------- ordering-field injection rejection tests ----------

#[test]
fn install_rejects_after_entry_with_injection() {
    let dir = TestRoot::new("inject-after");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("hhagent-core");
    spec.program = std::path::PathBuf::from("/bin/true");
    spec.after = vec!["pg\n[Service]\nExecStart=/bin/evil".into()];
    let err = sup.install(&spec).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    // No unit file should have been written.
    assert!(!dir.path().join("hhagent-core.service").exists());
}

#[test]
fn install_rejects_part_of_with_injection() {
    let dir = TestRoot::new("inject-partof");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("hhagent-core");
    spec.program = std::path::PathBuf::from("/bin/true");
    spec.part_of = Some("hhagent\nWantedBy=evil.target".into());
    let err = sup.install(&spec).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    assert!(!dir.path().join("hhagent-core.service").exists());
}

#[test]
fn install_target_rejects_member_with_injection() {
    let dir = TestRoot::new("inject-member");
    let sup = SystemdUser::with_units_dir(dir.path().to_path_buf());
    let target = TargetSpec {
        name: "hhagent".into(),
        members: vec!["pg\nExecStart=/bin/evil".into()],
    };
    // members slice can be empty here — the target-name/members validation
    // must fire before any member install.
    let err = sup.install_target(&target, &[]).unwrap_err();
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err:?}");
    assert!(!dir.path().join("hhagent.target").exists());
}

#[test]
fn install_target_writes_target_unit_and_members_into_units_dir() {
    let dir = std::env::temp_dir().join(format!(
        "hhagent-target-unit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let sup = SystemdUser::with_units_dir(dir.clone());

    let mut pg = minimal_spec("hhagent-postgres");
    pg.program = std::path::PathBuf::from("/usr/lib/postgresql/18/bin/postgres");
    pg.part_of = Some("hhagent".into());
    let mut core = minimal_spec("hhagent-core");
    core.program = std::path::PathBuf::from("/opt/hhagent/hhagent");
    core.after = vec!["hhagent-postgres".into()];
    core.part_of = Some("hhagent".into());

    let target = TargetSpec {
        name: "hhagent".into(),
        members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
    };
    sup.install_target(&target, &[pg, core]).expect("install_target");

    // Target unit written with Wants= of both members.
    let target_body =
        std::fs::read_to_string(dir.join("hhagent.target")).expect("target file");
    assert!(
        target_body.contains("Wants=hhagent-postgres.service hhagent-core.service\n"),
        "{target_body}"
    );
    // Member units written, core ordered After= postgres.
    assert!(dir.join("hhagent-postgres.service").exists());
    let core_body =
        std::fs::read_to_string(dir.join("hhagent-core.service")).expect("core file");
    assert!(core_body.contains("After=hhagent-postgres.service\n"), "{core_body}");

    std::fs::remove_dir_all(&dir).ok();
}
