//! Pure policy → Firecracker launch-plan translation. No KVM, no spawn.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::{Net, SandboxError, SandboxPolicy};

/// Where the guest kernel + rootfs live on the host. Defaulted from
/// constants; the `container_image` tag will later select per-worker rootfs.
#[derive(Clone, Debug)]
pub struct FirecrackerImage {
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
}

/// Fully-resolved inputs to one micro-VM boot. Pure data; the launcher
/// renders this into a Firecracker config + boots.
#[derive(Clone, Debug)]
pub struct FirecrackerLaunchPlan {
    pub kernel_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub vcpu_count: u8,
    pub mem_size_mib: usize,
    pub vsock_cid: u32,
    pub vsock_uds: PathBuf,
    pub vsock_port: u32,
    pub boot_args: String,
    /// Carried for future guest env-forwarding; NOT yet rendered into the
    /// Firecracker config in Slice 1 (guest init bakes a fixed env).
    pub env: Vec<(String, String)>,
    pub net_enabled: bool,
}

/// Placeholder guest CID baked into a freshly-built plan. CIDs 0–2 are reserved
/// (hypervisor/host/local), so the lowest legal value is 3. This default is a
/// stand-in only: `LinuxFirecracker::spawn_under_policy` ALWAYS overrides it with
/// a host-unique CID (`next_guest_cid`) before boot, so concurrent VMs never
/// share CID 3. The pure plan has no spawn context, so it cannot allocate a
/// unique CID itself; this constant exists so `build_launch_plan` stays a total
/// pure function (and so the plan-level unit tests have a deterministic value).
const WORKER_GUEST_CID: u32 = 3;
/// Fixed vsock port the guest init listens on for the JSON-RPC bridge.
pub const WORKER_VSOCK_PORT: u32 = 1024;
/// Kernel cmdline: serial console for *kernel* logs only (the launcher routes
/// it to a log fd, never stdout); JSON-RPC rides vsock, not the console.
const BASE_BOOT_ARGS: &str =
    "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux=1 i8042.nomux=1";

/// Translate a policy into a launch plan. Pure + fallible (rejects relative
/// FS paths, matching bwrap).
pub fn build_launch_plan(
    policy: &SandboxPolicy,
    image: &FirecrackerImage,
    _program: &str,
    _args: &[&str],
) -> Result<FirecrackerLaunchPlan, SandboxError> {
    for p in policy.fs_read.iter().chain(policy.fs_write.iter()) {
        if !p.is_absolute() {
            return Err(SandboxError::Backend(format!(
                "policy paths must be absolute, got {p:?}"
            )));
        }
    }

    // vcpu_count: None → 1; Some(pct) → ceil(pct/100), min 1, clamped to a
    // sane ceiling so a bad config can't request 256 vCPUs.
    let vcpu_count: u8 = match policy.cpu_quota_pct {
        None => 1,
        Some(pct) => pct.div_ceil(100).clamp(1, 8) as u8,
    };

    let net_enabled = !matches!(policy.net, Net::Deny);

    // Placeholder vsock UDS next to the rootfs image dir. Like `vsock_cid`, this
    // is a stand-in: `spawn_under_policy` ALWAYS replaces it with a per-spawn
    // unique path inside the spawn's private run dir before boot (the pure plan
    // has no spawn context to allocate a unique path). It is live only in the
    // plan-level unit tests; the real spawn never uses this value.
    let vsock_uds = image
        .rootfs_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/tmp"))
        .join("worker-vsock.sock");

    Ok(FirecrackerLaunchPlan {
        kernel_path: image.kernel_path.clone(),
        rootfs_path: image.rootfs_path.clone(),
        vcpu_count,
        mem_size_mib: policy.mem_mb.max(1) as usize,
        vsock_cid: WORKER_GUEST_CID,
        vsock_uds,
        vsock_port: WORKER_VSOCK_PORT,
        boot_args: BASE_BOOT_ARGS.to_string(),
        env: policy.env.clone(),
        net_enabled,
    })
}

/// Render the Firecracker `--config-file` JSON for a plan. The vsock device is
/// always present (the JSON-RPC transport); the net device only when allowed.
pub fn render_firecracker_config(plan: &FirecrackerLaunchPlan) -> Value {
    let mut cfg = json!({
        "boot-source": {
            "kernel_image_path": plan.kernel_path.to_string_lossy(),
            "boot_args": plan.boot_args,
        },
        "drives": [{
            "drive_id": "rootfs",
            "path_on_host": plan.rootfs_path.to_string_lossy(),
            "is_root_device": true,
            // Read-only is mandatory, not cosmetic: every spawn shares the one
            // `<image_dir>/python-exec.ext4` backing file (the spawn uniquifies
            // the vsock UDS + guest CID, but NOT the rootfs). Two concurrent VMs
            // opening the same ext4 RW have independent guest page caches, and
            // ext4 is not a cluster filesystem — any write (journal/atime, even
            // an mount-time recovery) corrupts the image for both. The guest
            // writes nothing to root (worker + python + stdlib are read-only;
            // scratch is the in-VM `/tmp` tmpfs the init mounts), so a read-only
            // golden image is both safe to share and correct. Per-worker writable
            // rootfs, if ever needed, is a per-spawn copy/overlay (a later slice),
            // NOT flipping this back to RW on a shared file.
            "is_read_only": true,
        }],
        "machine-config": {
            "vcpu_count": plan.vcpu_count,
            "mem_size_mib": plan.mem_size_mib,
        },
        "vsock": {
            "guest_cid": plan.vsock_cid,
            "uds_path": plan.vsock_uds.to_string_lossy(),
        },
    });
    if plan.net_enabled {
        // Slice 4 fills this in; slice 1 only reaches here for net workers,
        // which are out of scope, so leave a deterministic empty marker.
        cfg["network-interfaces"] = json!([]);
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Net, SandboxPolicy};
    use std::path::PathBuf;

    fn img() -> FirecrackerImage {
        FirecrackerImage {
            kernel_path: PathBuf::from("/var/lib/kastellan/microvm/vmlinux"),
            rootfs_path: PathBuf::from("/var/lib/kastellan/microvm/python-exec.ext4"),
        }
    }

    #[test]
    fn mem_mb_maps_to_mem_size_mib() {
        let policy = SandboxPolicy { mem_mb: 512, ..Default::default() };
        let plan = build_launch_plan(&policy, &img(), "/usr/local/bin/worker", &[]).unwrap();
        assert_eq!(plan.mem_size_mib, 512);
    }

    #[test]
    fn net_deny_disables_net_device() {
        let policy = SandboxPolicy { net: Net::Deny, ..Default::default() };
        let plan = build_launch_plan(&policy, &img(), "/usr/local/bin/worker", &[]).unwrap();
        assert!(!plan.net_enabled);
        let cfg = render_firecracker_config(&plan);
        assert!(cfg.get("network-interfaces").is_none());
    }

    #[test]
    fn vsock_device_present_in_config() {
        let policy = SandboxPolicy::default();
        let plan = build_launch_plan(&policy, &img(), "/usr/local/bin/worker", &[]).unwrap();
        let cfg = render_firecracker_config(&plan);
        let vsock = cfg.get("vsock").expect("vsock device");
        assert_eq!(vsock["guest_cid"], plan.vsock_cid);
        assert_eq!(vsock["uds_path"], &*plan.vsock_uds.to_string_lossy());
    }

    #[test]
    fn config_pins_kernel_and_rootfs_paths() {
        let plan = build_launch_plan(&SandboxPolicy::default(), &img(), "/w", &[]).unwrap();
        let cfg = render_firecracker_config(&plan);
        assert_eq!(cfg["boot-source"]["kernel_image_path"], &*img().kernel_path.to_string_lossy());
        assert_eq!(cfg["drives"][0]["path_on_host"], &*img().rootfs_path.to_string_lossy());
        assert_eq!(cfg["drives"][0]["is_root_device"], true);
    }

    #[test]
    fn rootfs_is_read_only() {
        // Security invariant: the rootfs backing file is shared by every spawn
        // (only the vsock UDS + CID are uniquified), so it MUST be attached
        // read-only — two concurrent VMs mounting the same ext4 RW corrupt it.
        let plan = build_launch_plan(&SandboxPolicy::default(), &img(), "/w", &[]).unwrap();
        let cfg = render_firecracker_config(&plan);
        assert_eq!(
            cfg["drives"][0]["is_read_only"], true,
            "shared rootfs must be read-only to be safe across concurrent VMs"
        );
    }

    #[test]
    fn relative_fs_paths_rejected() {
        let policy =
            SandboxPolicy { fs_read: vec![PathBuf::from("rel/path")], ..Default::default() };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(format!("{err}").contains("absolute"));
    }

    #[test]
    fn cpu_quota_maps_to_vcpu_count() {
        // None → 1 vcpu (slice-1 default); Some(250) → 3 vcpus (ceil 250/100).
        let p_none = SandboxPolicy { cpu_quota_pct: None, ..Default::default() };
        assert_eq!(build_launch_plan(&p_none, &img(), "/w", &[]).unwrap().vcpu_count, 1);
        let p_250 = SandboxPolicy { cpu_quota_pct: Some(250), ..Default::default() };
        assert_eq!(build_launch_plan(&p_250, &img(), "/w", &[]).unwrap().vcpu_count, 3);
    }
}
