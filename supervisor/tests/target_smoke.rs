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
use std::time::{Duration, Instant};

use kastellan_supervisor::{ServiceSpec, ServiceStatus, Supervisor, TargetSpec};

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{}", std::process::id(), nanos)
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
