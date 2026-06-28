# Firecracker micro-VM slice 3 — host-dir sharing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the generic `FirecrackerVm` backend per-spawn host-dir sharing — a read-only ext4 drive exposing `policy.fs_read` at original absolute paths, plus a disk-backed writable scratch drive for `policy.fs_write` — both ephemeral, no host write-back.

**Architecture:** Mirrors slice 1's three-layer split. The pure `plan.rs` derives mount intent from the policy and emits a `kastellan.mounts=<hex>` cmdline token; the backend `spawn_under_policy` builds the ext4 images into the run dir with `mkfs.ext4`; the guest PID1 `kastellan-microvm-init` decodes the manifest and mounts the drives (tmpfs anchors make bind-mount targets creatable on the read-only root). Verified by a synthetic DGX e2e driving the backend directly over the existing python-exec rootfs.

**Tech Stack:** Rust (std-only in `microvm-init`/`microvm-run`; `serde_json` in `sandbox`), Firecracker, ext4 via `mkfs.ext4` (e2fsprogs), bash (`build-rootfs.sh`).

**Spec:** [`docs/superpowers/specs/2026-06-27-firecracker-microvm-slice3-host-dir-sharing-design.md`](../specs/2026-06-27-firecracker-microvm-slice3-host-dir-sharing-design.md)

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new deps in this slice (std + existing `serde_json`).
- **Cross-platform: all new backend/guest code is `#[cfg(target_os = "linux")]`-gated.** Pure helpers (encode/decode/derive) compile and unit-test on macOS; the real mount/spawn paths are Linux-only. `cargo build --workspace` must stay green on macOS.
- **`kastellan-microvm-init` and `kastellan-microvm-run` must NOT depend on `kastellan-sandbox`.** Shared constants (`kastellan.mounts` key, hex codec) are manually kept in sync, with a roundtrip fixture pinned identically in both crates — same discipline as `kastellan.env` (#360).
- **Files under 500 LOC where feasible.** `plan.rs` is ~407 LOC today; keep additions tight, split if it crosses ~500.
- **TDD: every task is RED → GREEN → commit.** Pure logic is unit-tested; the spawn/mount path is covered by the `#[ignore]` DGX e2e.
- **`SandboxPolicy.fs_read`/`fs_write` paths must be absolute** (already enforced; this slice adds: `fs_read` top-level component must not be a reserved rootfs system dir).
- **Drive ceiling: at most 2 extra drives** (one RO share regardless of `fs_read` count + one RW scratch).
- **Device-node assignment is centralized in `build_launch_plan`** (RO before RW, starting at `/dev/vdb`) and the config drive order in `render_firecracker_config` MUST match — pinned by a test. The guest init never guesses device letters; it uses the nodes the manifest gives it.

---

### Task 1: Pure mount-intent derivation + system-dir rejection (`plan.rs`)

Derive `RoShare`/`RwScratch` from the policy, assign guest device nodes, and fail closed when an `fs_read` top-level component is a reserved system dir or when `fs_write` has more than one entry (slice-3 supports a single writable mountpoint).

**Files:**
- Create: `sandbox/src/linux_firecracker/mounts.rs` — slice-3 share types + `reserved_top_level` + their unit tests (keeps `plan.rs` under the 500-LOC guideline; Task 2 adds the encoder here too).
- Modify: `sandbox/src/linux_firecracker.rs` — register `mod mounts;` + re-export the new public items.
- Modify: `sandbox/src/linux_firecracker/plan.rs` — add the four plan fields + the derivation in `build_launch_plan` (calling `super::mounts::*`).
- Test: `mounts.rs` (`reserved_top_level` unit) + `plan.rs` `mod tests` (the `build_launch_plan` derivation integration tests below).

**Interfaces:**
- Produces (in `mounts.rs`, re-exported from `linux_firecracker.rs`):
  - `pub struct RoShare { pub sources: Vec<PathBuf>, pub guest_dev: String }`
  - `pub struct RwScratch { pub mountpoint: PathBuf, pub guest_dev: String }`
  - `pub fn reserved_top_level(path: &std::path::Path) -> Option<&str>` (returns the offending component name if the first path component is reserved)
- Produces (in `plan.rs`): new `FirecrackerLaunchPlan` fields `pub ro_share: Option<RoShare>`, `pub rw_scratch: Option<RwScratch>`, `pub ro_image_path: Option<PathBuf>`, `pub rw_image_path: Option<PathBuf>` (the `*_image_path` are placeholders the spawn overrides, like `vsock_uds`).
- Consumes: existing `build_launch_plan(policy, image, program, args)` signature (unchanged).

- [ ] **Step 1: Write the failing tests**

Add to `plan.rs` `mod tests`:

```rust
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
    assert!(format!("{err}").contains("system dir") || format!("{err}").contains("/usr"));
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
```

And in the new `mounts.rs` `#[cfg(test)] mod tests`, a direct unit for the pure helper:

```rust
#[test]
fn reserved_top_level_flags_system_dirs_only() {
    use std::path::Path;
    assert_eq!(reserved_top_level(Path::new("/usr/lib/foo")), Some("usr"));
    assert_eq!(reserved_top_level(Path::new("/etc/passwd")), Some("etc"));
    assert_eq!(reserved_top_level(Path::new("/opt/venv")), None);
    assert_eq!(reserved_top_level(Path::new("/data/x")), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kastellan-sandbox --target aarch64-unknown-linux-gnu fs_read_derives 2>&1 | tail -20` (on the Mac use `cargo build -p kastellan-sandbox --target aarch64-unknown-linux-gnu` for compile-check; the pure tests run on the DGX). On the DGX: `cargo test -p kastellan-sandbox plan:: -- --nocapture`.
Expected: FAIL — `RoShare`/fields/`reserved_top_level` not defined.

- [ ] **Step 3: Implement the module, structs, plan fields, and derivation**

Create `sandbox/src/linux_firecracker/mounts.rs` (the share types + the pure helper; Task 2 will add the encoder + `MOUNTS_CMDLINE_KEY` to this same file):

```rust
//! Slice-3 host-dir-share value types + the `kastellan.mounts` manifest encoder.
//! Split out of `plan.rs` to keep it under the 500-LOC guideline. Pure — no KVM,
//! no spawn; unit-tested without root.

use std::path::PathBuf;

/// A read-only host-dir share: the absolute `fs_read` roots exposed inside the
/// guest at their original paths, plus the guest device node the RO ext4 will
/// appear as. The image is built per-spawn into the run dir (see the backend).
#[derive(Clone, Debug, PartialEq)]
pub struct RoShare {
    pub sources: Vec<PathBuf>,
    pub guest_dev: String,
}

/// A writable, disk-backed scratch drive mounted in-guest at `mountpoint`.
/// Ephemeral — discarded with the run dir on teardown (no host write-back).
#[derive(Clone, Debug, PartialEq)]
pub struct RwScratch {
    pub mountpoint: PathBuf,
    pub guest_dev: String,
}

/// Reserved rootfs top-level dirs an `fs_read` path may not live under: mounting
/// a tmpfs anchor over one of these would shadow the worker's own files. Returns
/// the offending first component if reserved, else `None`.
pub fn reserved_top_level(path: &std::path::Path) -> Option<&str> {
    const RESERVED: &[&str] =
        &["usr", "bin", "lib", "lib64", "etc", "sbin", "proc", "sys", "dev", "boot", "root"];
    let first = path
        .components()
        .find_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })?;
    RESERVED.iter().copied().find(|&r| r == first)
}

#[cfg(test)]
mod tests {
    use super::*;
    // (the reserved_top_level unit from Step 1 goes here)
}
```

Register the module in `linux_firecracker.rs` (after the `mod plan; pub use plan::{…};` block):

```rust
mod mounts;
pub use mounts::{reserved_top_level, RoShare, RwScratch};
```

In `plan.rs`, import the types (`use super::mounts::{reserved_top_level, RoShare, RwScratch};`) and add the four fields to `FirecrackerLaunchPlan` (after `net_enabled`):

```rust
    /// Read-only host-dir share, derived from `policy.fs_read`. `None` if empty.
    pub ro_share: Option<RoShare>,
    /// Writable scratch drive, derived from `policy.fs_write`. `None` if empty.
    pub rw_scratch: Option<RwScratch>,
    /// Host path of the built RO ext4. Placeholder until the spawn sets the
    /// run-dir path (mirrors `vsock_uds`); `Some` iff `ro_share` is `Some`.
    pub ro_image_path: Option<std::path::PathBuf>,
    /// Host path of the built RW ext4. `Some` iff `rw_scratch` is `Some`.
    pub rw_image_path: Option<std::path::PathBuf>,
```

In `build_launch_plan`, after the existing absolute-path check and before the `Ok(...)`, add the derivation (and thread the four fields into the returned struct):

```rust
    // Slice 3: derive host-dir-sharing drives from the policy. Device nodes are
    // assigned RO-before-RW starting at /dev/vdb (vda is the rootfs); the config
    // drive order in render_firecracker_config MUST match (pinned by a test).
    for p in &policy.fs_read {
        if let Some(sys) = reserved_top_level(p) {
            return Err(SandboxError::Backend(format!(
                "fs_read path {p:?} is under reserved rootfs system dir /{sys}: the micro-VM \
                 backend cannot anchor a tmpfs there without hiding the worker's own files"
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
    let mut next_letter = b'b';
    let ro_share = if policy.fs_read.is_empty() {
        None
    } else {
        let dev = format!("/dev/vd{}", next_letter as char);
        next_letter += 1;
        Some(RoShare { sources: policy.fs_read.clone(), guest_dev: dev })
    };
    let rw_scratch = policy.fs_write.first().map(|mp| RwScratch {
        mountpoint: mp.clone(),
        guest_dev: format!("/dev/vd{}", next_letter as char),
    });
    // Placeholder image paths next to the rootfs (overridden per-spawn, like
    // vsock_uds). Present iff the corresponding share is present.
    let image_dir = image.rootfs_path.parent().unwrap_or_else(|| std::path::Path::new("/tmp"));
    let ro_image_path = ro_share.as_ref().map(|_| image_dir.join("ro-share.ext4"));
    let rw_image_path = rw_scratch.as_ref().map(|_| image_dir.join("rw-scratch.ext4"));
```

Add the four fields to the `Ok(FirecrackerLaunchPlan { … })` literal.

- [ ] **Step 4: Run tests to verify they pass**

Run (DGX): `cargo test -p kastellan-sandbox plan:: -- --nocapture`
Expected: PASS (all Task-1 tests + the existing plan tests).
Mac compile-check: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/mounts.rs sandbox/src/linux_firecracker.rs sandbox/src/linux_firecracker/plan.rs
git commit -m "feat(microvm): derive RO/RW host-dir-share drives from policy (slice 3)"
```

---

### Task 2: Mount-manifest encode + `kastellan.mounts` cmdline token (`plan.rs`)

Encode the derived shares into a hex `kastellan.mounts=<hex>` token appended to `boot_args`, sharing the existing cmdline cap. Tab/newline field separators, fail closed on either in any path.

**Files:**
- Modify: `sandbox/src/linux_firecracker/mounts.rs` — add `MOUNTS_CMDLINE_KEY` + `encode_mount_manifest` + their unit tests.
- Modify: `sandbox/src/linux_firecracker/plan.rs` — make `hex_encode` `pub(super)` so `mounts.rs` can share it; append the mounts token in `build_launch_plan`.
- Modify: `sandbox/src/linux_firecracker.rs` — re-export `encode_mount_manifest`.
- Test: `mounts.rs` (encoder units, incl. the cross-crate hex fixture) + `plan.rs` `mod tests` (the `build_launch_plan` token-append integration tests).

**Interfaces:**
- Produces (in `mounts.rs`): `pub fn encode_mount_manifest(ro: Option<&RoShare>, rw: Option<&RwScratch>) -> Result<Option<String>, SandboxError>` (returns the ` kastellan.mounts=<hex>` suffix, or `None` when both absent). Manifest block format: one line per drive, tab-separated fields:
  - RO: `ro\t<guest_dev>\t<path1>\t<path2>…`
  - RW: `rw\t<guest_dev>\t<mountpoint>`
  Lines joined by `\n`.
- Consumes: `RoShare`/`RwScratch` (Task 1, same module), `plan::hex_encode` (now `pub(super)`), `plan::MAX_CMDLINE_BYTES` (unchanged — the cap check stays in `build_launch_plan`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn encode_mount_manifest_none_when_empty() {
    assert_eq!(encode_mount_manifest(None, None).unwrap(), None);
}

#[test]
fn encode_mount_manifest_ro_only_fixture() {
    // Cross-crate sync guard: kastellan-microvm-init decodes this exact hex.
    // Block "ro\t/dev/vdb\t/opt/a" =
    //   72 6f 09 2f 64 65 76 2f 76 64 62 09 2f 6f 70 74 2f 61
    let ro = RoShare { sources: vec![PathBuf::from("/opt/a")], guest_dev: "/dev/vdb".into() };
    assert_eq!(
        encode_mount_manifest(Some(&ro), None).unwrap().unwrap(),
        " kastellan.mounts=726f092f6465762f766462092f6f70742f61"
    );
}

#[test]
fn encode_mount_manifest_ro_and_rw() {
    let ro = RoShare { sources: vec![PathBuf::from("/opt/a")], guest_dev: "/dev/vdb".into() };
    let rw = RwScratch { mountpoint: PathBuf::from("/tmp/s"), guest_dev: "/dev/vdc".into() };
    let suffix = encode_mount_manifest(Some(&ro), Some(&rw)).unwrap().unwrap();
    assert!(suffix.starts_with(" kastellan.mounts="));
    // Single whitespace-free token.
    assert_eq!(suffix.trim_start().split_whitespace().count(), 1);
}

#[test]
fn encode_mount_manifest_rejects_tab_and_newline_in_paths() {
    let ro = RoShare { sources: vec![PathBuf::from("/opt/a\tb")], guest_dev: "/dev/vdb".into() };
    assert!(encode_mount_manifest(Some(&ro), None).is_err());
    let ro2 = RoShare { sources: vec![PathBuf::from("/opt/a\nb")], guest_dev: "/dev/vdb".into() };
    assert!(encode_mount_manifest(Some(&ro2), None).is_err());
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run (DGX): `cargo test -p kastellan-sandbox encode_mount_manifest -- --nocapture`
Expected: FAIL — `encode_mount_manifest` not defined.

- [ ] **Step 3: Implement the encoder and wire it into `build_launch_plan`**

First make `hex_encode` shareable: in `plan.rs` change `fn hex_encode(` to `pub(super) fn hex_encode(`.

Re-export the encoder in `linux_firecracker.rs`: extend the `pub use mounts::{…}` line to include `encode_mount_manifest`.

Add to `mounts.rs` (with `use super::plan::hex_encode;` and `use crate::SandboxError;` at the top):

```rust
/// Cmdline token key carrying the hex-encoded mount manifest (slice 3). The guest
/// `kastellan-microvm-init` reads it from `/proc/cmdline`. Manually kept in sync
/// across the crate boundary (same constraint as `plan::ENV_CMDLINE_KEY`).
const MOUNTS_CMDLINE_KEY: &str = "kastellan.mounts";

/// Encode the derived host-dir shares as the ` kastellan.mounts=<hex>` cmdline
/// suffix. Block = one tab-separated line per drive (`ro\t<dev>\t<p1>\t<p2>…` /
/// `rw\t<dev>\t<mountpoint>`), lines joined by `\n`, hex-encoded. Returns
/// `Ok(None)` when both shares are absent so the cmdline stays byte-identical to
/// the pre-slice-3 baseline.
///
/// Fail closed if any path contains a `\t` (field separator) or `\n` (line
/// separator): such a path would silently shift the guest decoder's boundaries.
/// Absolute filesystem paths never legitimately contain these.
pub fn encode_mount_manifest(
    ro: Option<&RoShare>,
    rw: Option<&RwScratch>,
) -> Result<Option<String>, SandboxError> {
    if ro.is_none() && rw.is_none() {
        return Ok(None);
    }
    let mut lines: Vec<String> = Vec::new();
    let guard = |s: &str| -> Result<(), SandboxError> {
        if s.contains('\t') || s.contains('\n') {
            return Err(SandboxError::Backend(format!(
                "mount path {s:?} cannot be forwarded: it contains a tab or newline (the \
                 manifest's field/line separators)"
            )));
        }
        Ok(())
    };
    if let Some(ro) = ro {
        let mut fields = vec!["ro".to_string(), ro.guest_dev.clone()];
        for p in &ro.sources {
            let s = p.to_string_lossy();
            guard(&s)?;
            fields.push(s.into_owned());
        }
        lines.push(fields.join("\t"));
    }
    if let Some(rw) = rw {
        let mp = rw.mountpoint.to_string_lossy();
        guard(&mp)?;
        lines.push(format!("rw\t{}\t{}", rw.guest_dev, mp));
    }
    let block = lines.join("\n");
    Ok(Some(format!(" {MOUNTS_CMDLINE_KEY}={}", hex_encode(block.as_bytes()))))
}
```

Extend the `plan.rs` import added in Task 1 to bring in the encoder:
`use super::mounts::{encode_mount_manifest, reserved_top_level, RoShare, RwScratch};`

In `build_launch_plan`, after the env-token append and BEFORE the `MAX_CMDLINE_BYTES` cap check, append the mounts token (so the cap covers env + mounts):

```rust
    if let Some(suffix) = encode_mount_manifest(ro_share.as_ref(), rw_scratch.as_ref())? {
        boot_args.push_str(&suffix);
    }
```

(The existing `if boot_args.len() > MAX_CMDLINE_BYTES` check now guards the combined cmdline — no change needed there.)

- [ ] **Step 4: Run tests to verify they pass**

Run (DGX): `cargo test -p kastellan-sandbox -- --nocapture 2>&1 | tail -30`
Expected: PASS — Task 1 + Task 2 + existing plan tests.
Verify the fixture hex by eye against the comment, or temporarily `eprintln!` the encoder output.
Mac: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/mounts.rs sandbox/src/linux_firecracker.rs sandbox/src/linux_firecracker/plan.rs
git commit -m "feat(microvm): encode kastellan.mounts cmdline manifest (slice 3)"
```

---

### Task 3: Attach extra drives in `render_firecracker_config` (`plan.rs`)

Append the RO/RW drives to the config in the fixed order rootfs → RO → RW (matching the device-node assignment), with correct `is_read_only`. Byte-identical when both absent.

**Files:**
- Modify: `sandbox/src/linux_firecracker/plan.rs` (`render_firecracker_config`)
- Test: same file

**Interfaces:**
- Consumes: `plan.ro_image_path`/`rw_image_path` (Task 1), `plan.ro_share`/`rw_scratch` for the `drive_id`.
- Produces: config `drives` array entries `ro-share` (`is_read_only:true`) and `rw-scratch` (`is_read_only:false`).

- [ ] **Step 1: Write the failing tests**

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run (DGX): `cargo test -p kastellan-sandbox config_attaches -- --nocapture`
Expected: FAIL — only the rootfs drive is present.

- [ ] **Step 3: Implement the drive append**

In `render_firecracker_config`, after the `let mut cfg = json!({ … })` block and before the `if plan.net_enabled` block, add:

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run (DGX): `cargo test -p kastellan-sandbox plan:: -- --nocapture`
Expected: PASS, including the existing `rootfs_is_read_only` / `config_pins_kernel_and_rootfs_paths` regressions.
Mac: cross-clippy clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/plan.rs
git commit -m "feat(microvm): attach RO/RW host-dir drives in firecracker config (slice 3)"
```

---

### Task 4: Build ext4 images into the run dir (`linux_firecracker.rs`)

In `spawn_under_policy`, when the plan carries shares, stage the `fs_read` trees and `mkfs.ext4 -d` a RO image + `mkfs.ext4` a blank RW image, both into the run dir; set the resolved paths on the plan before rendering the config. The launcher's #362 RAII teardown reclaims them.

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs`
- Create: `sandbox/src/linux_firecracker/images.rs` (pure path/argv helpers + the I/O builder, keeps `linux_firecracker.rs` focused)
- Test: `images.rs` `#[cfg(test)] mod tests` (pure helpers only)

**Interfaces:**
- Produces (in `images.rs`):
  - `pub const RW_SCRATCH_MIB_DEFAULT: u64 = 64;`
  - `pub fn rw_scratch_mib(env: &[(String, String)]) -> u64` (reads `KASTELLAN_MICROVM_SCRATCH_MIB`, fail-safe to default)
  - `pub fn staged_path(stage_root: &Path, source: &Path) -> PathBuf` (mirror the absolute source under `stage_root`, e.g. `stage_root + "/opt/venv"`)
  - `pub fn mkfs_populate_argv(stage_dir: &str, out_img: &str, size_mib: u64) -> Vec<String>` (`mkfs.ext4 -q -F -O ^has_journal -d <stage> <out> <N>M`)
  - `pub fn mkfs_blank_argv(out_img: &str, size_mib: u64) -> Vec<String>`
  - `pub fn build_share_images(plan: &mut FirecrackerLaunchPlan, run_dir: &Path, env: &[(String,String)]) -> Result<(), SandboxError>` (does the staging + mkfs; sets `plan.ro_image_path`/`rw_image_path` to run-dir paths; Linux-gated I/O)
- Consumes: `FirecrackerLaunchPlan` (Tasks 1-3).

- [ ] **Step 1: Write the failing tests (pure helpers)**

Create `sandbox/src/linux_firecracker/images.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn staged_path_mirrors_absolute_source() {
        assert_eq!(
            staged_path(Path::new("/run/x/ro-stage"), Path::new("/opt/venv")),
            PathBuf::from("/run/x/ro-stage/opt/venv")
        );
    }

    #[test]
    fn rw_scratch_mib_defaults_and_overrides() {
        assert_eq!(rw_scratch_mib(&[]), RW_SCRATCH_MIB_DEFAULT);
        let env = vec![("KASTELLAN_MICROVM_SCRATCH_MIB".to_string(), "256".to_string())];
        assert_eq!(rw_scratch_mib(&env), 256);
        // Garbage → fail-safe to default.
        let bad = vec![("KASTELLAN_MICROVM_SCRATCH_MIB".to_string(), "abc".to_string())];
        assert_eq!(rw_scratch_mib(&bad), RW_SCRATCH_MIB_DEFAULT);
    }

    #[test]
    fn mkfs_argv_shapes() {
        let pop = mkfs_populate_argv("/run/x/ro-stage", "/run/x/ro-share.ext4", 32);
        assert_eq!(pop[0], "mkfs.ext4");
        assert!(pop.windows(2).any(|w| w[0] == "-d" && w[1] == "/run/x/ro-stage"));
        assert!(pop.iter().any(|a| a == "^has_journal"));
        assert!(pop.iter().any(|a| a == "/run/x/ro-share.ext4"));
        assert!(pop.iter().any(|a| a == "32M"));
        let blank = mkfs_blank_argv("/run/x/rw-scratch.ext4", 64);
        assert_eq!(blank[0], "mkfs.ext4");
        assert!(blank.iter().any(|a| a == "/run/x/rw-scratch.ext4"));
        assert!(blank.iter().any(|a| a == "64M"));
        assert!(!blank.iter().any(|a| a == "-d"), "blank image has no -d source");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run (DGX): `cargo test -p kastellan-sandbox images:: -- --nocapture`
Expected: FAIL — module/helpers not defined.

- [ ] **Step 3: Implement `images.rs` and wire it into `spawn_under_policy`**

Create `images.rs` (above the test mod):

```rust
//! Per-spawn ext4 image building for slice-3 host-dir sharing. Pure argv/path
//! helpers (unit-tested without root) + the I/O builder that stages fs_read
//! trees and runs `mkfs.ext4`. The images land in the spawn's run dir so the
//! launcher's RAII teardown (#362) reclaims them.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::plan::FirecrackerLaunchPlan;
use crate::SandboxError;

/// Default writable-scratch size. Disk-backed, so it does NOT consume the guest
/// `mem_size_mib` cap the way the existing tmpfs `/tmp` does.
pub const RW_SCRATCH_MIB_DEFAULT: u64 = 64;

/// Scratch size in MiB: `KASTELLAN_MICROVM_SCRATCH_MIB` if set+parseable, else
/// the default (fail-safe — a garbled value never aborts the boot).
pub fn rw_scratch_mib(env: &[(String, String)]) -> u64 {
    env.iter()
        .find(|(k, _)| k == "KASTELLAN_MICROVM_SCRATCH_MIB")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(RW_SCRATCH_MIB_DEFAULT)
}

/// Mirror an absolute `source` under `stage_root` so `mkfs.ext4 -d` reproduces
/// the absolute layout inside the image (e.g. `/opt/venv` → `<stage>/opt/venv`).
pub fn staged_path(stage_root: &Path, source: &Path) -> PathBuf {
    let rel = source.strip_prefix("/").unwrap_or(source);
    stage_root.join(rel)
}

/// `mkfs.ext4` argv that populates an image from a staged dir tree, journal-less
/// (a read-only ext4 that ever carried a journal needs recovery on RO mount —
/// the same reason the rootfs is built `-O ^has_journal`).
pub fn mkfs_populate_argv(stage_dir: &str, out_img: &str, size_mib: u64) -> Vec<String> {
    vec![
        "mkfs.ext4".into(), "-q".into(), "-F".into(),
        "-O".into(), "^has_journal".into(),
        "-d".into(), stage_dir.into(),
        out_img.into(), format!("{size_mib}M"),
    ]
}

/// `mkfs.ext4` argv for a blank writable image (no `-d`). Journalled is fine —
/// it is mounted read-write.
pub fn mkfs_blank_argv(out_img: &str, size_mib: u64) -> Vec<String> {
    vec![
        "mkfs.ext4".into(), "-q".into(), "-F".into(),
        out_img.into(), format!("{size_mib}M"),
    ]
}

/// Size the RO image to fit the staged tree with headroom (bytes → MiB, +16 MiB
/// slack, min 8 MiB). Keeps `mkfs.ext4` from rejecting a too-small size.
fn ro_image_mib(stage_root: &Path) -> u64 {
    fn dir_bytes(p: &Path) -> u64 {
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let md = match e.metadata() { Ok(m) => m, Err(_) => continue };
                total += if md.is_dir() { dir_bytes(&e.path()) } else { md.len() };
            }
        }
        total
    }
    (dir_bytes(stage_root) / (1024 * 1024) + 16).max(8)
}

/// Build the per-spawn share images into `run_dir`; set the plan's image paths.
/// Linux-only (runs `mkfs.ext4` + copies trees). No-op when no shares.
pub fn build_share_images(
    plan: &mut FirecrackerLaunchPlan,
    run_dir: &Path,
    env: &[(String, String)],
) -> Result<(), SandboxError> {
    let run = |argv: Vec<String>| -> Result<(), SandboxError> {
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .map_err(|e| SandboxError::Backend(format!("spawn {}: {e}", argv[0])))?;
        if !status.success() {
            return Err(SandboxError::Backend(format!("{} failed: {status}", argv[0])));
        }
        Ok(())
    };

    if let Some(ro) = plan.ro_share.clone() {
        let stage_root = run_dir.join("ro-stage");
        for src in &ro.sources {
            let dest = staged_path(&stage_root, src);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| SandboxError::Backend(format!("stage mkdir {parent:?}: {e}")))?;
            }
            copy_tree(src, &dest)?;
        }
        let out = run_dir.join("ro-share.ext4");
        let mib = ro_image_mib(&stage_root);
        run(mkfs_populate_argv(
            &stage_root.to_string_lossy(),
            &out.to_string_lossy(),
            mib,
        ))?;
        plan.ro_image_path = Some(out);
    }

    if plan.rw_scratch.is_some() {
        let out = run_dir.join("rw-scratch.ext4");
        run(mkfs_blank_argv(&out.to_string_lossy(), rw_scratch_mib(env)))?;
        plan.rw_image_path = Some(out);
    }
    Ok(())
}

/// Recursively copy a host tree (dirs, files, symlinks-as-targets) into `dest`.
/// Plain `std` (no `fs_extra` dep).
fn copy_tree(src: &Path, dest: &Path) -> Result<(), SandboxError> {
    let md = std::fs::symlink_metadata(src)
        .map_err(|e| SandboxError::Backend(format!("stat {src:?}: {e}")))?;
    if md.is_dir() {
        std::fs::create_dir_all(dest)
            .map_err(|e| SandboxError::Backend(format!("mkdir {dest:?}: {e}")))?;
        for e in std::fs::read_dir(src)
            .map_err(|e| SandboxError::Backend(format!("read_dir {src:?}: {e}")))?
            .flatten()
        {
            copy_tree(&e.path(), &dest.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(src, dest)
            .map_err(|e| SandboxError::Backend(format!("copy {src:?}->{dest:?}: {e}")))?;
    }
    Ok(())
}
```

Register the module + wire the call. In `linux_firecracker.rs`, add after the `mod plan; …` block:

```rust
mod images;
pub use images::{build_share_images, RW_SCRATCH_MIB_DEFAULT};
```

In `spawn_under_policy`, after `plan.vsock_cid = next_guest_cid();` and BEFORE `let config_path = run_dir.join("fc.json");`, insert:

```rust
        // Slice 3: build per-spawn host-dir-share images into the run dir (the
        // launcher's RAII teardown removes them with the dir). Sets the plan's
        // ro/rw image paths so the rendered config attaches the drives.
        build_share_images(&mut plan, &run_dir, &policy.env)?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run (DGX): `cargo test -p kastellan-sandbox images:: -- --nocapture` → PASS.
Run (DGX): `cargo test -p kastellan-sandbox -- --nocapture 2>&1 | tail -20` → all sandbox unit tests PASS.
Mac: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker.rs sandbox/src/linux_firecracker/images.rs
git commit -m "feat(microvm): build per-spawn RO/RW ext4 share images in spawn (slice 3)"
```

---

### Task 5: Probe `mkfs.ext4` presence (`probe.rs`)

Fail closed when `mkfs.ext4` is missing, naming the operator fix (install e2fsprogs).

**Files:**
- Modify: `sandbox/src/linux_firecracker/probe.rs`
- Test: same file

**Interfaces:**
- Produces: new `ProbeInputs` field `pub mkfs_ext4_on_path: bool`; `probe_report` returns the e2fsprogs error when false.
- Consumes: existing `probe_report`/`ProbeInputs`.

- [ ] **Step 1: Write the failing test**

In `probe.rs` `mod tests`, extend `ok()` with `mkfs_ext4_on_path: true,` and add:

```rust
#[test]
fn missing_mkfs_names_e2fsprogs() {
    let err = probe_report(&ProbeInputs { mkfs_ext4_on_path: false, ..ok() }).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("mkfs.ext4") && msg.contains("e2fsprogs"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run (DGX): `cargo test -p kastellan-sandbox missing_mkfs -- --nocapture`
Expected: FAIL — field `mkfs_ext4_on_path` not defined.

- [ ] **Step 3: Implement**

Add the field to `ProbeInputs`:

```rust
    /// `mkfs.ext4` (e2fsprogs) on `$PATH` — needed to build per-spawn host-dir
    /// share images (slice 3).
    pub mkfs_ext4_on_path: bool,
```

In `probe_report`, after the `rootfs_present` check and before `Ok(())`:

```rust
    if !inputs.mkfs_ext4_on_path {
        return Err(SandboxError::Backend(
            "mkfs.ext4 not on $PATH — install e2fsprogs (Ubuntu: `sudo apt-get install \
             e2fsprogs`); required to build per-spawn host-dir share images"
                .into(),
        ));
    }
```

In `LinuxFirecracker::probe`, set the bit via a generic `which`:

```rust
            mkfs_ext4_on_path: which_on_path("mkfs.ext4"),
```

Generalize `which_firecracker` into `which_on_path` (or add a sibling):

```rust
/// Cheap `$PATH` lookup for `bin` (no spawn).
fn which_on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}
```

and replace the `firecracker_on_path: which_firecracker()` call with `which_on_path("firecracker")` (keep `which_firecracker` or delete it — prefer delete to avoid dead code; update its one caller).

- [ ] **Step 4: Run test to verify it passes**

Run (DGX): `cargo test -p kastellan-sandbox probe -- --nocapture` → PASS (all probe tests).
Mac: cross-clippy clean (catches an unused `which_firecracker` if not deleted).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/probe.rs
git commit -m "feat(microvm): probe mkfs.ext4 presence fail-closed (slice 3)"
```

---

### Task 6: Guest init — decode manifest + mount drives (`microvm-init`)

Add a pure `parse_mount_manifest` (cross-crate twin of the encoder) + Linux mount logic: mount the RO drive at `/ro-share`, tmpfs-anchor + `mkdir -p` + bind-mount each fs_read root to its absolute path, and mount the RW drive at its mountpoint.

**Files:**
- Modify: `workers/microvm-init/src/main.rs`
- Test: same file's `mod tests`

**Interfaces:**
- Produces: `fn parse_mount_manifest(cmdline: &str) -> MountManifest` where
  `struct MountManifest { ro: Option<RoMount>, rw: Option<RwMount> }`,
  `struct RoMount { dev: String, targets: Vec<String> }`,
  `struct RwMount { dev: String, mountpoint: String }`.
- Consumes: existing `hex_decode`; the `kastellan.mounts` token from `/proc/cmdline`.

- [ ] **Step 1: Write the failing tests**

In `microvm-init/src/main.rs` `mod tests`:

```rust
#[test]
fn parse_mount_manifest_decodes_ro_fixture() {
    // Cross-crate sync guard: kastellan-sandbox's encoder emits this exact hex
    // for RoShare{sources:[/opt/a], guest_dev:/dev/vdb}. Block "ro\t/dev/vdb\t/opt/a".
    let cmdline = "console=ttyS0 kastellan.mounts=726f092f6465762f766462092f6f70742f61";
    let m = parse_mount_manifest(cmdline);
    let ro = m.ro.expect("ro mount");
    assert_eq!(ro.dev, "/dev/vdb");
    assert_eq!(ro.targets, vec!["/opt/a".to_string()]);
    assert!(m.rw.is_none());
}

#[test]
fn parse_mount_manifest_decodes_ro_and_rw() {
    // Block "ro\t/dev/vdb\t/opt/a\nrw\t/dev/vdc\t/tmp/s".
    // Build the hex from the bytes to avoid a hand-typo; assert structure.
    let block = "ro\t/dev/vdb\t/opt/a\nrw\t/dev/vdc\t/tmp/s";
    let hex: String = block.bytes().map(|b| format!("{b:02x}")).collect();
    let cmdline = format!("console=ttyS0 kastellan.mounts={hex}");
    let m = parse_mount_manifest(&cmdline);
    assert_eq!(m.ro.unwrap().dev, "/dev/vdb");
    let rw = m.rw.unwrap();
    assert_eq!(rw.dev, "/dev/vdc");
    assert_eq!(rw.mountpoint, "/tmp/s");
}

#[test]
fn parse_mount_manifest_missing_or_garbled_is_empty() {
    let m = parse_mount_manifest("console=ttyS0 panic=1");
    assert!(m.ro.is_none() && m.rw.is_none());
    let bad = parse_mount_manifest("kastellan.mounts=zz");
    assert!(bad.ro.is_none() && bad.rw.is_none());
}

#[test]
fn anchor_of_skips_tmp_and_takes_top_level() {
    assert_eq!(anchor_of("/opt/venv/lib"), Some("/opt".to_string()));
    assert_eq!(anchor_of("/work/scratch"), Some("/work".to_string()));
    // /tmp is already a writable tmpfs → no anchor needed.
    assert_eq!(anchor_of("/tmp/x"), None);
    assert_eq!(anchor_of("/"), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kastellan-microvm-init parse_mount_manifest`
Expected: FAIL — `parse_mount_manifest` / types not defined.

- [ ] **Step 3: Implement the pure decoder (cross-platform) + the Linux mount path**

Add the constant + types + parser near `parse_env_cmdline` (all `#[allow(dead_code)]` for the macOS build, like the env twins):

```rust
/// Cmdline token carrying the hex-encoded mount manifest (slice 3). Must stay in
/// sync with `kastellan-sandbox::linux_firecracker::plan::MOUNTS_CMDLINE_KEY`.
#[allow(dead_code)]
const MOUNTS_CMDLINE_KEY: &str = "kastellan.mounts";

#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
struct MountManifest {
    ro: Option<RoMount>,
    rw: Option<RwMount>,
}
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
struct RoMount {
    dev: String,
    targets: Vec<String>,
}
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
struct RwMount {
    dev: String,
    mountpoint: String,
}

/// Decode the `kastellan.mounts=<hex>` token into a [`MountManifest`]. Pure →
/// unit-testable on any platform. Fail-safe: a missing/garbled token, bad hex,
/// non-UTF-8, or a malformed line yields an empty/partial manifest rather than an
/// error (the guest still boots a working worker, just without that share).
#[allow(dead_code)]
fn parse_mount_manifest(cmdline: &str) -> MountManifest {
    let prefix = format!("{MOUNTS_CMDLINE_KEY}=");
    let Some(token) = cmdline.split_whitespace().find_map(|t| t.strip_prefix(&prefix)) else {
        return MountManifest::default();
    };
    let Some(bytes) = hex_decode(token) else {
        return MountManifest::default();
    };
    let Ok(block) = String::from_utf8(bytes) else {
        return MountManifest::default();
    };
    let mut m = MountManifest::default();
    for line in block.split('\n') {
        let mut fields = line.split('\t');
        match fields.next() {
            Some("ro") => {
                if let Some(dev) = fields.next() {
                    let targets: Vec<String> = fields.map(|s| s.to_string()).collect();
                    if !targets.is_empty() {
                        m.ro = Some(RoMount { dev: dev.to_string(), targets });
                    }
                }
            }
            Some("rw") => {
                if let (Some(dev), Some(mp)) = (fields.next(), fields.next()) {
                    m.rw = Some(RwMount { dev: dev.to_string(), mountpoint: mp.to_string() });
                }
            }
            _ => {}
        }
    }
    m
}
```

Add the pure `anchor_of` helper (cross-platform, `#[allow(dead_code)]`, next to the parser) so the anchor computation is unit-tested without a VM:

```rust
/// Top-level anchor of an absolute path ("/opt/venv" → "/opt"). Returns `None`
/// for `/tmp/*` (already a writable tmpfs, no anchor needed) and for `/`. Pure.
#[allow(dead_code)]
fn anchor_of(path: &str) -> Option<String> {
    let first = path.trim_start_matches('/').split('/').next()?;
    if first.is_empty() || first == "tmp" {
        return None;
    }
    Some(format!("/{first}"))
}
```

Add the Linux mount application (after `mount_pseudo_fs`, gated `#[cfg(target_os = "linux")]`):

```rust
/// Apply the host-dir-share mounts (slice 3). RO drive → /ro-share, then each
/// fs_read root bind-mounted to its absolute path (tmpfs-anchored so mkdir works
/// on the read-only root); RW drive → its mountpoint. Best-effort per mount: a
/// failure is logged to stderr (the kernel console) but does not abort PID1 —
/// the worker simply won't see that path, surfaced as a normal file error.
#[cfg(target_os = "linux")]
fn apply_host_mounts(m: &MountManifest) {
    use std::collections::BTreeSet;

    fn mount(src: &str, target: &str, fstype: Option<&str>, flags: libc::c_ulong) -> bool {
        let src = std::ffi::CString::new(src).unwrap();
        let target = std::ffi::CString::new(target).unwrap();
        let fst = fstype.map(|f| std::ffi::CString::new(f).unwrap());
        let fst_ptr = fst.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let rc = unsafe {
            libc::mount(src.as_ptr(), target.as_ptr(), fst_ptr, flags, std::ptr::null())
        };
        if rc != 0 {
            eprintln!("microvm-init: mount {target} failed (errno {})", unsafe {
                *libc::__errno_location()
            });
        }
        rc == 0
    }

    // Collect every target whose parent must be made writable.
    let mut targets: Vec<&str> = Vec::new();
    if let Some(ro) = &m.ro {
        for t in &ro.targets {
            targets.push(t);
        }
    }
    if let Some(rw) = &m.rw {
        targets.push(&rw.mountpoint);
    }
    // tmpfs each unique anchor once (makes the read-only root writable there).
    let anchors: BTreeSet<String> = targets.iter().filter_map(|t| anchor_of(t)).collect();
    for a in &anchors {
        let _ = std::fs::create_dir_all(a); // anchor dir is pre-created in rootfs; harmless if exists
        mount("tmpfs", a, Some("tmpfs"), 0);
    }

    // RO share: mount the ext4 read-only at /ro-share, then bind-mount each root.
    if let Some(ro) = &m.ro {
        let _ = std::fs::create_dir_all("/ro-share");
        if mount(&ro.dev, "/ro-share", Some("ext4"), libc::MS_RDONLY) {
            for t in &ro.targets {
                let from = format!("/ro-share{t}");
                if std::fs::create_dir_all(t).is_ok() {
                    mount(&from, t, None, libc::MS_BIND);
                }
            }
        }
    }

    // RW scratch: mount the blank ext4 read-write at its mountpoint.
    if let Some(rw) = &m.rw {
        let _ = std::fs::create_dir_all(&rw.mountpoint);
        mount(&rw.dev, &rw.mountpoint, Some("ext4"), 0);
    }
}
```

Wire it into `main()` (Linux), right after `mount_pseudo_fs();`:

```rust
    let cmdline_for_mounts = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    apply_host_mounts(&parse_mount_manifest(&cmdline_for_mounts));
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-microvm-init` → PASS (parser tests + existing env tests).
Run: `cargo build -p kastellan-microvm-init` (macOS stub still compiles).
Mac: `cargo clippy -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add workers/microvm-init/src/main.rs
git commit -m "feat(microvm): guest init decodes + mounts host-dir shares (slice 3)"
```

---

### Task 7: Rootfs anchor dirs (`build-rootfs.sh`)

Pre-create the empty anchor dirs the init tmpfs-mounts onto (`/ro-share` + the bind-target anchors). Without these the guest can't anchor a tmpfs to make mkdir work.

**Files:**
- Modify: `scripts/workers/microvm/build-rootfs.sh` (line ~89 pseudo-fs mkdir)

**Interfaces:** none (shell). Covered by the Task-8 e2e.

- [ ] **Step 1: Add the anchor dirs**

Change the pseudo-fs mountpoint line:

```bash
# 3d. Pseudo-fs mountpoints (kastellan-microvm-init mounts proc/sys/tmp at boot)
#     + slice-3 host-dir-share anchors: /ro-share holds the RO share mount; the
#     others are empty anchors the init tmpfs-mounts so bind-mount targets can be
#     mkdir'd on the otherwise read-only root. fs_read paths must live under one
#     of these (never under /usr|/bin|/lib|/etc — build_launch_plan rejects that).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"
```

- [ ] **Step 2: Verify the script still parses**

Run: `bash -n scripts/workers/microvm/build-rootfs.sh`
Expected: no output (syntax OK). (Full rebuild happens on the DGX in Task 8.)

- [ ] **Step 3: Commit**

```bash
git add scripts/workers/microvm/build-rootfs.sh
git commit -m "feat(microvm): pre-create host-dir-share anchor dirs in rootfs (slice 3)"
```

---

### Task 8: Synthetic DGX e2e (`core/tests`)

Drive the firecracker backend directly with a crafted policy: a host `fs_read` dir with a sentinel + a `/tmp` scratch path; assert in-VM Python reads the sentinel at its original absolute path and writes to scratch. `#[ignore]`, DGX-only.

**Files:**
- Create: `core/tests/python_exec_firecracker_hostdir_e2e.rs`

**Interfaces:**
- Consumes: `LinuxFirecracker`, `build_launch_plan`/`spawn_under_policy` via `SandboxBackend`, `kastellan-protocol` `Client`, the python-exec rootfs. Models `python_exec_firecracker_e2e.rs` for the skip/locate helpers.

- [ ] **Step 1: Write the e2e (the failing test)**

Create `core/tests/python_exec_firecracker_hostdir_e2e.rs`. Copy the `image_dir`/`firecracker_image`/`locate_microvm_run`/`skip_if_no_microvm`/`firecracker_backend` helpers verbatim from `python_exec_firecracker_e2e.rs` (copying matches the existing per-file pattern). Reuse the `spawn_worker` + `dispatch_with_sink` + `firecracker_mode_entry` path, mutating the entry's policy to add the shares.

Key construction (chosen so the test runs on the DGX as the worker user, no sudo, while exercising BOTH mount branches):
- **`fs_read = [host_ro]`** where `host_ro` is a real readable host dir under `/tmp` (top-level `/tmp` → in-guest the bind target's parent is the already-writable tmpfs, no anchor needed). `build_share_images` stages its tree into the RO ext4; in-guest it is bind-mounted at the **same absolute path**, so the sentinel is readable at `host_ro/sentinel.txt`. Exercises RO staging + drive + bind.
- **`fs_write = ["/work/scratch"]`** — `/work` is a rootfs anchor (Task 7), so this exercises the **tmpfs-anchor branch** (init mounts tmpfs at `/work`, mkdir `/work/scratch`, mounts the blank RW ext4). No host dir needed (the RW image is blank-built).

```rust
#![cfg(target_os = "linux")]
//! Synthetic slice-3 e2e: the firecracker backend exposes a host fs_read dir
//! read-only at its absolute path inside the guest, plus a writable disk-backed
//! scratch drive at an anchor path. Drives the backend via the existing
//! python-exec entry with the policy mutated to add the shares (no
//! production-manifest change) — a generic-mechanism test.
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + mkfs.ext4 + a built
//! rootfs (REBUILD via build-rootfs.sh — it must carry the slice-3 anchor dirs)
//! + the kastellan-microvm-run RELEASE launcher (rebuild it; target/release is
//! preferred and a stale one silently shadows source changes). Run:
//!   export PATH=$HOME/.local/bin:$PATH   # firecracker is off the ssh PATH
//!   cargo build --release -p kastellan-microvm-run
//!   cargo test -p kastellan-core --test python_exec_firecracker_hostdir_e2e -- --ignored --nocapture

use std::path::PathBuf;
use std::sync::Arc;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::NoopAuditSink;

// ── copy image_dir / firecracker_image / locate_microvm_run / skip_if_no_microvm
//    / firecracker_backend verbatim from python_exec_firecracker_e2e.rs ──

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "DGX-only: real KVM + vsock + rootfs with slice-3 anchors"]
async fn host_dir_is_readonly_and_scratch_writable_in_vm() {
    if skip_if_no_microvm() {
        return;
    }

    // Real readable host dir under /tmp with a sentinel; exposed in-guest at the
    // SAME absolute path (bind-mount path identity).
    let host_ro = std::env::temp_dir().join(format!("kastellan-s3-ro-{}", std::process::id()));
    std::fs::create_dir_all(&host_ro).unwrap();
    std::fs::write(host_ro.join("sentinel.txt"), b"slice3-ok").unwrap();
    let scratch_mount = PathBuf::from("/work/scratch"); // /work is a rootfs anchor

    let mut entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    entry.policy.fs_read = vec![host_ro.clone()];
    entry.policy.fs_write = vec![scratch_mount.clone()];

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn worker in micro-VM");

    let code = format!(
        "open('{}','w').write('w'); print(open('{}').read())",
        scratch_mount.join("out").display(),
        host_ro.join("sentinel.txt").display(),
    );
    let out = dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        &mut worker,
        "python-exec",
        "python.exec",
        serde_json::json!({ "code": code }),
    )
    .await
    .expect("dispatch python.exec");
    let _ = worker.close();

    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(stdout.contains("slice3-ok"), "guest read host sentinel: {out}");
    assert_eq!(out["exit_code"], 0, "scratch write + sentinel read both succeeded: {out}");

    let _ = std::fs::remove_dir_all(&host_ro);
}
```

- [ ] **Step 2: Run to verify it fails (or skips cleanly) before the rootfs rebuild**

Run (DGX, BEFORE rebuilding the rootfs): `cargo test -p kastellan-core --test python_exec_firecracker_hostdir_e2e -- --ignored --nocapture`
Expected: FAIL (no `/data` anchor / no mount) — proves the test exercises the new path. (If it errors at spawn for unrelated reasons, fix those first.)

- [ ] **Step 3: Rebuild rootfs + release launcher, then run GREEN**

Run (DGX):
```bash
export PATH=$HOME/.local/bin:$PATH
bash scripts/workers/microvm/build-rootfs.sh
cargo build --release -p kastellan-microvm-run
cargo test -p kastellan-core --test python_exec_firecracker_hostdir_e2e -- --ignored --nocapture
```
Expected: PASS — `slice3-ok` in stdout, `exit_code == 0`.

- [ ] **Step 4: No-regression sweep**

Run (DGX):
```bash
cargo test -p kastellan-sandbox -- --nocapture
cargo test -p kastellan-core --test python_exec_firecracker_e2e -- --ignored --nocapture
cargo test -p kastellan-core --test python_exec_firecracker_warm_idle_e2e -- --ignored --nocapture
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: all PASS / clean (slice-1 + slice-2 e2e unaffected; 0 orphan run-dirs).

- [ ] **Step 5: Commit**

```bash
git add core/tests/python_exec_firecracker_hostdir_e2e.rs
git commit -m "test(microvm): synthetic host-dir-sharing DGX e2e (slice 3)"
```

---

## Final verification (before PR)

- [ ] **Mac gate:** `cargo build --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo clippy -p kastellan-sandbox -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets -- -D warnings` clean (the only Mac-side check of the Linux-cfg modules).
- [ ] **DGX gate:** sandbox unit tests GREEN (incl. all new plan/images/probe tests); the new host-dir e2e GREEN; slice-1 + slice-2 firecracker e2e GREEN (no regression); 0 leftover `/tmp/kastellan-microvm-*` dirs after the suite; workspace `--all-targets` clippy clean.
- [ ] **Update `HANDOVER.md` + `ROADMAP.md`** (header + Next-TODO → slice 4; test-count deltas; the anchor-dir constraint + scratch-size env knob as operational facts) and the parent spec's staging table can note slice 3 done.
- [ ] **PR** to `main`, referencing the slice-3 spec + the parent design's row-3.

## Self-Review notes (coverage of the spec)

- §1 component split → Tasks 1-7. §2 RO share (mke2fs -d + bind-mounts) → Tasks 1,4,6,7. §3 RW scratch (blank ext4, ephemeral, 64 MiB env-overridable) → Tasks 1,4,6. §4 manifest transport → Tasks 2,6. §Probe → Task 5. §RO-root anchors + system-dir constraint → Tasks 1 (reject), 6 (tmpfs anchors), 7 (rootfs dirs). §Testing → unit tests in Tasks 1-6 + the e2e in Task 8. §Out of scope (write-back, overlayfs, caching, heavy worker, x86_64) → untouched.
- Type consistency: `RoShare{sources,guest_dev}` / `RwScratch{mountpoint,guest_dev}` (sandbox) ↔ `RoMount{dev,targets}` / `RwMount{dev,mountpoint}` (init) are deliberately distinct types either side of the crate boundary, linked only by the wire format (the `ro\t…`/`rw\t…` block) and pinned by the shared hex fixture in Tasks 2 + 6.
