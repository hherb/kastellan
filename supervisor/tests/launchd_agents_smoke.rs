//! End-to-end smoke test for the LaunchAgents supervisor.
//!
//! Unlike the unit tests in `src/launchd_agents.rs` (which use a temp
//! `agents_dir` and never invoke `launchctl`), this test exercises
//! the **real** lifecycle: write the plist into
//! `~/Library/LaunchAgents/`, run `launchctl bootstrap gui/<uid>` to
//! load + start, observe `state = running`, run `launchctl bootout`
//! to stop + unload, observe `print` failing (= not loaded),
//! `uninstall` to remove the plist. The whole sequence must be
//! no-trace: even if the test panics partway through, the test
//! guard's Drop calls `uninstall` so we never pollute
//! `~/Library/LaunchAgents/`.
//!
//! Every test that touches launchd skips on hosts where the GUI launchd
//! domain is unreachable (e.g. SSH session without an active console
//! login), printing a `[SKIP]` line to stderr (`cargo test -- --nocapture` to see it) —
//! **unless** `KASTELLAN_PG_REQUIRE_E2E` is truthy, in which case the
//! skip is a failure and a reachable domain prints an
//! `[E2E] supervisor-backed:` line. Run it as evidence with
//! `bash scripts/run-e2e-gate.sh launchd` (#872).

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use kastellan_supervisor::launchd_agents::LaunchAgents;
use kastellan_supervisor::{ServiceSpec, ServiceStatus, Supervisor, TargetSpec};
use kastellan_tests_common::skip::skip_if_no_supervisor;

/// Serialize all smoke-test bodies. The launchd GUI domain and
/// `~/Library/LaunchAgents/` are shared global resources; running
/// these tests in parallel produces races (one test's `bootstrap`
/// or directory churn can disrupt another's mid-flight write).
/// Each test acquires this mutex before doing any I/O and holds it
/// until the end of the test, which gives us deterministic ordering
/// without disabling cargo's default parallel test execution for
/// the whole workspace.
fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Generate a unique, easily-greppable agent label for this run.
///
/// The `kastellan-supervisor-test-` prefix lets a maintainer find and
/// remove leftovers from a crashed test with `find ~/Library/LaunchAgents
/// -name 'kastellan-supervisor-test-*'`.
fn unique_service_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("kastellan-supervisor-test-{}-{}", std::process::id(), nanos)
}

/// RAII guard that ensures we always uninstall the test agent, even
/// if a panic unwinds past the explicit cleanup at the end of the
/// test. Without this, a single failing assertion would leave a
/// stale plist in `~/Library/LaunchAgents/` and (potentially) a
/// loaded entry in the GUI launchd domain across reboots.
struct TestAgentGuard {
    sup: LaunchAgents,
    name: String,
}
impl Drop for TestAgentGuard {
    fn drop(&mut self) {
        // Best-effort cleanup. Errors are ignored so a partial-state
        // test still cleans up as much as it can.
        let _ = self.sup.uninstall(&self.name);
    }
}

/// Poll `status(name)` until it equals `want`, or timeout.
///
/// launchctl is asynchronous: `bootstrap` returns once the agent has
/// been loaded but the program may briefly be in a "spawn scheduled"
/// transitional state before settling at `state = running`. Polling
/// lets us observe a stable terminal state without flaky sleeps.
fn wait_for_status(
    sup: &LaunchAgents,
    name: &str,
    want: ServiceStatus,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let got = sup
            .status(name)
            .map_err(|e| format!("status({name}): {e}"))?;
        if got == want {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out waiting for status={:?}, last observed={:?}",
                want, got
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn install_start_status_stop_uninstall_round_trip() {
    let _serial = serial_lock();
    if skip_if_no_supervisor() {
        return;
    }
    let sup = LaunchAgents::new();
    let name = unique_service_name();
    // `_guard` keeps the cleanup-on-Drop alive; never read directly.
    let _guard = TestAgentGuard {
        sup: LaunchAgents::new(),
        name: name.clone(),
    };

    // Spec: a long-running `/bin/sleep 30` is plenty of time for the
    // assertions; we'll stop it explicitly well before that.
    let spec = ServiceSpec {
        name: name.clone(),
        program: PathBuf::from("/bin/sleep"),
        args: vec!["30".into()],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: None,
        restart_backoff: None,
        environment_files: Vec::new(),
    };

    sup.install(&spec).expect("install");
    // After install but before start: plist exists but isn't loaded
    // into the GUI domain. status() returns Inactive.
    assert_eq!(
        sup.status(&name).expect("status pre-start"),
        ServiceStatus::Inactive,
        "pre-start status must be Inactive (file present, not loaded)"
    );

    sup.start(&name).expect("start");
    wait_for_status(&sup, &name, ServiceStatus::Active, Duration::from_secs(5))
        .expect("agent should become Active within 5s");

    sup.stop(&name).expect("stop");
    wait_for_status(&sup, &name, ServiceStatus::Inactive, Duration::from_secs(5))
        .expect("agent should become Inactive within 5s");

    sup.uninstall(&name).expect("uninstall");
    // After uninstall the plist file is gone; status() must report
    // NotInstalled (and not error).
    assert_eq!(
        sup.status(&name).expect("status post-uninstall"),
        ServiceStatus::NotInstalled
    );

    // Defensive sanity check: the file really is gone.
    assert!(
        !sup.plist_path(&name).exists(),
        "plist file should be removed: {}",
        sup.plist_path(&name).display()
    );
}

#[test]
fn start_after_install_is_idempotent() {
    let _serial = serial_lock();
    if skip_if_no_supervisor() {
        return;
    }
    let sup = LaunchAgents::new();
    let name = unique_service_name();
    let _guard = TestAgentGuard {
        sup: LaunchAgents::new(),
        name: name.clone(),
    };
    let spec = ServiceSpec {
        name: name.clone(),
        program: PathBuf::from("/bin/sleep"),
        args: vec!["30".into()],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: None,
        restart_backoff: None,
        environment_files: Vec::new(),
    };
    sup.install(&spec).expect("install");
    sup.start(&name).expect("first start");
    wait_for_status(&sup, &name, ServiceStatus::Active, Duration::from_secs(5))
        .expect("first start: Active");
    // Second start must succeed (idempotent), not error with
    // "already loaded".
    sup.start(&name).expect("second start (idempotent)");
    // Status is unchanged.
    assert_eq!(
        sup.status(&name).expect("status after second start"),
        ServiceStatus::Active
    );

    // Cleanup is the guard's job; we still call uninstall here so
    // the test exits clean on the happy path.
    sup.uninstall(&name).expect("uninstall");
}

/// The running PID of agent `name`, from `launchctl print`'s `pid = `
/// line; `None` while it is not running or not in the domain at all.
fn running_pid(name: &str) -> Option<String> {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let out = std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{name}")])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = ").map(str::to_string))
}

/// Poll until `name` runs under a PID other than `not`, or `timeout`.
fn wait_for_pid_other_than(name: &str, not: Option<&str>, timeout: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(pid) = running_pid(name).filter(|p| Some(p.as_str()) != not) {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// An agent that exits 2 s after SIGTERM, like a database flushing on
/// shutdown. (`sleep & wait`, not a foreground `sleep`: `wait` is
/// interrupted by the signal, so the trap runs at once.)
fn slow_exiting_spec(name: &str) -> ServiceSpec {
    ServiceSpec {
        name: name.to_string(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            "trap 'sleep 2; exit 0' TERM; while :; do sleep 1 & wait $!; done".into(),
        ],
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

/// `stop_target` immediately followed by `start_target` must leave the
/// agent running again — as a **new** process — even when the old one
/// takes a while to exit.
///
/// This is exactly what `kastellan-cli install` does. `launchctl bootout`
/// can return before launchd has finished removing a slow-to-exit agent,
/// and `start` skips `bootstrap` for an agent still in the domain. Without
/// the wait in `stop_target` the `bootstrap` was skipped and the agent then
/// vanished. The live Mac install lost Postgres this way on 2026-10-18.
///
/// Asserting a different PID (rather than `Active` after a sleep) needs no
/// timing margin: a skipped `bootstrap` only ever shows the old PID, until
/// it exits, and then none.
#[test]
fn start_target_right_after_stop_target_leaves_a_slow_exiting_agent_running() {
    let _serial = serial_lock();
    if skip_if_no_supervisor() {
        return;
    }
    let sup = LaunchAgents::new();
    let name = unique_service_name();
    let _guard = TestAgentGuard {
        sup: LaunchAgents::new(),
        name: name.clone(),
    };
    let target = TargetSpec {
        name: format!("{name}-target"),
        members: vec![name.clone()],
    };
    sup.install_target(&target, &[slow_exiting_spec(&name)])
        .expect("install_target");
    sup.start_target(&target).expect("first start_target");
    let old = wait_for_pid_other_than(&name, None, Duration::from_secs(5))
        .expect("first start: the agent must be running");

    sup.stop_target(&target).expect("stop_target");
    sup.start_target(&target).expect("start_target right after stop_target");
    let new = wait_for_pid_other_than(&name, Some(&old), Duration::from_secs(6));
    assert!(
        new.is_some(),
        "the agent must run again, under a new PID, after stop_target + start_target \
         (old PID {old})"
    );

    sup.uninstall_target(&target).expect("uninstall_target");
}

/// `stop` itself must NOT wait for the agent to exit: PG tests call it
/// while their own pools may still hold connections, and Postgres's
/// SIGTERM shutdown waits for those clients — a waiting `stop` would
/// block until launchd's SIGKILL and leak the cluster's SysV shared
/// memory (#870's first attempt failed 154 PG tests exactly this way).
#[test]
fn stop_returns_without_waiting_for_a_slow_exiting_agent() {
    let _serial = serial_lock();
    if skip_if_no_supervisor() {
        return;
    }
    let sup = LaunchAgents::new();
    let name = unique_service_name();
    let _guard = TestAgentGuard {
        sup: LaunchAgents::new(),
        name: name.clone(),
    };
    sup.install(&slow_exiting_spec(&name)).expect("install");
    sup.start(&name).expect("start");
    wait_for_pid_other_than(&name, None, Duration::from_secs(5))
        .expect("the agent must be running");

    let t = Instant::now();
    sup.stop(&name).expect("stop");
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "stop took {:?} on an agent that exits 2 s after SIGTERM — it must not wait",
        t.elapsed()
    );

    sup.uninstall(&name).expect("uninstall");
}

#[test]
fn stop_when_not_started_is_idempotent() {
    let _serial = serial_lock();
    if skip_if_no_supervisor() {
        return;
    }
    let sup = LaunchAgents::new();
    let name = unique_service_name();
    let _guard = TestAgentGuard {
        sup: LaunchAgents::new(),
        name: name.clone(),
    };
    let spec = ServiceSpec {
        name: name.clone(),
        program: PathBuf::from("/bin/sleep"),
        args: vec!["30".into()],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: None,
        restart_backoff: None,
        environment_files: Vec::new(),
    };
    sup.install(&spec).expect("install");
    // Calling stop before start: the agent isn't bootstrapped, so
    // `bootout` fails with "could not find service". `stop` must
    // swallow that and return Ok.
    sup.stop(&name).expect("stop on not-started must be idempotent");
    sup.uninstall(&name).expect("uninstall");
}

#[test]
fn invalid_name_is_rejected_before_any_launchctl_call() {
    // No probe: name validation is pure and runs before any side
    // effect, so this test must pass even on hosts without a GUI
    // domain.
    let sup = LaunchAgents::new();
    let err = sup
        .start("../etc/passwd")
        .expect_err("traversal name must be rejected");
    assert!(
        matches!(err, kastellan_supervisor::SupervisorError::InvalidName(_)),
        "expected InvalidName, got: {err}"
    );
}
