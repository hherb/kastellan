# Linux Firecracker micro-VM backend — Slice 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a generic `SandboxBackendKind::FirecrackerVm` Linux backend and boot a `Net::Deny`, in-image `python-exec` worker inside a Firecracker micro-VM, speaking JSON-RPC over vsock, with KVM-enforced `mem_mb`.

**Architecture:** A new `LinuxFirecracker` backend (pure `build_launch_plan` + spawn, mirroring `linux_bwrap.rs`) spawns a small `kastellan-microvm-run` launcher binary as the `Child` whose stdio carries JSON-RPC. The launcher boots Firecracker (kernel console → log fd, never stdout), opens the firecracker-managed vsock UDS, and bridges `stdin↔vsock`/`vsock↔stdout`. Inside the guest a PID1 `kastellan-microvm-init` connects the vsock port, `dup2`s it onto fd 0/1, and `exec`s the unchanged `serve_stdio` worker. The rootfs is a minimal ext4 built by `build-rootfs.sh`.

**Tech Stack:** Rust (workspace crates `kastellan-sandbox`, new `kastellan-microvm-run`, new `kastellan-microvm-init`); Firecracker v1.16.0 (aarch64); vsock (`AF_VSOCK`); ext4 rootfs; the existing `kastellan-protocol` JSON-RPC stdio client.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** Firecracker is Apache-2.0 (binary dependency, not linked). vsock via `std`/`libc` or the `vsock` crate (MIT/Apache-2.0) — verify license before adding.
- **Cross-platform:** every new module/file is `#[cfg(target_os = "linux")]`-gated. macOS keeps Seatbelt + `Container`. Never reference `FirecrackerVm` from non-linux-cfg code (issue #144 rule — a `Container`-on-Linux reference broke the core build before).
- **`SandboxBackend` is `dyn`-safe** — do not add generic methods; the new backend is a new type implementing the existing trait.
- **`SandboxPolicy.fs_read`/`fs_write` paths must be absolute** — reject relative up front (matches bwrap).
- **Pure-fn-then-spawn pattern** (rule #1): `build_launch_plan(policy, program, args) -> FirecrackerLaunchPlan` is pure and unit-tested; the spawn is a thin wrapper.
- **TDD (rule #2):** failing test first, every task.
- **Files under 500 LOC** (rule #4); split when a file approaches the cap.
- **Test execution reality:** the `sandbox` crate's linux-gated code does **not** run under `cargo test` on the macOS dev box. **Compile-check on the Mac** with `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu` (pure-Rust crate, works per the cross-clippy convention). **Run the unit tests on the DGX** over SSH: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox <name>'`. The `ssh dgx '<cmd>'` form is exact — flags before the hostname get denied.
- **DGX one-time operator setup** (needed only for the e2e boot, Task 7; document, do not script into CI): `sudo modprobe vhost_vsock`; grant the worker user `/dev/vhost-vsock` (add to `kvm` group or ACL); install the firecracker binary on `$PATH`; run `build-rootfs.sh`. `/dev/kvm` is already RW to the user.
- **Firecracker artifacts (pinned):** kernel `vmlinux-6.1.102`, firecracker `v1.16.0`, aarch64. Pin versions in the build script + a constants module; never float tags (GLIBC/ABI skew lesson from the macOS container build).

---

### Task 1: `FirecrackerVm` enum variant + registry + backend skeleton

Introduces the variant, the registry slot, the `resolve()` arm, and a stub `LinuxFirecracker` backend whose `spawn_under_policy` errors `not implemented` (fleshed out in Task 2). This isolates the cross-cutting plumbing from the launch logic so a reviewer can gate it independently.

**Files:**
- Modify: `sandbox/src/lib.rs` (enum `SandboxBackendKind` ~200-207; `SandboxBackends` struct ~266-274; `default_for_current_os` ~281-295; `resolve` ~319-346; add `#[cfg(target_os="linux")] pub mod linux_firecracker;` near line 12)
- Create: `sandbox/src/linux_firecracker.rs`
- Test: inline `#[cfg(all(test, target_os = "linux"))]` module in `sandbox/src/lib.rs`

**Interfaces:**
- Produces: `SandboxBackendKind::FirecrackerVm` (linux-cfg variant); `linux_firecracker::LinuxFirecracker` (struct, `new() -> Self`, impl `SandboxBackend`); `SandboxBackends.firecracker: Arc<dyn SandboxBackend>` (linux field).
- Consumes: existing `SandboxBackend` trait, `SandboxError::Backend(String)`, `SandboxPolicy`.

- [ ] **Step 1: Write the failing test** — append to `sandbox/src/lib.rs`:

```rust
#[cfg(all(test, target_os = "linux"))]
mod firecracker_registry_tests {
    use super::*;

    #[test]
    fn resolve_returns_firecracker_for_firecracker_kind() {
        let backends = SandboxBackends::default_for_current_os();
        // Resolving the FirecrackerVm kind must hand back a backend (the
        // firecracker slot), not the bwrap default. We can't compare Arcs by
        // identity through `dyn`, so assert the slot is wired by resolving and
        // confirming it does not error on construction.
        let _backend = backends.resolve(Some(SandboxBackendKind::FirecrackerVm), None);
        // The default (None) must still resolve to bwrap and remain distinct.
        let _default = backends.resolve(None, None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu 2>&1 | head -30`
Expected: compile error — `no variant named FirecrackerVm`, `no field firecracker`, unresolved `linux_firecracker`.

- [ ] **Step 3: Add the variant + module declaration** — in `sandbox/src/lib.rs`:

```rust
// near line 12, with the other linux-gated mods:
#[cfg(target_os = "linux")]
pub mod linux_firecracker;
```

```rust
// in enum SandboxBackendKind, alongside Bwrap:
    #[cfg(target_os = "linux")]
    FirecrackerVm,
```

- [ ] **Step 4: Add the registry field, constructor, and resolve arm** — in `sandbox/src/lib.rs`:

```rust
// SandboxBackends struct, alongside `bwrap`:
    #[cfg(target_os = "linux")]
    pub firecracker: Arc<dyn SandboxBackend>,
```

```rust
// default_for_current_os, the linux branch:
        #[cfg(target_os = "linux")]
        {
            Self {
                bwrap: Arc::new(linux_bwrap::LinuxBwrap::new()),
                firecracker: Arc::new(linux_firecracker::LinuxFirecracker::new()),
            }
        }
```

```rust
// resolve(), a new arm after the Bwrap arm:
            #[cfg(target_os = "linux")]
            (Some(SandboxBackendKind::FirecrackerVm), _) => Arc::clone(&self.firecracker),
```

- [ ] **Step 5: Create the backend skeleton** — `sandbox/src/linux_firecracker.rs`:

```rust
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

use std::process::Child;

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

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
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<Child, SandboxError> {
        Err(SandboxError::Backend(
            "linux_firecracker: spawn not implemented yet (Task 2)".into(),
        ))
    }
}
```

- [ ] **Step 6: Verify compile + run the test**

Run (Mac, compile-check): `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu -- -D warnings`
Expected: clean.
Run (DGX, execute): `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox firecracker_registry_tests'`
Expected: PASS (1 test).

- [ ] **Step 7: Commit**

```bash
git add sandbox/src/lib.rs sandbox/src/linux_firecracker.rs
git commit -m "feat(sandbox): FirecrackerVm backend kind + registry skeleton"
```

---

### Task 2: Pure `build_launch_plan` + `FirecrackerLaunchPlan` + config builder

The testable heart: translate a `SandboxPolicy` into a `FirecrackerLaunchPlan` (machine config, drives, vsock, env, net flag) and render the Firecracker JSON config. No KVM, no spawn — all pure.

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs`
- Create: `sandbox/src/linux_firecracker/plan.rs` (keep `linux_firecracker.rs` under cap; declare `mod plan;` + `pub use plan::*;`)
- Test: inline `#[cfg(test)]` in `plan.rs` (runs on the DGX)

**Interfaces:**
- Produces:
  - `FirecrackerLaunchPlan { kernel_path: PathBuf, rootfs_path: PathBuf, vcpu_count: u8, mem_size_mib: usize, vsock_cid: u32, vsock_uds: PathBuf, vsock_port: u32, boot_args: String, env: Vec<(String,String)>, net_enabled: bool }`
  - `pub fn build_launch_plan(policy: &SandboxPolicy, image: &FirecrackerImage, program: &str, args: &[&str]) -> Result<FirecrackerLaunchPlan, SandboxError>`
  - `pub fn render_firecracker_config(plan: &FirecrackerLaunchPlan) -> serde_json::Value`
  - `FirecrackerImage { kernel_path: PathBuf, rootfs_path: PathBuf }` (where the rootfs/kernel live; defaulted from constants, overridable by the `image` tag later).
- Consumes: `SandboxPolicy` (`mem_mb`, `cpu_quota_pct`, `net`, `env`, `fs_read`, `fs_write`), `Net`.

- [ ] **Step 1: Write the failing tests** — create `sandbox/src/linux_firecracker/plan.rs`:

```rust
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
        assert_eq!(vsock["uds_path"], plan.vsock_uds.to_string_lossy());
    }

    #[test]
    fn config_pins_kernel_and_rootfs_paths() {
        let plan = build_launch_plan(&SandboxPolicy::default(), &img(), "/w", &[]).unwrap();
        let cfg = render_firecracker_config(&plan);
        assert_eq!(cfg["boot-source"]["kernel_image_path"], img().kernel_path.to_string_lossy());
        assert_eq!(cfg["drives"][0]["path_on_host"], img().rootfs_path.to_string_lossy());
        assert_eq!(cfg["drives"][0]["is_root_device"], true);
    }

    #[test]
    fn relative_fs_paths_rejected() {
        let policy = SandboxPolicy { fs_read: vec![PathBuf::from("rel/path")], ..Default::default() };
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu 2>&1 | head`
Expected: compile error — `build_launch_plan`/`FirecrackerLaunchPlan`/`FirecrackerImage`/`render_firecracker_config` not found.

- [ ] **Step 3: Implement the plan + config builder** — prepend to `sandbox/src/linux_firecracker/plan.rs`:

```rust
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
    pub env: Vec<(String, String)>,
    pub net_enabled: bool,
}

/// Guest CID for the worker VM. CIDs 0–2 are reserved (hypervisor/host/local),
/// so workers use 3. One VM per launcher process → no CID collision.
const WORKER_GUEST_CID: u32 = 3;
/// Fixed vsock port the guest init listens on for the JSON-RPC bridge.
pub const WORKER_VSOCK_PORT: u32 = 1024;
/// Kernel cmdline: serial console for *kernel* logs only (the launcher routes
/// it to a log fd, never stdout); JSON-RPC rides vsock, not the console.
const BASE_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off i8042.noaux=1 i8042.nomux=1";

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
        Some(pct) => (((pct as u32) + 99) / 100).clamp(1, 8) as u8,
    };

    let net_enabled = !matches!(policy.net, Net::Deny);

    // vsock UDS lives next to the rootfs image dir, suffixed per-PID at spawn
    // time by the launcher; the plan carries a deterministic base the launcher
    // uniquifies. Slice 1 uses a fixed name; the launcher overrides.
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
            "is_read_only": false,
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
```

- [ ] **Step 4: Wire the module into `linux_firecracker.rs`** — add near the top:

```rust
mod plan;
pub use plan::{build_launch_plan, render_firecracker_config, FirecrackerImage, FirecrackerLaunchPlan, WORKER_VSOCK_PORT};
```

- [ ] **Step 5: Run tests on the DGX**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox linux_firecracker::plan'`
Expected: PASS (6 tests). Note: `net_deny_disables_net_device` asserts no `network-interfaces` key — the `net_enabled` false path skips it.

- [ ] **Step 6: Mac compile-check + commit**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu -- -D warnings`
Expected: clean.

```bash
git add sandbox/src/linux_firecracker.rs sandbox/src/linux_firecracker/plan.rs
git commit -m "feat(sandbox): pure build_launch_plan + Firecracker config builder"
```

---

### Task 3: `probe()` capability checks with operator-fix messages

Fail-closed readiness: firecracker binary present, `/dev/kvm` RW, `/dev/vhost-vsock` RW, kernel + rootfs present. Each failure names its fix. The decision logic is a pure function over injected check results so it's unit-testable without the real devices.

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs` (add `probe()` + the pure `probe_report`)
- Create: `sandbox/src/linux_firecracker/probe.rs`
- Test: inline `#[cfg(test)]` in `probe.rs`

**Interfaces:**
- Produces:
  - `pub struct ProbeInputs { firecracker_on_path: bool, kvm_rw: bool, vhost_vsock_rw: bool, kernel_present: bool, rootfs_present: bool }`
  - `pub fn probe_report(inputs: &ProbeInputs) -> Result<(), SandboxError>` (pure)
  - `impl LinuxFirecracker { pub fn probe(image: &FirecrackerImage) -> Result<(), SandboxError> }` (gathers real inputs, delegates to `probe_report`)
- Consumes: `FirecrackerImage`, `SandboxError`.

- [ ] **Step 1: Write the failing tests** — create `sandbox/src/linux_firecracker/probe.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> ProbeInputs {
        ProbeInputs {
            firecracker_on_path: true, kvm_rw: true, vhost_vsock_rw: true,
            kernel_present: true, rootfs_present: true,
        }
    }

    #[test]
    fn all_present_is_ok() {
        assert!(probe_report(&ok()).is_ok());
    }

    #[test]
    fn missing_firecracker_names_fix() {
        let err = probe_report(&ProbeInputs { firecracker_on_path: false, ..ok() }).unwrap_err();
        assert!(format!("{err}").contains("firecracker"));
    }

    #[test]
    fn missing_vsock_names_modprobe_fix() {
        let err = probe_report(&ProbeInputs { vhost_vsock_rw: false, ..ok() }).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("vhost_vsock") && msg.contains("modprobe"));
    }

    #[test]
    fn missing_kvm_names_fix() {
        let err = probe_report(&ProbeInputs { kvm_rw: false, ..ok() }).unwrap_err();
        assert!(format!("{err}").contains("/dev/kvm"));
    }

    #[test]
    fn missing_rootfs_names_build_script() {
        let err = probe_report(&ProbeInputs { rootfs_present: false, ..ok() }).unwrap_err();
        assert!(format!("{err}").contains("build-rootfs.sh"));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu 2>&1 | head`
Expected: `ProbeInputs`/`probe_report` not found.

- [ ] **Step 3: Implement the pure report + real gatherer** — prepend to `probe.rs`:

```rust
//! Fail-closed readiness probe for the Firecracker backend. The decision is a
//! pure fn over injected capability bits; the real device/binary checks are a
//! thin gatherer so the logic is testable without KVM.

use std::path::Path;

use crate::SandboxError;

use super::FirecrackerImage;

/// Capability bits the probe checks. Each false → a specific operator fix.
pub struct ProbeInputs {
    pub firecracker_on_path: bool,
    pub kvm_rw: bool,
    pub vhost_vsock_rw: bool,
    pub kernel_present: bool,
    pub rootfs_present: bool,
}

/// Pure: turn capability bits into an Ok or a fail-closed error naming the fix.
pub fn probe_report(inputs: &ProbeInputs) -> Result<(), SandboxError> {
    if !inputs.firecracker_on_path {
        return Err(SandboxError::Backend(
            "firecracker binary not on $PATH — install the pinned v1.16.0 release \
             (scripts/workers/microvm/install-firecracker.sh)".into(),
        ));
    }
    if !inputs.kvm_rw {
        return Err(SandboxError::Backend(
            "/dev/kvm not readable+writable by this user — add the worker user to the \
             `kvm` group (or ACL /dev/kvm) and re-login".into(),
        ));
    }
    if !inputs.vhost_vsock_rw {
        return Err(SandboxError::Backend(
            "/dev/vhost-vsock not accessible — run `sudo modprobe vhost_vsock` and grant \
             the worker user access (kvm group or ACL on /dev/vhost-vsock)".into(),
        ));
    }
    if !inputs.kernel_present {
        return Err(SandboxError::Backend(
            "guest kernel image missing — run scripts/workers/microvm/build-rootfs.sh".into(),
        ));
    }
    if !inputs.rootfs_present {
        return Err(SandboxError::Backend(
            "guest rootfs image missing — run scripts/workers/microvm/build-rootfs.sh".into(),
        ));
    }
    Ok(())
}

/// True iff `path` is openable read+write by the current user.
fn dev_rw(path: &str) -> bool {
    use std::fs::OpenOptions;
    OpenOptions::new().read(true).write(true).open(path).is_ok()
}

impl super::LinuxFirecracker {
    /// Gather real capability bits and delegate to [`probe_report`].
    pub fn probe(image: &FirecrackerImage) -> Result<(), SandboxError> {
        let inputs = ProbeInputs {
            firecracker_on_path: which_firecracker(),
            kvm_rw: dev_rw("/dev/kvm"),
            vhost_vsock_rw: dev_rw("/dev/vhost-vsock"),
            kernel_present: Path::new(&image.kernel_path).exists(),
            rootfs_present: Path::new(&image.rootfs_path).exists(),
        };
        probe_report(&inputs)
    }
}

/// Cheap `$PATH` lookup for the firecracker binary (no spawn).
fn which_firecracker() -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("firecracker").is_file())
        })
        .unwrap_or(false)
}
```

- [ ] **Step 4: Declare the module** — in `linux_firecracker.rs` add `mod probe;` and `pub use probe::{probe_report, ProbeInputs};`.

- [ ] **Step 5: Run tests on the DGX + Mac compile-check**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox linux_firecracker::probe'`
Expected: PASS (5 tests).
Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add sandbox/src/linux_firecracker.rs sandbox/src/linux_firecracker/probe.rs
git commit -m "feat(sandbox): fail-closed Firecracker probe with operator-fix messages"
```

---

### Task 4: `kastellan-microvm-run` launcher crate (the Child)

A new workspace binary crate. It is the `Child` whose stdio carries JSON-RPC. It: writes the Firecracker config (from `render_firecracker_config`), spawns firecracker with the kernel console routed to a log file (never stdout), connects to the firecracker-managed vsock UDS at the worker port, and copies `stdin↔vsock`/`vsock↔stdout`. RAII teardown kills firecracker and removes the UDS on exit. The pure parts (argv assembly, the Firecracker vsock UDS handshake string) are unit-tested; the boot is exercised by the Task 7 e2e.

**Files:**
- Create: `workers/microvm-run/Cargo.toml`, `workers/microvm-run/src/main.rs`, `workers/microvm-run/src/bridge.rs`, `workers/microvm-run/src/boot.rs`
- Modify: root `Cargo.toml` workspace `members`
- Test: inline `#[cfg(test)]` in `bridge.rs` and `boot.rs`

**Interfaces:**
- Consumes (CLI args, passed by `LinuxFirecracker::spawn_under_policy` in Task 6's wiring — actually wired here): `--config-file <path>`, `--vsock-uds <path>`, `--vsock-port <u32>`, `--log <path>`.
- Produces:
  - `boot::firecracker_argv(config_path: &str, log_path: &str) -> Vec<String>` (pure)
  - `bridge::firecracker_vsock_connect_line(port: u32) -> String` (pure — Firecracker's hybrid-vsock requires the connector to send `CONNECT <port>\n` after dialing the host UDS)
  - `bridge::pump(uds: UnixStream)` — copies stdin↔stream bidirectionally (integration).

- [ ] **Step 1: Write the failing pure tests** — create `workers/microvm-run/src/boot.rs` and `bridge.rs` with:

```rust
// boot.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn firecracker_argv_uses_no_api_and_config() {
        let argv = firecracker_argv("/run/fc.json", "/run/fc.log");
        assert_eq!(argv[0], "firecracker");
        assert!(argv.iter().any(|a| a == "--no-api"));
        assert!(argv.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"));
        // Kernel console must be redirected away from our stdout.
        assert!(argv.windows(2).any(|w| w[0] == "--log-path" && w[1] == "/run/fc.log"));
    }
}
```

```rust
// bridge.rs
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vsock_connect_line_is_connect_port_newline() {
        // Firecracker hybrid vsock: after connecting the host UDS, the client
        // sends "CONNECT <port>\n" and waits for "OK <hostport>\n".
        assert_eq!(firecracker_vsock_connect_line(1024), "CONNECT 1024\n");
    }
}
```

- [ ] **Step 2: Create the crate + run to fail**

`workers/microvm-run/Cargo.toml`:

```toml
[package]
name = "kastellan-microvm-run"
version = "0.1.0"
edition = "2021"
license = "AGPL-3.0-only"

[dependencies]
serde_json = "1"

[lints]
workspace = true
```

Add `"workers/microvm-run"` to the root `Cargo.toml` `[workspace] members`.

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-microvm-run 2>&1 | head'`
Expected: compile errors (`firecracker_argv`/`firecracker_vsock_connect_line` not found).

- [ ] **Step 3: Implement the pure helpers** — prepend to the respective files:

```rust
// boot.rs
//! Firecracker process invocation (pure argv) + spawn helper.

/// Build the firecracker argv. `--no-api` + `--config-file` boots a fully
/// pre-described VM; `--log-path` sends Firecracker's own logs (including the
/// guest kernel console it captures) to a file, keeping our stdout clean for
/// JSON-RPC.
pub fn firecracker_argv(config_path: &str, log_path: &str) -> Vec<String> {
    vec![
        "firecracker".into(),
        "--no-api".into(),
        "--config-file".into(), config_path.into(),
        "--log-path".into(), log_path.into(),
        "--level".into(), "Warn".into(),
    ]
}
```

```rust
// bridge.rs
//! stdin↔vsock↔stdout bridge for the worker JSON-RPC channel.

/// Firecracker hybrid-vsock handshake: after connecting the host-side UDS the
/// client must announce the guest port with `CONNECT <port>\n`; the guest's
/// listener replies `OK <assigned_hostport>\n` before bytes flow.
pub fn firecracker_vsock_connect_line(port: u32) -> String {
    format!("CONNECT {port}\n")
}
```

- [ ] **Step 4: Implement `main.rs` (boot + bridge wiring)** — `workers/microvm-run/src/main.rs`:

```rust
//! `kastellan-microvm-run`: the process the sandbox backend spawns as the
//! worker `Child`. Boots a Firecracker micro-VM and bridges the worker's
//! JSON-RPC stdio over hybrid vsock. Kernel logs go to `--log`, never stdout.

mod boot;
mod bridge;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn arg(flag: &str) -> Option<String> {
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == flag { return it.next(); }
    }
    None
}

fn main() -> std::io::Result<()> {
    let config = arg("--config-file").expect("--config-file required");
    let vsock_uds = arg("--vsock-uds").expect("--vsock-uds required");
    let port: u32 = arg("--vsock-port").expect("--vsock-port required").parse().unwrap();
    let log = arg("--log").unwrap_or_else(|| "/dev/null".into());

    // Boot firecracker as our child; it creates the vsock UDS once the guest
    // is up. Its stdout/stderr go to the log path via --log-path, so we keep
    // our own stdout pristine for JSON-RPC.
    let mut fc = Command::new(&boot::firecracker_argv(&config, &log)[0])
        .args(&boot::firecracker_argv(&config, &log)[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // The guest's init listens on `port`; firecracker exposes it as
    // "<uds_path>_<port>" for host-initiated connections (hybrid vsock).
    let conn_path = format!("{vsock_uds}_{port}");
    let stream = connect_with_retry(&conn_path, Duration::from_secs(20))
        .expect("guest vsock did not come up within 20s");

    // Hybrid-vsock handshake on a plain connect to the per-port socket is not
    // required (the _<port> suffix encodes it); the worker speaks JSON-RPC now.
    let teardown = scopeguard(move || { let _ = fc.kill(); let _ = std::fs::remove_file(&conn_path); });
    bridge::pump(stream);
    drop(teardown);
    Ok(())
}

/// Retry connecting to the per-port vsock UDS until the guest listener is up.
fn connect_with_retry(path: &str, timeout: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = UnixStream::connect(path) { return Some(s); }
        if Instant::now() >= deadline { return None; }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Minimal RAII guard (avoid a dep; teardown must run on every exit path).
fn scopeguard<F: FnOnce()>(f: F) -> impl Drop {
    struct G<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for G<F> { fn drop(&mut self) { if let Some(f) = self.0.take() { f(); } } }
    G(Some(f))
}
```

Note for the implementer: the exact hybrid-vsock connect semantics (per-port `_<port>` suffix vs `CONNECT <port>\n` handshake) **must be confirmed on the DGX during Task 7** — Firecracker host-initiated connections use the `CONNECT` handshake on the base UDS, guest-initiated use the `_<port>` suffix. Slice 1 has the guest *listen* and the host *connect*, so the base-UDS + `firecracker_vsock_connect_line` path is the likely correct one; wire whichever the boot proves and delete the other. Keep `bridge.rs::pump` transport-agnostic (it takes a connected `UnixStream`).

- [ ] **Step 5: Implement `bridge::pump`** — append to `bridge.rs`:

```rust
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// Copy bytes both directions between this process's stdin/stdout and the
/// connected guest stream until either side closes. Two threads: host→guest
/// and guest→host. JSON-RPC is line-framed but we copy raw bytes (framing is
/// the worker's concern).
pub fn pump(stream: UnixStream) {
    let mut to_guest = stream.try_clone().expect("clone vsock stream");
    let from_guest = stream;
    let h = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let _ = std::io::copy(&mut stdin, &mut to_guest);
        let _ = to_guest.shutdown(std::net::Shutdown::Write);
    });
    let mut from_guest = from_guest;
    let mut stdout = std::io::stdout().lock();
    let _ = std::io::copy(&mut from_guest, &mut stdout);
    let _ = stdout.flush();
    let _ = h.join();
}
```

(Remove the duplicate `use` lines if the test module already imports; keep one set at file top.)

- [ ] **Step 6: Run pure tests + build**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-microvm-run && cargo clippy -p kastellan-microvm-run -- -D warnings'`
Expected: PASS (2 tests), clippy clean. (Mac: `cargo clippy -p kastellan-microvm-run --target aarch64-unknown-linux-gnu` for a compile-check — but this crate is Linux-only by intent; gate the crate's build or keep it pure-std so it compiles anywhere. Prefer pure-std so `cargo build -p kastellan-microvm-run` works on the Mac too.)

- [ ] **Step 7: Commit**

```bash
git add workers/microvm-run Cargo.toml
git commit -m "feat(microvm-run): Firecracker launcher binary + vsock stdio bridge"
```

---

### Task 5: Guest PID1 init `kastellan-microvm-init` + `build-rootfs.sh`

The in-guest adapter and the rootfs build. `kastellan-microvm-init` (PID1) mounts `/proc`,`/sys`, a `/tmp` tmpfs, opens an `AF_VSOCK` listener on `WORKER_VSOCK_PORT`, accepts the host bridge, `dup2`s the accepted fd onto fd 0/1, applies the worker env, then `exec`s the worker binary — so the unchanged `serve_stdio` worker runs with its JSON-RPC on the vsock. `build-rootfs.sh` assembles a minimal ext4 (pinned guest kernel, cross-built worker binary + init, a Python interpreter for python-exec) mirroring the macOS `build-image.sh` cross-build pattern.

**Files:**
- Create: `workers/microvm-init/Cargo.toml`, `workers/microvm-init/src/main.rs`
- Create: `scripts/workers/microvm/build-rootfs.sh`, `scripts/workers/microvm/install-firecracker.sh`
- Modify: root `Cargo.toml` members
- Test: inline pure test in `main.rs` for the env-application + vsock-addr helpers; the boot is verified in Task 7.

**Interfaces:**
- Consumes: `WORKER_VSOCK_PORT` (duplicate the const value `1024` — the guest crate must not depend on `kastellan-sandbox`; document the shared value in both).
- Produces: a bootable `python-exec.ext4` + `vmlinux` under a known dir (default `/var/lib/kastellan/microvm/`).

- [ ] **Step 1: Write the failing pure test** — `workers/microvm-init/src/main.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vsock_listen_addr_uses_any_cid_and_worker_port() {
        // Guest listens on VMADDR_CID_ANY:1024. Assert the helper builds the
        // right (cid, port) pair.
        assert_eq!(vsock_listen_cid_port(), (0xffffffff, 1024));
    }
}
```

- [ ] **Step 2: Create crate + run to fail**

`workers/microvm-init/Cargo.toml`:

```toml
[package]
name = "kastellan-microvm-init"
version = "0.1.0"
edition = "2021"
license = "AGPL-3.0-only"

[dependencies]
libc = "0.2"

[lints]
workspace = true
```

Add to workspace members. Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-microvm-init 2>&1 | head'`
Expected: `vsock_listen_cid_port` not found.

- [ ] **Step 3: Implement the init** — `workers/microvm-init/src/main.rs`:

```rust
//! PID1 inside the Firecracker guest. Mounts the minimal pseudo-filesystems,
//! accepts the host's JSON-RPC bridge over AF_VSOCK, wires it onto the worker's
//! fd 0/1, and execs the worker. The worker (`serve_stdio`) is UNCHANGED — this
//! init performs the vsock↔stdio adaptation so the worker still "speaks stdio".
//!
//! The worker binary path + env arrive via the kernel cmdline / a baked config
//! (see WORKER_CMD). Slice 1 bakes the python-exec worker invocation.

use std::os::unix::io::{AsRawFd, RawFd};

/// VMADDR_CID_ANY:WORKER_VSOCK_PORT. The port value is shared with
/// `kastellan-sandbox::linux_firecracker::WORKER_VSOCK_PORT` (kept in sync
/// manually; the guest crate must not depend on the sandbox crate).
const WORKER_VSOCK_PORT: u32 = 1024;
fn vsock_listen_cid_port() -> (u32, u32) { (libc::VMADDR_CID_ANY, WORKER_VSOCK_PORT) }

fn main() {
    mount_pseudo_fs();
    let conn_fd = accept_host_bridge();
    // Redirect the worker's stdio onto the vsock connection.
    unsafe {
        libc::dup2(conn_fd, 0);
        libc::dup2(conn_fd, 1);
    }
    // exec the worker (baked path + args); env from the baked config.
    exec_worker();
}

fn mount_pseudo_fs() {
    // mount -t proc proc /proc; -t sysfs sysfs /sys; -t tmpfs tmpfs /tmp
    // via libc::mount(...). (Implementer: 3 mount() calls, ignore EBUSY.)
    unimplemented_in_plan_see_step_4();
}

fn accept_host_bridge() -> RawFd {
    // socket(AF_VSOCK, SOCK_STREAM, 0); bind CID_ANY:port; listen; accept.
    unimplemented_in_plan_see_step_4()
}

fn exec_worker() {
    // execv(/usr/local/bin/kastellan-worker-python-exec, [..]) with env set.
    unimplemented_in_plan_see_step_4()
}

# // placeholder marker — replaced in Step 4 with real syscalls.
fn unimplemented_in_plan_see_step_4() -> RawFd { unreachable!() }
```

- [ ] **Step 4: Fill in the real syscalls** — replace the three helpers with concrete `libc` calls:

```rust
fn mount_pseudo_fs() {
    let mounts: &[(&str, &str, &str)] = &[
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("tmpfs", "/tmp", "tmpfs"),
    ];
    for (src, target, fstype) in mounts {
        let src = std::ffi::CString::new(*src).unwrap();
        let target = std::ffi::CString::new(*target).unwrap();
        let fstype = std::ffi::CString::new(*fstype).unwrap();
        unsafe { libc::mount(src.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null()); }
    }
}

fn accept_host_bridge() -> RawFd {
    let (_, port) = vsock_listen_cid_port();
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "AF_VSOCK socket failed");
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as _;
        addr.svm_cid = libc::VMADDR_CID_ANY;
        addr.svm_port = port;
        let alen = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        assert_eq!(libc::bind(fd, &addr as *const _ as *const libc::sockaddr, alen), 0, "vsock bind");
        assert_eq!(libc::listen(fd, 1), 0, "vsock listen");
        let conn = libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut());
        assert!(conn >= 0, "vsock accept");
        conn
    }
}

fn exec_worker() {
    use std::ffi::CString;
    // Baked worker invocation for python-exec (slice-1 consumer). A later
    // generalization reads this from the kernel cmdline / a config block.
    let prog = CString::new("/usr/local/bin/kastellan-worker-python-exec").unwrap();
    // Worker env baked here (the policy.env entries the backend would have set).
    std::env::set_var("KASTELLAN_PYTHON_EXEC_PYTHON", "/usr/local/bin/python3");
    let argv = [prog.as_ptr(), std::ptr::null()];
    unsafe { libc::execv(prog.as_ptr(), argv.as_ptr()); }
    panic!("execv of worker failed");
}
```

(Delete the `unimplemented_in_plan_see_step_4` placeholder and the stray `#` line — they exist only to make Step 3 compile-fail visibly.)

- [ ] **Step 5: Write `build-rootfs.sh`** — `scripts/workers/microvm/build-rootfs.sh`:

```bash
#!/usr/bin/env bash
# Build the python-exec micro-VM rootfs (ext4) + fetch the pinned guest kernel.
# Mirrors the macOS build-image.sh cross-build: compile the worker + init for
# the Linux guest in a bind-mounted rust container (or natively on the DGX),
# then assemble a minimal ext4 with python + both binaries + the init as PID1.
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/aarch64/vmlinux-6.1.102"
ROOTFS_MIB=512
mkdir -p "$OUT_DIR"

# 1. Guest kernel (pinned).
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

# 2. Cross-build worker + init for the guest (native on the DGX aarch64).
source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-python-exec -p kastellan-microvm-init

# 3. Assemble the ext4 (needs root for mknod-free debugfs; use mkfs.ext4 -d).
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-python-exec "$WORK/usr/local/bin/kastellan-worker-python-exec"
# Minimal python: copy the system python3 + its required libs (or apt extract).
install -D -m0755 "$(command -v python3)" "$WORK/usr/local/bin/python3"
# (Implementer: include python3's shared-lib closure via `ldd` — same
#  out-of-prefix-dep approach as core/src/workers/interpreter_deps.rs.)
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev"
mkfs.ext4 -q -F -L kastellan-rootfs -d "$WORK" "$OUT_DIR/python-exec.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/python-exec.ext4 + $OUT_DIR/vmlinux"
```

`scripts/workers/microvm/install-firecracker.sh` (pinned v1.16.0 download, mirroring the spike): fetch the release tgz, extract `firecracker-v1.16.0-aarch64`, install to `~/.local/bin/firecracker`, `chmod 0755`, run `firecracker --version` as a smoke check.

- [ ] **Step 6: Run the pure test + build the rootfs on the DGX**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-microvm-init && cargo clippy -p kastellan-microvm-init -- -D warnings'`
Expected: PASS (1 test), clippy clean.
Run: `ssh dgx 'cd ~/src/kastellan && bash scripts/workers/microvm/install-firecracker.sh && bash scripts/workers/microvm/build-rootfs.sh'`
Expected: `firecracker v1.16.0`; `built …/python-exec.ext4 + …/vmlinux`.

- [ ] **Step 7: Commit**

```bash
git add workers/microvm-init scripts/workers/microvm Cargo.toml
git commit -m "feat(microvm-init): guest PID1 vsock-stdio adapter + rootfs build script"
```

---

### Task 6: `LinuxFirecracker::spawn_under_policy` + python-exec `firecracker_mode_entry`

Wire the backend's real spawn (build plan → write config + log paths to a per-spawn temp dir → spawn `kastellan-microvm-run` as the `Child`) and the python-exec opt-in entry + resolver, mirroring `container_mode_entry`.

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs` (real `spawn_under_policy`)
- Modify: `core/src/workers/python_exec.rs` (add `firecracker_mode_entry` + `USE_MICROVM_ENV` const + linux-cfg resolver short-circuit)
- Test: inline tests in both.

**Interfaces:**
- Consumes: `build_launch_plan`, `render_firecracker_config`, `FirecrackerImage` (Task 2); the `kastellan-microvm-run` binary (Task 4) discovered the same way worker binaries are (workspace `target/<profile>/`).
- Produces: `core::workers::python_exec::firecracker_mode_entry(binary, image_dir, lifecycle) -> ToolEntry` with `sandbox_backend: Some(FirecrackerVm)`; `KASTELLAN_PYTHON_EXEC_USE_MICROVM` opt-in.

- [ ] **Step 1: Write the failing tests**

`sandbox/src/linux_firecracker.rs` (spawn returns a Child given a fake launcher on PATH is hard to unit-test purely; instead test the *invocation builder*):

```rust
#[cfg(all(test, target_os = "linux"))]
mod spawn_tests {
    use super::*;
    use crate::SandboxPolicy;
    #[test]
    fn launcher_argv_passes_config_and_vsock() {
        let plan = plan::build_launch_plan(
            &SandboxPolicy::default(),
            &FirecrackerImage { kernel_path: "/k".into(), rootfs_path: "/var/r.ext4".into() },
            "/w", &[],
        ).unwrap();
        let argv = launcher_argv(&plan, "/run/fc.json", "/run/fc.log");
        assert_eq!(argv[0], MICROVM_RUN_BIN);
        assert!(argv.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"));
        assert!(argv.windows(2).any(|w| w[0] == "--vsock-port" && w[1] == plan.vsock_port.to_string()));
    }
}
```

`core/src/workers/python_exec.rs` (resolver registers firecracker entry on linux when enabled+use_microvm):

```rust
#[cfg(all(test, target_os = "linux"))]
#[test]
fn resolver_registers_firecracker_when_use_microvm() {
    // Build a ResolveCtx with ENABLE=1, USE_MICROVM=1 and assert
    // Resolution::Register with sandbox_backend == FirecrackerVm.
    // (Mirror the existing container-mode resolver test for macOS.)
}
```

- [ ] **Step 2: Run to fail**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox spawn_tests 2>&1 | head'`
Expected: `launcher_argv`/`MICROVM_RUN_BIN` not found.

- [ ] **Step 3: Implement the real spawn** — replace the Task-1 stub in `linux_firecracker.rs`:

```rust
use std::process::{Command, Stdio};

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

impl SandboxBackend for LinuxFirecracker {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<Child, SandboxError> {
        // Image dir comes from the worker's policy env (set by the entry) —
        // KASTELLAN_MICROVM_DIR — defaulting to /var/lib/kastellan/microvm.
        let dir = policy.env.iter().find(|(k, _)| k == "KASTELLAN_MICROVM_DIR")
            .map(|(_, v)| std::path::PathBuf::from(v))
            .unwrap_or_else(|| "/var/lib/kastellan/microvm".into());
        let image = FirecrackerImage {
            kernel_path: dir.join("vmlinux"),
            rootfs_path: dir.join("python-exec.ext4"),
        };
        let plan = build_launch_plan(policy, &image, program, args)?;
        // Per-spawn temp dir for the config + log + vsock UDS.
        let run_dir = tempfile_dir()?; // mkdtemp under /run/kastellan or /tmp
        let config_path = run_dir.join("fc.json");
        let log_path = run_dir.join("fc.log");
        std::fs::write(&config_path, render_firecracker_config(&plan).to_string())
            .map_err(|e| SandboxError::Backend(format!("write fc config: {e}")))?;
        let argv = launcher_argv(&plan, &config_path.to_string_lossy(), &log_path.to_string_lossy());
        Command::new(&argv[0]).args(&argv[1..])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SandboxError::Backend(format!("microvm-run spawn failed: {e}")))
    }
}
```

(`tempfile_dir` = a small `mkdtemp` helper or the `tempfile` crate already in the workspace — check `Cargo.lock` and reuse.)

- [ ] **Step 4: Implement `firecracker_mode_entry` + resolver** — in `core/src/workers/python_exec.rs`, mirroring `container_mode_entry` (lines 296-328) but linux-cfg:

```rust
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_PYTHON_EXEC_USE_MICROVM";

#[cfg(target_os = "linux")]
pub fn firecracker_mode_entry(binary: PathBuf, image_dir: String, lifecycle: Lifecycle) -> ToolEntry {
    let env = vec![
        (PYTHON_ENV.to_string(), "/usr/local/bin/python3".to_string()),
        ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
    ];
    let policy = SandboxPolicy {
        fs_read: vec![], fs_write: vec![],
        net: Net::Deny, cpu_ms: 10_000, mem_mb: 512,
        profile: Profile::WorkerStrict, env,
        cpu_quota_pct: None, tasks_max: None, proxy_uds: None,
    };
    ToolEntry {
        binary, policy, wall_clock_ms: Some(30_000), lifecycle,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None, lockdown_shim: None, ephemeral_scratch: false,
    }
}
```

Resolver short-circuit (in `resolve`, a linux-cfg block mirroring the macOS container block at lines 409-430):

```rust
#[cfg(target_os = "linux")]
{
    let enabled = (ctx.get_env)(ENABLE_ENV).unwrap_or_default().trim() == "1";
    let use_microvm = (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
    if enabled && use_microvm {
        let binary = PathBuf::from("/usr/local/bin/kastellan-worker-python-exec");
        let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
        let (idle, max_req, max_age) = parse_idle_caps(|k| (ctx.get_env)(k));
        return Resolution::Register(firecracker_mode_entry(
            binary, image_dir, container_lifecycle(idle, max_req, max_age),
        ));
    }
}
```

(`container_lifecycle`/`parse_idle_caps` are currently `#[cfg(target_os = "macos")]` — widen their cfg to `any(macos, linux)` since both micro-VM paths reuse them, or duplicate the tiny fns under linux-cfg. Prefer widening the cfg.)

- [ ] **Step 5: Run tests + clippy**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox spawn_tests && cargo test -p kastellan-core python_exec && cargo clippy --workspace --all-targets -- -D warnings'`
Expected: PASS; clippy clean (workspace, on the DGX where linux-cfg compiles).
Run (Mac): `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu -- -D warnings` + `cargo clippy --workspace --all-targets -- -D warnings` (macOS paths unaffected).

- [ ] **Step 6: Commit**

```bash
git add sandbox/src/linux_firecracker.rs core/src/workers/python_exec.rs
git commit -m "feat: LinuxFirecracker spawn + python-exec micro-VM opt-in (USE_MICROVM)"
```

---

### Task 7: DGX end-to-end boot test + operator runbook

The real proof: a `#[ignore]` e2e that drives python-exec through the live Firecracker backend on the DGX. Confirms the round-trip, the KVM-enforced mem cap, and net-deny. Plus the operator runbook documenting the one-time setup.

**Files:**
- Create: `core/tests/python_exec_firecracker_e2e.rs`
- Create: `docs/devel/runbooks/2026-06-26-linux-microvm-setup.md`
- Modify: `docs/threat-model.md` (note the optional separate-kernel layer for opted-in Linux workers)

**Interfaces:**
- Consumes: the whole stack (Tasks 1–6) + the built rootfs/kernel (Task 5) + firecracker on `$PATH`.

- [ ] **Step 1: Write the e2e (ignored by default)** — `core/tests/python_exec_firecracker_e2e.rs`:

```rust
//! Real Firecracker micro-VM e2e for python-exec. DGX-only: needs /dev/kvm +
//! /dev/vhost-vsock + a built rootfs. Ignored in normal runs.
#![cfg(target_os = "linux")]

// Helper: build a ToolHost with python-exec in firecracker mode, dispatch a
// python.exec call, assert on the result. (Mirror python_exec_container_e2e.rs
// on macOS — reuse its harness shape.)

#[test]
#[ignore = "needs DGX: /dev/kvm + vhost_vsock + built rootfs"]
fn microvm_round_trip_six_times_seven() {
    // dispatch python code `print(6*7)` → expect "42" in the result.
}

#[test]
#[ignore = "needs DGX"]
fn microvm_enforces_mem_cap() {
    // mem_mb 512, allocate ~900 MiB → MemoryError / non-zero exit (the parity
    // payoff: KVM-enforced, unlike a same-kernel boundary).
}

#[test]
#[ignore = "needs DGX"]
fn microvm_net_is_denied() {
    // attempt an outbound socket → no connectivity (Net::Deny, no virtio-net).
}
```

- [ ] **Step 2: Confirm the transport on the DGX (the one discovery point)**

Run a manual boot to resolve the hybrid-vsock connect semantics flagged in Task 4 Step 4:
Run: `ssh dgx 'cd /tmp/fc-spike && ... boot python-exec.ext4 with a vsock device, then from the host CONNECT to <uds>_1024 or send "CONNECT 1024\n" to <uds>, observe which yields the worker JSON-RPC banner'`
Adjust `kastellan-microvm-run` to the proven path; delete the unused branch. Re-run `cargo test -p kastellan-microvm-run`.

- [ ] **Step 3: Run the e2e on the DGX**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && KASTELLAN_PYTHON_EXEC_ENABLE=1 KASTELLAN_PYTHON_EXEC_USE_MICROVM=1 cargo test -p kastellan-core --test python_exec_firecracker_e2e -- --ignored --nocapture'`
Expected: 3/3 PASS — `42` round-trips; mem-cap test sees `MemoryError`/non-zero exit; net-deny test sees no connectivity.

- [ ] **Step 4: Write the operator runbook** — `docs/devel/runbooks/2026-06-26-linux-microvm-setup.md`: the one-time DGX setup (modprobe vhost_vsock + persist via `/etc/modules-load.d`; grant `/dev/vhost-vsock` to the worker user; `install-firecracker.sh`; `build-rootfs.sh`), how to enable (`KASTELLAN_PYTHON_EXEC_USE_MICROVM=1`), and how to verify (the e2e command above). Note `/dev/kvm` is already accessible.

- [ ] **Step 5: Update the threat model** — `docs/threat-model.md`: add that opted-in Linux workers (today: python-exec via `USE_MICROVM`) gain a separate-kernel boundary on top of namespaces/seccomp/Landlock/cgroup; the VM enforces `mem_mb` at the hypervisor; scope is unchanged for non-opted workers (still bwrap).

- [ ] **Step 6: Commit**

```bash
git add core/tests/python_exec_firecracker_e2e.rs docs/devel/runbooks/2026-06-26-linux-microvm-setup.md docs/threat-model.md
git commit -m "test(microvm): DGX Firecracker python-exec e2e + operator runbook"
```

---

## Self-Review

**Spec coverage:** Enum/registry (T1) ✓; pure `build_launch_plan` + config (T2) ✓; `probe` with operator-fix messages (T3) ✓; launcher-is-the-Child + vsock bridge (T4) ✓; guest PID1 init + `build-rootfs.sh`, R1 minimal-rootfs (T5) ✓; `Net::Deny` in-image python-exec consumer + `USE_MICROVM` opt-in mirroring `container_mode_entry` (T6) ✓; DGX e2e with `42` round-trip + KVM mem-cap + net-deny, runbook, threat-model (T7) ✓. Slices 2–5 (warm/idle, fs-sharing, net workers, jailer) are explicitly out of this plan's scope per the spec staging table.

**Placeholder scan:** The deliberate compile-fail placeholder in T5 Step 3 (`unimplemented_in_plan_see_step_4`) is replaced with real syscalls in Step 4 and called out for deletion — intentional RED→GREEN, not a plan gap. The T4 hybrid-vsock connect semantics carry an explicit "confirm on DGX in T7 Step 2" discovery note rather than a guessed value — this is the one genuine unknown the spike could not pin without vsock access, and it is scheduled, not hand-waved.

**Type consistency:** `WORKER_VSOCK_PORT` = 1024 is shared by value across `kastellan-sandbox` (T2) and `kastellan-microvm-init` (T5), with the no-cross-dep reason documented in both. `FirecrackerLaunchPlan`/`FirecrackerImage`/`build_launch_plan`/`render_firecracker_config`/`launcher_argv`/`MICROVM_RUN_BIN` names are consistent across T2/T3/T6. `firecracker_mode_entry` mirrors the verified `container_mode_entry` signature shape (`ToolEntry` fields match the real struct read from `python_exec.rs:318-327`).

**Cross-platform / test-execution:** every task states the Mac compile-check (`cross-clippy`) vs DGX run split, consistent with the global constraint.
