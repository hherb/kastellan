//! End-to-end smoke for the cross-platform supervisor wiring.
//!
//! Builds a [`ServiceSpec`] for the hhagent core daemon via the typed
//! helper [`hhagent_supervisor::specs::core_service_spec`], then drives
//! it through [`hhagent_supervisor::default_supervisor`] (so the test
//! exercises [`hhagent_supervisor::systemd_user::SystemdUser`] on
//! Linux and [`hhagent_supervisor::launchd_agents::LaunchAgents`] on
//! macOS without per-OS branching). Verifies install → start →
//! observe-the-daemon's-startup-log-line → stop → uninstall.
//!
//! This is the "first concrete service" item in the ROADMAP — it
//! proves both supervisor backends can host the real `hhagent` binary
//! and pins the observable startup contract: today's daemon
//! (`core/src/main.rs`) is a placeholder that emits one JSON-formatted
//! tracing line ("hhagent core starting (skeleton)" with a `version`
//! field) and exits 0. We observe via the supervisor's stdout
//! redirect, not by trying to catch the brief `Active` window.
//!
//! Skips silently on hosts where the supervisor probe fails:
//!   - Linux: headless session without `loginctl enable-linger`
//!     (so `systemctl --user` cannot reach the user manager).
//!   - macOS: SSH-only session (so `launchctl gui/<uid>` is
//!     unreachable, no console login).
//! Skipped runs print `[SKIP]` to stderr; `cargo test -- --nocapture`
//! to see them.
//!
//! Other-Unix builds are excluded entirely — no supervisor backend.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hhagent_supervisor::specs::core_service_spec;
use hhagent_supervisor::{
    default_probe, default_supervisor, ServiceStatus, Supervisor,
};

/// On macOS, `~/Library/LaunchAgents/` and the GUI launchd domain are
/// shared global resources. The supervisor crate's launchd smoke test
/// uses an intra-binary static mutex; we mirror that here so the two
/// tests don't race when both run during `cargo test --workspace`.
/// Linux's systemd-user namespace is per-user but per-test unique
/// names already prevent collisions, so the lock is a no-op there.
#[cfg(target_os = "macos")]
fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Skip if the supervisor backend can't talk to its underlying
/// service manager on this host. See module doc for the two known
/// causes per OS.
fn skip_if_no_supervisor() -> bool {
    match default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

/// Locate the freshly-built `hhagent` binary in `target/debug/`.
///
/// Mirrors the `worker_binary()` helper in `shell_exec_e2e.rs`.
/// `CARGO_TARGET_DIR` overrides the conventional path so a workspace
/// with a non-standard target dir still works.
fn core_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("hhagent")
}

/// Process+timestamp-unique name; the `hhagent-supervisor-test-`
/// prefix matches the convention in the supervisor crate's smoke
/// tests so a single `find` cleans up post-crash residue from any
/// of them. Globally unique per-OS within a single test run, which
/// is the only collision domain that matters here.
fn unique_test_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("hhagent-supervisor-test-{}-{}", std::process::id(), nanos)
}

/// RAII guard: removes the test service even if the body panics
/// midway. Without this a single failing assertion would leave a
/// stale unit/agent in the user's real supervisor dir.
struct ServiceGuard {
    sup: Box<dyn Supervisor>,
    name: String,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        // Best-effort: ignore errors so a partial-state test still
        // tries to clean up everything it can.
        let _ = self.sup.uninstall(&self.name);
    }
}

/// RAII guard: removes the per-test log directory even if the body
/// panics midway. Paired with [`ServiceGuard`] so a failure leaves
/// neither a stale unit/agent nor an orphaned `temp_dir/hhagent-…/`
/// behind. Best-effort like the service guard.
struct LogDirGuard {
    path: PathBuf,
}
impl Drop for LogDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Poll `path` until it exists and `predicate(contents)` returns
/// true, or `timeout` elapses. Returns the matching contents on
/// success, or a diagnostic string with whatever was actually
/// observed on timeout.
///
/// Used to wait for the daemon's startup log line. Polling rather
/// than sleeping keeps the test fast on a healthy host (typically
/// well under 200 ms) without making it flaky on a slow one.
fn wait_for_log_match<F: Fn(&str) -> bool>(
    path: &Path,
    predicate: F,
    timeout: Duration,
) -> Result<String, String> {
    let start = Instant::now();
    loop {
        if let Ok(body) = std::fs::read_to_string(path) {
            if predicate(&body) {
                return Ok(body);
            }
        }
        if start.elapsed() > timeout {
            let observed = std::fs::read_to_string(path).unwrap_or_default();
            return Err(format!(
                "timed out after {:?} waiting for predicate; log file at {}:\n---\n{}\n---",
                timeout,
                path.display(),
                observed
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn core_service_install_start_observe_log_uninstall() {
    // Hold the macOS-only mutex for the full body so the launchd
    // domain is never being touched concurrently by the launchd
    // smoke test in the supervisor crate.
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() {
        return;
    }
    let binary = core_binary();
    if !binary.exists() {
        eprintln!(
            "\n[SKIP] hhagent binary not found at {}; run `cargo build --workspace` first\n",
            binary.display()
        );
        return;
    }

    // Per-test log dir under temp_dir keeps tests independent and
    // avoids touching `~/.local/state/hhagent/` (which a real
    // installed core daemon might own).
    let log_dir = std::env::temp_dir().join(format!(
        "hhagent-supervisor-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&log_dir).expect("create per-test log dir");
    let _log_dir_guard = LogDirGuard { path: log_dir.clone() };

    // Build the canonical spec, then rename so concurrent test runs
    // don't collide on the single shared `hhagent-core` name and so a
    // real installed `hhagent-core` service on the host is never
    // clobbered. The log filenames also need to follow the new name
    // so we read back the right file below.
    let mut spec = core_service_spec(&binary, &log_dir);
    spec.name = unique_test_name();
    // Cheap insurance against a future change to the name template that
    // could push past either backend's 200-char limit. Today's worst
    // case is ~54 chars; this trips well before `install` would.
    assert!(
        spec.name.len() <= 200,
        "constructed test name {} chars exceeds backend MAX_NAME_LEN=200; rework unique_test_name()",
        spec.name.len()
    );
    let stdout_path = log_dir.join(format!("{}.out", spec.name));
    let stderr_path = log_dir.join(format!("{}.err", spec.name));
    spec.stdout_log = Some(stdout_path.clone());
    spec.stderr_log = Some(stderr_path.clone());

    let sup = default_supervisor();
    let _guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };

    sup.install(&spec).expect("install via default_supervisor");
    assert_eq!(
        sup.status(&spec.name).expect("status pre-start"),
        ServiceStatus::Inactive,
        "post-install, pre-start status must be Inactive"
    );

    sup.start(&spec.name).expect("start");

    // The daemon as currently shaped logs one JSON line and exits 0
    // (`core/src/main.rs`). Both supervisors will see clean exit and
    // mark the service Inactive almost immediately. We don't try to
    // catch the brief Active window — we observe the durable side
    // effect (the log line appearing in the redirected stdout file).
    let body = wait_for_log_match(
        &stdout_path,
        |s| s.contains("hhagent core starting"),
        Duration::from_secs(5),
    )
    .expect("daemon should write its startup log line within 5s");

    // Pin the JSON shape: a future change to main.rs that drops the
    // version field or swaps tracing-subscriber away from JSON would
    // trip here. (We don't parse the JSON — substring match is enough
    // for a contract test, and avoids pulling serde_json into core's
    // dev-deps just for this.)
    assert!(
        body.contains("\"version\":"),
        "log line should be JSON with a version field, got:\n{body}"
    );

    // `stop` is a no-op when the process has already exited — both
    // `systemctl --user stop` and `launchctl bootout` succeed (or
    // map their idempotent error to Ok) in that case. Asserting Ok
    // here pins the "stop is always safe" contract.
    sup.stop(&spec.name).expect("stop must be safe even after natural exit");

    sup.uninstall(&spec.name).expect("uninstall");
    assert_eq!(
        sup.status(&spec.name).expect("status post-uninstall"),
        ServiceStatus::NotInstalled,
        "post-uninstall status must be NotInstalled"
    );

    // `LogDirGuard` (declared at the top of the test) removes the
    // per-test log dir on drop — both on the success path here and on
    // any panic midway.
}
