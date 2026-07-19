#![cfg(target_os = "linux")]
//! browser-driver × Firecracker micro-VM — slice 1 (the rootfs).
//!
//! ## Tiers
//!
//! * `vm_policy_flows_through_plan_to_in_rootfs_guest_path` — hermetic; always
//!   runs on Linux (no KVM, no network, no rootfs image needed). It feeds a
//!   browser-driver VM policy through the REAL `build_launch_plan` and pins
//!   that the guest execs the **in-rootfs** worker path rather than a host
//!   `target/` path. That failure mode is nasty and has cost a debugging
//!   session before: PID1 `execv`s a path that does not exist inside the guest,
//!   panics, the VM boot-loops, and the dispatch simply hangs to wall-clock —
//!   presenting as a channel hang with no error naming the real cause. It also
//!   pins the cmdline budget, because env is hex-encoded and therefore costs
//!   two cmdline bytes per env byte.
//!
//! * `vm_booted_browser_driver_launches_chromium` — the live DGX tier
//!   (`#[ignore]`): boots `browser-driver.ext4` and proves Chromium starts
//!   inside the guest.
//!
//! Note `kastellan_sandbox::linux_firecracker` is `#[cfg(target_os = "linux")]`
//! (`sandbox/src/lib.rs:11-16`), so this whole file is compiled out on macOS.
//! The DGX `clippy -p kastellan-core --all-targets -D warnings` gate is the
//! authoritative check for it; Mac clippy cannot see this code at all.

use std::path::PathBuf;

use kastellan_sandbox::linux_firecracker::{build_launch_plan, FirecrackerImage};
use kastellan_sandbox::{Net, Profile, SandboxPolicy};

/// The worker path baked into the rootfs by
/// `scripts/workers/microvm/build-browser-driver-rootfs.sh` (as a symlink into
/// the staged venv). Slice 2's `MICROVM_WORKER_BIN` const must match this
/// byte for byte.
const IN_ROOTFS_WORKER: &str = "/usr/local/bin/kastellan-worker-browser-driver";

/// Decode the lowercase-hex cmdline tokens `microvm-init` consumes.
///
/// `plan.rs::hex_encode` is `pub(super)`, so a test cannot reach its inverse;
/// this is the minimal decoder needed to read one token back.
fn hex_decode(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "hex token has odd length: {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

/// The VM policy slice 2's `browser_driver_firecracker_entry` will produce.
///
/// Built inline because that production entry does not exist yet — slice 1 is
/// the rootfs only. Mirrors the shape of `web_fetch_firecracker_entry`: empty
/// `fs_read` (a VM shares no host paths in), force-routed, VM backend.
fn browser_driver_vm_policy() -> SandboxPolicy {
    SandboxPolicy {
        // Empty: the per-instance CA is appended at spawn, and browser-driver
        // runs the sidecar in no-MITM transparent-tunnel mode anyway
        // (force_route::disable_mitm_for names this worker).
        fs_read: vec![],
        fs_write: vec![],
        // `Net::Allowlist` WITH `proxy_uds` == force-routed. Without `proxy_uds`
        // `build_launch_plan` rejects it fail-closed, because a VM carries no
        // virtio-net device (plan.rs:255-267).
        net: Net::Allowlist(vec!["example.org:443".to_string()]),
        cpu_ms: 30_000,
        // Chromium plus a RAM-backed /tmp tmpfs; see the design spec §6.
        mem_mb: 2048,
        profile: Profile::WorkerBrowserClient,
        tasks_max: Some(512),
        env: vec![
            (
                "KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(),
                r#"["example.org"]"#.to_string(),
            ),
            (
                "PLAYWRIGHT_BROWSERS_PATH".to_string(),
                "/usr/local/lib/kastellan-browser-driver/browsers".to_string(),
            ),
            ("TMPDIR".to_string(), "/tmp".to_string()),
            // Playwright's Node driver calls uv_os_homedir(); without HOME it
            // dies with "Connection closed while reading from the driver".
            ("HOME".to_string(), "/tmp".to_string()),
        ],
        proxy_uds: Some(PathBuf::from("/tmp/kastellan-egress.sock")),
        ..Default::default()
    }
}

/// Image coordinates for the browser-driver micro-VM. The paths need not exist
/// for the hermetic tier — `build_launch_plan` is pure and does not touch the
/// filesystem.
fn browser_driver_image() -> FirecrackerImage {
    FirecrackerImage {
        kernel_path: PathBuf::from("/var/lib/kastellan/microvm/vmlinux"),
        rootfs_path: PathBuf::from("/var/lib/kastellan/microvm/browser-driver.ext4"),
    }
}

#[test]
fn vm_policy_flows_through_plan_to_in_rootfs_guest_path() {
    let plan = build_launch_plan(
        &browser_driver_vm_policy(),
        &browser_driver_image(),
        IN_ROOTFS_WORKER,
        &[],
    )
    .expect("a force-routed browser-driver VM policy must produce a launch plan");

    // 1. The guest execs the in-rootfs path, NOT a host target/ path.
    let token = plan
        .boot_args
        .split_whitespace()
        .find_map(|t| t.strip_prefix("kastellan.worker="))
        .expect("boot args must carry a kastellan.worker= token");
    let decoded = String::from_utf8(hex_decode(token)).expect("worker token is utf8");
    assert_eq!(
        decoded, IN_ROOTFS_WORKER,
        "the guest must exec the in-rootfs worker path; a host target/ path \
         ENOENTs inside the guest, panics PID1 and boot-loops, which surfaces \
         only as a dispatch hang to wall-clock"
    );

    // 2. Force-routed ⇒ the VM carries no virtio-net device at all. This is
    //    strictly stronger than the bwrap private-netns path browser-driver
    //    uses in host mode.
    assert!(
        !plan.net_enabled,
        "a force-routed VM worker must boot with no NIC"
    );

    // 3. Cmdline budget. Env is hex-encoded (two cmdline bytes per env byte),
    //    so the env set is the real constraint on this entry.
    //    `build_launch_plan` already fails closed above MAX_CMDLINE_BYTES
    //    (1920, plan.rs:137) — reaching this line proves we are under the hard
    //    cap. Assert real HEADROOM too, so that a production-sized allowlist
    //    (longer than this fixture's single host) cannot silently tip a future
    //    slice over the cap.
    let used = plan.boot_args.len();
    assert!(
        used < 1536,
        "cmdline is {used} bytes, leaving under 384 bytes of headroom below the \
         1920-byte cap; a production-sized allowlist would not fit"
    );
}
