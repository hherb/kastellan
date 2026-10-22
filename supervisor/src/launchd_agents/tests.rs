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
use super::launchctl::parse_print_state;
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

/// What `launchctl bootout` said when the agent was never loaded.
const NOT_LOADED: &str = "launchctl bootout gui/501/svc (exit 113): Could not find service";

fn backend(msg: &str) -> Result<String, SupervisorError> {
    Err(SupervisorError::Backend(msg.into()))
}

// ---------- bootout_outcome (#894): only "not loaded" is benign ----------

#[test]
fn bootout_outcome_accepts_success_and_not_loaded() {
    assert!(bootout_outcome(Ok(String::new())).is_ok());
    assert!(bootout_outcome(backend(NOT_LOADED)).is_ok());
}

/// A wedged domain, an unreadable answer or a refusal leaves the agent
/// possibly still loaded — never read as "already gone".
#[test]
fn bootout_outcome_keeps_a_timeout_an_io_error_and_a_refusal() {
    let timed_out = backend("launchctl bootout gui/501/svc timed out after 20s");
    assert!(matches!(bootout_outcome(timed_out), Err(SupervisorError::Backend(_))));
    let unread = Err(SupervisorError::Io("run launchctl: Broken pipe".into()));
    assert!(matches!(bootout_outcome(unread), Err(SupervisorError::Io(_))));
    let refused = backend("launchctl bootout gui/501/svc (exit 1): Operation not permitted");
    assert!(bootout_outcome(refused).is_err());
}

// ---------- status_from_print (#895): an unreadable answer is not "inactive" ----------

#[test]
fn status_from_print_maps_the_state_line() {
    let running = Ok("gui/501/svc = {\n\tstate = running\n}".to_string());
    assert_eq!(status_from_print("gui/501/svc", running).unwrap(), ServiceStatus::Active);
    let waiting = Ok("\tstate = not running\n".to_string());
    assert_eq!(status_from_print("gui/501/svc", waiting).unwrap(), ServiceStatus::Inactive);
}

#[test]
fn status_from_print_reads_not_loaded_as_inactive() {
    let not_loaded = backend("launchctl print gui/501/svc (exit 113): Could not find service");
    assert_eq!(status_from_print("gui/501/svc", not_loaded).unwrap(), ServiceStatus::Inactive);
}

/// A successful `print` with no `state = …` line (a format drift, a cut-short
/// answer) is an error naming the target — never a confident `Inactive`.
#[test]
fn status_from_print_refuses_an_answer_without_a_state_line() {
    let drifted = Ok("gui/501/svc = {\n\tstatus = running\n}".to_string());
    let err = status_from_print("gui/501/svc", drifted).expect_err("no state line");
    assert!(matches!(&err, SupervisorError::Backend(m) if m.contains("gui/501/svc")), "{err}");
}

#[test]
fn status_from_print_passes_through_a_timeout_and_an_io_error() {
    let timed_out = backend("launchctl print gui/501/svc timed out after 20s");
    assert!(status_from_print("gui/501/svc", timed_out).is_err());
    let unread = Err(SupervisorError::Io("run launchctl: Broken pipe".into()));
    assert!(matches!(status_from_print("gui/501/svc", unread), Err(SupervisorError::Io(_))));
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

/// #894: a bootout that did not answer "not loaded" keeps the plist, so
/// `uninstall` can be retried — and the error says the plist was kept.
#[test]
fn remove_plist_after_a_failed_bootout_keeps_the_plist() {
    let dir = TestRoot::new("kept-plist");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.plist_path("svc");
    let timed_out = Err(SupervisorError::Backend("launchctl bootout timed out".into()));
    let err = remove_plist_after(timed_out, &path).expect_err("bootout failed");
    assert!(path.exists(), "plist removed although the agent may still be loaded");
    assert!(matches!(&err, SupervisorError::Backend(m) if m.contains("kept")), "{err}");
}

#[test]
fn remove_plist_after_a_clean_bootout_removes_it() {
    let dir = TestRoot::new("removed-plist");
    let sup = LaunchAgents::with_agents_dir(dir.path().to_path_buf());
    sup.install(&minimal_spec("svc")).expect("install");
    let path = sup.plist_path("svc");
    remove_plist_after(Ok(()), &path).expect("removed");
    assert!(!path.exists());
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
