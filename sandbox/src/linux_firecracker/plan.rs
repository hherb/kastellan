//! Pure policy → Firecracker launch-plan translation. No KVM, no spawn.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::{Net, SandboxError, SandboxPolicy};
use super::mounts::{encode_mount_manifest, non_anchor_top_level, PersistentMount, RoShare, RwScratch};

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
    /// The worker env, forwarded into the guest via the hex `kastellan.env=`
    /// cmdline token baked into [`Self::boot_args`] (#360). Retained as a
    /// structured field for inspection/tests; the boot_args token is the
    /// load-bearing copy the guest init actually decodes.
    pub env: Vec<(String, String)>,
    pub net_enabled: bool,
    /// Read-only host-dir share, derived from `policy.fs_read`. `None` if empty.
    pub ro_share: Option<RoShare>,
    /// Writable scratch drive, derived from `policy.fs_write`. `None` if empty.
    pub rw_scratch: Option<RwScratch>,
    /// Host path of the built RO ext4. Placeholder until the spawn sets the
    /// run-dir path (mirrors `vsock_uds`); `Some` iff `ro_share` is `Some`.
    pub ro_image_path: Option<std::path::PathBuf>,
    /// Host path of the built RW ext4. `Some` iff `rw_scratch` is `Some`.
    pub rw_image_path: Option<std::path::PathBuf>,
    /// Slice 4a: the guest-initiated egress vsock port, `Some(EGRESS_VSOCK_PORT)`
    /// iff the worker is force-routed (`Net::Allowlist` + `proxy_uds`). Drives
    /// the ` kastellan.egress=1` cmdline token and the launcher's reverse-relay.
    pub egress_proxy_vsock_port: Option<u32>,
    /// Slice 4a: the **host** egress-proxy UDS (from `policy.proxy_uds`) the
    /// launcher relays the guest's egress connections to. `Some` iff force-routed.
    pub egress_host_uds: Option<std::path::PathBuf>,
    /// Slice 5b: the `PersistentStore` from the policy, if any. Copied here so
    /// the spawn can use `host_backing` when building or reusing the ext4 image.
    pub persistent_store: Option<crate::PersistentStore>,
    /// Slice 5b: host path of the persistent ext4 image to attach. `None` here
    /// (plan is pure); `spawn_under_policy` fills it with the real path before
    /// boot (mirrors `ro_image_path` / `rw_image_path`).
    pub persistent_image_path: Option<PathBuf>,
    /// Slice 5b: the in-guest device + mountpoint for the persistent store.
    /// `Some` iff `persistent_store` is `Some`.
    pub persistent_mount: Option<PersistentMount>,
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
/// Fixed vsock port the launcher's reverse-relay listens on and the guest init
/// dials for the egress channel (slice 4a). A force-routed `Net::Allowlist`
/// worker reaches the host egress proxy over this second, guest-initiated vsock
/// port; the JSON-RPC channel keeps `WORKER_VSOCK_PORT`. Shared with
/// `kastellan-microvm-init` (kept in sync manually; same constraint as
/// `WORKER_VSOCK_PORT`).
pub const EGRESS_VSOCK_PORT: u32 = 1025;
/// In-guest path the worker dials for egress (its `KASTELLAN_EGRESS_PROXY_UDS`)
/// and the init binds the relay listener at. Shared with `kastellan-microvm-init`.
const GUEST_EGRESS_UDS: &str = "/run/kastellan-egress.sock";
/// Kernel cmdline: serial console for *kernel* logs only (the launcher routes
/// it to a log fd, never stdout); JSON-RPC rides vsock, not the console.
const BASE_BOOT_ARGS: &str =
    "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux=1 i8042.nomux=1";

/// Cmdline token carrying the hex-encoded worker env (#360). The guest
/// `kastellan-microvm-init` reads this from `/proc/cmdline`. The key is a
/// manually-kept-in-sync constant across the crate boundary (microvm-init must
/// not depend on the sandbox crate — same pattern as [`WORKER_VSOCK_PORT`]).
const ENV_CMDLINE_KEY: &str = "kastellan.env";

/// Cmdline token carrying the hex-encoded worker program path the guest init
/// execs (generalizes slice-1's baked python-exec path). Kept in sync with
/// `kastellan-microvm-init`'s WORKER_CMDLINE_KEY.
const WORKER_CMDLINE_KEY: &str = "kastellan.worker";

/// Cmdline token carrying the hex-encoded worker argv (#374). Each arg is
/// hex-encoded independently and the list joined with ','. Kept in sync with
/// `kastellan-microvm-init`'s WORKER_ARGS_CMDLINE_KEY. Separate from
/// [`WORKER_CMDLINE_KEY`] (which carries argv[0]/the program) so the no-args
/// cmdline stays byte-identical to the pre-#374 baseline.
const WORKER_ARGS_CMDLINE_KEY: &str = "kastellan.worker.args";

/// Conservative ceiling for the whole kernel cmdline (base args + the env
/// token). Well under arm64's 2048-byte `COMMAND_LINE_SIZE`; the slice-1 env is
/// ~3 small vars (~120 hex chars), so this only ever trips on a pathological
/// policy. `build_launch_plan` fails closed above it rather than emit a
/// truncated cmdline that would corrupt the boot.
const MAX_CMDLINE_BYTES: usize = 1024;

/// Lowercase-hex encode (`[0-9a-f]`, two chars/byte). Hand-rolled so the crate
/// takes no codec dependency; the guest's decoder in `kastellan-microvm-init`
/// mirrors this exact scheme (roundtrip-pinned in both crates' unit tests).
pub(super) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Encode the worker env as the ` kastellan.env=<hex>` cmdline suffix (#360).
///
/// The env block is `K1=V1\nK2=V2\n…` (UTF-8), hex-encoded. Hex keeps the token
/// whitespace/quote/`=`-safe so it survives as a single cmdline argument for any
/// value. Returns `Ok(None)` for an empty env so the no-env cmdline is
/// byte-identical to the pre-#360 baseline.
///
/// Fail closed on the two delimiters the guest decoder splits on, so a token is
/// never emitted that would decode to something other than what was forwarded:
///
/// * A `\n` in any key or value — `\n` is the pair separator. A value newline
///   would split one var into two, and the trailing fragment (no `=`) is
///   silently dropped in-guest; the forwarded value would also be truncated.
/// * An `=` in any key — the guest splits each line on its FIRST `=`, so an `=`
///   in a key silently shifts the boundary (a prefix of the key leaks into the
///   value). POSIX env names cannot contain `=`, so this only ever rejects a
///   malformed policy.
///
/// Values may freely contain `=` (the first-`=` split preserves them) and any
/// other byte; only the newline separator is off-limits there.
pub fn encode_env_cmdline(env: &[(String, String)]) -> Result<Option<String>, SandboxError> {
    if env.is_empty() {
        return Ok(None);
    }
    for (k, v) in env {
        if k.contains('\n') || v.contains('\n') || k.contains('=') {
            return Err(SandboxError::Backend(format!(
                "env var {k:?} cannot be forwarded via the kernel cmdline: keys may not \
                 contain '=' and neither key nor value may contain a newline (the \
                 guest decoder's pair/field separators)"
            )));
        }
    }
    let block = env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some(format!(
        " {ENV_CMDLINE_KEY}={}",
        hex_encode(block.as_bytes())
    )))
}

/// Encode the worker argv as the ` kastellan.worker.args=<hex0>,<hex1>,…`
/// cmdline suffix (#374).
///
/// Each arg is hex-encoded independently so it may contain ANY byte (paths,
/// flags) — the ',' separator can never collide with the hex alphabet
/// `[0-9a-f]`, and per-arg hex needs no fail-closed delimiter check (unlike the
/// `\n`-joined env block in [`encode_env_cmdline`]). Returns `None` for an empty
/// argv so the no-args cmdline is byte-identical to the pre-#374 baseline (every
/// current FC worker has `lockdown_shim: None` ⇒ empty args).
fn encode_worker_args_cmdline(args: &[&str]) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    let joined = args
        .iter()
        .map(|a| hex_encode(a.as_bytes()))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!(" {WORKER_ARGS_CMDLINE_KEY}={joined}"))
}

/// Translate a policy into a launch plan. Pure + fallible (rejects relative
/// FS paths, matching bwrap).
pub fn build_launch_plan(
    policy: &SandboxPolicy,
    image: &FirecrackerImage,
    program: &str,
    args: &[&str],
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

    // Slice 4a: force-routing detection. A `Net::Allowlist` worker with a
    // `proxy_uds` reaches the network ONLY through the host egress proxy, tunneled
    // over a second guest-initiated vsock port — so the VM carries NO virtio-net
    // device (stronger than the bwrap private-netns path). A `Net::Allowlist`
    // worker WITHOUT `proxy_uds` would need a virtio-net device this slice does
    // not build, so reject it fail-closed rather than boot an egress-less VM.
    let (net_enabled, egress_proxy_vsock_port, egress_host_uds) = match (&policy.net, &policy.proxy_uds) {
        (Net::Deny, _) => (false, None, None),
        (Net::Allowlist(_), Some(uds)) => (false, Some(EGRESS_VSOCK_PORT), Some(uds.clone())),
        (Net::Allowlist(_), None) => {
            return Err(SandboxError::Backend(
                "micro-VM net workers require force-routing: Net::Allowlist needs proxy_uds set \
                 (direct-net in a VM is unsupported — no virtio-net device)"
                    .to_string(),
            ));
        }
        // The egress proxy itself never runs in a VM; keep prior behaviour.
        (Net::ProxyEgress, _) => (true, None, None),
    };

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

    // Slice 3: derive host-dir-sharing drives from the policy. Device nodes are
    // assigned RO-before-RW starting at /dev/vdb (vda is the rootfs); the config
    // drive order in render_firecracker_config MUST match (pinned by a test).
    //
    // Every share path's top-level must be a rootfs anchor the guest init can
    // tmpfs-mount (see `non_anchor_top_level`). This is an allowlist, not just a
    // system-dir blocklist: a path under e.g. /home or /var would pass an old
    // "reject /usr|/etc" check but then SILENTLY fail to mount in-guest (the
    // anchor dir doesn't exist on the read-only rootfs). Reject it here so the
    // worker never believes it has access to a directory the guest cannot expose.
    const ANCHOR_HINT: &str = "allowed share anchors: /opt /data /srv /mnt /work /tmp";
    for p in &policy.fs_read {
        if let Some(top) = non_anchor_top_level(p) {
            return Err(SandboxError::Backend(format!(
                "fs_read path {p:?} has top-level /{top}, which is not a micro-VM share anchor \
                 ({ANCHOR_HINT}): the guest cannot mount it on the read-only rootfs — place the \
                 shared dir under one of those anchors"
            )));
        }
    }
    if policy.fs_write.len() > 1 {
        return Err(SandboxError::Backend(format!(
            "micro-VM backend supports a single writable mountpoint per spawn, got {} fs_write \
             paths",
            policy.fs_write.len()
        )));
    }
    if let Some(mp) = policy.fs_write.first() {
        if let Some(top) = non_anchor_top_level(mp) {
            return Err(SandboxError::Backend(format!(
                "fs_write path {mp:?} has top-level /{top}, which is not a micro-VM share anchor \
                 ({ANCHOR_HINT}): the guest cannot mount the scratch drive on the read-only \
                 rootfs — place the writable mountpoint under one of those anchors"
            )));
        }
    }
    let mut next_letter = b'b';
    let ro_share = if policy.fs_read.is_empty() {
        None
    } else {
        let dev = format!("/dev/vd{}", next_letter as char);
        next_letter += 1;
        Some(RoShare { sources: policy.fs_read.clone(), guest_dev: dev })
    };
    let rw_scratch = policy.fs_write.first().map(|mp| {
        let dev = format!("/dev/vd{}", next_letter as char);
        next_letter += 1;
        RwScratch { mountpoint: mp.clone(), guest_dev: dev }
    });
    let persistent_mount = policy.persistent_store.as_ref().map(|ps| {
        let dev = format!("/dev/vd{}", next_letter as char);
        next_letter += 1;
        PersistentMount { mountpoint: ps.guest_mount.clone(), guest_dev: dev }
    });

    // Forward policy.env into the guest via a hex cmdline token (#360). When
    // force-routed (slice 4a), override KASTELLAN_EGRESS_PROXY_UDS to the
    // IN-GUEST path: the worker dials the in-guest relay UDS, not the
    // (unreachable-from-a-VM) host sidecar path. Backend-local translation —
    // SandboxPolicy and the bwrap backend are untouched.
    let mut env = policy.env.clone();
    if egress_host_uds.is_some() {
        const K: &str = "KASTELLAN_EGRESS_PROXY_UDS";
        match env.iter_mut().find(|(k, _)| k == K) {
            Some(slot) => slot.1 = GUEST_EGRESS_UDS.to_string(),
            None => env.push((K.to_string(), GUEST_EGRESS_UDS.to_string())),
        }
    }
    // Backend-only config — consumed host-side by resolve_image (this fn already
    // receives the resolved `image`), never by the worker. Don't forward into the
    // guest: it's noise there and costs scarce cmdline budget.
    env.retain(|(k, _)| k != "KASTELLAN_MICROVM_DIR" && k != "KASTELLAN_MICROVM_ROOTFS");
    let mut boot_args = BASE_BOOT_ARGS.to_string();
    if let Some(suffix) = encode_env_cmdline(&env)? {
        boot_args.push_str(&suffix);
    }
    if let Some(suffix) = encode_mount_manifest(ro_share.as_ref(), rw_scratch.as_ref(), persistent_mount.as_ref())? {
        boot_args.push_str(&suffix);
    }
    if egress_proxy_vsock_port.is_some() {
        boot_args.push_str(" kastellan.egress=1");
        // Test-only: emit the self-test token when the operator/test sets the knob.
        if policy.env.iter().any(|(k, v)| k == "KASTELLAN_MICROVM_EGRESS_SELFTEST" && v == "1") {
            boot_args.push_str(" kastellan.egress.selftest=1");
        }
    }
    // Forward the worker program path so the guest init execs the right binary
    // (slice 4b: python-exec and web-fetch share one init). Hex-encoded so any
    // absolute path is cmdline-safe, mirroring the #360 env token.
    boot_args.push_str(&format!(" {WORKER_CMDLINE_KEY}={}", hex_encode(program.as_bytes())));
    // Forward the worker argv too (#374). Empty for every worker with
    // `lockdown_shim: None` (no token emitted ⇒ baseline unchanged); a shimmed
    // worker carries [target_binary, …] so the guest init can build the full
    // execv argv and the lockdown-exec shim finds its target in argv[1].
    if let Some(suffix) = encode_worker_args_cmdline(args) {
        boot_args.push_str(&suffix);
    }
    if boot_args.len() > MAX_CMDLINE_BYTES {
        return Err(SandboxError::Backend(format!(
            "kernel cmdline {} bytes exceeds {MAX_CMDLINE_BYTES}-byte cap \
             (worker env + mount manifest too large to forward)",
            boot_args.len()
        )));
    }
    // Placeholder image paths next to the rootfs (overridden per-spawn, like
    // vsock_uds). Present iff the corresponding share is present.
    let image_dir = image.rootfs_path.parent().unwrap_or_else(|| std::path::Path::new("/tmp"));
    let ro_image_path = ro_share.as_ref().map(|_| image_dir.join("ro-share.ext4"));
    let rw_image_path = rw_scratch.as_ref().map(|_| image_dir.join("rw-scratch.ext4"));

    Ok(FirecrackerLaunchPlan {
        kernel_path: image.kernel_path.clone(),
        rootfs_path: image.rootfs_path.clone(),
        vcpu_count,
        mem_size_mib: policy.mem_mb.max(1) as usize,
        vsock_cid: WORKER_GUEST_CID,
        vsock_uds,
        vsock_port: WORKER_VSOCK_PORT,
        boot_args,
        env,
        net_enabled,
        ro_share,
        rw_scratch,
        ro_image_path,
        rw_image_path,
        egress_proxy_vsock_port,
        egress_host_uds,
        persistent_store: policy.persistent_store.clone(),
        persistent_image_path: None,
        persistent_mount,
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
    // Slice 3: attach the host-dir-share drives in the fixed order RO → RW, which
    // MUST agree with the /dev/vdb,/dev/vdc device nodes build_launch_plan
    // assigned (the guest init mounts by those nodes). `*_image_path` is `Some`
    // iff the corresponding share is present (set together in build_launch_plan,
    // overridden to the run-dir path by build_share_images); path_on_host is the
    // per-spawn image (a placeholder here in unit tests).
    if let Some(img) = &plan.ro_image_path {
        cfg["drives"].as_array_mut().unwrap().push(json!({
            "drive_id": "ro-share",
            "path_on_host": img.to_string_lossy(),
            "is_root_device": false,
            "is_read_only": true,
        }));
    }
    if let Some(img) = &plan.rw_image_path {
        cfg["drives"].as_array_mut().unwrap().push(json!({
            "drive_id": "rw-scratch",
            "path_on_host": img.to_string_lossy(),
            "is_root_device": false,
            "is_read_only": false,
        }));
    }
    // Slice 5b: persistent store drive, attached after rw-scratch so device
    // letter order (vdb=ro, vdc=rw-scratch, vdd=persistent) agrees with
    // build_launch_plan's next_letter assignment.
    if let Some(img) = &plan.persistent_image_path {
        cfg["drives"].as_array_mut().unwrap().push(json!({
            "drive_id": "persistent-store",
            "path_on_host": img.to_string_lossy(),
            "is_root_device": false,
            "is_read_only": false,
        }));
    }
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
    fn encode_env_cmdline_empty_is_none() {
        // No env → no token, so the cmdline stays byte-identical to the
        // pre-#360 baseline.
        assert_eq!(encode_env_cmdline(&[]).unwrap(), None);
    }

    #[test]
    fn encode_env_cmdline_roundtrip_fixture() {
        // Cross-crate sync guard: `kastellan-microvm-init` decodes this exact
        // hex. Keep this fixture identical in both crates' tests. Block
        // "A=1\nB=2" = bytes 41 3d 31 0a 42 3d 32.
        let env = vec![("A".to_string(), "1".to_string()), ("B".to_string(), "2".to_string())];
        assert_eq!(
            encode_env_cmdline(&env).unwrap().unwrap(),
            " kastellan.env=413d310a423d32"
        );
    }

    #[test]
    fn encode_env_cmdline_rejects_separator_chars() {
        // Fail closed rather than emit a token the guest would silently
        // mis-decode: a newline in a value would split one var into two and the
        // trailing fragment is dropped in-guest; a newline in a key, or an '='
        // in a key, shifts the field boundary. Each must surface as an error,
        // not a silent drop/corruption.
        let newline_value =
            vec![("K".to_string(), "line1\nline2".to_string())];
        assert!(encode_env_cmdline(&newline_value).is_err());

        let newline_key = vec![("K\nX".to_string(), "v".to_string())];
        assert!(encode_env_cmdline(&newline_key).is_err());

        let equals_key = vec![("K=X".to_string(), "v".to_string())];
        assert!(encode_env_cmdline(&equals_key).is_err());

        // A value with '=' is fine — the guest splits on the first '=' only.
        let equals_value = vec![("K".to_string(), "a=b".to_string())];
        assert!(encode_env_cmdline(&equals_value).is_ok());
    }

    #[test]
    fn build_launch_plan_fails_closed_on_unforwardable_env() {
        // The guard propagates through the (already fallible) plan builder.
        let policy = SandboxPolicy {
            env: vec![("K".to_string(), "has\nnewline".to_string())],
            ..Default::default()
        };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(
            format!("{err}").contains("newline"),
            "expected a separator-guard error, got: {err}"
        );
    }

    #[test]
    fn build_launch_plan_appends_env_token_to_boot_args() {
        let policy = SandboxPolicy {
            env: vec![("KASTELLAN_PYTHON_PARAMS_FILE_MAX".to_string(), "100".to_string())],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert!(
            plan.boot_args.starts_with(BASE_BOOT_ARGS),
            "base kernel args must be preserved: {}",
            plan.boot_args
        );
        assert!(
            plan.boot_args.contains(" kastellan.env="),
            "env token must be appended: {}",
            plan.boot_args
        );
        // Hex token carries no whitespace, so it is a single cmdline arg. The
        // worker token (slice 4b) is appended after env, so find env by prefix.
        let token = plan
            .boot_args
            .split_whitespace()
            .find(|t| t.starts_with("kastellan.env="))
            .unwrap();
        assert!(token.starts_with("kastellan.env="));
        assert!(token["kastellan.env=".len()..].bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn build_launch_plan_no_env_leaves_boot_args_baseline() {
        // No env/mounts/egress → boot_args starts with the baseline and the ONLY
        // kastellan.* token is kastellan.worker (always forwarded, slice 4b).
        let policy = SandboxPolicy { env: vec![], ..Default::default() };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert!(
            plan.boot_args.starts_with(BASE_BOOT_ARGS),
            "base kernel args must be preserved: {}",
            plan.boot_args
        );
        assert!(
            !plan.boot_args.contains("kastellan.env"),
            "no env token when env is empty: {}",
            plan.boot_args
        );
        assert!(
            !plan.boot_args.contains("kastellan.mounts"),
            "no mounts token when no shares: {}",
            plan.boot_args
        );
        assert!(
            !plan.boot_args.contains("kastellan.egress"),
            "no egress token when net is deny: {}",
            plan.boot_args
        );
        assert!(
            plan.boot_args.contains(" kastellan.worker="),
            "worker token always forwarded (slice 4b): {}",
            plan.boot_args
        );
    }

    #[test]
    fn build_launch_plan_appends_worker_token() {
        // The guest init reads kastellan.worker=<hex(program)> to exec the right
        // binary. Pinned so kastellan-microvm-init decodes this exact hex.
        let policy = SandboxPolicy { env: vec![], ..Default::default() };
        let plan = build_launch_plan(&policy, &img(), "/usr/local/bin/kastellan-worker-web-fetch", &[])
            .expect("plan");
        let hex = super::hex_encode(b"/usr/local/bin/kastellan-worker-web-fetch");
        assert!(
            plan.boot_args.contains(&format!(" kastellan.worker={hex}")),
            "boot_args missing worker token: {}",
            plan.boot_args
        );
    }

    #[test]
    fn encode_worker_args_cmdline_empty_is_none() {
        // No args (every current FC worker has lockdown_shim:None) → no token, so
        // the cmdline stays byte-identical to the pre-#374 baseline.
        assert_eq!(super::encode_worker_args_cmdline(&[]), None);
    }

    #[test]
    fn encode_worker_args_cmdline_roundtrip_fixture() {
        // Cross-crate sync guard: `kastellan-microvm-init`'s
        // parse_worker_args_cmdline_decodes_fixture decodes this exact token.
        // Keep this fixture identical in both crates' tests. Each arg is
        // hex-encoded independently, joined with ','. "/bin/x" = 2f62696e2f78,
        // "y" = 79.
        assert_eq!(
            super::encode_worker_args_cmdline(&["/bin/x", "y"]).unwrap(),
            " kastellan.worker.args=2f62696e2f78,79"
        );
    }

    #[test]
    fn build_launch_plan_forwards_worker_args() {
        // A shimmed worker (lockdown_shim:Some) carries the real binary as
        // args[0]; the guest must receive it so the shim knows what to execve.
        let policy = SandboxPolicy::default();
        let plan = build_launch_plan(&policy, &img(), "/shim", &["/real-worker", "--x"]).unwrap();
        let expected = format!(
            " kastellan.worker.args={},{}",
            super::hex_encode(b"/real-worker"),
            super::hex_encode(b"--x")
        );
        assert!(
            plan.boot_args.contains(&expected),
            "boot_args missing worker.args token: {}",
            plan.boot_args
        );
    }

    #[test]
    fn build_launch_plan_omits_args_token_when_empty() {
        // The no-args path (every current FC worker) emits no args token.
        let plan = build_launch_plan(&SandboxPolicy::default(), &img(), "/w", &[]).unwrap();
        assert!(
            !plan.boot_args.contains("kastellan.worker.args"),
            "no args token when argv is empty: {}",
            plan.boot_args
        );
    }

    #[test]
    fn build_launch_plan_does_not_forward_backend_only_env() {
        let policy = SandboxPolicy {
            env: vec![
                ("KASTELLAN_MICROVM_DIR".to_string(), "/var/lib/kastellan/microvm".to_string()),
                ("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-fetch.ext4".to_string()),
                ("KASTELLAN_WEB_FETCH_ALLOWLIST".to_string(), "[\"example.com\"]".to_string()),
            ],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        // The worker key IS forwarded; the two backend-only keys are NOT.
        assert!(plan.boot_args.contains(&super::hex_encode(b"KASTELLAN_WEB_FETCH_ALLOWLIST")));
        assert!(!plan.boot_args.contains(&super::hex_encode(b"KASTELLAN_MICROVM_DIR")));
        assert!(!plan.boot_args.contains(&super::hex_encode(b"KASTELLAN_MICROVM_ROOTFS")));
    }

    #[test]
    fn build_launch_plan_fails_closed_over_cmdline_cap() {
        // A pathologically large env must fail closed, never truncate the
        // cmdline (which would corrupt the boot).
        let big = "x".repeat(MAX_CMDLINE_BYTES);
        let policy =
            SandboxPolicy { env: vec![("HUGE".to_string(), big)], ..Default::default() };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(
            format!("{err}").contains("cmdline"),
            "expected a cmdline-cap error, got: {err}"
        );
    }

    #[test]
    fn cpu_quota_maps_to_vcpu_count() {
        // None → 1 vcpu (slice-1 default); Some(250) → 3 vcpus (ceil 250/100).
        let p_none = SandboxPolicy { cpu_quota_pct: None, ..Default::default() };
        assert_eq!(build_launch_plan(&p_none, &img(), "/w", &[]).unwrap().vcpu_count, 1);
        let p_250 = SandboxPolicy { cpu_quota_pct: Some(250), ..Default::default() };
        assert_eq!(build_launch_plan(&p_250, &img(), "/w", &[]).unwrap().vcpu_count, 3);
    }

    #[test]
    fn fs_read_derives_ro_share_with_device_node() {
        let policy = SandboxPolicy {
            fs_read: vec![PathBuf::from("/opt/venv"), PathBuf::from("/data/models")],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        let ro = plan.ro_share.expect("ro_share derived from fs_read");
        assert_eq!(ro.sources, vec![PathBuf::from("/opt/venv"), PathBuf::from("/data/models")]);
        assert_eq!(ro.guest_dev, "/dev/vdb", "RO share is the first extra drive");
        // Placeholder image path present so render attaches the drive; spawn overrides it.
        assert!(plan.ro_image_path.is_some());
    }

    #[test]
    fn fs_write_derives_rw_scratch_after_ro() {
        let policy = SandboxPolicy {
            fs_read: vec![PathBuf::from("/opt/venv")],
            fs_write: vec![PathBuf::from("/tmp/scratch")],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert_eq!(plan.ro_share.unwrap().guest_dev, "/dev/vdb");
        let rw = plan.rw_scratch.expect("rw_scratch derived from fs_write");
        assert_eq!(rw.mountpoint, PathBuf::from("/tmp/scratch"));
        assert_eq!(rw.guest_dev, "/dev/vdc", "RW is the second extra drive when RO present");
        assert!(plan.rw_image_path.is_some());
    }

    #[test]
    fn rw_scratch_is_vdb_when_no_ro_share() {
        let policy = SandboxPolicy {
            fs_write: vec![PathBuf::from("/tmp/scratch")],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert!(plan.ro_share.is_none());
        assert_eq!(plan.rw_scratch.unwrap().guest_dev, "/dev/vdb");
    }

    #[test]
    fn empty_policy_has_no_extra_drives() {
        let plan = build_launch_plan(&SandboxPolicy::default(), &img(), "/w", &[]).unwrap();
        assert!(plan.ro_share.is_none() && plan.rw_scratch.is_none());
        assert!(plan.ro_image_path.is_none() && plan.rw_image_path.is_none());
    }

    #[test]
    fn fs_read_under_system_dir_fails_closed() {
        // Mounting a tmpfs anchor over /usr would hide the worker's own files.
        let policy = SandboxPolicy { fs_read: vec![PathBuf::from("/usr/lib/foo")], ..Default::default() };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(format!("{err}").contains("share anchor") && format!("{err}").contains("/usr"));
    }

    #[test]
    fn fs_read_under_non_anchor_top_level_fails_closed() {
        // /home and /var are not system dirs, but the rootfs has no anchor for
        // them — an old "reject /usr|/etc" blocklist would have let these through
        // and they'd silently fail to mount in-guest. The allowlist rejects them.
        for p in ["/home/user/data", "/var/lib/models"] {
            let policy = SandboxPolicy { fs_read: vec![PathBuf::from(p)], ..Default::default() };
            let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
            assert!(
                format!("{err}").contains("share anchor"),
                "non-anchor fs_read {p} must be rejected: {err}"
            );
        }
    }

    #[test]
    fn fs_write_under_non_anchor_top_level_fails_closed() {
        let policy =
            SandboxPolicy { fs_write: vec![PathBuf::from("/home/user/scratch")], ..Default::default() };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(format!("{err}").contains("share anchor"), "non-anchor fs_write must be rejected: {err}");
    }

    #[test]
    fn multiple_fs_write_fails_closed() {
        let policy = SandboxPolicy {
            fs_write: vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")],
            ..Default::default()
        };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(format!("{err}").contains("single writable"));
    }

    #[test]
    fn build_launch_plan_appends_mounts_token() {
        let policy = SandboxPolicy {
            fs_read: vec![PathBuf::from("/opt/venv")],
            fs_write: vec![PathBuf::from("/tmp/scratch")],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert!(plan.boot_args.contains(" kastellan.mounts="), "mounts token in boot_args: {}", plan.boot_args);
    }

    #[test]
    fn build_launch_plan_no_shares_omits_mounts_token() {
        let plan = build_launch_plan(&SandboxPolicy::default(), &img(), "/w", &[]).unwrap();
        assert!(!plan.boot_args.contains("kastellan.mounts"));
    }

    #[test]
    fn config_attaches_ro_and_rw_drives_in_order() {
        let policy = SandboxPolicy {
            fs_read: vec![PathBuf::from("/opt/venv")],
            fs_write: vec![PathBuf::from("/tmp/scratch")],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        let cfg = render_firecracker_config(&plan);
        let drives = cfg["drives"].as_array().unwrap();
        assert_eq!(drives.len(), 3, "rootfs + ro-share + rw-scratch");
        assert_eq!(drives[0]["drive_id"], "rootfs");
        assert_eq!(drives[1]["drive_id"], "ro-share");
        assert_eq!(drives[1]["is_read_only"], true);
        assert_eq!(drives[2]["drive_id"], "rw-scratch");
        assert_eq!(drives[2]["is_read_only"], false);
    }

    #[test]
    fn config_drive_order_matches_device_letters() {
        // Pin the invariant: ro=vdb (drives[1]), rw=vdc (drives[2]). The guest relies
        // on the manifest's device nodes, which this order must agree with.
        let policy = SandboxPolicy {
            fs_read: vec![PathBuf::from("/opt/venv")],
            fs_write: vec![PathBuf::from("/tmp/scratch")],
            ..Default::default()
        };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert_eq!(plan.ro_share.as_ref().unwrap().guest_dev, "/dev/vdb");
        assert_eq!(plan.rw_scratch.as_ref().unwrap().guest_dev, "/dev/vdc");
        let cfg = render_firecracker_config(&plan);
        // rootfs=vda (drives[0]), then ro (drives[1]) → vdb, rw (drives[2]) → vdc.
        assert_eq!(cfg["drives"][1]["drive_id"], "ro-share");
        assert_eq!(cfg["drives"][2]["drive_id"], "rw-scratch");
    }

    #[test]
    fn config_no_extra_drives_when_no_shares() {
        let plan = build_launch_plan(&SandboxPolicy::default(), &img(), "/w", &[]).unwrap();
        let cfg = render_firecracker_config(&plan);
        assert_eq!(cfg["drives"].as_array().unwrap().len(), 1, "only the rootfs drive");
    }

    fn forced_policy(uds: &str) -> SandboxPolicy {
        SandboxPolicy {
            net: Net::Allowlist(vec!["example.com:443".into()]),
            proxy_uds: Some(PathBuf::from(uds)),
            ..Default::default()
        }
    }

    #[test]
    fn force_routed_sets_egress_port_and_disables_net() {
        let plan = build_launch_plan(&forced_policy("/scratch/egress.sock"), &img(), "/w", &[]).unwrap();
        assert_eq!(plan.egress_proxy_vsock_port, Some(EGRESS_VSOCK_PORT));
        assert_eq!(plan.egress_host_uds.as_deref(), Some(std::path::Path::new("/scratch/egress.sock")));
        assert!(!plan.net_enabled, "force-routed VM has no virtio-net device");
        assert!(plan.boot_args.contains(" kastellan.egress=1"), "egress cmdline token present");
        assert!(!plan.boot_args.contains("selftest"), "no selftest token without the knob");
    }

    #[test]
    fn force_routed_overrides_guest_proxy_uds_env() {
        // A pre-set host UDS in env is rewritten to the in-guest path the worker dials.
        let mut policy = forced_policy("/scratch/egress.sock");
        policy.env = vec![("KASTELLAN_EGRESS_PROXY_UDS".into(), "/scratch/egress.sock".into())];
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        let val = plan.env.iter().find(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_UDS").map(|(_, v)| v.as_str());
        assert_eq!(val, Some("/run/kastellan-egress.sock"));
    }

    #[test]
    fn selftest_knob_emits_selftest_token() {
        let mut policy = forced_policy("/scratch/egress.sock");
        policy.env = vec![("KASTELLAN_MICROVM_EGRESS_SELFTEST".into(), "1".into())];
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert!(plan.boot_args.contains(" kastellan.egress.selftest=1"));
    }

    #[test]
    fn allowlist_without_proxy_uds_is_rejected() {
        let policy = SandboxPolicy { net: Net::Allowlist(vec!["x:443".into()]), ..Default::default() };
        let err = build_launch_plan(&policy, &img(), "/w", &[]).unwrap_err();
        assert!(format!("{err:?}").contains("force-routing"), "fail-closed reject: {err:?}");
    }

    #[test]
    fn net_deny_has_no_egress_channel() {
        let policy = SandboxPolicy { net: Net::Deny, ..Default::default() };
        let plan = build_launch_plan(&policy, &img(), "/w", &[]).unwrap();
        assert_eq!(plan.egress_proxy_vsock_port, None);
        assert!(plan.egress_host_uds.is_none());
        assert!(!plan.boot_args.contains("kastellan.egress"));
    }

    #[test]
    fn persistent_store_assigns_drive_and_rw_mount() {
        let mut policy = SandboxPolicy { net: Net::Deny, ..Default::default() };
        policy.persistent_store = Some(crate::PersistentStore {
            host_backing: std::path::PathBuf::from("/var/lib/kastellan/kv/store.ext4"),
            guest_mount: std::path::PathBuf::from("/data"),
            size_mib: 64,
        });
        let plan = build_launch_plan(&policy, &img(), "/usr/local/bin/kastellan-worker-kv-demo", &[]).unwrap();
        let pm = plan.persistent_mount.as_ref().expect("persistent mount present");
        assert_eq!(pm.mountpoint, std::path::PathBuf::from("/data"));
        // distinct guest_dev from any ro/rw share device (no ro/rw here → first
        // available letter after vda=rootfs is vdb)
        assert!(pm.guest_dev.starts_with("/dev/vd"));
        assert!(plan.boot_args.contains("kastellan.mounts="));
        // persistent_store is copied from policy
        assert!(plan.persistent_store.is_some());
        // persistent_image_path is None at plan-build time (spawn fills it)
        assert!(plan.persistent_image_path.is_none());
        // rendered config attaches a non-root RW drive for the persistent image
        // only when persistent_image_path is Some (spawn sets it); at plan time it
        // is None so the drive is NOT yet in the config — the spawn supplies it.
        // Set it manually to exercise the render path.
        let mut plan_with_img = plan;
        plan_with_img.persistent_image_path =
            Some(std::path::PathBuf::from("/var/lib/kastellan/kv/store.ext4"));
        let cfg = render_firecracker_config(&plan_with_img);
        let drives = cfg["drives"].as_array().unwrap();
        assert!(drives.iter().any(|d| d["drive_id"] == "persistent-store" && d["is_read_only"] == false));
    }
}
