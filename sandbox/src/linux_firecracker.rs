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
    BROKER_VSOCK_PORT, EGRESS_VSOCK_PORT, WORKER_VSOCK_PORT,
};

mod mounts;
pub use mounts::{encode_mount_manifest, non_anchor_top_level, RoShare, RwScratch};

mod images;
pub use images::{build_persistent_image, build_share_images, persistent_mkfs_decision, RW_SCRATCH_MIB_DEFAULT};

mod probe;
pub use probe::{probe_report, ProbeInputs};

mod cleanup;
pub use cleanup::{
    orphaned_run_dir_should_remove, pid_is_alive, sweep_orphaned_run_dirs, LAUNCHER_PID_FILE,
    RUN_DIR_PREFIX, TEARDOWN_MARKER_FILE,
};

mod confine;
pub use confine::{build_confined_spawn_argv, confinement_from_env, VmmConfinement};

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

/// The launcher binary name; discovered on `$PATH` / next to the daemon.
pub const MICROVM_RUN_BIN: &str = "kastellan-microvm-run";

/// Default micro-VM image dir + rootfs filename. The dir (and the pinned
/// `vmlinux`) is shared across workers; the rootfs *filename* is what differs
/// per worker (`python-exec.ext4`, `web-fetch.ext4`, …).
const DEFAULT_MICROVM_DIR: &str = "/var/lib/kastellan/microvm";
const DEFAULT_ROOTFS_FILE: &str = "python-exec.ext4";

/// Resolve the guest kernel + rootfs from the worker's policy env. Pure →
/// unit-tested without KVM. `KASTELLAN_MICROVM_DIR` picks the shared image dir;
/// `KASTELLAN_MICROVM_ROOTFS` picks the rootfs filename inside it (default keeps
/// the existing python-exec path byte-identical).
fn resolve_image(env: &[(String, String)]) -> FirecrackerImage {
    let get = |key: &str| {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    let dir = std::path::PathBuf::from(get("KASTELLAN_MICROVM_DIR").unwrap_or(DEFAULT_MICROVM_DIR));
    let rootfs = get("KASTELLAN_MICROVM_ROOTFS").unwrap_or(DEFAULT_ROOTFS_FILE);
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join(rootfs),
    }
}

/// Pure: the launcher argv for a plan + its rendered config/log/run-dir paths.
pub fn launcher_argv(
    plan: &FirecrackerLaunchPlan,
    config_path: &str,
    log_path: &str,
    run_dir: &str,
) -> Vec<String> {
    let mut argv = vec![
        MICROVM_RUN_BIN.into(),
        "--config-file".into(), config_path.into(),
        "--vsock-uds".into(), plan.vsock_uds.to_string_lossy().into_owned(),
        "--vsock-port".into(), plan.vsock_port.to_string(),
        "--log".into(), log_path.into(),
        "--run-dir".into(), run_dir.into(),
    ];
    // Slice 4a: when force-routed, the launcher also runs the egress reverse-relay
    // (listen on `<vsock_uds>_<port>`, forward to the host proxy UDS).
    if let (Some(uds), Some(port)) = (&plan.egress_host_uds, plan.egress_proxy_vsock_port) {
        argv.push("--egress-uds".into());
        argv.push(uds.to_string_lossy().into_owned());
        argv.push("--egress-vsock-port".into());
        argv.push(port.to_string());
    }
    // VM × broker: when the worker declares a broker, the launcher also runs the
    // broker reverse-relay (listen on `<vsock_uds>_<broker_port>`, forward to the
    // host broker UDS).
    if let (Some(uds), Some(port)) = (&plan.broker_host_uds, plan.broker_vsock_port) {
        argv.push("--broker-uds".into());
        argv.push(uds.to_string_lossy().into_owned());
        argv.push("--broker-vsock-port".into());
        argv.push(port.to_string());
    }
    // Slice 5b-2: when a persistent store image is present (mkfs-once, stable
    // host path outside run_dir), pass it to the launcher so it attaches the
    // drive and sets up teardown-safe handling.
    if let Some(img) = &plan.persistent_image_path {
        argv.push("--persistent-image".into());
        argv.push(img.to_string_lossy().into_owned());
    }
    argv
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
        // Image dir + rootfs filename come from the worker's policy env (set by
        // the entry): KASTELLAN_MICROVM_DIR / KASTELLAN_MICROVM_ROOTFS. The dir
        // (and vmlinux) is shared; the rootfs filename differs per worker.
        let image = resolve_image(&policy.env);
        // #479: verify the guest kernel is the pinned one, on EVERY boot.
        //
        // #471 verifies it at rootfs-build time, which does not constrain
        // the file afterwards — it is booted for months without another
        // check. The image dir is reachable by the agent's own OS user,
        // exactly what the threat model assumes a compromise reaches, and
        // the guest kernel is what enforces the containment boundary the
        // rest of the model rests on.
        //
        // Deliberately the first thing after resolving the paths: a bad
        // kernel costs no run dir, no images and no launcher. Fails closed
        // with no bypass env var — that would be the "spawn unsandboxed"
        // escape hatch CLAUDE.md forbids, on the one file that defines the
        // boundary. See `guest_kernel_pin` for the TOCTOU caveat and for
        // why the ownership half of #479 is the part that closes it.
        crate::guest_kernel_pin::verify_pinned_kernel(&image.kernel_path, std::env::consts::ARCH)
            .map_err(|e| SandboxError::Backend(e.to_string()))?;
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
        // Slice 3: build per-spawn host-dir-share images into the run dir (the
        // launcher's RAII teardown removes them with the dir). Sets the plan's
        // ro/rw image paths so the rendered config attaches the drives.
        build_share_images(&mut plan, &run_dir, &policy.env)?;
        // Slice 5b: mkfs the persistent store image once (no-op if already
        // present). Sets plan.persistent_image_path so render attaches the drive.
        build_persistent_image(&mut plan)?;
        let config_path = run_dir.join("fc.json");
        let log_path = run_dir.join("fc.log");
        std::fs::write(&config_path, render_firecracker_config(&plan).to_string())
            .map_err(|e| SandboxError::Backend(format!("write fc config: {e}")))?;
        let confine = confinement_from_env(
            std::env::var("KASTELLAN_MICROVM_CONFINE_VMM").ok().as_deref(),
        );
        let config_s = config_path.to_string_lossy().into_owned();
        let log_s = log_path.to_string_lossy().into_owned();
        let run_s = run_dir.to_string_lossy().into_owned();

        let argv = match confine {
            VmmConfinement::None => launcher_argv(&plan, &config_s, &log_s, &run_s),
            VmmConfinement::BwrapCgroup => {
                // Resolve the two binaries to absolute paths so they can be bound
                // into the jail (which has no $PATH). Fail closed: a missing
                // binary under the (default) confined strategy refuses to spawn —
                // never a silent bare-spawn fallback.
                let path_env = std::env::var("PATH").ok();
                let fc = confine::find_executable("firecracker", path_env.as_deref()).ok_or_else(|| {
                    SandboxError::Backend(
                        "VMM confinement on but firecracker not found on $PATH to bind into the \
                         jail (set KASTELLAN_MICROVM_CONFINE_VMM=0 to disable, or fix $PATH)".into(),
                    )
                })?;
                let launcher = confine::find_executable(MICROVM_RUN_BIN, path_env.as_deref()).ok_or_else(|| {
                    SandboxError::Backend(format!(
                        "VMM confinement on but {MICROVM_RUN_BIN} not found on $PATH to bind into \
                         the jail (set KASTELLAN_MICROVM_CONFINE_VMM=0 to disable, or fix $PATH)"
                    ))
                })?;
                build_confined_spawn_argv(policy, &plan, &run_dir, &fc, &launcher, &config_s, &log_s)?
            }
        };
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

    #[test]
    fn launcher_argv_passes_egress_flags_when_force_routed() {
        let policy = SandboxPolicy {
            net: crate::Net::Allowlist(vec!["h:443".into()]),
            proxy_uds: Some("/scratch/egress.sock".into()),
            ..Default::default()
        };
        let plan = plan::build_launch_plan(
            &policy,
            &FirecrackerImage { kernel_path: "/k".into(), rootfs_path: "/var/r.ext4".into() },
            "/w",
            &[],
        )
        .unwrap();
        let argv = launcher_argv(&plan, "/run/fc.json", "/run/fc.log", "/run");
        assert!(
            argv.windows(2).any(|w| w[0] == "--egress-uds" && w[1] == "/scratch/egress.sock"),
            "argv must pass --egress-uds <host sidecar path>: {argv:?}"
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "--egress-vsock-port" && w[1] == EGRESS_VSOCK_PORT.to_string()),
            "argv must pass --egress-vsock-port: {argv:?}"
        );
    }

    #[test]
    fn launcher_argv_passes_broker_flags_when_broker_backed() {
        // A force-routed worker that also declares a broker: the launcher argv must
        // carry BOTH the egress pair and the broker pair (independent channels).
        let policy = SandboxPolicy {
            net: crate::Net::Allowlist(vec!["h:443".into()]),
            proxy_uds: Some("/scratch/egress.sock".into()),
            broker_uds: Some("/scratch/embed.sock".into()),
            ..Default::default()
        };
        let plan = plan::build_launch_plan(
            &policy,
            &FirecrackerImage { kernel_path: "/k".into(), rootfs_path: "/var/r.ext4".into() },
            "/w",
            &[],
        )
        .unwrap();
        let argv = launcher_argv(&plan, "/run/fc.json", "/run/fc.log", "/run");
        assert!(
            argv.windows(2).any(|w| w[0] == "--broker-uds" && w[1] == "/scratch/embed.sock"),
            "argv must pass --broker-uds <host broker path>: {argv:?}"
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "--broker-vsock-port" && w[1] == BROKER_VSOCK_PORT.to_string()),
            "argv must pass --broker-vsock-port: {argv:?}"
        );
        // Egress channel still present alongside the broker channel.
        assert!(argv.windows(2).any(|w| w[0] == "--egress-uds" && w[1] == "/scratch/egress.sock"));
    }

    #[test]
    fn launcher_argv_includes_persistent_image_flag_when_set() {
        let mut plan = plan::build_launch_plan(
            &SandboxPolicy::default(),
            &FirecrackerImage { kernel_path: "/k".into(), rootfs_path: "/var/r.ext4".into() },
            "/w",
            &[],
        )
        .unwrap();
        plan.persistent_image_path =
            Some(std::path::PathBuf::from("/var/lib/kastellan/kv/store.ext4"));
        let argv = launcher_argv(&plan, "fc.json", "fc.log", "run");
        let i = argv
            .iter()
            .position(|a| a == "--persistent-image")
            .expect("--persistent-image flag must be present");
        assert_eq!(argv[i + 1], "/var/lib/kastellan/kv/store.ext4");

        // absent ⇒ no flag (byte-identical legacy argv)
        let mut plan2 = plan::build_launch_plan(
            &SandboxPolicy::default(),
            &FirecrackerImage { kernel_path: "/k".into(), rootfs_path: "/var/r.ext4".into() },
            "/w",
            &[],
        )
        .unwrap();
        plan2.persistent_image_path = None;
        assert!(
            !launcher_argv(&plan2, "fc.json", "fc.log", "run")
                .iter()
                .any(|a| a == "--persistent-image"),
            "no --persistent-image flag when persistent_image_path is None"
        );
    }

    #[test]
    fn launcher_argv_omits_egress_flags_for_net_deny() {
        let plan = plan::build_launch_plan(
            &SandboxPolicy::default(),
            &FirecrackerImage { kernel_path: "/k".into(), rootfs_path: "/var/r.ext4".into() },
            "/w",
            &[],
        )
        .unwrap();
        let argv = launcher_argv(&plan, "/run/fc.json", "/run/fc.log", "/run");
        assert!(!argv.iter().any(|a| a == "--egress-uds"), "no egress flags for Net::Deny: {argv:?}");
    }

    #[test]
    fn resolve_image_defaults_to_python_exec_rootfs() {
        let img = resolve_image(&[]);
        assert_eq!(img.kernel_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/vmlinux"));
        assert_eq!(img.rootfs_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/python-exec.ext4"));
    }

    #[test]
    fn resolve_image_honours_rootfs_filename_env() {
        let env = vec![("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-fetch.ext4".to_string())];
        let img = resolve_image(&env);
        assert_eq!(img.rootfs_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/web-fetch.ext4"));
        // Kernel is still the shared vmlinux in the same dir.
        assert_eq!(img.kernel_path, std::path::PathBuf::from("/var/lib/kastellan/microvm/vmlinux"));
    }

    #[test]
    fn resolve_image_honours_dir_and_ignores_blank_rootfs() {
        let env = vec![
            ("KASTELLAN_MICROVM_DIR".to_string(), "/srv/vm".to_string()),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "  ".to_string()),
        ];
        let img = resolve_image(&env);
        // Blank ROOTFS falls back to the python default; DIR is honoured.
        assert_eq!(img.rootfs_path, std::path::PathBuf::from("/srv/vm/python-exec.ext4"));
        assert_eq!(img.kernel_path, std::path::PathBuf::from("/srv/vm/vmlinux"));
    }

    // --- #479: the guest-kernel pin is enforced on the boot path ---
    //
    // These need no KVM, no firecracker binary and no root: the pin check
    // runs before any VM work, so a bogus image dir short-circuits the
    // spawn. They therefore run in an ordinary `cargo test` on any Linux
    // host, not only where a micro-VM can actually boot.

    /// A unique temp dir holding a fake image set, using `std` only.
    fn fake_image_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("kastellan-fcpin-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fake image dir");
        dir
    }

    fn policy_pointing_at(dir: &std::path::Path) -> SandboxPolicy {
        SandboxPolicy {
            env: vec![(
                "KASTELLAN_MICROVM_DIR".to_string(),
                dir.to_string_lossy().into_owned(),
            )],
            ..Default::default()
        }
    }

    /// The #479 gate. A `vmlinux` that is not the pinned one must stop
    /// the spawn outright — no VM, no run dir, no launcher.
    #[test]
    fn spawn_refuses_a_guest_kernel_that_does_not_match_the_pin() {
        let dir = fake_image_dir("bad-kernel");
        std::fs::write(dir.join("vmlinux"), b"not the pinned kernel").expect("write fake kernel");
        std::fs::write(dir.join("python-exec.ext4"), b"fake rootfs").expect("write fake rootfs");

        let err = LinuxFirecracker::new()
            .spawn_under_policy(&policy_pointing_at(&dir), "/bin/true", &[])
            .expect_err("an unpinned kernel must never boot");

        let msg = err.to_string();
        assert!(
            msg.contains("does not match the pinned sha256"),
            "the failure must name the pin, not some downstream symptom: {msg}"
        );

        // The check must run BEFORE any spawn work. Code order says so
        // today, but nothing pinned it — so moving the call below
        // make_spawn_dir() would still pass the assertion above while
        // quietly doing work on behalf of a kernel we are about to
        // reject. Count run dirs instead of trusting the ordering.
        let leaked: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(cleanup::RUN_DIR_PREFIX))
            .collect();
        assert!(
            leaked.is_empty(),
            "a rejected kernel must cost no run dir; found {} — the pin check has moved \
             below make_spawn_dir()",
            leaked.len()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing kernel must fail on the pin check with an actionable
    /// message, rather than surfacing later as an opaque launcher error.
    #[test]
    fn spawn_refuses_a_missing_guest_kernel() {
        let dir = fake_image_dir("no-kernel");
        let err = LinuxFirecracker::new()
            .spawn_under_policy(&policy_pointing_at(&dir), "/bin/true", &[])
            .expect_err("a missing kernel must never reach the launcher");
        assert!(
            err.to_string().contains("cannot read the micro-VM guest kernel"),
            "got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
