//! End-to-end smoke test for the SystemdUser supervisor.
//!
//! Unlike the unit tests in `src/systemd_user.rs` (which use a temp
//! `units_dir` and never invoke `systemctl --user`), this test
//! exercises the **real** lifecycle: write the unit into
//! `~/.config/systemd/user/`, run `daemon-reload`, `start`, observe
//! `is-active=active`, `stop`, observe `is-active=inactive`,
//! `uninstall`. The whole sequence must be no-trace: even if the test
//! panics partway through, the test guard's Drop cleans up the unit
//! file and runs `daemon-reload` so we never pollute the user's real
//! systemd config.
//!
//! The test skips silently on hosts where `systemctl --user` cannot
//! reach a live user manager (e.g. headless boxes without
//! `loginctl enable-linger`). Skipped runs print a `[SKIP]` line to
//! stderr (`cargo test -- --nocapture` to see them), mirroring the
//! pattern in `sandbox/tests/linux_smoke.rs`.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use kastellan_supervisor::systemd_user::{probe, SystemdUser};
use kastellan_supervisor::{ServiceSpec, ServiceStatus, Supervisor};

/// Skip the test when there's no usable user manager to talk to.
fn skip_if_no_user_manager() -> bool {
    match probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] systemctl --user probe failed: {e}\n");
            true
        }
    }
}

/// Generate a unique, easily-greppable service name for this run.
///
/// The `kastellan-supervisor-test-` prefix lets a maintainer find and
/// remove leftovers from a crashed test with a single `find` command.
fn unique_service_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("kastellan-supervisor-test-{}-{}", std::process::id(), nanos)
}

/// RAII guard that ensures we always uninstall the test unit, even
/// if a panic unwinds past the explicit cleanup at the end of the
/// test. Without this, a single failing assertion would leave a
/// stale unit file in `~/.config/systemd/user/`.
struct TestUnitGuard {
    sup: SystemdUser,
    name: String,
}
impl Drop for TestUnitGuard {
    fn drop(&mut self) {
        // Best-effort cleanup. We deliberately ignore errors here so
        // a partial-state test still cleans up as much as it can.
        let _ = self.sup.uninstall(&self.name);
    }
}

/// Poll `status(name)` until it equals `want`, or timeout.
///
/// systemctl is asynchronous: `start` returns once the service has
/// been kicked off, but `is-active` may briefly report
/// `activating`/`deactivating` while transitioning. Polling lets us
/// observe a stable terminal state without flaky sleeps.
fn wait_for_status(
    sup: &SystemdUser,
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
    if skip_if_no_user_manager() {
        return;
    }
    let sup = SystemdUser::new();
    let name = unique_service_name();
    // `_guard` keeps the cleanup-on-Drop alive; never read directly.
    let _guard = TestUnitGuard {
        sup: SystemdUser::new(),
        name: name.clone(),
    };

    // Spec: a long-running `sleep 30` is plenty of time for the
    // assertions; we'll stop it explicitly well before that.
    let spec = ServiceSpec {
        name: name.clone(),
        program: PathBuf::from("/usr/bin/sleep"),
        args: vec!["30".into()],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after: vec![],
        part_of: None,
        restart_backoff: None,
        environment_file: None,
    };

    sup.install(&spec).expect("install");
    // After install + daemon-reload but before start: the unit
    // exists on disk but the manager hasn't activated it.
    assert_eq!(
        sup.status(&name).expect("status pre-start"),
        ServiceStatus::Inactive,
        "pre-start status must be Inactive"
    );

    sup.start(&name).expect("start");
    wait_for_status(&sup, &name, ServiceStatus::Active, Duration::from_secs(5))
        .expect("service should become Active within 5s");

    sup.stop(&name).expect("stop");
    wait_for_status(&sup, &name, ServiceStatus::Inactive, Duration::from_secs(5))
        .expect("service should become Inactive within 5s");

    sup.uninstall(&name).expect("uninstall");
    // After uninstall the unit file is gone; status() must report
    // NotInstalled (and not error).
    assert_eq!(
        sup.status(&name).expect("status post-uninstall"),
        ServiceStatus::NotInstalled
    );

    // Defensive sanity check: the file really is gone.
    assert!(
        !sup.unit_path(&name).exists(),
        "unit file should be removed: {}",
        sup.unit_path(&name).display()
    );

    // And systemctl agrees nothing's loaded with this name.
    let out = Command::new("systemctl")
        .args(["--user", "list-units", "--all", "--no-legend", "--no-pager"])
        .output()
        .expect("list-units");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains(&name),
        "systemctl still lists the unit after uninstall:\n{stdout}"
    );
}

#[test]
fn invalid_name_is_rejected_before_any_systemctl_call() {
    // No probe: name validation is pure and runs before any side
    // effect, so this test must pass even on hosts without a user
    // manager.
    let sup = SystemdUser::new();
    let err = sup
        .start("../etc/passwd")
        .expect_err("traversal name must be rejected");
    assert!(
        matches!(err, kastellan_supervisor::SupervisorError::InvalidName(_)),
        "expected InvalidName, got: {err}"
    );
}
