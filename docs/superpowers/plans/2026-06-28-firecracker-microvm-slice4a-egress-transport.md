# Firecracker micro-VM slice 4a — egress vsock reverse-channel — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Forward the host egress-proxy UDS into a Firecracker guest over a second, guest-initiated vsock channel so a force-routed `Net::Allowlist` worker can run in a VM (no virtio-net device) with unchanged worker code; transport only, proven by a real-KVM init self-test.

**Architecture:** Pure plan changes (`plan.rs`) detect force-routing (`Net::Allowlist` + `proxy_uds`), set an egress vsock port, disable the net device, override the guest's proxy-UDS env to an in-guest path, and emit a `kastellan.egress=1` cmdline token. The launcher (`microvm-run`) pre-binds a listener at `<base_uds>_<port>` (firecracker's guest-initiated host path) and relays each connection to the real host proxy UDS. The guest init (`microvm-init`) binds an in-guest UDS, forks a relay child piping it to `AF_VSOCK(host, port)`, then execs the worker; a test-gated self-test does a PING/PONG round-trip.

**Tech Stack:** Rust (std only — no new deps), libc (guest-side vsock/AF_UNIX), Firecracker hybrid vsock, bash (rootfs build).

## Global Constraints

- AGPL-3.0 project; AGPL-compatible deps only. **No new dependencies** — std + the existing `libc` (microvm-init) only.
- Cross-platform discipline: all Firecracker code is `#[cfg(target_os = "linux")]`. The pure parsers/relay helpers must still compile on macOS.
- **Test-exec reality:** `kastellan-sandbox` `linux_firecracker` modules and `microvm-init`'s libc code do NOT run under `cargo test` on macOS. Per-task Mac gate = cross-clippy `cargo clippy -p <crate> --target aarch64-unknown-linux-gnu --all-targets -D warnings` (a compile failure on a missing field/fn IS the RED signal on Mac); the actual unit-test run is on the DGX via `ssh dgx '<cmd>'`. Pure parsers that compile on macOS (`microvm-init` parse fns, `microvm-run` `egress_relay`) run normally with `cargo test` on the Mac.
- DGX SSH form is exactly `ssh dgx '<cmd>'` (the allow-rule is a prefix match; flags before the hostname get denied).
- Shared cross-crate constants (`microvm-init` must not depend on `kastellan-sandbox`) are duplicated with a "keep in sync" comment — the established pattern for `WORKER_VSOCK_PORT`, `ENV_CMDLINE_KEY`, `MOUNTS_CMDLINE_KEY`.
- Stage specific files in every commit (`git add <paths>`), never `git add -A` (an untracked `assets/agent_with_the_keys.png` and `.claude/*` must stay out).
- Shared constant values (copy verbatim): `EGRESS_VSOCK_PORT = 1025`; `GUEST_EGRESS_UDS = "/run/kastellan-egress.sock"`; host CID `VMADDR_CID_HOST = 2`; cmdline tokens `kastellan.egress=1` and `kastellan.egress.selftest=1`; selftest env knob `KASTELLAN_MICROVM_EGRESS_SELFTEST=1`; the guest worker env key overridden in-VM `KASTELLAN_EGRESS_PROXY_UDS`.

---

### Task 1: `plan.rs` — force-routing detection, egress fields, env override, cmdline tokens, fail-closed reject

**Files:**
- Modify: `sandbox/src/linux_firecracker/plan.rs` (constants near line 56; `FirecrackerLaunchPlan` struct ~line 21-45; `build_launch_plan` ~line 134-258; tests `mod tests` ~line 324)
- Modify: `sandbox/src/linux_firecracker.rs:13-16` (re-export `EGRESS_VSOCK_PORT`)

**Interfaces:**
- Consumes: `SandboxPolicy { net: Net, proxy_uds: Option<PathBuf>, env: Vec<(String,String)>, fs_read, fs_write, ... }`; `Net::{Deny, Allowlist(Vec<String>), ProxyEgress}`.
- Produces: two new public fields on `FirecrackerLaunchPlan`:
  - `pub egress_proxy_vsock_port: Option<u32>` — `Some(EGRESS_VSOCK_PORT)` iff force-routed.
  - `pub egress_host_uds: Option<PathBuf>` — `Some(policy.proxy_uds)` iff force-routed.
  - `pub const EGRESS_VSOCK_PORT: u32 = 1025;`
  Force-routing rule: `policy.net == Net::Allowlist(_) && policy.proxy_uds.is_some()`. When force-routed: `net_enabled = false`, the guest `KASTELLAN_EGRESS_PROXY_UDS` env is overridden to `GUEST_EGRESS_UDS`, and `boot_args` gains ` kastellan.egress=1`. `Net::Allowlist` without `proxy_uds` → `Err(SandboxError::Backend(...))`.

- [ ] **Step 1: Add constants + struct fields**

In `sandbox/src/linux_firecracker/plan.rs`, after the `WORKER_VSOCK_PORT` const (line 56), add:

```rust
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
```

In the `FirecrackerLaunchPlan` struct (after `rw_image_path`, line 44), add:

```rust
    /// Slice 4a: the guest-initiated egress vsock port, `Some(EGRESS_VSOCK_PORT)`
    /// iff the worker is force-routed (`Net::Allowlist` + `proxy_uds`). Drives
    /// the ` kastellan.egress=1` cmdline token and the launcher's reverse-relay.
    pub egress_proxy_vsock_port: Option<u32>,
    /// Slice 4a: the **host** egress-proxy UDS (from `policy.proxy_uds`) the
    /// launcher relays the guest's egress connections to. `Some` iff force-routed.
    pub egress_host_uds: Option<std::path::PathBuf>,
```

- [ ] **Step 2: Write the failing tests**

In `sandbox/src/linux_firecracker/plan.rs` `mod tests`, add (and add `use std::path::PathBuf;` is already present):

```rust
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
```

- [ ] **Step 3: Verify RED on Mac (compile-fail = missing fields/logic)**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --tests 2>&1 | tail -20`
Expected: compile errors — the new fields exist (Step 1) but `build_launch_plan` does not yet set them, and the reject/override logic is missing, so `allowlist_without_proxy_uds_is_rejected` / `force_routed_*` assertions reference behavior not yet implemented. (If it compiles, the asserts will only truly fail on the DGX run in Step 6 — the Mac gate is the compile + the DGX run together.)

- [ ] **Step 4: Implement the force-routing logic in `build_launch_plan`**

Replace the single line `let net_enabled = !matches!(policy.net, Net::Deny);` (line 155) with:

```rust
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
```

Then override the guest proxy-UDS env. Replace the `boot_args` block (lines 217-235) so it builds a local `env` first:

```rust
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
    let mut boot_args = BASE_BOOT_ARGS.to_string();
    if let Some(suffix) = encode_env_cmdline(&env)? {
        boot_args.push_str(&suffix);
    }
    if let Some(suffix) = encode_mount_manifest(ro_share.as_ref(), rw_scratch.as_ref())? {
        boot_args.push_str(&suffix);
    }
    if egress_proxy_vsock_port.is_some() {
        boot_args.push_str(" kastellan.egress=1");
        // Test-only: emit the self-test token when the operator/test sets the knob.
        if policy.env.iter().any(|(k, v)| k == "KASTELLAN_MICROVM_EGRESS_SELFTEST" && v == "1") {
            boot_args.push_str(" kastellan.egress.selftest=1");
        }
    }
    if boot_args.len() > MAX_CMDLINE_BYTES {
        return Err(SandboxError::Backend(format!(
            "kernel cmdline {} bytes exceeds {MAX_CMDLINE_BYTES}-byte cap \
             (worker env + mount manifest too large to forward)",
            boot_args.len()
        )));
    }
```

In the returned struct literal, change `env: policy.env.clone(),` to `env,` and add the two new fields:

```rust
        env,
        net_enabled,
        ro_share,
        rw_scratch,
        ro_image_path,
        rw_image_path,
        egress_proxy_vsock_port,
        egress_host_uds,
```

Re-export the constant in `sandbox/src/linux_firecracker.rs` — change the `pub use plan::{ ... WORKER_VSOCK_PORT };` (lines 13-16) to also export `EGRESS_VSOCK_PORT`:

```rust
pub use plan::{
    build_launch_plan, render_firecracker_config, FirecrackerImage, FirecrackerLaunchPlan,
    EGRESS_VSOCK_PORT, WORKER_VSOCK_PORT,
};
```

- [ ] **Step 5: Verify GREEN-compile on Mac**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean (no warnings/errors).

- [ ] **Step 6: Run the unit tests on the DGX**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox --lib linux_firecracker::plan 2>&1 | tail -25'`
Expected: the 5 new tests + the existing plan tests PASS (e.g. `force_routed_sets_egress_port_and_disables_net ... ok`).

- [ ] **Step 7: Commit**

```bash
git add sandbox/src/linux_firecracker/plan.rs sandbox/src/linux_firecracker.rs
git commit -m "feat(microvm): slice 4a plan — force-routing detection + egress vsock port + env override"
```

---

### Task 2: `launcher_argv` — pass `--egress-uds` + `--egress-vsock-port` when force-routed

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs` (`launcher_argv` ~line 42-56; `spawn_tests` ~line 183)

**Interfaces:**
- Consumes: `FirecrackerLaunchPlan { egress_host_uds: Option<PathBuf>, egress_proxy_vsock_port: Option<u32>, .. }` (from Task 1).
- Produces: `launcher_argv` appends `--egress-uds <host_uds>` and `--egress-vsock-port <port>` iff `egress_host_uds.is_some()`. `spawn_under_policy` is **unchanged** (the egress fields ride the plan).

- [ ] **Step 1: Write the failing test**

In `sandbox/src/linux_firecracker.rs` `mod spawn_tests`, add:

```rust
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
```

- [ ] **Step 2: Verify RED on Mac (compile-fail / logic-missing)**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --tests 2>&1 | tail -15`
Expected: compiles, but the egress-flag assertions are not yet satisfiable (no code appends them) — confirmed by the DGX run in Step 5.

- [ ] **Step 3: Implement**

In `launcher_argv` (line 42), change to build a `mut` vec and append the egress flags:

```rust
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
    argv
}
```

- [ ] **Step 4: Verify GREEN-compile on Mac**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Run the unit tests on the DGX**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox --lib spawn_tests 2>&1 | tail -15'`
Expected: `launcher_argv_passes_egress_flags_when_force_routed ... ok` + the two existing `launcher_argv*` tests pass.

- [ ] **Step 6: Commit**

```bash
git add sandbox/src/linux_firecracker.rs
git commit -m "feat(microvm): slice 4a launcher_argv passes --egress-uds/--egress-vsock-port when force-routed"
```

---

### Task 3: `microvm-run` — host-side egress reverse-relay module + main wiring (hermetic, runs on Mac)

**Files:**
- Create: `workers/microvm-run/src/egress_relay.rs`
- Modify: `workers/microvm-run/src/main.rs` (add `mod egress_relay;`; parse the two flags; start the relay before booting firecracker)

**Interfaces:**
- Consumes: the `--egress-uds` / `--egress-vsock-port` launcher args (from Task 2); the base `--vsock-uds` path.
- Produces:
  - `pub fn guest_initiated_uds_path(base_uds: &str, port: u32) -> String` → `format!("{base_uds}_{port}")`.
  - `pub fn parse_egress_relay_args(uds: Option<String>, port: Option<String>) -> Option<(String, u32)>` → `Some((uds, port))` only when both present and the port parses.
  - `pub fn spawn_egress_relay(base_uds: &str, port: u32, proxy_uds: String) -> std::io::Result<String>` → binds `<base>_<port>`, spawns a detached accept loop relaying each connection to `proxy_uds`, returns the bound path.

- [ ] **Step 1: Write the failing tests (run on Mac)**

Create `workers/microvm-run/src/egress_relay.rs` with ONLY the test module + empty signatures first is awkward; instead write the full module (Step 3) but begin by adding this test block at the bottom and confirm it fails to compile until the fns exist. Test block:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn guest_initiated_uds_path_appends_port_suffix() {
        assert_eq!(guest_initiated_uds_path("/run/vsock.sock", 1025), "/run/vsock.sock_1025");
    }

    #[test]
    fn parse_egress_relay_args_requires_both() {
        assert_eq!(
            parse_egress_relay_args(Some("/p.sock".into()), Some("1025".into())),
            Some(("/p.sock".to_string(), 1025))
        );
        assert_eq!(parse_egress_relay_args(None, Some("1025".into())), None);
        assert_eq!(parse_egress_relay_args(Some("/p.sock".into()), None), None);
        assert_eq!(parse_egress_relay_args(Some("/p.sock".into()), Some("nope".into())), None);
    }

    #[test]
    fn relay_pipes_guest_connection_to_proxy_uds_and_back() {
        let dir = std::env::temp_dir().join(format!("kastellan-egressrelay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proxy_path = dir.join("proxy.sock");
        let _ = std::fs::remove_file(&proxy_path);
        // Echo "proxy": read 5 bytes, reply PONG.
        let proxy = UnixListener::bind(&proxy_path).unwrap();
        thread::spawn(move || {
            if let Ok((mut c, _)) = proxy.accept() {
                let mut buf = [0u8; 5];
                if c.read_exact(&mut buf).is_ok() {
                    let _ = c.write_all(b"PONG\n");
                }
            }
        });
        let base = dir.join("vsock.sock");
        let bound = spawn_egress_relay(
            &base.to_string_lossy(),
            1025,
            proxy_path.to_string_lossy().into_owned(),
        )
        .unwrap();
        assert_eq!(bound, format!("{}_1025", base.to_string_lossy()));
        // Simulate firecracker delivering a guest-initiated connection.
        let mut c = UnixStream::connect(&bound).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(b"PING\n").unwrap();
        let mut buf = [0u8; 5];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"PONG\n", "relay forwarded PING to the proxy and PONG back");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p kastellan-microvm-run egress_relay 2>&1 | tail -15`
Expected: FAIL to compile — `guest_initiated_uds_path` / `parse_egress_relay_args` / `spawn_egress_relay` not defined.

- [ ] **Step 3: Implement the module**

At the TOP of `workers/microvm-run/src/egress_relay.rs` (above the test module), add:

```rust
//! Host-side egress reverse-relay (slice 4a). Firecracker delivers a
//! guest-initiated vsock connection on port P to the host UDS `<base>_P`; this
//! module listens there and pipes every such connection to the real host egress
//! proxy UDS, so an in-VM worker reaches the proxy with unchanged code. Detached
//! threads die on launcher exit (VM teardown); the listener socket lives in the
//! run-dir, so the launcher's RAII teardown reclaims it.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

/// Host-side path firecracker connects to for a guest-initiated vsock connection
/// on `port`: the base UDS with a `_<port>` suffix.
pub fn guest_initiated_uds_path(base_uds: &str, port: u32) -> String {
    format!("{base_uds}_{port}")
}

/// Parse the optional egress reverse-relay args; `Some((proxy_uds, port))` only
/// when both `--egress-uds` and a parseable `--egress-vsock-port` are present.
pub fn parse_egress_relay_args(uds: Option<String>, port: Option<String>) -> Option<(String, u32)> {
    let uds = uds?;
    let port = port?.parse().ok()?;
    Some((uds, port))
}

/// Bind the reverse-relay listener at `<base_uds>_<port>` and spawn a detached
/// accept loop that pipes each accepted connection to `proxy_uds`. Returns the
/// bound path.
pub fn spawn_egress_relay(
    base_uds: &str,
    port: u32,
    proxy_uds: String,
) -> std::io::Result<String> {
    let path = guest_initiated_uds_path(base_uds, port);
    let _ = std::fs::remove_file(&path); // clear a stale socket so bind() succeeds
    let listener = UnixListener::bind(&path)?;
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(guest) = conn else { continue };
            let proxy_uds = proxy_uds.clone();
            thread::spawn(move || match UnixStream::connect(&proxy_uds) {
                Ok(proxy) => relay_bidirectional(guest, proxy),
                Err(e) => eprintln!("microvm-run egress: dial proxy {proxy_uds} failed: {e}"),
            });
        }
    });
    Ok(path)
}

/// Pipe bytes both directions between two connected streams until either closes.
fn relay_bidirectional(left: UnixStream, right: UnixStream) {
    let (Ok(left_rd), Ok(right_rd)) = (left.try_clone(), right.try_clone()) else {
        return;
    };
    let up = thread::spawn(move || pipe(left_rd, right)); // left -> right
    pipe(right_rd, left); // right -> left
    let _ = up.join();
}

/// One-direction byte copy with per-chunk flush; shuts the writer down on EOF.
fn pipe(mut src: UnixStream, mut dst: UnixStream) {
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = dst.shutdown(Shutdown::Write);
                break;
            }
            Ok(n) => {
                if dst.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = dst.flush();
            }
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kastellan-microvm-run egress_relay 2>&1 | tail -15`
Expected: 3 tests pass.

- [ ] **Step 5: Wire into `main.rs`**

In `workers/microvm-run/src/main.rs`, add `mod egress_relay;` next to `mod boot;` / `mod bridge;` (line 5-6). Then, in `main()`, after parsing `run_dir` (line 31) and BEFORE the `boot::firecracker_argv` spawn (line 36), add:

```rust
    // Slice 4a: when force-routed, start the egress reverse-relay BEFORE booting
    // firecracker so the host listener at `<vsock_uds>_<port>` exists before the
    // guest can dial it (firecracker connects there for a guest-initiated vsock
    // connection on that port). The detached accept loop relays each connection
    // to the host egress-proxy UDS.
    if let Some((proxy_uds, egress_port)) =
        egress_relay::parse_egress_relay_args(arg("--egress-uds"), arg("--egress-vsock-port"))
    {
        egress_relay::spawn_egress_relay(&vsock_uds, egress_port, proxy_uds)?;
    }
```

- [ ] **Step 6: Verify build + Linux cross-compile**

Run: `cargo build -p kastellan-microvm-run && cargo clippy -p kastellan-microvm-run --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 7: Commit**

```bash
git add workers/microvm-run/src/egress_relay.rs workers/microvm-run/src/main.rs
git commit -m "feat(microvm): slice 4a launcher egress reverse-relay (host UDS <- guest vsock)"
```

---

### Task 4: `microvm-init` — egress cmdline parser + in-guest relay + self-test + `/run` mountpoint

**Files:**
- Modify: `workers/microvm-init/src/main.rs` (constants ~line 18-31; pure parser + `EgressConfig` near the other parsers ~line 147; Linux libc relay/self-test after `apply_host_mounts` ~line 253; wire into `main` ~line 337; tests ~line 369)
- Modify: `scripts/workers/microvm/build-rootfs.sh` (pre-create the `/run` mountpoint, ~line 88)

**Interfaces:**
- Consumes: the `kastellan.egress=1` / `kastellan.egress.selftest=1` cmdline tokens (Task 1) and `EGRESS_VSOCK_PORT`/`GUEST_EGRESS_UDS`/`VMADDR_CID_HOST` (duplicated constants).
- Produces:
  - `fn parse_egress_config(cmdline: &str) -> EgressConfig` where `struct EgressConfig { enabled: bool, selftest: bool }` (pure; runs on macOS).
  - Linux-only: `setup_egress_relay()` (mount `/run` tmpfs, bind `GUEST_EGRESS_UDS`, fork a relay child piping it to `AF_VSOCK(VMADDR_CID_HOST, EGRESS_VSOCK_PORT)`); `egress_selftest()` (PING→PONG round-trip, logs `EGRESS_CHANNEL_OK`).

- [ ] **Step 1: Add constants + the pure parser + failing tests**

In `workers/microvm-init/src/main.rs`, after the `MOUNTS_CMDLINE_KEY` const (line 89), add:

```rust
/// Egress vsock port (slice 4a). Shared with
/// `kastellan-sandbox::linux_firecracker::plan::EGRESS_VSOCK_PORT` (kept in sync
/// manually; this crate must not depend on the sandbox crate).
#[allow(dead_code)]
const EGRESS_VSOCK_PORT: u32 = 1025;
/// In-guest UDS the worker dials and the relay binds. Shared with the sandbox
/// crate's `GUEST_EGRESS_UDS`.
#[allow(dead_code)]
const GUEST_EGRESS_UDS: &str = "/run/kastellan-egress.sock";
/// The host's vsock CID from inside the guest (mirrors `libc::VMADDR_CID_HOST`).
/// Plain literal so the parser/tests compile on macOS without the libc item.
#[allow(dead_code)]
const VMADDR_CID_HOST: u32 = 2;

/// Egress channel config parsed from the kernel cmdline (slice 4a). Pure.
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
struct EgressConfig {
    enabled: bool,
    selftest: bool,
}

/// Parse the egress tokens out of the kernel cmdline. `enabled` from
/// `kastellan.egress=1`, `selftest` from `kastellan.egress.selftest=1`. Pure →
/// unit-testable on any platform.
#[allow(dead_code)]
fn parse_egress_config(cmdline: &str) -> EgressConfig {
    let mut c = EgressConfig::default();
    for t in cmdline.split_whitespace() {
        match t {
            "kastellan.egress=1" => c.enabled = true,
            "kastellan.egress.selftest=1" => c.selftest = true,
            _ => {}
        }
    }
    c
}
```

In `mod tests`, add:

```rust
    #[test]
    fn parse_egress_config_reads_tokens() {
        assert_eq!(parse_egress_config("console=ttyS0 panic=1"), EgressConfig::default());
        assert_eq!(
            parse_egress_config("console=ttyS0 kastellan.egress=1"),
            EgressConfig { enabled: true, selftest: false }
        );
        assert_eq!(
            parse_egress_config("kastellan.egress=1 kastellan.egress.selftest=1"),
            EgressConfig { enabled: true, selftest: true }
        );
    }
```

- [ ] **Step 2: Run the pure-parser test on Mac to verify pass (it has no impl gap once Step 1 lands)**

Run: `cargo test -p kastellan-microvm-init parse_egress_config 2>&1 | tail -10`
Expected: `parse_egress_config_reads_tokens ... ok` (this step also confirms the new constants/struct compile on macOS).

- [ ] **Step 3: Add the Linux-only relay + self-test**

In `workers/microvm-init/src/main.rs`, after `apply_host_mounts` (line 253), add (all `#[cfg(target_os = "linux")]`):

```rust
/// Slice 4a: stand up the in-guest egress relay. Mount a writable `/run` tmpfs,
/// bind the in-guest UDS the worker dials, and fork a child that pipes every
/// accepted UDS connection to the host over `AF_VSOCK(VMADDR_CID_HOST,
/// EGRESS_VSOCK_PORT)` (firecracker forwards that to the launcher's reverse-relay
/// listener at `<base>_<port>`, which dials the real host egress proxy). Bind
/// happens in the parent BEFORE `exec`, so the worker can never dial before the
/// listener exists. Best-effort: a failure logs and returns (the worker then
/// fails its first dial, surfaced as a normal error — PID1 is never aborted).
#[cfg(target_os = "linux")]
fn setup_egress_relay() {
    // `/run` must be a writable tmpfs (the rootfs is a read-only superblock).
    let _ = std::fs::create_dir_all("/run");
    if let (Ok(src), Ok(tgt), Ok(fst)) = (
        std::ffi::CString::new("tmpfs"),
        std::ffi::CString::new("/run"),
        std::ffi::CString::new("tmpfs"),
    ) {
        unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), fst.as_ptr(), 0, std::ptr::null()) };
    }
    let listener = match bind_unix_listener(GUEST_EGRESS_UDS) {
        Some(fd) => fd,
        None => {
            eprintln!("microvm-init: egress UDS bind failed; worker egress disabled");
            return;
        }
    };
    // SAFETY: single-threaded PID1 here; fork is safe (no other threads to race).
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        egress_relay_loop(listener); // never returns
        unsafe { libc::_exit(0) };
    }
    // Parent: drop its copy of the listener fd so the exec'd worker can't inherit
    // a stray listening fd (#361 hygiene); the child owns the accept loop.
    unsafe { libc::close(listener) };
}

/// Bind an AF_UNIX SOCK_STREAM listener at `path`. Returns the listening fd or
/// `None` on any failure. Unlinks a stale socket first.
#[cfg(target_os = "linux")]
fn bind_unix_listener(path: &str) -> Option<RawFd> {
    let _ = std::fs::remove_file(path);
    let cpath = std::ffi::CString::new(path).ok()?;
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        let bytes = cpath.as_bytes_with_nul();
        if bytes.len() > addr.sun_path.len() {
            libc::close(fd);
            return None;
        }
        for (dst, &b) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = b as libc::c_char;
        }
        let alen = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, alen) != 0
            || libc::listen(fd, 8) != 0
        {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Connect an AF_VSOCK SOCK_STREAM to `(cid, port)`. Returns the connected fd.
#[cfg(target_os = "linux")]
fn connect_host_vsock(cid: u32, port: u32) -> Option<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as _;
        addr.svm_cid = cid;
        addr.svm_port = port;
        let alen = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, alen) != 0 {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Connect an AF_UNIX SOCK_STREAM to `path` (the self-test client side).
#[cfg(target_os = "linux")]
fn connect_unix(path: &str) -> Option<RawFd> {
    let cpath = std::ffi::CString::new(path).ok()?;
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        let bytes = cpath.as_bytes_with_nul();
        if bytes.len() > addr.sun_path.len() {
            libc::close(fd);
            return None;
        }
        for (dst, &b) in addr.sun_path.iter_mut().zip(bytes) {
            *dst = b as libc::c_char;
        }
        let alen = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, alen) != 0 {
            libc::close(fd);
            return None;
        }
        Some(fd)
    }
}

/// Accept loop for the in-guest relay child: each UDS connection gets its own
/// vsock connection to the host and a bidirectional byte pump.
#[cfg(target_os = "linux")]
fn egress_relay_loop(listener: RawFd) {
    loop {
        let conn = unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn < 0 {
            continue;
        }
        match connect_host_vsock(VMADDR_CID_HOST, EGRESS_VSOCK_PORT) {
            Some(vfd) => {
                // conn/vfd are RawFd (Copy); both directions run concurrently on
                // the same full-duplex sockets.
                let up = std::thread::spawn(move || pump_raw(conn, vfd));
                pump_raw(vfd, conn);
                let _ = up.join();
                unsafe {
                    libc::close(conn);
                    libc::close(vfd);
                }
            }
            None => unsafe {
                libc::close(conn);
            },
        }
    }
}

/// One-direction raw-fd byte copy until EOF/err; half-closes the writer on EOF.
#[cfg(target_os = "linux")]
fn pump_raw(from_fd: RawFd, to_fd: RawFd) {
    let mut buf = [0u8; 8192];
    loop {
        let n = unsafe { libc::read(from_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            unsafe { libc::shutdown(to_fd, libc::SHUT_WR) };
            break;
        }
        let mut off = 0isize;
        while off < n {
            let w = unsafe {
                libc::write(
                    to_fd,
                    buf.as_ptr().offset(off) as *const libc::c_void,
                    (n - off) as usize,
                )
            };
            if w <= 0 {
                return;
            }
            off += w;
        }
    }
}

/// Slice 4a self-test: connect our own in-guest UDS, write `PING`, expect `PONG`.
/// Proves the full guest→host reverse path on real KVM. Logs `EGRESS_CHANNEL_OK`
/// to the kernel console on success. Best-effort; never aborts PID1.
#[cfg(target_os = "linux")]
fn egress_selftest() {
    let Some(fd) = connect_unix(GUEST_EGRESS_UDS) else {
        eprintln!("microvm-init: egress selftest connect failed");
        return;
    };
    let ping = b"PING\n";
    unsafe { libc::write(fd, ping.as_ptr() as *const libc::c_void, ping.len()) };
    let mut buf = [0u8; 16];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    unsafe { libc::close(fd) };
    if n >= 4 && &buf[..4] == b"PONG" {
        eprintln!("EGRESS_CHANNEL_OK");
    } else {
        eprintln!("microvm-init: egress selftest got no PONG (n={n})");
    }
}
```

- [ ] **Step 4: Wire into `main`**

In the Linux `main()` (line 337), after the `apply_host_mounts(...)` line (340) and before `accept_host_bridge()` (341), add:

```rust
    let egress = parse_egress_config(&cmdline_for_mounts);
    if egress.enabled {
        setup_egress_relay();
        if egress.selftest {
            egress_selftest();
        }
    }
```

- [ ] **Step 5: Pre-create the `/run` mountpoint in the rootfs**

In `scripts/workers/microvm/build-rootfs.sh`, find the pseudo-fs mountpoint section ("3d. Pseudo-fs mountpoints") near line 88 and add `/run` alongside the existing `mkdir -p` of `/proc /sys /tmp`. If the line creates them like `mkdir -p "$WORK"/{proc,sys,tmp}`, change to `mkdir -p "$WORK"/{proc,sys,tmp,run}`; otherwise add a sibling line:

```sh
mkdir -p "$WORK/run"   # slice 4a: egress relay tmpfs mountpoint (in-guest UDS lives here)
```

- [ ] **Step 6: Verify Mac compile (stub path) + Linux cross-compile**

Run: `cargo build -p kastellan-microvm-init && cargo clippy -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: macOS build green (the Linux libc fns are cfg'd out; the stub `main` stands); Linux cross-clippy clean (the libc relay compiles).

- [ ] **Step 7: Run the pure-parser test on Mac**

Run: `cargo test -p kastellan-microvm-init 2>&1 | tail -15`
Expected: all microvm-init pure tests pass (incl. `parse_egress_config_reads_tokens`).

- [ ] **Step 8: Commit**

```bash
git add workers/microvm-init/src/main.rs scripts/workers/microvm/build-rootfs.sh
git commit -m "feat(microvm): slice 4a guest egress relay + self-test + /run mountpoint"
```

---

### Task 5: DGX e2e — real-KVM guest→host egress reverse-channel proof

**Files:**
- Create: `core/tests/firecracker_egress_channel_e2e.rs` (DGX-only, `#[ignore]`; mirrors `core/tests/python_exec_firecracker_hostdir_e2e.rs`'s harness)

**Interfaces:**
- Consumes: `LinuxFirecracker`, `firecracker_mode_entry`, `spawn_worker`, the `skip_if_no_microvm`/`image_dir`/`locate_microvm_run` harness pattern.
- Produces: a `#[ignore]` test that spawns a force-routed VM with the self-test knob and asserts a host echo UnixListener (the `proxy_uds` target) receives `PING\n` from the guest.

- [ ] **Step 1: Write the e2e**

Create `core/tests/firecracker_egress_channel_e2e.rs`:

```rust
#![cfg(target_os = "linux")]
//! Slice 4a e2e: proves the guest-initiated vsock egress reverse-channel on real
//! KVM. A force-routed VM (Net::Allowlist + proxy_uds) boots with the self-test
//! knob; the guest init dials the in-guest egress UDS, which relays over a second
//! vsock port to the launcher's reverse-relay and on to a host echo UnixListener
//! standing in for the egress proxy. We assert the host echo RECEIVES the guest's
//! PING — the novel guest→host direction, observed entirely host-side.
//!
//! DGX-only / #[ignore]: needs /dev/kvm + /dev/vhost-vsock + a built rootfs
//! (REBUILD via build-rootfs.sh so it carries the /run mountpoint) + the
//! kastellan-microvm-run RELEASE launcher (rebuild it; target/release is
//! preferred and a stale one silently shadows source changes). Run:
//!
//!     export PATH=$HOME/.local/bin:$PATH   # firecracker is off the ssh PATH
//!     cargo build --release -p kastellan-microvm-run
//!     cargo test -p kastellan-core --test firecracker_egress_channel_e2e -- --ignored --nocapture

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use kastellan_core::tool_host::{spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{Net, SandboxBackend, SandboxBackendKind, SandboxBackends};

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage { kernel_path: dir.join("vmlinux"), rootfs_path: dir.join("python-exec.ext4") }
}

fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .join("target");
    for profile in ["release", "debug"] {
        let p = target.join(profile).join("kastellan-microvm-run");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed: {e}\n");
        return true;
    }
    match locate_microvm_run() {
        Some(bin) => {
            use std::sync::Once;
            static PATH_ONCE: Once = Once::new();
            PATH_ONCE.call_once(|| {
                let dir = bin.parent().unwrap().to_path_buf();
                let cur = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![dir];
                paths.extend(std::env::split_paths(&cur));
                let joined = std::env::join_paths(paths).expect("join PATH");
                std::env::set_var("PATH", joined);
            });
            false
        }
        None => {
            eprintln!("\n[SKIP] kastellan-microvm-run not built; run `cargo build -p kastellan-microvm-run`\n");
            true
        }
    }
}

fn firecracker_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(Some(SandboxBackendKind::FirecrackerVm), None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "DGX-only: real KVM + vsock + rootfs with /run mountpoint"]
async fn egress_reverse_channel_delivers_guest_ping_to_host_proxy_uds() {
    if skip_if_no_microvm() {
        return;
    }

    // Host echo "proxy": the proxy_uds target. On accept, read PING and reply PONG,
    // signalling receipt back to the test thread.
    let dir = std::env::temp_dir().join(format!("kastellan-s4a-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let echo_path = dir.join("egress.sock");
    let _ = std::fs::remove_file(&echo_path);
    let listener = UnixListener::bind(&echo_path).unwrap();
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        if let Ok((mut c, _)) = listener.accept() {
            let mut buf = [0u8; 5];
            if c.read_exact(&mut buf).is_ok() {
                let _ = tx.send(buf.to_vec());
                let _ = c.write_all(b"PONG\n");
            }
        }
    });

    // Force-routed entry: python-exec rootfs, but Net::Allowlist + proxy_uds +
    // the self-test knob. The worker process is irrelevant here — the init's
    // self-test originates the PING during boot.
    let mut entry = firecracker_mode_entry(
        PathBuf::from("/usr/local/bin/kastellan-worker-python-exec"),
        image_dir(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
    entry.policy.net = Net::Allowlist(vec!["example.com:443".into()]);
    entry.policy.proxy_uds = Some(echo_path.clone());
    entry.policy.env.push(("KASTELLAN_MICROVM_EGRESS_SELFTEST".into(), "1".into()));

    let backend = firecracker_backend();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &entry.policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn force-routed worker in micro-VM");

    let got = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("host proxy UDS never received the guest PING (reverse channel broken)");
    assert_eq!(&got, b"PING\n", "guest-initiated egress reached the host proxy UDS");

    let _ = worker.close();
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Cross-check the imports compile path (Mac best-effort)**

Run: `cargo clippy -p kastellan-core --tests 2>&1 | tail -20` (NOTE: `kastellan-core` does not cross-compile to Linux on the Mac due to the `ring` C dep, and this file is `#![cfg(target_os = "linux")]` so it is empty on macOS — this step only confirms the workspace still builds on the Mac.)
Expected: clean on macOS (the test file compiles to nothing under `cfg(target_os="linux")`).

- [ ] **Step 3: Rebuild the rootfs + release launcher on the DGX, then run the e2e**

Run:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && bash scripts/workers/microvm/build-rootfs.sh && cargo build --release -p kastellan-microvm-run'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo test -p kastellan-core --test firecracker_egress_channel_e2e -- --ignored --nocapture 2>&1 | tail -30'
```
Expected: `egress_reverse_channel_delivers_guest_ping_to_host_proxy_uds ... ok`; `--nocapture` shows `EGRESS_CHANNEL_OK` from the guest init. NOT a `[SKIP]` line (a skip means firecracker/KVM/launcher wasn't found — fix PATH / rebuild and re-run).

- [ ] **Step 4: Commit**

```bash
git add core/tests/firecracker_egress_channel_e2e.rs
git commit -m "test(microvm): slice 4a DGX e2e — guest-initiated egress reverse-channel reaches host proxy UDS"
```

---

### Task 6: No-regression sweep, docs, and finalize

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (new "Last updated" header entry + Next-TODO update: slice 4a done, 4b next)
- Modify: `docs/devel/ROADMAP.md` (slice-4 row → split 4a done / 4b pending)

- [ ] **Step 1: Mac full-workspace gate**

Run: `cargo build --workspace && cargo test -p kastellan-microvm-run && cargo test -p kastellan-microvm-init && cargo clippy --workspace --all-targets -- -D warnings`
Expected: build green; microvm-run + microvm-init tests pass; workspace clippy clean.

- [ ] **Step 2: Mac cross-clippy for the Linux-gated crates**

Run: `cargo clippy -p kastellan-sandbox -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean (the slice-4a `linux_firecracker` + microvm-init libc code compiles for aarch64-linux).

- [ ] **Step 3: DGX no-regression — sandbox lib + slice-1/2/3 e2e**

Run:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox --lib 2>&1 | tail -8'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo test -p kastellan-core --test python_exec_firecracker_hostdir_e2e -- --ignored --nocapture 2>&1 | tail -8'
```
Expected: sandbox lib all-pass (slice-4a plan/launcher units included); the slice-3 host-dir e2e still passes (no regression); confirm no leftover `/tmp/kastellan-microvm-*` dirs (`ssh dgx 'ls -d /tmp/kastellan-microvm-* 2>/dev/null | wc -l'` → `0`).

- [ ] **Step 4: Update HANDOVER.md + ROADMAP.md**

Add a new "Last updated" header entry to `docs/devel/handovers/HANDOVER.md` summarizing slice 4a (what shipped: the egress vsock reverse-channel transport — plan force-routing detection + env override + cmdline token, launcher reverse-relay, guest in-guest-UDS↔vsock relay + self-test; the real-KVM e2e proof; constants `EGRESS_VSOCK_PORT=1025` / `GUEST_EGRESS_UDS=/run/kastellan-egress.sock`; test counts; **4b is next** = first real net-worker rootfs (web-fetch) + CA-into-guest via slice-3 RO-share + full fetch-through-proxy e2e). Update the "Next TODO" so slice 4a moves to "recently completed" and 4b is the leading micro-VM pick. In `docs/devel/ROADMAP.md`, split the slice-4 row into `[x] 4a transport` / `[ ] 4b first net-worker consumer`. Keep both docs concise (prune per the session-end checklist).

- [ ] **Step 5: Commit the docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(microvm): slice 4a done (egress vsock transport); 4b (first net worker) next"
```

- [ ] **Step 6: Push + open PR**

```bash
git push -u origin feat/firecracker-microvm-slice4a-egress-transport
gh pr create --base main --title "feat(microvm): Firecracker slice 4a — egress-proxy vsock reverse-channel transport" --body "<summary + DGX verification evidence; link the design spec>"
```

---

## Self-Review

**Spec coverage:**
- Plan field `egress_proxy_vsock_port` + force-routing detection + `net_enabled=false` → Task 1 ✓
- Fail-closed reject of bare `Net::Allowlist` in a VM → Task 1 (`allowlist_without_proxy_uds_is_rejected`) ✓
- Backend-local UDS translation (guest env override + `--egress-uds` launcher arg) → Task 1 (env override) + Task 2 (launcher arg) ✓
- `kastellan.egress` / `kastellan.egress.selftest` cmdline tokens → Task 1 (emit) + Task 4 (parse) ✓
- Launcher reverse-relay (pre-bind `<base>_<port>`, forward to host proxy UDS) → Task 3 ✓
- Guest in-guest-UDS↔vsock relay (bind-before-exec, fork relay child) → Task 4 ✓
- Self-test PING/PONG → Task 4 (`egress_selftest`) + Task 5 (assert host echo got PING) ✓
- `/run` tmpfs + rootfs mountpoint → Task 4 (Steps 3, 5) ✓
- Host-side hermetic tests (run on Mac) → Task 3 (egress_relay) + Task 4 (parse_egress_config) ✓
- Real-KVM proof of the guest-initiated direction → Task 5 ✓
- 4b deferrals (web-fetch rootfs, CA-into-guest, full fetch e2e) → out of scope, recorded in Task 6 docs ✓

**Placeholder scan:** PR body in Task 6 Step 6 is the one `<...>` — intentional (the author fills in DGX evidence at PR time). All code steps contain complete code.

**Type consistency:** `egress_proxy_vsock_port: Option<u32>` / `egress_host_uds: Option<PathBuf>` (Task 1) consumed unchanged in Task 2 `launcher_argv`. `EGRESS_VSOCK_PORT` = `1025` and `GUEST_EGRESS_UDS` = `/run/kastellan-egress.sock` identical in sandbox (Task 1) and microvm-init (Task 4). `parse_egress_relay_args`/`spawn_egress_relay`/`guest_initiated_uds_path` defined and consumed in Task 3. `parse_egress_config` → `EgressConfig{enabled,selftest}` defined in Task 4 Step 1, consumed in Task 4 Step 4. The self-test contract (`PING\n` → `PONG\n`, marker `EGRESS_CHANNEL_OK`) is consistent across Task 4 (`egress_selftest`) and Task 5 (host echo).
