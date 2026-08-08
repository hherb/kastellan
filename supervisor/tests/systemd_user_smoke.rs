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
use std::sync::atomic::{AtomicU64, Ordering};
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
/// The `AtomicU64` is load-bearing: every test here shares a pid *and* a
/// prefix, so a bare `pid + nanos` suffix leaves the clock as the only
/// discriminator and two parallel tests can read the same tick. The
/// symptom that first exposed it was a race on one `<name>.service.tmp`;
/// that staging path is now unique per writer (the crate's shared
/// `atomic_write` helper), but two tests sharing a *unit name* would
/// still collide on the live manager — enabling, uninstalling and
/// disabling each other's unit. The counter makes uniqueness
/// deterministic rather than clock-granularity-dependent (cf. `TestRoot` in
/// `src/systemd_user/tests.rs`; issue #104 tracks the pattern elsewhere).
///
/// **Cleaning up after a crashed run.** `TestUnitGuard`'s Drop covers a
/// panic but not a SIGKILL, and since #508 `install` also *enables* the
/// unit — so a hard-killed run can strand a symlink in the operator's real
/// `default.target.wants/`, which the user manager then fails to start on
/// every login (a stray unit file alone was merely inert). Both live under
/// `~/.config/systemd/user`, and `target_smoke.rs` uses its own
/// `kastellan-test-` prefix, so a sweep needs both patterns:
///
/// ```sh
/// find ~/.config/systemd/user \
///   \( -name 'kastellan-supervisor-test-*' -o -name 'kastellan-test-*' \) -delete
/// systemctl --user daemon-reload
/// ```
fn unique_service_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "kastellan-supervisor-test-{}-{}-{}",
        std::process::id(),
        nanos,
        n
    )
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
        environment_files: Vec::new(),
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

/// Ask the live user manager whether `unit` is enabled.
///
/// `systemctl --user is-enabled` prints the canonical state on stdout
/// (`enabled` / `disabled` / `static` / …) and uses the exit code to
/// signal it as well. We trust stdout and ignore the exit code, exactly
/// as [`SystemdUser::status`] does for `is-active` — a unit that has been
/// removed prints nothing and exits non-zero, which trims to the empty
/// string and is therefore distinguishable from `"enabled"`.
fn is_enabled_state(unit: &str) -> String {
    let out = Command::new("systemctl")
        .args(["--user", "is-enabled", unit])
        .output()
        .expect("spawn systemctl is-enabled");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn install_enables_the_unit_so_it_comes_back_after_a_reboot() {
    // Regression test for #508. Installing a unit is not enough to make
    // it start again after a reboot: the user manager starts
    // `default.target`, and only units linked into `default.target.wants/`
    // by `systemctl --user enable` are pulled in. Without the enable, a
    // rebooted Linux host comes up with kastellan simply not running —
    // while macOS comes back, because `RunAtLoad=true` in the plist is
    // unconditional. This test pins the parity.
    if skip_if_no_user_manager() {
        return;
    }
    let sup = SystemdUser::new();
    let name = unique_service_name();
    let _guard = TestUnitGuard {
        sup: SystemdUser::new(),
        name: name.clone(),
    };

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
        environment_files: Vec::new(),
    };

    sup.install(&spec).expect("install");

    let unit = format!("{name}.service");
    assert_eq!(
        is_enabled_state(&unit),
        "enabled",
        "install must enable the unit, or it will not start after a reboot"
    );

    // The enable must be what the generated `[Install] WantedBy=` asks
    // for: a standalone service (`part_of: None`) is wanted by
    // `default.target`, so that is where the symlink belongs. Asserting
    // the link itself — not just systemctl's summary word — is what
    // actually proves the boot path is wired.
    let wants_link = sup
        .units_dir()
        .join("default.target.wants")
        .join(format!("{name}.service"));
    assert!(
        wants_link.exists(),
        "expected a default.target.wants symlink at {}",
        wants_link.display()
    );

    // Symmetry: uninstall must leave nothing behind. `uninstall` already
    // ran a best-effort `disable`, so the link must be gone with the unit.
    sup.uninstall(&name).expect("uninstall");
    assert_ne!(
        is_enabled_state(&unit),
        "enabled",
        "uninstall must leave the unit disabled"
    );
    assert!(
        !wants_link.exists(),
        "uninstall left a dangling symlink at {}",
        wants_link.display()
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
