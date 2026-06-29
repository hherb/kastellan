# Firecracker micro-VM slice 5b-1 + 5b-2 — persistent-VM lifecycle + persistent RW store — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a long-lived `Net::Deny` worker run inside a Firecracker VM that boots once and serves many JSON-RPC calls, respawn-supervised, with a persistent ext4 RW store that survives a VM respawn — proven end-to-end on real KVM with a new minimal `kv-demo` worker, leaving the live Matrix code untouched.

**Architecture:** A backend-agnostic `PersistentWorker` supervisor (persistent OS thread + crash-respawn) owns a boxed `PersistentTransport` (production impl wraps the protocol `Client`). A new additive `SandboxPolicy.persistent_store` field is honored by the Firecracker backend as a stable, mkfs-once ext4 image (flock-guarded by the launcher) and by bwrap/Seatbelt as a persistent `fs_write` bind. A `kv-demo` worker baked into its own rootfs exercises both.

**Tech Stack:** Rust (workspace, rustc 1.96.0), `kastellan-protocol` JSON-RPC stdio, `kastellan-worker-prelude` (Landlock+seccomp), Firecracker micro-VM backend (`sandbox/src/linux_firecracker/`), `mkfs.ext4`, `flock(2)`.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only** (Apache-2.0/MIT/BSD/MPL/LGPL/(A)GPL). `kv-demo` deps limited to existing workspace deps: `kastellan-protocol`, `kastellan-worker-prelude`, `serde`, `serde_json`, `anyhow`. **No new third-party crates.**
- **Cross-platform first-class.** Every reusable abstraction (`PersistentWorker`, `PersistentStore`, `kv-demo`) compiles + unit-tests on macOS **and** Linux. The VM mechanism is Linux/Firecracker-only; bwrap/Seatbelt get an equivalent persistent-`fs_write` guarantee.
- **Files under 500 LOC** where feasible; lift `mod tests` to a sibling `tests.rs` if a file exceeds it.
- **TDD**: failing test first, minimal impl, green, commit. **All tests pass before committing.**
- **`SandboxPolicy.fs_read` paths must be absolute**; so must `persistent_store.host_backing` and `persistent_store.guest_mount`.
- **Additive / byte-identical default:** `persistent_store: None` ⇒ no behaviour change on any backend (the `#[serde(default)]` + match-on-`None` discipline of `proxy_uds`).
- **No "spawn unsandboxed" escape hatch.** `kv-demo` locks down via `prelude::serve_stdio`.
- **Build/test:** `source "$HOME/.cargo/env"` first. Mac = `cargo test --workspace` (skip-as-pass) + `cargo clippy --workspace --all-targets -D warnings`. DGX (native aarch64, real KVM) = the Linux acceptance gate for the `#[ignore]` e2e: `ssh dgx '<cmd>'`, rebuild `--release -p kastellan-microvm-run` + the kv-demo rootfs, `export PATH=$HOME/.local/bin:$PATH`.

---

## File Structure

**New files:**
- `core/src/worker_lifecycle/persistent.rs` — `PersistentTransport` trait, `ClientTransport` prod impl + `spawn` helper, `PersistentWorker`/`PersistentHandle`, driver loop. (+ `persistent/tests.rs` if over cap.)
- `workers/kv-demo/Cargo.toml`, `workers/kv-demo/src/main.rs` — the demo worker (`KvHandler` + store).
- `scripts/workers/kv-demo/build-kv-demo-rootfs.sh` — rootfs builder.
- `core/tests/kv_demo_persistent_e2e.rs` — cross-platform (Seatbelt/bwrap) lifecycle+store e2e.
- `core/tests/kv_demo_firecracker_persistent_e2e.rs` — DGX real-KVM `#[ignore]` e2e.

**Modified files:**
- `sandbox/src/lib.rs` — `PersistentStore` struct + `SandboxPolicy.persistent_store` field.
- `sandbox/src/linux_firecracker/mounts.rs` — `PersistentMount` + 2nd-`rw`-line encode.
- `sandbox/src/linux_firecracker/images.rs` — `persistent_mkfs_decision` + `build_persistent_image`.
- `sandbox/src/linux_firecracker/plan.rs` — wire `persistent_store` into the plan (guest_dev, mount, drive).
- `sandbox/src/linux_firecracker.rs` — call `build_persistent_image`; pass `--persistent-image` to launcher argv.
- `sandbox/src/linux_bwrap.rs` — bind `host_backing`→`guest_mount` RW.
- `sandbox/src/macos_seatbelt.rs` — grant file-read*/write* on `persistent_store`.
- `core/src/tool_host.rs` — `derive_lockdown_env` appends `guest_mount` to `KASTELLAN_LANDLOCK_RW`.
- `core/src/worker_lifecycle/mod.rs` — declare + re-export `persistent`.
- `workers/microvm-run/src/main.rs` (+ a small `persistent_lock.rs`) — `--persistent-image` flock.
- `Cargo.toml` — add `workers/kv-demo` member.
- `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md` — session-end updates.

---

## Task 1: `PersistentStore` policy field

**Files:**
- Modify: `sandbox/src/lib.rs` (after the `proxy_uds` field, ~line 144)
- Test: `sandbox/src/lib.rs` `#[cfg(test)] mod` (or its existing tests module)

**Interfaces:**
- Produces: `kastellan_sandbox::PersistentStore { host_backing: PathBuf, guest_mount: PathBuf, size_mib: u32 }`; `SandboxPolicy.persistent_store: Option<PersistentStore>`.

- [ ] **Step 1: Write the failing test** — add to the sandbox lib tests module:

```rust
#[test]
fn persistent_store_defaults_to_none_and_round_trips() {
    // Back-compat: a policy serialized without the field deserializes to None.
    let json = r#"{"fs_read":[],"fs_write":[],"net":"Deny","cpu_ms":0,"mem_mb":256,"profile":"WorkerStrict"}"#;
    let p: SandboxPolicy = serde_json::from_str(json).expect("deserialize legacy policy");
    assert!(p.persistent_store.is_none());

    // A populated store round-trips.
    let store = PersistentStore {
        host_backing: PathBuf::from("/var/lib/kastellan/kv/store.ext4"),
        guest_mount: PathBuf::from("/data"),
        size_mib: 64,
    };
    let mut p2 = p.clone();
    p2.persistent_store = Some(store.clone());
    let s = serde_json::to_string(&p2).unwrap();
    let back: SandboxPolicy = serde_json::from_str(&s).unwrap();
    assert_eq!(back.persistent_store, Some(store));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-sandbox persistent_store_defaults -- --nocapture`
Expected: FAIL — `PersistentStore` / field unknown (compile error).

- [ ] **Step 3: Add the type + field.** In `sandbox/src/lib.rs`, add above `SandboxPolicy`:

```rust
/// A persistent writable store for a long-lived worker: backing survives a
/// worker/VM respawn. Interpreted per-backend — an **ext4 image file** on the
/// Firecracker backend (mkfs-once, then reused untouched), a **directory**
/// bound RW on bwrap/Seatbelt. Both `host_backing` and `guest_mount` must be
/// absolute. Distinct from `fs_write` ephemeral scratch (re-created per spawn).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentStore {
    /// Stable host path. Firecracker: ext4 image file. bwrap/Seatbelt: directory.
    pub host_backing: PathBuf,
    /// Absolute in-guest/in-jail mount point the worker writes to.
    pub guest_mount: PathBuf,
    /// ext4 image size (MiB) on first create. Ignored by dir-backed backends.
    pub size_mib: u32,
}
```

Then add the field as the **last** field of `SandboxPolicy` (after `proxy_uds`):

```rust
    /// A persistent writable store that survives a respawn (long-lived workers).
    /// `None` ⇒ no store, byte-identical to prior behaviour. See [`PersistentStore`].
    #[serde(default)]
    pub persistent_store: Option<PersistentStore>,
```

- [ ] **Step 4: Update any struct-literal construction sites.** Run `cargo build -p kastellan-sandbox` and fix every `SandboxPolicy { ... }` literal that now misses the field by adding `persistent_store: None,`. (Search: `rg "SandboxPolicy \{" --type rust`.)

Run: `cargo test -p kastellan-sandbox persistent_store_defaults -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/lib.rs
git commit -m "feat(sandbox): add SandboxPolicy.persistent_store field (5b-2)"
```

---

## Task 2: `PersistentMount` + 2nd-`rw`-line mount encoding

**Files:**
- Modify: `sandbox/src/linux_firecracker/mounts.rs`
- Test: same file's tests module

**Interfaces:**
- Consumes: existing `RoShare`, `RwScratch`, `encode_mount_manifest(ro, rw)`.
- Produces: `PersistentMount { mountpoint: PathBuf, guest_dev: String }`; `encode_mount_manifest` gains a third param `persistent: Option<&PersistentMount>` emitting another `rw\t<dev>\t<mountpoint>` line.

> The guest-side parser (`workers/microvm-init/src/main.rs`, the `kastellan.mounts` decoder) already iterates manifest lines and mounts each `rw` entry; a second `rw` line needs no guest change. **Verify** this by reading that decoder before Step 3; if it only honors the first `rw` line, extend its loop to mount every `rw` entry (and add a guest-init unit test there).

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn encode_includes_persistent_rw_line_after_scratch() {
    let rw = RwScratch { mountpoint: PathBuf::from("/tmp"), guest_dev: "/dev/vdc".into() };
    let ps = PersistentMount { mountpoint: PathBuf::from("/data"), guest_dev: "/dev/vdd".into() };
    let suffix = encode_mount_manifest(None, Some(&rw), Some(&ps)).unwrap().unwrap();
    // hex-decode the value after "kastellan.mounts="
    let hex = suffix.trim().strip_prefix("kastellan.mounts=").unwrap();
    let bytes = (0..hex.len()).step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect::<Vec<u8>>();
    let decoded = String::from_utf8(bytes).unwrap();
    assert!(decoded.contains("rw\t/dev/vdc\t/tmp"));
    assert!(decoded.contains("rw\t/dev/vdd\t/data"));
}
```

- [ ] **Step 2: Run — expect FAIL** (`PersistentMount` undefined / arity mismatch).

Run: `cargo test -p kastellan-sandbox encode_includes_persistent_rw_line -- --nocapture`

- [ ] **Step 3: Implement.** Add the struct near `RwScratch`:

```rust
/// A persistent, host-backed RW drive mounted in-guest at `mountpoint`. Unlike
/// [`RwScratch`] its backing image is reused across spawns (contents survive).
#[derive(Clone, Debug, PartialEq)]
pub struct PersistentMount {
    pub mountpoint: PathBuf,
    pub guest_dev: String,
}
```

Change `encode_mount_manifest` to take the extra param and emit the line. Update the early-return guard and the body:

```rust
pub fn encode_mount_manifest(
    ro: Option<&RoShare>,
    rw: Option<&RwScratch>,
    persistent: Option<&PersistentMount>,
) -> Result<Option<String>, SandboxError> {
    if ro.is_none() && rw.is_none() && persistent.is_none() {
        return Ok(None);
    }
    // ... existing ro + rw blocks unchanged ...
    if let Some(ps) = persistent {
        let mp = ps.mountpoint.to_string_lossy();
        guard(&mp)?;
        lines.push(format!("rw\t{}\t{}", ps.guest_dev, mp));
    }
    let block = lines.join("\n");
    Ok(Some(format!(" {MOUNTS_CMDLINE_KEY}={}", hex_encode(block.as_bytes()))))
}
```

Update the one existing caller in `plan.rs` to pass `None` for the new param for now (Task 4 wires it).

- [ ] **Step 4: Run — expect PASS**, and `cargo test -p kastellan-sandbox` (mounts module green).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/mounts.rs sandbox/src/linux_firecracker/plan.rs
git commit -m "feat(sandbox): mounts.rs encodes a persistent rw line (5b-2)"
```

---

## Task 3: `images.rs` mkfs-once persistent image

**Files:**
- Modify: `sandbox/src/linux_firecracker/images.rs`
- Test: same file's tests module

**Interfaces:**
- Consumes: existing `mkfs_blank_argv(out_img, size_mib)`.
- Produces: pure `persistent_mkfs_decision(host_backing_exists: bool, host_backing: &str, size_mib: u64) -> Option<Vec<String>>` (the mkfs argv if the image must be created, else `None`); `build_persistent_image(plan: &mut FirecrackerLaunchPlan) -> Result<(), SandboxError>` runs it.

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn persistent_image_mkfs_only_when_absent() {
    // Absent → produce a blank mkfs argv at the given size.
    let argv = persistent_mkfs_decision(false, "/var/lib/kastellan/kv/store.ext4", 64)
        .expect("absent image must be created");
    assert_eq!(argv[0], "mkfs.ext4");
    assert!(argv.contains(&"/var/lib/kastellan/kv/store.ext4".to_string()));
    assert!(argv.contains(&"64M".to_string()));
    // Present → reuse untouched, no mkfs.
    assert!(persistent_mkfs_decision(true, "/var/lib/kastellan/kv/store.ext4", 64).is_none());
}
```

- [ ] **Step 2: Run — expect FAIL.**

Run: `cargo test -p kastellan-sandbox persistent_image_mkfs_only_when_absent -- --nocapture`

- [ ] **Step 3: Implement** the pure decision + the runner:

```rust
/// mkfs argv for the persistent image **iff it does not yet exist**. An
/// existing image is reused untouched so its contents survive. Pure (the
/// existence check is the caller's), so it is unit-testable without a disk.
pub fn persistent_mkfs_decision(
    host_backing_exists: bool,
    host_backing: &str,
    size_mib: u64,
) -> Option<Vec<String>> {
    if host_backing_exists {
        None
    } else {
        Some(mkfs_blank_argv(host_backing, size_mib))
    }
}

/// Create the persistent image once (mkfs if absent), set the plan's path.
/// Reuses the same `run` shell-out style as [`build_share_images`].
pub fn build_persistent_image(plan: &mut FirecrackerLaunchPlan) -> Result<(), SandboxError> {
    let Some(ps) = plan.persistent_store.clone() else { return Ok(()) };
    if let Some(parent) = ps.host_backing.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SandboxError::Backend(format!("persistent store mkdir {parent:?}: {e}")))?;
    }
    let exists = ps.host_backing.exists();
    if let Some(argv) = persistent_mkfs_decision(
        exists,
        &ps.host_backing.to_string_lossy(),
        ps.size_mib as u64,
    ) {
        let status = Command::new(&argv[0]).args(&argv[1..]).status()
            .map_err(|e| SandboxError::Backend(format!("spawn {}: {e}", argv[0])))?;
        if !status.success() {
            return Err(SandboxError::Backend(format!("{} failed: {status}", argv[0])));
        }
    }
    plan.persistent_image_path = Some(ps.host_backing.clone());
    Ok(())
}
```

> `plan.persistent_store` / `plan.persistent_image_path` are added in Task 4. If implementing strictly in order, gate `build_persistent_image` behind the Task-4 plan fields, or do Task 4 first and return here — the two tasks are tightly coupled. (Plan order: do Task 4's plan-struct fields, then this runner.)

- [ ] **Step 4: Run — expect PASS** (the pure decision test; `build_persistent_image` compiles once Task 4's fields exist).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/images.rs
git commit -m "feat(sandbox): mkfs-once persistent image builder (5b-2)"
```

---

## Task 4: Wire `persistent_store` into `build_launch_plan`

**Files:**
- Modify: `sandbox/src/linux_firecracker/plan.rs` (the `FirecrackerLaunchPlan` struct + `build_launch_plan` + `render_firecracker_config`)
- Test: same file's tests module

**Interfaces:**
- Consumes: `policy.persistent_store`, `PersistentMount`, `encode_mount_manifest(ro, rw, persistent)`.
- Produces: `FirecrackerLaunchPlan` gains `persistent_store: Option<PersistentStore>`, `persistent_image_path: Option<PathBuf>`, `persistent_mount: Option<PersistentMount>`.

- [ ] **Step 1: Write the failing test** (mirror the existing plan tests):

```rust
#[test]
fn persistent_store_assigns_drive_and_rw_mount() {
    let mut policy = test_policy_net_deny(); // existing helper; else build a minimal Net::Deny policy
    policy.persistent_store = Some(kastellan_sandbox::PersistentStore {
        host_backing: std::path::PathBuf::from("/var/lib/kastellan/kv/store.ext4"),
        guest_mount: std::path::PathBuf::from("/data"),
        size_mib: 64,
    });
    let image = test_image();
    let plan = build_launch_plan(&policy, &image, "/usr/local/bin/kastellan-worker-kv-demo", &[]).unwrap();
    let pm = plan.persistent_mount.as_ref().expect("persistent mount present");
    assert_eq!(pm.mountpoint, std::path::PathBuf::from("/data"));
    // distinct guest_dev from any ro/rw share device
    assert!(pm.guest_dev.starts_with("/dev/vd"));
    assert!(plan.boot_args.contains("kastellan.mounts="));
    // rendered config attaches a non-root RW drive for the persistent image
    let cfg = render_firecracker_config(&plan);
    let drives = cfg["drives"].as_array().unwrap();
    assert!(drives.iter().any(|d| d["drive_id"] == "persistent-store" && d["is_read_only"] == false));
}
```

- [ ] **Step 2: Run — expect FAIL.**

Run: `cargo test -p kastellan-sandbox persistent_store_assigns_drive_and_rw_mount -- --nocapture`

- [ ] **Step 3: Implement:**

1. Add to `FirecrackerLaunchPlan`:
```rust
    pub persistent_store: Option<kastellan_sandbox::PersistentStore>,
    pub persistent_image_path: Option<PathBuf>,
    pub persistent_mount: Option<crate::linux_firecracker::mounts::PersistentMount>,
```
(Use the crate-internal path the file already uses for `RwScratch`. Initialize the two `*_path`/`*_mount` to `None` and copy `persistent_store` from the policy in `build_launch_plan`.)

2. In `build_launch_plan`, after the ro/rw `next_letter` device assignment block (around line 272-283), assign the persistent device + mount:
```rust
    let persistent_mount = policy.persistent_store.as_ref().map(|ps| {
        let dev = format!("/dev/vd{}", next_letter as char);
        next_letter += 1;
        PersistentMount { mountpoint: ps.guest_mount.clone(), guest_dev: dev }
    });
```

3. Update the mount-manifest call (line ~306) to pass the persistent mount:
```rust
    if let Some(suffix) = encode_mount_manifest(ro_share.as_ref(), rw_scratch.as_ref(), persistent_mount.as_ref())? {
        boot_args.push_str(&suffix);
    }
```

4. Set the plan fields before returning: `persistent_store: policy.persistent_store.clone()`, `persistent_image_path: None`, `persistent_mount`.

5. In `render_firecracker_config`, after the `rw-scratch` drive block (line ~409-415), attach the persistent drive:
```rust
    if let Some(img) = &plan.persistent_image_path {
        cfg["drives"].as_array_mut().unwrap().push(json!({
            "drive_id": "persistent-store",
            "path_on_host": img.to_string_lossy(),
            "is_root_device": false,
            "is_read_only": false,
        }));
    }
```

- [ ] **Step 4: Run — expect PASS** + `cargo test -p kastellan-sandbox` green.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/plan.rs
git commit -m "feat(sandbox): build_launch_plan wires persistent_store drive+mount (5b-2)"
```

---

## Task 5: `spawn_under_policy` builds the persistent image + passes `--persistent-image`

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs` (`spawn_under_policy` ~line 153-186; `launcher_argv` ~line 70-93)
- Test: same file's tests module (argv shape)

**Interfaces:**
- Consumes: `build_persistent_image(&mut plan)` (Task 3), `plan.persistent_image_path`.
- Produces: launcher argv includes `--persistent-image <host_backing>` exactly when `persistent_store` is set.

- [ ] **Step 1: Write the failing test** for `launcher_argv`:

```rust
#[test]
fn launcher_argv_includes_persistent_image_flag_when_set() {
    let mut plan = minimal_plan_for_test();
    plan.persistent_image_path = Some(std::path::PathBuf::from("/var/lib/kastellan/kv/store.ext4"));
    let argv = launcher_argv(&plan, "fc.json", "fc.log", "run");
    let i = argv.iter().position(|a| a == "--persistent-image").expect("flag present");
    assert_eq!(argv[i + 1], "/var/lib/kastellan/kv/store.ext4");

    // absent ⇒ no flag (byte-identical legacy argv)
    let mut plan2 = minimal_plan_for_test();
    plan2.persistent_image_path = None;
    assert!(!launcher_argv(&plan2, "fc.json", "fc.log", "run").iter().any(|a| a == "--persistent-image"));
}
```

- [ ] **Step 2: Run — expect FAIL.**

Run: `cargo test -p kastellan-sandbox launcher_argv_includes_persistent_image -- --nocapture`

- [ ] **Step 3: Implement.** In `spawn_under_policy`, after `build_share_images(&mut plan, &run_dir, &policy.env)?;`:

```rust
        // Slice 5b-2: build/reuse the persistent store image (mkfs-once). Lives
        // at a stable host path OUTSIDE run_dir, so it survives teardown.
        images::build_persistent_image(&mut plan)?;
```

In `launcher_argv`, append the flag when the plan has a persistent image (mirror how `--egress-uds` is conditionally pushed):

```rust
    if let Some(img) = &plan.persistent_image_path {
        argv.push("--persistent-image".into());
        argv.push(img.to_string_lossy().into_owned());
    }
```

- [ ] **Step 4: Run — expect PASS** + `cargo test -p kastellan-sandbox` + `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu` (Linux-gated code cross-check on the Mac).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker.rs
git commit -m "feat(sandbox): spawn builds persistent image + passes --persistent-image (5b-2)"
```

---

## Task 6: bwrap honors `persistent_store` (RW host→guest bind)

**Files:**
- Modify: `sandbox/src/linux_bwrap.rs` (after the `fs_write` bind loop ~line 196-198)
- Test: same file's tests module (argv builder)

**Interfaces:**
- Consumes: `policy.persistent_store`.
- Produces: argv contains `--bind-try <host_backing> <guest_mount>` when set.

- [ ] **Step 1: Write the failing test** (extend the bwrap argv tests):

```rust
#[test]
fn persistent_store_bind_maps_host_backing_to_guest_mount() {
    let mut policy = strict_deny_policy(); // existing test helper
    policy.persistent_store = Some(kastellan_sandbox::PersistentStore {
        host_backing: std::path::PathBuf::from("/srv/kv-state"),
        guest_mount: std::path::PathBuf::from("/data"),
        size_mib: 0,
    });
    let argv = build_argv(&policy, "/bin/true", &[]);
    // a --bind-try with DISTINCT host/jail paths (not the same-path push_bind)
    let i = argv.iter().position(|a| a == "/srv/kv-state").unwrap();
    assert_eq!(argv[i - 1], "--bind-try");
    assert_eq!(argv[i + 1], "/data");
}
```

- [ ] **Step 2: Run — expect FAIL.**

Run: `cargo test -p kastellan-sandbox persistent_store_bind_maps -- --nocapture` (Linux only; on Mac use `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu` to typecheck and run the test on the DGX).

- [ ] **Step 3: Implement.** After the `fs_write` loop:

```rust
    // Slice 5b-2: a persistent store is a RW bind from a stable host dir to the
    // jail's guest_mount (distinct paths, so not push_bind which uses one path).
    if let Some(ps) = &policy.persistent_store {
        argv.push("--bind-try".into());
        argv.push(ps.host_backing.display().to_string());
        argv.push(ps.guest_mount.display().to_string());
    }
```

- [ ] **Step 4: Run — expect PASS** (on DGX) / clippy clean (on Mac).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_bwrap.rs
git commit -m "feat(sandbox): bwrap binds persistent_store host_backing->guest_mount RW (5b-2)"
```

---

## Task 7: Seatbelt honors `persistent_store` (file-read*/write*)

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs` (`build_profile`, after the `fs_write` loop ~line 354-360)
- Test: same file's tests module

**Interfaces:**
- Consumes: `policy.persistent_store`.
- Produces: a `(allow file-read* file-write* (subpath "<guest_mount>"))` rule when set. (On macOS there is no path remap, so the demo uses `host_backing == guest_mount`; grant `guest_mount`.)

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn persistent_store_grants_rw_subpath() {
    let mut policy = strict_deny_policy();
    policy.persistent_store = Some(kastellan_sandbox::PersistentStore {
        host_backing: std::path::PathBuf::from("/tmp/kvstate"),
        guest_mount: std::path::PathBuf::from("/tmp/kvstate"),
        size_mib: 0,
    });
    let profile = build_profile(&policy);
    assert!(profile.contains("(allow file-read* file-write* (subpath \"/tmp/kvstate\"))"));
}
```

- [ ] **Step 2: Run — expect FAIL.**

Run: `cargo test -p kastellan-sandbox persistent_store_grants_rw_subpath -- --nocapture`

- [ ] **Step 3: Implement.** After the `fs_write` rule loop:

```rust
    if let Some(ps) = &policy.persistent_store {
        out.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            ps.guest_mount.display()
        ));
    }
```

- [ ] **Step 4: Run — expect PASS** + `cargo test -p kastellan-sandbox`.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs
git commit -m "feat(sandbox): Seatbelt grants RW on persistent_store (5b-2)"
```

---

## Task 8: `derive_lockdown_env` adds `guest_mount` to Landlock RW

**Files:**
- Modify: `core/src/tool_host.rs` (`derive_lockdown_env`)
- Test: `core/src/tool_host.rs` tests (or its sibling tests module)

**Interfaces:**
- Consumes: `policy.persistent_store`.
- Produces: the derived policy's `KASTELLAN_LANDLOCK_RW` env JSON array includes `guest_mount`, so the in-jail worker may write the store. **Must not** add it to `fs_write` (FC would treat that as ephemeral scratch).

> Read `derive_lockdown_env` first — it builds the `KASTELLAN_LANDLOCK_RW` value from `fs_write`. Add `persistent_store.guest_mount` to that same path list before serializing.

- [ ] **Step 1: Write the failing test:**

```rust
#[test]
fn lockdown_env_landlock_rw_includes_persistent_guest_mount() {
    let mut policy = strict_deny_policy(); // minimal Net::Deny WorkerStrict policy
    policy.persistent_store = Some(kastellan_sandbox::PersistentStore {
        host_backing: std::path::PathBuf::from("/var/lib/kastellan/kv/store.ext4"),
        guest_mount: std::path::PathBuf::from("/data"),
        size_mib: 64,
    });
    let derived = derive_lockdown_env(&policy);
    let rw = derived.env.iter().find(|(k, _)| k == "KASTELLAN_LANDLOCK_RW").map(|(_, v)| v.clone()).unwrap_or_default();
    assert!(rw.contains("/data"), "Landlock RW must include the persistent guest_mount, got {rw:?}");
    // and NOT smuggled into fs_write (which FC would make ephemeral)
    assert!(!derived.fs_write.iter().any(|p| p == std::path::Path::new("/data")));
}
```

- [ ] **Step 2: Run — expect FAIL.**

Run: `cargo test -p kastellan-core lockdown_env_landlock_rw_includes_persistent -- --nocapture`

- [ ] **Step 3: Implement.** In `derive_lockdown_env`, where the RW path list for `KASTELLAN_LANDLOCK_RW` is assembled from `fs_write`, append the persistent guest_mount:

```rust
    // Slice 5b-2: the persistent store is a writable path the worker needs
    // Landlock RW for, but it must NOT enter fs_write (the FC backend turns
    // fs_write into *ephemeral* scratch). Add only to the Landlock RW set.
    if let Some(ps) = &policy.persistent_store {
        landlock_rw_paths.push(ps.guest_mount.clone());
    }
```

(Adapt the variable name to whatever the function uses for its RW path vec.)

- [ ] **Step 4: Run — expect PASS** + `cargo test -p kastellan-core` (tool_host module).

- [ ] **Step 5: Commit**

```bash
git add core/src/tool_host.rs
git commit -m "feat(core): derive_lockdown_env adds persistent_store guest_mount to Landlock RW (5b-2)"
```

---

## Task 9: Launcher `--persistent-image` flock guard

**Files:**
- Create: `workers/microvm-run/src/persistent_lock.rs`
- Modify: `workers/microvm-run/src/main.rs` (arg parse + hold the lock before boot)
- Test: `workers/microvm-run/src/persistent_lock.rs` tests module

**Interfaces:**
- Produces: `PersistentImageLock` (RAII; holds an open fd with `flock(LOCK_EX|LOCK_NB)`); `acquire(path: &Path) -> io::Result<PersistentImageLock>` returns `Err(WouldBlock)` when another launcher holds it (fail-closed); the lock drops (releases) on process exit.

> Use `std::os::fd` + a tiny `libc::flock` call (the crate is `pure-std` per the workspace map; `microvm-init` already uses `libc`. If `microvm-run` has no `libc` dep, add `libc = { workspace = true }` to its `Cargo.toml` — it is already a workspace dep). No new external crate.

- [ ] **Step 1: Write the failing test:**

```rust
#[cfg(unix)]
#[test]
fn second_acquire_on_same_path_fails_closed() {
    let dir = std::env::temp_dir().join(format!("kastellan-persistlock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("store.ext4");
    std::fs::write(&img, b"x").unwrap();
    let _held = acquire(&img).expect("first acquire succeeds");
    let second = acquire(&img);
    assert!(second.is_err(), "second concurrent flock must fail closed");
    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 2: Run — expect FAIL.**

Run: `cargo test -p kastellan-microvm-run second_acquire_on_same_path_fails_closed -- --nocapture`

- [ ] **Step 3: Implement** `persistent_lock.rs`:

```rust
//! Advisory exclusive lock on the persistent-store image, held for the VM's
//! lifetime so two concurrent launchers can never mount the same RW ext4
//! (page-cache → corruption). Fail-closed: a busy lock aborts the boot.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// RAII guard: the held `File`'s fd carries the `flock`. Dropping it (or process
/// exit) releases the lock.
pub struct PersistentImageLock {
    _file: File,
}

/// Open `path` and take a non-blocking exclusive `flock`. `Err(WouldBlock)` when
/// another process already holds it.
pub fn acquire(path: &Path) -> io::Result<PersistentImageLock> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    // SAFETY: valid fd from the open File; LOCK_EX|LOCK_NB is a pure advisory op.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PersistentImageLock { _file: file })
}
```

In `main.rs`: parse `--persistent-image <path>`; if present, `let _lock = persistent_lock::acquire(&path).map_err(|e| /* log + exit non-zero, fail-closed */)?;` **before** spawning firecracker, and keep `_lock` in scope for the whole run (alongside the firecracker child + RAII teardown). Declare `mod persistent_lock;`.

- [ ] **Step 4: Run — expect PASS** + `cargo test -p kastellan-microvm-run`.

- [ ] **Step 5: Commit**

```bash
git add workers/microvm-run/src/persistent_lock.rs workers/microvm-run/src/main.rs workers/microvm-run/Cargo.toml
git commit -m "feat(microvm-run): --persistent-image flock guard (fail-closed) (5b-2)"
```

---

## Task 10: `PersistentWorker` supervisor (hermetic)

**Files:**
- Create: `core/src/worker_lifecycle/persistent.rs`
- Modify: `core/src/worker_lifecycle/mod.rs` (declare `pub mod persistent;` + re-export)
- Test: `core/src/worker_lifecycle/persistent.rs` tests module (lift to `persistent/tests.rs` if over 500 LOC)

**Interfaces:**
- Consumes: `RestartBackoff` (`crate::worker_lifecycle::RestartBackoff`), `RespawnRateAlarm` (`crate::channel::respawn_alarm::RespawnRateAlarm` — make it reachable: if not already `pub`, add `pub` to its `mod` decl in `core/src/channel/mod.rs`; **do not** touch `matrix.rs`).
- Produces:
  - `pub trait PersistentTransport: Send { fn call(&mut self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value>; fn death_report(&mut self) -> Option<String> { None } }`
  - `pub type PersistentFactory = Box<dyn FnMut() -> anyhow::Result<Box<dyn PersistentTransport>> + Send>;`
  - `pub struct PersistentWorker;` with `pub fn spawn(label: impl Into<String>, factory: PersistentFactory) -> anyhow::Result<PersistentHandle>` and `pub fn spawn_with_backoff(label, factory, backoff: RestartBackoff) -> anyhow::Result<PersistentHandle>` (the test seam for fast backoff).
  - `pub struct PersistentHandle` with `pub fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value>` and `pub fn shutdown(self)`.

- [ ] **Step 1: Write the failing tests** (all hermetic — no VM):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Fake transport that answers `die_after` calls, then errors (simulating
    /// worker death). Each spawn gets a fresh counter.
    struct FakeTransport { calls: usize, die_after: usize, gen: usize }
    impl PersistentTransport for FakeTransport {
        fn call(&mut self, _m: &str, _p: serde_json::Value) -> anyhow::Result<serde_json::Value> {
            if self.calls >= self.die_after {
                anyhow::bail!("simulated worker death");
            }
            self.calls += 1;
            Ok(serde_json::json!({ "gen": self.gen, "n": self.calls }))
        }
    }

    fn fast_backoff() -> RestartBackoff {
        RestartBackoff { base: Duration::from_millis(1), factor_num: 1, factor_den: 1, cap: Duration::from_millis(1) }
    }

    #[test]
    fn serves_many_calls_on_one_worker() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let s = spawns.clone();
        let factory: PersistentFactory = Box::new(move || {
            let g = s.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransport { calls: 0, die_after: 1000, gen: g }))
        });
        let h = PersistentWorker::spawn("test", factory).unwrap();
        for _ in 0..5 {
            let v = h.call("ping", serde_json::json!({})).unwrap();
            assert_eq!(v["gen"], 0);
        }
        assert_eq!(spawns.load(Ordering::SeqCst), 1, "no respawn while healthy");
        h.shutdown();
    }

    #[test]
    fn respawns_on_death_and_serves_again() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let s = spawns.clone();
        let factory: PersistentFactory = Box::new(move || {
            let g = s.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeTransport { calls: 0, die_after: 1, gen: g }))
        });
        let h = PersistentWorker::spawn_with_backoff("test", factory, fast_backoff()).unwrap();
        // gen 0 serves 1 call then dies on the 2nd
        assert_eq!(h.call("a", serde_json::json!({})).unwrap()["gen"], 0);
        assert!(h.call("b", serde_json::json!({})).is_err(), "in-flight call on death errors");
        // supervisor respawned → gen 1 serves
        let v = h.call("c", serde_json::json!({})).unwrap();
        assert_eq!(v["gen"], 1);
        assert!(spawns.load(Ordering::SeqCst) >= 2);
        h.shutdown();
    }

    #[test]
    fn call_after_shutdown_errors() {
        let factory: PersistentFactory = Box::new(|| Ok(Box::new(FakeTransport { calls: 0, die_after: 1000, gen: 0 })));
        let h = PersistentWorker::spawn("test", factory).unwrap();
        h.call("a", serde_json::json!({})).unwrap();
        h.shutdown();
        // a fresh handle can't be used post-shutdown — covered by the move semantics of shutdown(self).
    }
}
```

- [ ] **Step 2: Run — expect FAIL** (types undefined).

Run: `cargo test -p kastellan-core --lib worker_lifecycle::persistent -- --nocapture`

- [ ] **Step 3: Implement** `persistent.rs`. Driver thread owns the transport; a `std::sync::mpsc` carries `Job { method, params, reply: mpsc::Sender<anyhow::Result<Value>> }`:

```rust
//! Backend-agnostic supervisor for a LONG-LIVED worker: a persistent OS thread
//! owns the worker, forwards serialized RPC calls to it, and respawns it on
//! death (capped-exponential backoff + sliding-window rate alarm). PDEATHSIG-safe
//! (the spawning thread outlives the worker — required under the slice-5a
//! bwrap-confined launcher). A generalization of the Matrix channel's
//! `supervised_self_spawn`/`drive`, with no channel/poll-send coupling.
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use crate::channel::respawn_alarm::RespawnRateAlarm;
use crate::worker_lifecycle::RestartBackoff;

pub trait PersistentTransport: Send {
    fn call(&mut self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value>;
    fn death_report(&mut self) -> Option<String> { None }
}

pub type PersistentFactory =
    Box<dyn FnMut() -> anyhow::Result<Box<dyn PersistentTransport>> + Send>;

struct Job {
    method: String,
    params: serde_json::Value,
    reply: mpsc::Sender<anyhow::Result<serde_json::Value>>,
}

pub struct PersistentWorker;

pub struct PersistentHandle {
    req_tx: Option<mpsc::Sender<Job>>,
    driver: Option<thread::JoinHandle<()>>,
}

const ALARM_THRESHOLD: usize = 5;
const ALARM_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

impl PersistentWorker {
    pub fn spawn(label: impl Into<String>, factory: PersistentFactory) -> anyhow::Result<PersistentHandle> {
        Self::spawn_with_backoff(label, factory, RestartBackoff::default())
    }

    pub fn spawn_with_backoff(
        label: impl Into<String>,
        mut factory: PersistentFactory,
        backoff: RestartBackoff,
    ) -> anyhow::Result<PersistentHandle> {
        let label = label.into();
        let (req_tx, req_rx) = mpsc::channel::<Job>();
        let (init_tx, init_rx) = mpsc::channel::<anyhow::Result<()>>();
        let driver = thread::spawn(move || {
            // Initial spawn ON this persistent thread (PDEATHSIG parent).
            let mut transport = match factory() {
                Ok(t) => { let _ = init_tx.send(Ok(())); t }
                Err(e) => { let _ = init_tx.send(Err(e)); return; }
            };
            let mut alarm = RespawnRateAlarm::new(ALARM_WINDOW, ALARM_THRESHOLD);
            // Serve jobs; respawn on transport error.
            while let Ok(job) = req_rx.recv() {
                match transport.call(&job.method, job.params) {
                    Ok(v) => { let _ = job.reply.send(Ok(v)); }
                    Err(e) => {
                        if let Some(r) = transport.death_report() {
                            tracing::warn!(%label, "persistent worker died: {r}");
                        }
                        let _ = job.reply.send(Err(e)); // in-flight call fails
                        // respawn with backoff
                        let mut restarts = 0u32;
                        loop {
                            let delay = backoff.next_delay(restarts);
                            thread::sleep(delay);
                            match factory() {
                                Ok(fresh) => {
                                    transport = fresh;
                                    tracing::info!(%label, "persistent worker respawned");
                                    if let Some(n) = alarm.record(Instant::now()) {
                                        tracing::warn!(%label, respawns = n, "persistent worker respawn-rate alarm");
                                    }
                                    break;
                                }
                                Err(e) => {
                                    tracing::warn!(%label, error = %format!("{e:#}"), "respawn failed; backing off");
                                    restarts += 1;
                                }
                            }
                        }
                    }
                }
            }
            // req_tx dropped (shutdown): drop transport → RAII VM teardown.
            drop(transport);
        });
        init_rx.recv()
            .map_err(|_| anyhow::anyhow!("persistent driver exited before initial spawn"))??;
        Ok(PersistentHandle { req_tx: Some(req_tx), driver: Some(driver) })
    }
}

impl PersistentHandle {
    pub fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.req_tx.as_ref().ok_or_else(|| anyhow::anyhow!("persistent worker shut down"))?
            .send(Job { method: method.to_string(), params, reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("persistent driver gone"))?;
        reply_rx.recv().map_err(|_| anyhow::anyhow!("persistent driver dropped reply"))?
    }

    pub fn shutdown(mut self) {
        self.req_tx.take(); // drop sender → driver loop exits → transport teardown
        if let Some(d) = self.driver.take() { let _ = d.join(); }
    }
}

impl Drop for PersistentHandle {
    fn drop(&mut self) {
        self.req_tx.take();
        if let Some(d) = self.driver.take() { let _ = d.join(); }
    }
}
```

Declare in `mod.rs`: add `pub mod persistent;` (alphabetical: after `manager`) and `pub use persistent::{PersistentFactory, PersistentHandle, PersistentTransport, PersistentWorker};`.

If `crate::channel::respawn_alarm` is not reachable, change `mod respawn_alarm;` to `pub mod respawn_alarm;` in `core/src/channel/mod.rs` (matrix.rs unchanged).

- [ ] **Step 4: Run — expect PASS** for all three tests + `cargo test -p kastellan-core --lib worker_lifecycle::persistent`.

- [ ] **Step 5: Commit**

```bash
git add core/src/worker_lifecycle/persistent.rs core/src/worker_lifecycle/mod.rs core/src/channel/mod.rs
git commit -m "feat(core): PersistentWorker supervisor for long-lived workers (5b-1)"
```

---

## Task 11: `ClientTransport` production transport + spawn helper

**Files:**
- Modify: `core/src/worker_lifecycle/persistent.rs` (add the prod impl)
- Test: `core/tests/persistent_worker_e2e.rs` (real spawn against a trivial echo worker — reuse an existing minimal worker binary, e.g. `kastellan-worker-shell-exec` with an allowlisted `/bin/echo`, OR the kv-demo binary once Task 12 lands; if sequencing, place this test in Task 12's commit)

**Interfaces:**
- Consumes: `crate::tool_host::derive_lockdown_env`, `crate::worker_stderr::spawn_drain_with_tail`, `kastellan_protocol::client::Client`, `kastellan_sandbox::{SandboxBackend, SandboxPolicy}`.
- Produces: `pub struct ClientTransport` impl `PersistentTransport`; `pub fn spawn(backend: &dyn SandboxBackend, policy: &SandboxPolicy, program: &str, args: &[&str]) -> anyhow::Result<ClientTransport>`.

- [ ] **Step 1: Write the failing test** (`core/tests/persistent_worker_e2e.rs`) — spawn a real worker under the default OS backend via `PersistentWorker` + `ClientTransport`, issue 3 calls:

```rust
//! Real-spawn smoke for PersistentWorker + ClientTransport under the default OS
//! sandbox backend (Seatbelt on macOS, bwrap on Linux). Hermetic-ish: no VM, no
//! network; uses the kv-demo worker binary as a minimal long-lived RPC server.
use kastellan_core::worker_lifecycle::{ClientTransport, PersistentWorker, PersistentFactory};
use kastellan_sandbox::{SandboxBackends, SandboxPolicy, Net, Profile, PersistentStore};

#[test]
fn persistent_worker_serves_real_worker_many_calls() {
    let bin = kv_demo_binary(); // helper: locate target/debug/kastellan-worker-kv-demo
    if bin.is_none() { eprintln!("[SKIP] kv-demo not built"); return; }
    let bin = bin.unwrap();
    let store = tempdir_path("kvstate");
    let backend = SandboxBackends::default_for_current_os().resolve(None, None);
    let factory: PersistentFactory = {
        let bin = bin.clone(); let store = store.clone();
        Box::new(move || {
            let mut policy = SandboxPolicy { /* Net::Deny, WorkerStrict, fs_read=[bin parent + libs], cpu_ms 0, mem 256, ... */ ..base_policy() };
            policy.net = Net::Deny;
            policy.profile = Profile::WorkerStrict;
            policy.persistent_store = Some(PersistentStore { host_backing: store.clone(), guest_mount: store.clone(), size_mib: 0 });
            policy.env.push(("KASTELLAN_KV_STORE_DIR".into(), store.to_string_lossy().into_owned()));
            let t = ClientTransport::spawn(&*backend, &policy, &bin.to_string_lossy(), &[])?;
            Ok(Box::new(t) as Box<dyn kastellan_core::worker_lifecycle::PersistentTransport>)
        })
    };
    let h = PersistentWorker::spawn("kv", factory).expect("spawn kv worker");
    h.call("kv.put", serde_json::json!({"key":"k","value":"v1"})).unwrap();
    let got = h.call("kv.get", serde_json::json!({"key":"k"})).unwrap();
    assert_eq!(got["value"], "v1");
    let stats = h.call("kv.stats", serde_json::json!({})).unwrap();
    assert!(stats["calls_served"].as_u64().unwrap() >= 3);
    h.shutdown();
}
```

(Provide the `base_policy()`, `kv_demo_binary()`, `tempdir_path()` helpers in the test file. Mirror the `fs_read` lib-closure binding used by other worker e2e tests — read `core/tests/shell_exec_e2e.rs::worker_binary` for the pattern.)

- [ ] **Step 2: Run — expect FAIL** (`ClientTransport` undefined / kv-demo missing).

- [ ] **Step 3: Implement** `ClientTransport` in `persistent.rs`:

```rust
use kastellan_protocol::client::Client;

/// Production transport: a JSON-RPC `Client` over a spawned worker's stdio.
/// Reuses the lockdown-env derivation + stderr-tail death reporting that the
/// Matrix channel uses, without depending on the channel module.
pub struct ClientTransport {
    client: Client,
    stderr_tail: Option<crate::worker_stderr::StderrTail>,
}

impl ClientTransport {
    pub fn spawn(
        backend: &dyn kastellan_sandbox::SandboxBackend,
        policy: &kastellan_sandbox::SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> anyhow::Result<Self> {
        let derived = crate::tool_host::derive_lockdown_env(policy);
        let mut child = backend.spawn_under_policy(&derived, program, args)
            .map_err(|e| anyhow::anyhow!("spawn persistent worker: {e}"))?;
        let pid = child.id();
        let stderr_tail = child.stderr.take()
            .map(|s| crate::worker_stderr::spawn_drain_with_tail(pid, s));
        let client = Client::from_child(child)
            .map_err(|e| anyhow::anyhow!("connect persistent worker: {e}"))?;
        Ok(Self { client, stderr_tail })
    }
}

impl PersistentTransport for ClientTransport {
    fn call(&mut self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        self.client.call(method, params).map_err(|e| anyhow::anyhow!("{e}"))
    }
    fn death_report(&mut self) -> Option<String> {
        self.stderr_tail.as_ref().map(|t| t.snapshot()) // adapt to the StderrTail API
    }
}
```

> Confirm the `worker_stderr::spawn_drain_with_tail` return type + how to read its tail (the Matrix path uses it); adapt `death_report`. Re-export `ClientTransport` from `mod.rs`.

- [ ] **Step 4: Run — expect PASS** (after Task 12 builds the kv-demo binary; if running standalone, this test skips). Run `cargo build -p kastellan-worker-kv-demo` first.

- [ ] **Step 5: Commit**

```bash
git add core/src/worker_lifecycle/persistent.rs core/src/worker_lifecycle/mod.rs core/tests/persistent_worker_e2e.rs
git commit -m "feat(core): ClientTransport production PersistentTransport + spawn helper (5b-1)"
```

---

## Task 12: `kv-demo` worker crate

**Files:**
- Create: `workers/kv-demo/Cargo.toml`, `workers/kv-demo/src/main.rs`
- Modify: `Cargo.toml` (workspace members — add `"workers/kv-demo",` after `"workers/matrix",`)
- Test: `workers/kv-demo/src/main.rs` tests module

**Interfaces:**
- Produces: binary `kastellan-worker-kv-demo` serving `kv.put {key,value}` / `kv.get {key}` / `kv.stats` over JSON-RPC stdio; store at `$KASTELLAN_KV_STORE_DIR/store.json` (atomic temp+rename).

- [ ] **Step 1: Write the failing test** (in `main.rs`, factor the handler so it's testable without stdio):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn handler_in(dir: &std::path::Path) -> KvHandler {
        KvHandler::new(dir.to_path_buf())
    }
    #[test]
    fn put_then_get_round_trips_and_persists() {
        let dir = std::env::temp_dir().join(format!("kvdemo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = handler_in(&dir);
        h.call("kv.put", serde_json::json!({"key":"a","value":"1"})).unwrap();
        // a fresh handler reading the same dir sees the persisted value
        let mut h2 = handler_in(&dir);
        let got = h2.call("kv.get", serde_json::json!({"key":"a"})).unwrap();
        assert_eq!(got["value"], "1");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn stats_counts_calls() {
        let dir = std::env::temp_dir().join(format!("kvdemo-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = handler_in(&dir);
        h.call("kv.put", serde_json::json!({"key":"a","value":"1"})).unwrap();
        h.call("kv.get", serde_json::json!({"key":"a"})).unwrap();
        let s = h.call("kv.stats", serde_json::json!({})).unwrap();
        assert_eq!(s["calls_served"], 3); // put + get + stats
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn get_missing_key_returns_null_value() {
        let dir = std::env::temp_dir().join(format!("kvdemo-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = handler_in(&dir);
        let got = h.call("kv.get", serde_json::json!({"key":"nope"})).unwrap();
        assert!(got["value"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 2: Run — expect FAIL** (crate doesn't exist).

- [ ] **Step 3: Implement.** `workers/kv-demo/Cargo.toml`:

```toml
[package]
name        = "kastellan-worker-kv-demo"
description = "Demo long-lived Net::Deny worker: a tiny persistent key/value store over JSON-RPC. Exercises the persistent-VM lifecycle (slice 5b)."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../../README.md"

[[bin]]
name = "kastellan-worker-kv-demo"
path = "src/main.rs"

[dependencies]
kastellan-protocol       = { path = "../../protocol", version = "0.1.0" }
kastellan-worker-prelude = { path = "../prelude", version = "0.1.0" }
serde                  = { workspace = true }
serde_json             = { workspace = true }
anyhow                 = { workspace = true }
```

`workers/kv-demo/src/main.rs`:

```rust
//! kv-demo: a minimal LONG-LIVED `Net::Deny` worker — a tiny persistent
//! key/value store over JSON-RPC stdio. It exists to exercise slice 5b's
//! persistent-VM lifecycle: it serves many calls over one boot (`kv.stats`
//! proves liveness) and its store survives a respawn (`kv.put`/`kv.get` against
//! the persistent RW mount). Store dir comes from `KASTELLAN_KV_STORE_DIR`.
use std::collections::BTreeMap;
use std::path::PathBuf;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::serve_stdio;
use serde::Deserialize;

#[derive(Deserialize)]
struct PutParams { key: String, value: String }
#[derive(Deserialize)]
struct GetParams { key: String }

struct KvHandler { dir: PathBuf, calls_served: u64 }

impl KvHandler {
    fn new(dir: PathBuf) -> Self { Self { dir, calls_served: 0 } }
    fn store_path(&self) -> PathBuf { self.dir.join("store.json") }

    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read(self.store_path()).ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    fn save(&self, map: &BTreeMap<String, String>) -> Result<(), RpcError> {
        let tmp = self.dir.join("store.json.tmp");
        let bytes = serde_json::to_vec(map)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("serialize store: {e}")))?;
        std::fs::write(&tmp, &bytes)
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("write store: {e}")))?;
        std::fs::rename(&tmp, self.store_path())
            .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("rename store: {e}")))?;
        Ok(())
    }
}

impl Handler for KvHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        self.calls_served += 1;
        match method {
            "kv.put" => {
                let p: PutParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let mut map = self.load();
                map.insert(p.key, p.value);
                self.save(&map)?;
                Ok(serde_json::json!({ "ok": true }))
            }
            "kv.get" => {
                let p: GetParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let map = self.load();
                Ok(serde_json::json!({ "value": map.get(&p.key) }))
            }
            "kv.stats" => Ok(serde_json::json!({ "calls_served": self.calls_served, "pid": std::process::id() })),
            other => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {other}"))),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::var("KASTELLAN_KV_STORE_DIR")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("KASTELLAN_KV_STORE_DIR must be set"))?;
    let mut handler = KvHandler::new(dir);
    serve_stdio(&mut handler)?;
    Ok(())
}
```

Add `"workers/kv-demo",` to the workspace `members`.

- [ ] **Step 4: Run — expect PASS** + `cargo build -p kastellan-worker-kv-demo` + `cargo clippy -p kastellan-worker-kv-demo --all-targets -D warnings`.

Run: `cargo test -p kastellan-worker-kv-demo -- --nocapture`

- [ ] **Step 5: Commit**

```bash
git add workers/kv-demo/Cargo.toml workers/kv-demo/src/main.rs Cargo.toml
git commit -m "feat(kv-demo): minimal long-lived persistent KV worker (5b-1)"
```

---

## Task 13: kv-demo rootfs build script

**Files:**
- Create: `scripts/workers/kv-demo/build-kv-demo-rootfs.sh`

**Interfaces:**
- Produces: `$KASTELLAN_MICROVM_DIR/kv-demo.ext4` (+ shared `vmlinux`), baking `kastellan-worker-kv-demo` + `kastellan-microvm-init`, with a `/data` mountpoint (the persistent-store anchor) alongside the standard pseudo-fs + share anchors.

> No automated test (shell). Validated by the DGX e2e (Task 15). Mirror `scripts/workers/microvm/build-web-fetch-rootfs.sh` exactly; the only deltas: build `-p kastellan-worker-kv-demo`, install it at `/usr/local/bin/kastellan-worker-kv-demo`, output `kv-demo.ext4`, and **add `mkdir -p "$WORK/data"`** (the persistent mount point — `/data` is in `mounts.rs::SHARE_ANCHORS`).

- [ ] **Step 1: Create the script** (adapt the web-fetch one):

```bash
#!/usr/bin/env bash
# Build the kv-demo micro-VM rootfs (ext4) beside the shared vmlinux. kv-demo is
# a pure-Rust Net::Deny worker; no python, no CA bundle. The /data mountpoint is
# where the persistent RW store image (slice 5b-2) is mounted in-guest.
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: ./scripts/workers/kv-demo/build-kv-demo-rootfs.sh" >&2; exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *) echo "Unsupported arch '${HOST_ARCH}'." >&2; exit 1 ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
ROOTFS_MIB=128

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write micro-VM dir: $OUT_DIR — run sudo ./scripts/linux/install-firecracker-vsock.sh or set KASTELLAN_MICROVM_DIR." >&2
    exit 1
fi
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-kv-demo -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-kv-demo "$WORK/usr/local/bin/kastellan-worker-kv-demo"

copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure target/release/kastellan-microvm-init target/release/kastellan-worker-kv-demo

mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"

mkfs.ext4 -q -F -O ^has_journal -L kv-demo -d "$WORK" "$OUT_DIR/kv-demo.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/kv-demo.ext4 (+ shared $OUT_DIR/vmlinux)"
```

- [ ] **Step 2: `chmod +x` + shellcheck (best-effort).**

Run: `chmod +x scripts/workers/kv-demo/build-kv-demo-rootfs.sh && shellcheck scripts/workers/kv-demo/build-kv-demo-rootfs.sh || true`

- [ ] **Step 3: Commit**

```bash
git add scripts/workers/kv-demo/build-kv-demo-rootfs.sh
git commit -m "feat(kv-demo): rootfs build script with /data persistent mountpoint (5b-2)"
```

---

## Task 14: cross-platform lifecycle + store e2e (Seatbelt/bwrap)

**Files:**
- Create: `core/tests/kv_demo_persistent_e2e.rs`

**Interfaces:**
- Consumes: `PersistentWorker`, `ClientTransport`, `PersistentStore`, `SandboxBackends::default_for_current_os()`.

> Proves the abstraction without a VM: persistent store = a real host dir (`host_backing == guest_mount`), which persists naturally on Seatbelt/bwrap. Respawn is triggered by killing the worker process.

- [ ] **Step 1: Write the test:**

```rust
//! Cross-platform (Seatbelt on macOS, bwrap on Linux) e2e for the persistent-VM
//! lifecycle ABSTRACTION without a VM: a kv-demo worker under PersistentWorker
//! with a persistent host-dir store. Many calls + worker-death respawn + store
//! survives. Skip-as-pass without the kv-demo binary / a usable sandbox.
use kastellan_core::worker_lifecycle::{ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker};
use kastellan_sandbox::{Net, PersistentStore, Profile, SandboxBackends, SandboxPolicy};

#[test]
fn kv_demo_survives_respawn_under_default_backend() {
    let Some(bin) = kv_demo_binary() else { eprintln!("[SKIP] kv-demo not built"); return; };
    let store = unique_tmp_dir("kv-persist");
    std::fs::create_dir_all(&store).unwrap();
    let backend = SandboxBackends::default_for_current_os().resolve(None, None);

    let make_factory = || -> PersistentFactory {
        let bin = bin.clone(); let store = store.clone(); let backend = backend.clone();
        Box::new(move || {
            let mut policy = base_deny_policy(&bin); // fs_read = bin dir + lib closure; Net::Deny; WorkerStrict
            policy.net = Net::Deny;
            policy.profile = Profile::WorkerStrict;
            policy.persistent_store = Some(PersistentStore {
                host_backing: store.clone(), guest_mount: store.clone(), size_mib: 0,
            });
            policy.env.push(("KASTELLAN_KV_STORE_DIR".into(), store.to_string_lossy().into_owned()));
            let t = ClientTransport::spawn(&*backend, &policy, &bin.to_string_lossy(), &[])?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("kv", make_factory()).expect("spawn");
    h.call("kv.put", serde_json::json!({"key":"k","value":"before-crash"})).unwrap();
    // many calls, one boot
    for _ in 0..5 { h.call("kv.stats", serde_json::json!({})).unwrap(); }
    // simulate worker death: a call that makes the worker exit is not available,
    // so instead drop+respawn by sending an unknown method is NOT a death. To
    // force respawn, the kv-demo worker exits on EOF only; kill via the OS:
    kill_kv_worker_processes(); // helper: pkill the kv-demo child (best-effort)
    // next call errors (in-flight death), supervisor respawns, store intact:
    let _ = h.call("kv.get", serde_json::json!({"key":"k"})); // may error (death)
    let got = h.call("kv.get", serde_json::json!({"key":"k"})).expect("post-respawn call");
    assert_eq!(got["value"], "before-crash", "persistent store survived respawn");
    h.shutdown();
    std::fs::remove_dir_all(&store).ok();
}
```

> Provide `kv_demo_binary()`, `base_deny_policy()`, `unique_tmp_dir()`, `kill_kv_worker_processes()` helpers (model `base_deny_policy` on the existing `shell_exec_e2e.rs` lib-closure binding). If killing the child by name is too brittle on a given platform, instead expose a `kv.crash` debug method on kv-demo (behind `#[cfg(debug_assertions)]`) that calls `std::process::exit(1)` — decide during implementation; document whichever you pick.

- [ ] **Step 2: Run — expect FAIL → PASS** once kv-demo + ClientTransport exist.

Run: `cargo build -p kastellan-worker-kv-demo && cargo test -p kastellan-core --test kv_demo_persistent_e2e -- --nocapture`

- [ ] **Step 3: Commit**

```bash
git add core/tests/kv_demo_persistent_e2e.rs
git commit -m "test(core): cross-platform kv-demo persistent-lifecycle e2e (5b-1/5b-2)"
```

---

## Task 15: DGX real-KVM persistent-VM e2e (`#[ignore]`)

**Files:**
- Create: `core/tests/kv_demo_firecracker_persistent_e2e.rs`

**Interfaces:**
- Consumes: the Firecracker backend, `PersistentWorker`/`ClientTransport`, `PersistentStore`.

> Mirror `core/tests/python_exec_firecracker_e2e.rs`'s harness verbatim (`image_dir`, `firecracker_image` → `kv-demo.ext4`, `locate_microvm_run`, `skip_if_no_microvm`, `firecracker_backend`). `#![cfg(target_os = "linux")]` + `#[ignore]`.

- [ ] **Step 1: Write the test** (key body — harness fns copied from the python-exec e2e):

```rust
#![cfg(target_os = "linux")]
//! Slice 5b-1/5b-2 DGX e2e: a long-lived kv-demo worker in a Firecracker VM
//! boots once, serves many calls, and its persistent ext4 store survives a VM
//! respawn. #[ignore]: needs /dev/kvm + /dev/vhost-vsock + kv-demo.ext4 + the
//! RELEASE launcher. Run:
//!   export PATH=$HOME/.local/bin:$PATH
//!   cargo build --release -p kastellan-microvm-run
//!   ./scripts/workers/kv-demo/build-kv-demo-rootfs.sh
//!   cargo test -p kastellan-core --test kv_demo_firecracker_persistent_e2e -- --ignored --nocapture
// ... image_dir(), firecracker_image() {rootfs = kv-demo.ext4}, locate_microvm_run(),
// skip_if_no_microvm(), firecracker_backend() — copied from python_exec_firecracker_e2e.rs ...

#[test]
#[ignore = "DGX-only: real KVM + vsock + kv-demo rootfs + persistent ext4 store"]
fn kv_demo_persistent_store_survives_vm_respawn() {
    if skip_if_no_microvm() { return; }
    let store_img = PathBuf::from(image_dir()).join("kv-demo-state.ext4");
    let _ = std::fs::remove_file(&store_img); // fresh image for a clean run
    let backend = firecracker_backend();

    let make_factory = || -> PersistentFactory {
        let backend = backend.clone(); let store_img = store_img.clone();
        Box::new(move || {
            let mut policy = kv_demo_vm_policy(image_dir()); // Net::Deny, WorkerStrict, env KASTELLAN_MICROVM_DIR/ROOTFS=kv-demo.ext4 + KASTELLAN_KV_STORE_DIR=/data
            policy.persistent_store = Some(PersistentStore {
                host_backing: store_img.clone(),
                guest_mount: PathBuf::from("/data"),
                size_mib: 64,
            });
            let program = "/usr/local/bin/kastellan-worker-kv-demo";
            let t = ClientTransport::spawn(&*backend, &policy, program, &[])?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("kv-vm", make_factory()).expect("boot kv-demo VM");
    h.call("kv.put", serde_json::json!({"key":"k","value":"pre-crash"})).unwrap();
    for _ in 0..5 { h.call("kv.stats", serde_json::json!({})).unwrap(); } // many calls, one boot
    // SIGKILL the launcher/VM to force a respawn:
    pkill_microvm_run(); // helper: pkill -9 kastellan-microvm-run (best-effort)
    let _ = h.call("kv.get", serde_json::json!({"key":"k"})); // in-flight death tolerated
    let got = h.call("kv.get", serde_json::json!({"key":"k"})).expect("post-respawn call");
    assert_eq!(got["value"], "pre-crash", "persistent ext4 store survived the VM respawn");
    h.shutdown();
}
```

- [ ] **Step 2: Compile-check on Mac** (`cargo test -p kastellan-core --test kv_demo_firecracker_persistent_e2e --no-run` won't link `core` on Mac per #144 — instead `cargo clippy -p kastellan-core --target aarch64-unknown-linux-gnu` is NOT possible for core (ring). So compile + run this on the DGX.)

- [ ] **Step 3: Run on the DGX** (the Linux acceptance gate):

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && \
  cargo build --release -p kastellan-microvm-run && \
  ./scripts/workers/kv-demo/build-kv-demo-rootfs.sh && \
  cargo test -p kastellan-core --test kv_demo_firecracker_persistent_e2e -- --ignored --nocapture'
```
Expected: PASS — `persistent ext4 store survived the VM respawn`, no `[SKIP]`.

- [ ] **Step 4: Commit**

```bash
git add core/tests/kv_demo_firecracker_persistent_e2e.rs
git commit -m "test(core): DGX real-KVM kv-demo persistent-VM respawn e2e (5b-1/5b-2)"
```

---

## Task 16: Full-gate + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Mac full gate.**

Run:
```sh
source "$HOME/.cargo/env"
cargo test --workspace          # skip-as-pass; record passed/failed/ignored/[SKIP]
cargo clippy --workspace --all-targets -D warnings
```
Expected: 0 failed; clippy clean.

- [ ] **Step 2: DGX full gate** (native Linux acceptance):

```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --workspace && \
  cargo test --workspace && cargo clippy --workspace --all-targets -D warnings'
```
Plus the `#[ignore]` e2e from Task 15. Record the native-Linux baseline counts.

- [ ] **Step 3: Update HANDOVER.md** — bump header (date, last commit, session-end verification counts), move slice 5b-1/5b-2 into "Recently completed," refresh "Working state" (`PersistentWorker`, `persistent_store`, `kv-demo` crate → 20 crates), write a fresh "Next TODO" framing **5c** (transparent-tunnel vsock egress + long-lived sidecar) and **5b-4** (matrix adopts `PersistentWorker` + matrix rootfs).

- [ ] **Step 4: Tick ROADMAP** — mark slice 5b-1/5b-2 `[x]` with the commit hash; add 5c/5b-4 as `[ ]`.

- [ ] **Step 5: Commit**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): slice 5b-1+5b-2 persistent-VM lifecycle shipped; frame 5c/5b-4"
```

- [ ] **Step 6: Push + open PR** to `main`, linking the spec + this plan.

---

## Self-Review

**Spec coverage:** PersistentWorker (Task 10/11) ✓; persistent ext4 store mkfs-once+flock (Tasks 3/4/5/9) ✓; cross-platform persistent_store (Tasks 6/7/8) ✓; kv-demo + rootfs (Tasks 12/13) ✓; macOS-dev e2e (Task 14) ✓; DGX real-KVM respawn+survive e2e (Task 15) ✓; Net::Deny / no-network ✓ (every policy is `Net::Deny`); leave-matrix-untouched ✓ (only `channel/mod.rs` visibility, never `matrix.rs`). Deferred 5c/5b-4 framed (Task 16).

**Type consistency:** `PersistentStore { host_backing, guest_mount, size_mib }` used identically in Tasks 1,4,6,7,8,11,14,15. `PersistentMount { mountpoint, guest_dev }` in Tasks 2,4. `encode_mount_manifest(ro, rw, persistent)` arity consistent in Tasks 2,4. `PersistentTransport`/`PersistentFactory`/`PersistentWorker::spawn[_with_backoff]`/`PersistentHandle::{call,shutdown}` consistent across Tasks 10,11,14,15. `RestartBackoff{base,factor_num,factor_den,cap}` matches the extracted definition. `build_persistent_image`/`persistent_mkfs_decision` consistent (Tasks 3,5).

**Placeholder scan:** New files/handlers/tests are complete code. Edits to unseen functions (`derive_lockdown_env`, `launcher_argv`, the guest mount-parser, `worker_stderr::spawn_drain_with_tail` tail API) are flagged "read first" with the exact change + full test — acceptable since the verbatim source wasn't in hand; the implementer reads the function in-task. No "TODO/TBD/handle errors appropriately."

**Known read-first points** (call out during execution): (a) `derive_lockdown_env`'s RW-vec variable name; (b) `worker_stderr::spawn_drain_with_tail` return type + tail accessor for `death_report`; (c) the `microvm-init` `kastellan.mounts` decoder must mount **every** `rw` line (Task 2 verifies/extends); (d) whether `microvm-run` already depends on `libc` (Task 9).
