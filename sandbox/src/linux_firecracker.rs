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

mod mounts;
pub use mounts::{encode_mount_manifest, reserved_top_level, RoShare, RwScratch};

mod probe;
pub use probe::{probe_report, ProbeInputs};

mod cleanup;
pub use cleanup::{
    orphaned_run_dir_should_remove, pid_is_alive, sweep_orphaned_run_dirs, LAUNCHER_PID_FILE,
    RUN_DIR_PREFIX,
};

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

/// The launcher binary name; discovered on `$PATH` / next to the daemon.
pub const MICROVM_RUN_BIN: &str = "kastellan-microvm-run";

/// Pure: the launcher argv for a plan + its rendered config/log/run-dir paths.
pub fn launcher_argv(
    plan: &FirecrackerLaunchPlan,
    config_path: &str,
    log_path: &str,
    run_dir: &str,
) -> Vec<String> {
    vec![
        MICROVM_RUN_BIN.into(),
        "--config-file".into(), config_path.into(),
        "--vsock-uds".into(), plan.vsock_uds.to_string_lossy().into_owned(),
        "--vsock-port".into(), plan.vsock_port.to_string(),
        "--log".into(), log_path.into(),
        "--run-dir".into(), run_dir.into(),
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
///
/// The name is built from [`cleanup::RUN_DIR_PREFIX`] so the orphan sweep's
/// prefix match (#362) can never silently drift out of sync with the dirs it is
/// meant to GC — the producer and the matcher share one constant.
fn make_spawn_dir() -> Result<std::path::PathBuf, SandboxError> {
    let seq = SPAWN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "{}{}-{}",
        cleanup::RUN_DIR_PREFIX,
        std::process::id(),
        seq
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| SandboxError::Backend(format!("create per-spawn dir {dir:?}: {e}")))?;
    Ok(dir)
}

/// Counter backing [`next_guest_cid`].
static CID_COUNTER: AtomicU32 = AtomicU32::new(0);

/// A host-unique guest vsock CID for one VM. CIDs 0–2 (hypervisor/host/local)
/// and `0xffffffff` (`VMADDR_CID_ANY`) are reserved and must be avoided. The
/// guest init binds `VMADDR_CID_ANY`, so the exact value is host-side
/// bookkeeping only — its sole job is to be **unique among concurrently-running
/// VMs on this host** (firecracker rejects a duplicate CID, which would early-
/// exit the worker). Seeded from the PID so distinct daemons rarely collide,
/// plus a per-spawn counter for concurrent spawns within one process. Wrapping
/// is acceptable: a collision only costs one failed boot, surfaced as a spawn
/// error. The plan's compile-time `WORKER_GUEST_CID` is the (unused) default; a
/// real spawn always overrides it here.
fn next_guest_cid() -> u32 {
    let seq = CID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = std::process::id().wrapping_mul(64).wrapping_add(seq);
    // Range [3, 0xffff_fff2] — clear of 0,1,2 and 0xffffffff.
    3 + (base % 0xffff_fff0)
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
        // Backstop GC (#362): remove run-dirs left by SIGKILLed launchers whose
        // own pid is now dead. Runs before we create THIS spawn's dir, so it
        // never races the in-flight spawn. Best-effort; ignores the count.
        let _ = cleanup::sweep_orphaned_run_dirs(&std::env::temp_dir(), cleanup::pid_is_alive);
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
        let mut plan = build_launch_plan(policy, &image, program, args)?;
        // Per-spawn temp dir for the config + log files. No new dep: uses std
        // only (atomic counter + create_dir_all).
        let run_dir = make_spawn_dir()?;
        // Make the vsock UDS path AND the guest CID per-spawn unique so multiple
        // VMs can run concurrently. The pure `build_launch_plan` carries a fixed
        // default for both (it has no spawn context); the spawn — which owns the
        // unique run_dir — is where uniqueness is assigned. Without this, parallel
        // spawns collide on the single image-dir UDS / CID 3 and all but one
        // worker EarlyExits.
        plan.vsock_uds = run_dir.join("vsock.sock");
        plan.vsock_cid = next_guest_cid();
        let config_path = run_dir.join("fc.json");
        let log_path = run_dir.join("fc.log");
        std::fs::write(&config_path, render_firecracker_config(&plan).to_string())
            .map_err(|e| SandboxError::Backend(format!("write fc config: {e}")))?;
        let argv = launcher_argv(
            &plan,
            &config_path.to_string_lossy(),
            &log_path.to_string_lossy(),
            &run_dir.to_string_lossy(),
        );
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SandboxError::Backend(format!("microvm-run spawn failed: {e}")))?;
        // Record the launcher's own pid so the orphan sweep can later tell this
        // VM's run-dir from a dead one (#362). Best-effort: a write failure only
        // means this one dir won't be swept if its launcher is later SIGKILLed;
        // the launcher's own teardown still cleans the dir on a graceful exit.
        let _ = std::fs::write(
            run_dir.join(cleanup::LAUNCHER_PID_FILE),
            child.id().to_string(),
        );
        Ok(child)
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
        let argv = launcher_argv(&plan, "/run/fc.json", "/run/fc.log", "/run");
        assert_eq!(argv[0], MICROVM_RUN_BIN);
        assert!(
            argv.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"),
            "argv must pass --config-file /run/fc.json"
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "--run-dir" && w[1] == "/run"),
            "argv must pass --run-dir <dir>"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--vsock-port" && w[1] == plan.vsock_port.to_string()),
            "argv must pass --vsock-port <port>"
        );
    }
}
