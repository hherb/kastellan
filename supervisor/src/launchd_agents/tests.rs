//! Driver + launchctl-helper tests for the macOS `launchd` backend.
//!
//! Lifted out of the inline `#[cfg(test)] mod tests` in
//! `launchd_agents.rs` when that file outgrew the 500-LOC cap.
//! The launchctl helpers live in `launchd_agents/launchctl.rs`, and the
//! tests of their pure readers in `launchctl/tests.rs`; the helpers used
//! here are reached through the parent's private `use` (via `use super::*`).
//! The pure-builder/validator tests live alongside their code in the sibling
//! `builders.rs`.

use super::*;
use super::launchctl::tests::{backend, NOT_LOADED};
use crate::EnvFileRef;
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

// ---------- driver tests using a custom agents dir ----------
//
// These exercise the file-writing half of `install`/`uninstall`
// without touching the real `launchctl` GUI domain. They run on
// any host with a writable /tmp.

/// Tempdir helper mirroring `systemd_user::tests::TestRoot`:
/// unique per process+test+call, removed on drop.
struct TestRoot(PathBuf);
impl TestRoot {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kastellan-launchd-test-{}-{}-{}",
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

#[test]
fn install_writes_plist_with_expected_content() {
    let dir = TestRoot::new("install-content");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let spec = minimal_spec("kastellan-test");
    sup.install(&spec).expect("install");

    let path = sup.plist_path("kastellan-test");
    assert!(path.exists(), "plist not written: {}", path.display());
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("<?xml version=\"1.0\""), "{body}");
    assert!(
        body.contains("<key>Label</key>\n    <string>kastellan-test</string>"),
        "{body}"
    );
    assert!(
        body.contains("<string>/usr/bin/true</string>"),
        "{body}"
    );
}

#[test]
fn install_rejects_relative_program_path() {
    let dir = TestRoot::new("rel-program");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.program = PathBuf::from("relative/foo");
    let err = sup.install(&spec).expect_err("relative program");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
}

#[test]
fn install_rejects_invalid_name() {
    let dir = TestRoot::new("bad-name");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.name = "../traversal".into();
    let err = sup.install(&spec).expect_err("traversal name");
    assert!(matches!(err, SupervisorError::InvalidName(_)), "{err}");
}

#[test]
fn install_rejects_relative_working_dir() {
    let dir = TestRoot::new("rel-wd");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.working_dir = Some(PathBuf::from("relative/wd"));
    let err = sup.install(&spec).expect_err("relative wd");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
}

#[test]
fn install_creates_agents_dir_if_missing() {
    let dir = TestRoot::new("nested-dir");
    let nested = dir.path().join("a").join("b").join("c");
    let sup = LaunchAgents::with_agents_dir(nested.clone());
    sup.install(&minimal_spec("svc")).expect("install");
    assert!(nested.is_dir(), "nested agents dir should be created");
    assert!(nested.join("svc.plist").is_file());
}

#[test]
fn uninstall_removes_plist_file() {
    let dir = TestRoot::new("uninstall");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.plist_path("svc");
    assert!(path.exists());
    sup.uninstall("svc").expect("uninstall");
    assert!(!path.exists(), "plist still present after uninstall");
}

#[test]
fn uninstall_is_idempotent_when_nothing_installed() {
    let dir = TestRoot::new("idempotent");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.uninstall("nonexistent")
        .expect("uninstall must be idempotent");
}

/// #894: a bootout that did not answer "not loaded" keeps the plist, so
/// `uninstall` can be retried — and the error says the plist was kept.
#[test]
fn remove_plist_after_a_failed_bootout_keeps_the_plist() {
    let dir = TestRoot::new("kept-plist");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.plist_path("svc");
    let failures = [
        SupervisorError::Backend("launchctl bootout timed out".into()),
        SupervisorError::Io("run launchctl: Broken pipe".into()),
    ];
    for failure in failures {
        let err = remove_plist_after(Err(failure), &path).expect_err("bootout failed");
        assert!(path.exists(), "plist removed although the agent may still be loaded");
        let msg = match &err {
            SupervisorError::Backend(m) | SupervisorError::Io(m) => m,
            other => panic!("unexpected variant: {other}"),
        };
        assert!(msg.contains("kept"), "{err}");
    }
}

#[test]
fn remove_plist_after_a_clean_bootout_removes_it() {
    let dir = TestRoot::new("removed-plist");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.plist_path("svc");
    remove_plist_after(Ok(()), &path).expect("removed");
    assert!(!path.exists());
    remove_plist_after(Ok(()), &path).expect("already absent is success");
}

/// The whole "already not loaded" chain with launchctl's real wording:
/// uninstalling a stopped agent still removes its plist.
#[test]
fn remove_plist_after_a_not_loaded_bootout_removes_it() {
    let dir = TestRoot::new("not-loaded-plist");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.plist_path("svc");
    remove_plist_after(bootout_outcome(backend(NOT_LOADED)), &path).expect("removed");
    assert!(!path.exists());
}

#[test]
fn status_returns_not_installed_when_plist_absent() {
    let dir = TestRoot::new("status-absent");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let s = sup.status("never-installed").expect("status");
    assert_eq!(s, ServiceStatus::NotInstalled);
}

/// An agents dir that cannot be searched is an error naming the stat, not
/// "plist not found (call install first)" from `start` or `NotInstalled`
/// from `status` (#898). Neither reaches `launchctl`: both stop at the stat.
#[test]
fn start_and_status_report_an_unreadable_agents_dir() {
    use std::os::unix::fs::PermissionsExt;
    if super::launchctl::current_uid() == 0 {
        eprintln!("[SKIP] root ignores directory permissions");
        return;
    }
    let root = TestRoot::new("unreadable");
    let locked = root.path().join("locked");
    fs::create_dir_all(locked.join("agents")).expect("mkdir");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    let sup = LaunchAgents::with_agents_dir(locked.join("agents"));
    let start = sup.start("svc").expect_err("start must fail");
    let status = sup.status("svc");
    // Restore before asserting, so a failure still lets TestRoot clean up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod 700");
    assert!(start.to_string().contains("stat "), "{start}");
    assert!(matches!(&status, Err(e) if e.to_string().contains("stat ")), "{status:?}");
}

// ---------- environment_files folding (launchd's EnvironmentFile= counterpart) ----------

#[test]
fn install_folds_environment_files_in_order_with_later_winning() {
    let dir = TestRoot::new("env-file");
    let base = dir.path().join("kastellan.env");
    let local = dir.path().join("kastellan.env.local");
    fs::write(&base, "# tuned by operator\nKASTELLAN_DATA_DIR=/srv/data\nKASTELLAN_LLM_LOCAL_MODEL=stock-tag\n").unwrap();
    // The overlay overrides the regenerated value and adds a key of its own —
    // the two shapes #458 exists to fix.
    fs::write(&local, "KASTELLAN_LLM_LOCAL_MODEL=tuned-tag\nKASTELLAN_MAIL_ENDPOINT=https://10.0.0.3:8443\n").unwrap();

    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("kastellan-core");
    spec.environment_files = vec![
        EnvFileRef { path: base, optional: false },
        EnvFileRef { path: local, optional: true },
    ];
    sup.install(&spec).expect("install");

    let body = fs::read_to_string(sup.plist_path("kastellan-core")).unwrap();
    assert!(body.contains("<key>KASTELLAN_DATA_DIR</key>"), "{body}");
    assert!(body.contains("<string>/srv/data</string>"), "{body}");
    assert!(body.contains("<string>tuned-tag</string>"), "{body}");
    assert!(!body.contains("<string>stock-tag</string>"), "later file must win: {body}");
    assert!(body.contains("<key>KASTELLAN_MAIL_ENDPOINT</key>"), "{body}");
}

#[test]
fn install_folds_environment_files_over_an_inline_spec_env_value() {
    // Both existing environment_files tests use `minimal_spec`, whose `env`
    // is empty — they only cover file-vs-file ordering. `spec.env` is the
    // base `merged` starts from (see `install`'s `let mut merged =
    // spec.clone();`), so an inline entry must survive when NOT overridden,
    // and lose when a later env file sets the same key — the same
    // later-wins contract the file-vs-file test pins, extended to cover the
    // inline half of the fold. Concrete regression this guards: replacing
    // that `spec.clone()` with a fresh empty vec in the fold would drop
    // `KASTELLAN_EGRESS_FORCE_ROUTING=1` — a real containment setting —
    // from the macOS plist while every other test still passed.
    let dir = TestRoot::new("env-file-over-inline");
    let base = dir.path().join("kastellan.env");
    fs::write(&base, "KASTELLAN_LLM_LOCAL_MODEL=tuned-tag\n").unwrap();

    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("kastellan-core");
    spec.env = vec![
        ("KASTELLAN_LLM_LOCAL_MODEL".to_string(), "inline-default-tag".to_string()),
        ("KASTELLAN_EGRESS_FORCE_ROUTING".to_string(), "1".to_string()),
    ];
    spec.environment_files = vec![EnvFileRef { path: base, optional: false }];
    sup.install(&spec).expect("install");

    let body = fs::read_to_string(sup.plist_path("kastellan-core")).unwrap();
    assert!(body.contains("<key>KASTELLAN_LLM_LOCAL_MODEL</key>"), "{body}");
    assert!(
        body.contains("<string>tuned-tag</string>"),
        "file value must beat the inline spec.env value: {body}"
    );
    assert!(
        !body.contains("<string>inline-default-tag</string>"),
        "the overridden inline value must not survive: {body}"
    );
    assert!(body.contains("<key>KASTELLAN_EGRESS_FORCE_ROUTING</key>"), "{body}");
    assert!(
        body.contains("<string>1</string>"),
        "a non-overridden inline entry must survive the fold: {body}"
    );
}

#[test]
fn install_skips_a_missing_optional_environment_file() {
    let dir = TestRoot::new("env-file-optional-missing");
    let base = dir.path().join("kastellan.env");
    fs::write(&base, "KASTELLAN_DATA_DIR=/srv/data\n").unwrap();
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    // The normal case: the operator has not created a `.local` yet.
    spec.environment_files = vec![
        EnvFileRef { path: base, optional: false },
        EnvFileRef { path: dir.path().join("kastellan.env.local"), optional: true },
    ];
    sup.install(&spec).expect("a missing OPTIONAL env file must not fail the install");
    let body = fs::read_to_string(sup.plist_path("svc")).unwrap();
    assert!(body.contains("<key>KASTELLAN_DATA_DIR</key>"), "{body}");
}

#[test]
fn install_errors_when_a_required_environment_file_is_missing() {
    let dir = TestRoot::new("env-file-missing");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.environment_files = vec![EnvFileRef {
        path: dir.path().join("does-not-exist.env"),
        optional: false,
    }];
    let err = sup.install(&spec).expect_err("missing REQUIRED env file");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
}

#[test]
fn install_rejects_a_control_character_in_an_environment_file_path() {
    // #530 moved the injection guarantee from the systemd renderer (where
    // `quote_if_needed` made it structural) to `validate_env_file_path` at each
    // backend's `install`. That makes the CALL the guarantee — delete it and a
    // newline reaches the Linux renderer, where `EnvironmentFile=/tmp/x` and
    // `ExecStartPre=/evil` become two directives.
    //
    // Pinned here, on macOS, as well as on Linux for a reason worth stating:
    // this backend has no second control-character sweep to fall back on, so on
    // this host the one call IS the whole check, and nothing else in the suite
    // would notice its removal.
    let dir = TestRoot::new("env-file-ctrl");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.environment_files = vec![EnvFileRef {
        path: PathBuf::from("/tmp/x\nExecStartPre=/evil"),
        optional: true,
    }];
    let err = sup.install(&spec).expect_err("a newline in an env-file path must be refused");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(
        !sup.plist_path("svc").exists(),
        "no plist may be written when an env-file path is rejected"
    );
}

#[test]
fn install_rejects_a_relative_environment_file_path() {
    // The other half of the same guard, and the fail-OPEN one: systemd drops a
    // relative `EnvironmentFile=` and starts the unit anyway. This backend
    // resolves it against the INSTALLER's cwd, which is a different wrong file
    // rather than none — both are silent, so both fail closed here.
    let dir = TestRoot::new("env-file-rel");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let mut spec = minimal_spec("svc");
    spec.environment_files =
        vec![EnvFileRef { path: PathBuf::from("kastellan.env"), optional: true }];
    let err = sup.install(&spec).expect_err("a relative env-file path must be refused");
    assert!(matches!(err, SupervisorError::Io(_)), "{err}");
    assert!(!sup.plist_path("svc").exists(), "no plist may be written");
}

// ---------- atomic-write staging (#509 review) ----------
//
// The staging contract itself (unique-per-writer names, cleanup on the
// error paths, the `.tmp.<pid>.<n>` suffix staying AFTER `.plist` so the
// staging file is not itself a loadable agent) lives with the shared
// helper in `crate::atomic_write` and runs on both hosts. What is left
// here is the wiring: that `install` publishes through it, leaving the
// agents dir holding exactly the plist and nothing else — anything else
// in that directory is something launchd may try to load.

#[test]
fn install_publishes_the_plist_and_leaves_no_staging_file_behind() {
    let dir = TestRoot::new("staging-clean");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");

    let names: Vec<String> = fs::read_dir(dir.path())
        .expect("read agents dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["svc.plist"]);
}
