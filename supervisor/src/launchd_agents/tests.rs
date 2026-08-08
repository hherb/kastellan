//! Driver + launchctl-helper tests for the macOS `launchd` backend.
//!
//! Lifted out of the inline `#[cfg(test)] mod tests` in
//! `launchd_agents.rs` when that file outgrew the 500-LOC cap.
//! `use super::*` resolves to the parent `launchd_agents` module, which
//! gives these tests access to its private launchctl-parsing helpers
//! (`parse_print_state`, `is_no_such_service_error`, `user_domain_target`)
//! and the `LaunchAgents` driver. The pure-builder/validator tests live
//! alongside their code in the sibling `builders.rs`.

use super::*;
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

// ---------- launchctl-parsing helper tests ----------

#[test]
fn parse_print_state_finds_state_line_with_indentation() {
    let stdout = "gui/501/foo = {\n\ttype = LaunchAgent\n\tstate = running\n\tlast exit code = 0\n}";
    assert_eq!(parse_print_state(stdout), Some("running".into()));
}

#[test]
fn parse_print_state_returns_none_when_absent() {
    let stdout = "Could not find service \"foo\" in domain for login: 501";
    assert_eq!(parse_print_state(stdout), None);
}

#[test]
fn parse_print_state_handles_multi_word_state() {
    let stdout = "    state = not running\n";
    assert_eq!(parse_print_state(stdout), Some("not running".into()));
}

#[test]
fn is_no_such_service_error_recognises_known_phrases() {
    assert!(is_no_such_service_error(
        "Could not find service \"foo\" in domain"
    ));
    assert!(is_no_such_service_error("No such process"));
    assert!(!is_no_such_service_error("permission denied"));
}

#[test]
fn user_domain_target_starts_with_gui() {
    // We can't pin the UID (varies per host) but the prefix is
    // invariant.
    let t = user_domain_target().expect("uid resolves");
    assert!(t.starts_with("gui/"), "got: {t}");
    // Must be `gui/<digits>` and nothing else.
    let suffix = t.strip_prefix("gui/").unwrap();
    assert!(
        suffix.chars().all(|c| c.is_ascii_digit()),
        "uid suffix must be all digits, got: {t}"
    );
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

#[test]
fn status_returns_not_installed_when_plist_absent() {
    let dir = TestRoot::new("status-absent");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    let s = sup.status("never-installed").expect("status");
    assert_eq!(s, ServiceStatus::NotInstalled);
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
