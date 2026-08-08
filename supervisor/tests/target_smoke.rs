//! End-to-end smoke test for the target bring-up (`install_target` →
//! `start_target` → `stop_target` → `uninstall_target`).
//!
//! Linux exercises the native `kastellan.target` (real `systemctl --user`).
//! macOS exercises the generic readiness-based bundle (real `launchctl`).
//! Both use trivial long-running dummy programs (`sleep`) so the test
//! validates the *target orchestration mechanics* in isolation — real
//! Postgres + core bring-up is a heavier system test, out of scope here.
//!
//! Skips silently (`[SKIP]` on `--nocapture`) when the per-user service
//! manager is unreachable, mirroring `systemd_user_smoke.rs`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kastellan_supervisor::{ServiceSpec, ServiceStatus, Supervisor, TargetSpec};

/// Generate a unique, easily-greppable service name for this run.
///
/// The counter is load-bearing, not decoration. Every test in this binary
/// shares a pid *and* the same three prefixes, so a bare `pid + nanos`
/// suffix leaves the clock as the only discriminator — and two tests
/// running in parallel can read the same tick and produce the same unit
/// name. The symptom that exposed it was a race on one
/// `<name>.service.tmp`, surfacing as a bogus `install_target` I/O error
/// in an unrelated test; that staging path is now unique per writer
/// (the crate's shared `atomic_write` helper), but a shared unit name
/// would still collide on the live manager itself. The process-wide
/// `AtomicU64` makes uniqueness deterministic instead of
/// clock-granularity-dependent, mirroring `TestRoot` in
/// `src/systemd_user/tests.rs` (issue #104 tracks the same pattern
/// elsewhere in the workspace).
///
/// See `systemd_user_smoke.rs` for how to sweep leftovers from a run that
/// was killed hard enough to skip `Guard`'s Drop — since #508 that can
/// include an enable symlink under `default.target.wants/`, which the user
/// manager fails to start on every login until it is removed.
fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{}-{}", std::process::id(), nanos, n)
}

fn dummy_spec(name: &str, target: &str, after: Vec<String>) -> ServiceSpec {
    ServiceSpec {
        name: name.into(),
        program: PathBuf::from(SLEEP_BIN),
        args: vec!["30".into()],
        env: vec![],
        working_dir: None,
        keep_alive: false,
        stdout_log: None,
        stderr_log: None,
        after,
        part_of: Some(target.into()),
        restart_backoff: None,
        environment_files: Vec::new(),
    }
}

#[cfg(target_os = "linux")]
const SLEEP_BIN: &str = "/usr/bin/sleep";
#[cfg(target_os = "macos")]
const SLEEP_BIN: &str = "/bin/sleep";

fn wait_for(
    sup: &dyn Supervisor,
    name: &str,
    want: ServiceStatus,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let got = sup.status(name).map_err(|e| format!("status({name}): {e}"))?;
        if got == want {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout waiting status={want:?}, last={got:?}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use kastellan_supervisor::systemd_user::{probe, SystemdUser};

    struct Guard {
        sup: SystemdUser,
        target: TargetSpec,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.sup.uninstall_target(&self.target);
        }
    }

    #[test]
    fn target_round_trip_native_systemd() {
        if let Err(e) = probe() {
            eprintln!("\n[SKIP] systemctl --user probe failed: {e}\n");
            return;
        }
        let sup = SystemdUser::new();
        let target_name = unique("kastellan-test-target");
        let pg = unique("kastellan-test-pg");
        let core = unique("kastellan-test-core");
        let target = TargetSpec {
            name: target_name.clone(),
            members: vec![pg.clone(), core.clone()],
        };
        let _guard = Guard {
            sup: SystemdUser::new(),
            target: target.clone(),
        };

        let members = [
            dummy_spec(&pg, &target_name, vec![]),
            dummy_spec(&core, &target_name, vec![pg.clone()]),
        ];
        sup.install_target(&target, &members).expect("install_target");

        // The target unit Wants= both members; core is ordered After= pg.
        let units = sup.units_dir();
        let target_body =
            std::fs::read_to_string(units.join(format!("{target_name}.target"))).expect("target unit");
        assert!(target_body.contains(&format!("Wants={pg}.service {core}.service\n")), "{target_body}");
        let core_body =
            std::fs::read_to_string(units.join(format!("{core}.service"))).expect("core unit");
        assert!(core_body.contains(&format!("After={pg}.service\n")), "{core_body}");

        sup.start_target(&target).expect("start_target");
        wait_for(&sup, &pg, ServiceStatus::Active, Duration::from_secs(5)).expect("pg active");
        wait_for(&sup, &core, ServiceStatus::Active, Duration::from_secs(5)).expect("core active");

        sup.stop_target(&target).expect("stop_target");
        wait_for(&sup, &core, ServiceStatus::Inactive, Duration::from_secs(5)).expect("core inactive");
        wait_for(&sup, &pg, ServiceStatus::Inactive, Duration::from_secs(5)).expect("pg inactive");

        sup.uninstall_target(&target).expect("uninstall_target");
        assert_eq!(sup.status(&pg).unwrap(), ServiceStatus::NotInstalled);
        assert_eq!(sup.status(&core).unwrap(), ServiceStatus::NotInstalled);
    }

    /// Ask the live user manager whether `unit` is enabled. Trusts stdout
    /// and ignores the exit code (a removed unit prints nothing), mirroring
    /// `systemd_user_smoke.rs`.
    fn is_enabled_state(unit: &str) -> String {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", unit])
            .output()
            .expect("spawn systemctl is-enabled");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn install_target_enables_the_target_so_the_bundle_comes_back_after_a_reboot() {
        // Regression test for #508 — the leg that actually matters in a real
        // deployment. `kastellan.target` is the boot entry point: it is what
        // carries `WantedBy=default.target`, and its `Wants=` line pulls the
        // members in. If the target is never enabled, a rebooted host runs
        // nothing, and `loginctl enable-linger` does not help — lingering
        // starts the user *manager*, but with nothing wanted by
        // `default.target` there is nothing for it to start.
        if let Err(e) = probe() {
            eprintln!("\n[SKIP] systemctl --user probe failed: {e}\n");
            return;
        }
        let sup = SystemdUser::new();
        let target_name = unique("kastellan-test-target");
        let pg = unique("kastellan-test-pg");
        let core = unique("kastellan-test-core");
        let target = TargetSpec {
            name: target_name.clone(),
            members: vec![pg.clone(), core.clone()],
        };
        let _guard = Guard {
            sup: SystemdUser::new(),
            target: target.clone(),
        };

        let members = [
            dummy_spec(&pg, &target_name, vec![]),
            dummy_spec(&core, &target_name, vec![pg.clone()]),
        ];
        sup.install_target(&target, &members).expect("install_target");

        let unit = format!("{target_name}.target");
        assert_eq!(
            is_enabled_state(&unit),
            "enabled",
            "install_target must enable the target, or the bundle will not \
             come back after a reboot"
        );
        let wants_link = sup
            .units_dir()
            .join("default.target.wants")
            .join(&unit);
        assert!(
            wants_link.exists(),
            "expected a default.target.wants symlink at {}",
            wants_link.display()
        );

        // Members are deliberately NOT enabled: the target unit already
        // carries an explicit `Wants=<member>.service` line, which pulls them
        // in whenever the target starts. Enabling them as well would add a
        // second, redundant link expressing the same edge — pinned here so a
        // later change has to be deliberate rather than accidental.
        assert_ne!(
            is_enabled_state(&format!("{core}.service")),
            "enabled",
            "members are pulled in by the target's Wants=, not by their own \
             enable link"
        );

        // Symmetry: tearing the target down must not leave a dangling
        // symlink in the user's real `default.target.wants/`.
        sup.uninstall_target(&target).expect("uninstall_target");
        assert_ne!(
            is_enabled_state(&unit),
            "enabled",
            "uninstall_target must disable the target"
        );
        assert!(
            !wants_link.exists(),
            "uninstall_target left a dangling symlink at {}",
            wants_link.display()
        );
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use kastellan_supervisor::launchd_agents::{probe, LaunchAgents};

    struct Guard {
        sup: LaunchAgents,
        target: TargetSpec,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.sup.uninstall_target(&self.target);
        }
    }

    #[test]
    fn target_round_trip_generic_bundle() {
        if let Err(e) = probe() {
            eprintln!("\n[SKIP] launchctl probe failed: {e}\n");
            return;
        }
        let sup = LaunchAgents::new();
        let target_name = unique("kastellan-test-target");
        let pg = unique("kastellan-test-pg");
        let core = unique("kastellan-test-core");
        let target = TargetSpec {
            name: target_name.clone(),
            members: vec![pg.clone(), core.clone()],
        };
        let _guard = Guard {
            sup: LaunchAgents::new(),
            target: target.clone(),
        };

        let members = [
            dummy_spec(&pg, &target_name, vec![]),
            dummy_spec(&core, &target_name, vec![pg.clone()]),
        ];
        sup.install_target(&target, &members).expect("install_target");
        sup.start_target(&target).expect("start_target");
        wait_for(&sup, &pg, ServiceStatus::Active, Duration::from_secs(5)).expect("pg active");
        wait_for(&sup, &core, ServiceStatus::Active, Duration::from_secs(5)).expect("core active");

        sup.stop_target(&target).expect("stop_target");
        // launchctl bootout is synchronous, so no wait_for poll is needed
        // here (unlike the systemd path, where stop returns asynchronously).
        sup.uninstall_target(&target).expect("uninstall_target");
        assert_eq!(sup.status(&pg).unwrap(), ServiceStatus::NotInstalled);
        assert_eq!(sup.status(&core).unwrap(), ServiceStatus::NotInstalled);
    }
}
