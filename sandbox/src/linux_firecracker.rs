//! Linux micro-VM backend for [`SandboxBackend`]: boots a Firecracker guest
//! and bridges the worker's JSON-RPC stdio over vsock.
//!
//! Defense-in-depth on top of (not instead of) bwrap/seccomp/Landlock/cgroup:
//! a throwaway guest kernel is the blast wall. The backend itself is a thin
//! pure-fn-then-spawn shell (mirrors [`crate::linux_bwrap`]); the boot + vsock
//! bridge live in the `kastellan-microvm-run` launcher binary that this
//! backend spawns as the `Child`.
//!
//! All of this module is `#[cfg(target_os = "linux")]`-gated (see lib.rs).

mod plan;
pub use plan::{
    build_launch_plan, render_firecracker_config, FirecrackerImage, FirecrackerLaunchPlan,
    WORKER_VSOCK_PORT,
};

mod probe;
pub use probe::{probe_report, ProbeInputs};

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

/// The launcher binary name; discovered on `$PATH` / next to the daemon.
pub const MICROVM_RUN_BIN: &str = "kastellan-microvm-run";

/// Pure: the launcher argv for a plan + its rendered config/log paths.
pub fn launcher_argv(plan: &FirecrackerLaunchPlan, config_path: &str, log_path: &str) -> Vec<String> {
    vec![
        MICROVM_RUN_BIN.into(),
        "--config-file".into(), config_path.into(),
        "--vsock-uds".into(), plan.vsock_uds.to_string_lossy().into_owned(),
        "--vsock-port".into(), plan.vsock_port.to_string(),
        "--log".into(), log_path.into(),
    ]
}

/// Counter for unique per-spawn temp dir suffixes (avoids collisions on rapid
/// successive spawns within the same process without requiring the `tempfile` crate).
static SPAWN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create a unique per-spawn temp directory under the system temp dir using
/// only `std` — no `tempfile` dependency (which is not in `kastellan-sandbox`'s
/// `Cargo.toml`). The PID + atomic counter pair guarantees uniqueness across
/// concurrent spawns within one process and across multiple daemon instances
/// sharing the same `/tmp`.
fn make_spawn_dir() -> Result<std::path::PathBuf, SandboxError> {
    let seq = SPAWN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("kastellan-microvm-{}-{}", std::process::id(), seq));
    std::fs::create_dir_all(&dir)
        .map_err(|e| SandboxError::Backend(format!("create per-spawn dir {dir:?}: {e}")))?;
    Ok(dir)
}

/// Boots workers inside a Firecracker micro-VM. Holds no mutable state
/// (`Send + Sync` via the empty struct), matching the other backends.
#[derive(Default)]
pub struct LinuxFirecracker;

impl LinuxFirecracker {
    pub fn new() -> Self {
        Self
    }
}

impl SandboxBackend for LinuxFirecracker {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<Child, SandboxError> {
        // Image dir comes from the worker's policy env (set by the entry) —
        // KASTELLAN_MICROVM_DIR — defaulting to /var/lib/kastellan/microvm.
        let dir = policy
            .env
            .iter()
            .find(|(k, _)| k == "KASTELLAN_MICROVM_DIR")
            .map(|(_, v)| std::path::PathBuf::from(v))
            .unwrap_or_else(|| "/var/lib/kastellan/microvm".into());
        let image = FirecrackerImage {
            kernel_path: dir.join("vmlinux"),
            rootfs_path: dir.join("python-exec.ext4"),
        };
        let plan = build_launch_plan(policy, &image, program, args)?;
        // Per-spawn temp dir for the config + log files. No new dep: uses std
        // only (atomic counter + create_dir_all).
        let run_dir = make_spawn_dir()?;
        let config_path = run_dir.join("fc.json");
        let log_path = run_dir.join("fc.log");
        std::fs::write(&config_path, render_firecracker_config(&plan).to_string())
            .map_err(|e| SandboxError::Backend(format!("write fc config: {e}")))?;
        let argv = launcher_argv(
            &plan,
            &config_path.to_string_lossy(),
            &log_path.to_string_lossy(),
        );
        Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SandboxError::Backend(format!("microvm-run spawn failed: {e}")))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod spawn_tests {
    use super::*;
    use crate::SandboxPolicy;

    #[test]
    fn launcher_argv_passes_config_and_vsock() {
        let plan = plan::build_launch_plan(
            &SandboxPolicy::default(),
            &FirecrackerImage {
                kernel_path: "/k".into(),
                rootfs_path: "/var/r.ext4".into(),
            },
            "/w",
            &[],
        )
        .unwrap();
        let argv = launcher_argv(&plan, "/run/fc.json", "/run/fc.log");
        assert_eq!(argv[0], MICROVM_RUN_BIN);
        assert!(
            argv.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"),
            "argv must pass --config-file /run/fc.json"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--vsock-port" && w[1] == plan.vsock_port.to_string()),
            "argv must pass --vsock-port <port>"
        );
    }
}
