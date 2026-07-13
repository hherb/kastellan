# web-research VM × embed-broker (second vsock channel) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a `USE_MICROVM` web-research worker reach the host-side embed-broker over a second vsock port (1026), so a VM worker gets hybrid ranking against a local/loopback embed backend with zero embed egress.

**Architecture:** Clone the proven slice-4a egress vsock channel (port 1025) as a parallel broker channel (port 1026) over the same single hybrid-vsock device. Core's broker chokepoint (`spawn_broker` + `rewrite_policy_for_broker`) and the worker binary are unchanged; the new plumbing lives in the sandbox FC plan, the launcher, the guest init, and the `web_research.rs` resolver. The FC plan rewrites the worker's broker-UDS env from the host path to the in-guest path via a kind-agnostic value-match.

**Tech Stack:** Rust (workspace crates `kastellan-sandbox`, `kastellan-microvm-run`, `kastellan-microvm-init`, `kastellan-core`), Firecracker hybrid-vsock, AF_UNIX/AF_VSOCK relays.

**Spec:** `docs/superpowers/specs/2026-07-13-web-research-vm-broker-embed-channel-design.md`

## Global Constraints

- **Approach A** (clone the egress channel), **not** a generic multi-channel abstraction. Kind-agnostic **value-match** env override (rewrite the env entry whose *value* equals `policy.broker_uds`), never a hardcoded broker-kind env key in the sandbox crate.
- **Cross-platform parity + platform gating.** `sandbox/src/linux_firecracker/**` and `microvm-init/src/guest/**` are `#[cfg(target_os = "linux")]`; `core`'s web-research VM code is `#[cfg(target_os = "linux")]`.
- **Verification reality (this box is macOS):**
  - `kastellan-sandbox` + `kastellan-microvm-init` are pure Rust → **cross-clippy** on the Mac: `cargo clippy -p <crate> --target aarch64-unknown-linux-gnu --all-targets -- -D warnings` (run `rustup target add aarch64-unknown-linux-gnu` once). This compiles + lints the `cfg(linux)` code but does **not** run its tests.
  - `kastellan-microvm-run` is cross-platform std → **builds + tests natively on the Mac**.
  - `kastellan-microvm-init` `cmdline.rs` is cross-platform → its unit tests **run on the Mac** (`cargo test -p kastellan-microvm-init`).
  - `kastellan-core`'s `#[cfg(target_os="linux")]` code **cannot be compiled on the Mac** (the #144 `ring` cross wall); it is first compiled + tested on the **DGX**.
- **DGX driving:** exactly `ssh dgx '<cmd>'` (the allow-rule is a prefix match; flags before the hostname get denied). Long runs: `setsid bash -lc '… > ~/log 2>&1' </dev/null &` then poll; write logs to `~`, never `/tmp` (scrubbed mid-run). FC e2e needs `export PATH=$HOME/.local/bin:$PATH`.
- **No worker-binary change**, **no rootfs-script change** (the `/run` tmpfs already exists; rebuild is needed only because `microvm-init` changed).
- **Commit style:** end messages with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Stage specific files (never `git add -A`).
- Files should stay under 500 LOC where feasible; `plan.rs` is already ~1061 (a known test-lift candidate) — keep new code minimal, do not split in this PR.

---

### Task 1: microvm-init — broker cmdline constants + parser

**Files:**
- Modify: `workers/microvm-init/src/cmdline.rs` (add constants + `BrokerConfig` + `parse_broker_config` near the egress equivalents at lines ~158-194, and tests in the file's existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub(crate) const BROKER_VSOCK_PORT: u32 = 1026;`, `pub(crate) const GUEST_BROKER_UDS: &str = "/run/kastellan-broker.sock";`, `pub(crate) struct BrokerConfig { pub(crate) enabled: bool }`, `pub(crate) fn parse_broker_config(cmdline: &str) -> BrokerConfig`.

- [ ] **Step 1: Write the failing tests**

Add to `workers/microvm-init/src/cmdline.rs` inside `mod tests`:

```rust
#[test]
fn parse_broker_config_enabled_from_token() {
    let c = parse_broker_config("console=ttyS0 kastellan.broker=1 kastellan.egress=1");
    assert!(c.enabled, "kastellan.broker=1 must enable the broker channel");
}

#[test]
fn parse_broker_config_disabled_when_token_absent() {
    let c = parse_broker_config("console=ttyS0 kastellan.egress=1");
    assert!(!c.enabled, "no kastellan.broker token => disabled");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kastellan-microvm-init parse_broker_config`
Expected: FAIL — `cannot find function parse_broker_config`.

- [ ] **Step 3: Add the constants + type + parser**

In `workers/microvm-init/src/cmdline.rs`, after the egress block (after line ~170, following `VMADDR_CID_HOST`):

```rust
/// Broker vsock port (VM × broker). Shared with the sandbox crate's
/// `BROKER_VSOCK_PORT` (kept in sync manually; this crate must not depend on the
/// sandbox crate). Distinct from the egress port so both channels coexist on the
/// one vsock device.
#[allow(dead_code)]
pub(crate) const BROKER_VSOCK_PORT: u32 = 1026;
/// In-guest UDS the worker dials for its broker and the relay binds. One generic
/// path suffices (a worker binds at most one broker socket). Shared with the
/// sandbox crate's `GUEST_BROKER_UDS`.
#[allow(dead_code)]
pub(crate) const GUEST_BROKER_UDS: &str = "/run/kastellan-broker.sock";

/// Broker channel config parsed from the kernel cmdline (VM × broker). Pure →
/// unit-testable on any platform.
#[allow(dead_code)]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct BrokerConfig {
    pub(crate) enabled: bool,
}

/// Parse the broker token out of the kernel cmdline: `enabled` from
/// `kastellan.broker=1`. Pure.
#[allow(dead_code)]
pub(crate) fn parse_broker_config(cmdline: &str) -> BrokerConfig {
    let mut c = BrokerConfig::default();
    for t in cmdline.split_whitespace() {
        if t == "kastellan.broker=1" {
            c.enabled = true;
        }
    }
    c
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p kastellan-microvm-init parse_broker_config`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add workers/microvm-init/src/cmdline.rs
git commit -m "feat(microvm-init): broker cmdline constants + parse_broker_config

VM x embed-broker: the guest learns to stand up a broker relay from a
kastellan.broker=1 cmdline token. Constants mirror the sandbox crate's
BROKER_VSOCK_PORT (1026) / GUEST_BROKER_UDS (kept in sync manually).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: microvm-init guest — generalize the reverse relay + add the broker relay

**Files:**
- Modify: `workers/microvm-init/src/guest/egress.rs` (split the `/run` mount out of `setup_egress_relay`; parameterize the relay by guest-UDS + vsock-port)
- Modify: `workers/microvm-init/src/main.rs` (parse the broker config; mount `/run` once; set up both relays)

**Interfaces:**
- Consumes: `BROKER_VSOCK_PORT`, `GUEST_BROKER_UDS`, `parse_broker_config` (Task 1); existing `EGRESS_VSOCK_PORT`, `GUEST_EGRESS_UDS`, `VMADDR_CID_HOST`, `parse_egress_config`, `egress_selftest`.
- Produces: `pub(crate) fn mount_run_tmpfs()`, `pub(crate) fn setup_relay(guest_uds: &str, vsock_port: u32)` (replacing `setup_egress_relay`).

> **Why the split:** the `/run` tmpfs mount currently lives inside `setup_egress_relay`. Calling relay setup twice (egress + broker) would stack a **second** tmpfs over `/run`, hiding the first relay's bound socket. `/run` must be mounted exactly once before either socket is bound. This is a VM-only bug that only surfaces on real KVM — the plan removes it by construction. No unit test can catch it on the Mac (guest code is `cfg(linux)`); the DGX e2e (Task 6) is the gate.

- [ ] **Step 1: Split `/run` mount out + parameterize the relay** in `workers/microvm-init/src/guest/egress.rs`

Replace `setup_egress_relay` (lines ~27-58) and the two relay helpers with:

```rust
/// Mount a writable `/run` tmpfs (the rootfs is a read-only superblock).
/// Call EXACTLY ONCE before binding any relay UDS: mounting tmpfs on `/run`
/// twice stacks a second tmpfs over the first and hides the earlier socket.
/// Best-effort; a mount failure logs and the first UDS bind then fails loudly.
pub(crate) fn mount_run_tmpfs() {
    let _ = std::fs::create_dir_all("/run");
    if let (Ok(src), Ok(tgt), Ok(fst)) = (
        std::ffi::CString::new("tmpfs"),
        std::ffi::CString::new("/run"),
        std::ffi::CString::new("tmpfs"),
    ) {
        unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), fst.as_ptr(), 0, std::ptr::null()) };
    }
}

/// Stand up one in-guest reverse relay: bind the in-guest UDS the worker dials
/// (`guest_uds`) and fork a child that pipes every accepted UDS connection to the
/// host over `AF_VSOCK(VMADDR_CID_HOST, vsock_port)` (firecracker forwards that to
/// the launcher's reverse-relay listener at `<base>_<vsock_port>`). Bind happens
/// in the parent BEFORE `exec`, so the worker can never dial before the listener
/// exists. Requires `/run` already mounted (see [`mount_run_tmpfs`]). Best-effort:
/// a failure logs and returns (the worker then fails its first dial as a normal
/// error — PID1 is never aborted). Generic over the port so egress (1025) and the
/// embed broker (1026) share one implementation.
pub(crate) fn setup_relay(guest_uds: &str, vsock_port: u32) {
    let listener = match bind_unix_listener(guest_uds) {
        Some(fd) => fd,
        None => {
            eprintln!("microvm-init: relay UDS bind failed for {guest_uds}; channel disabled");
            return;
        }
    };
    // SAFETY: single-threaded PID1 here; fork is safe (no other threads to race).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("microvm-init: fork for relay {guest_uds} failed; channel disabled");
        unsafe { libc::close(listener) };
        return;
    }
    if pid == 0 {
        relay_loop(listener, vsock_port); // never returns
        unsafe { libc::_exit(0) };
    }
    // Parent: drop its copy of the listener fd so the exec'd worker can't inherit
    // a stray listening fd (#361 hygiene); the child owns the accept loop.
    unsafe { libc::close(listener) };
}
```

Rename `egress_relay_loop` → `relay_loop` and thread the port through to `relay_one_connection`:

```rust
/// Accept loop for an in-guest relay child: each UDS connection gets its own
/// vsock connection to the host on `vsock_port` and a bidirectional byte pump.
fn relay_loop(listener: RawFd, vsock_port: u32) {
    loop {
        let conn = unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
        if conn < 0 {
            let err = unsafe { *libc::__errno_location() };
            if err == libc::EINTR {
                continue;
            }
            eprintln!("microvm-init: relay accept failed (errno {err}); relay exiting");
            break;
        }
        std::thread::spawn(move || relay_one_connection(conn, vsock_port));
    }
}

/// Pump one accepted in-guest UDS connection to the host over vsock and back.
/// Takes ownership of `conn` (closes it on return).
fn relay_one_connection(conn: RawFd, vsock_port: u32) {
    match connect_host_vsock(VMADDR_CID_HOST, vsock_port) {
        Some(vfd) => {
            let up = std::thread::spawn(move || pump_raw(conn, vfd));
            pump_raw(vfd, conn);
            unsafe {
                libc::shutdown(conn, libc::SHUT_RDWR);
                libc::shutdown(vfd, libc::SHUT_RDWR);
            }
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
```

(Leave `bind_unix_listener`, `connect_host_vsock`, `connect_unix`, `pump_raw`, and `egress_selftest` unchanged. `egress_selftest` still uses `GUEST_EGRESS_UDS`.)

- [ ] **Step 2: Wire both relays in `workers/microvm-init/src/main.rs`**

Update the guest `use` (lines ~33-36) — replace `setup_egress_relay` with `mount_run_tmpfs, setup_relay`, and import `parse_broker_config` + the broker consts:

```rust
#[cfg(target_os = "linux")]
use cmdline::{
    parse_broker_config, parse_egress_config, parse_mount_manifest, BROKER_VSOCK_PORT,
    EGRESS_VSOCK_PORT, GUEST_BROKER_UDS, GUEST_EGRESS_UDS,
};
#[cfg(target_os = "linux")]
use guest::{
    accept_host_bridge, apply_host_mounts, bring_loopback_up, egress_selftest, exec_worker,
    mount_pseudo_fs, mount_run_tmpfs, setup_relay,
};
```

Replace the egress block (lines ~48-54) with:

```rust
let egress = parse_egress_config(&cmdline_for_mounts);
let broker = parse_broker_config(&cmdline_for_mounts);
// `/run` must be a writable tmpfs before ANY relay binds its UDS there — mount
// it exactly once (a second tmpfs mount would hide the first relay's socket).
if egress.enabled || broker.enabled {
    mount_run_tmpfs();
}
if egress.enabled {
    setup_relay(GUEST_EGRESS_UDS, EGRESS_VSOCK_PORT);
    if egress.selftest {
        egress_selftest();
    }
}
if broker.enabled {
    setup_relay(GUEST_BROKER_UDS, BROKER_VSOCK_PORT);
}
```

- [ ] **Step 3: Cross-clippy on the Mac (compile + lint the cfg(linux) guest code)**

Run: `rustup target add aarch64-unknown-linux-gnu 2>/dev/null; cargo clippy -p kastellan-microvm-init --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean (no warnings/errors). This is the only Mac-side gate for the guest relay; behaviour is verified on the DGX in Task 6.

- [ ] **Step 4: Native Mac build/test still green (cmdline + macOS stub main compile)**

Run: `cargo test -p kastellan-microvm-init`
Expected: PASS (Task 1's cmdline tests + existing cmdline tests; the macOS `fn main` stub compiles).

- [ ] **Step 5: Commit**

```bash
git add workers/microvm-init/src/guest/egress.rs workers/microvm-init/src/main.rs
git commit -m "feat(microvm-init): generalize the reverse relay; add the broker channel

Split the /run tmpfs mount out of setup_egress_relay into mount_run_tmpfs
(called once) and parameterize relay setup by (guest_uds, vsock_port) so
egress (1025) and the embed broker (1026) share one relay implementation.
main.rs mounts /run once, then stands up each enabled channel.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: microvm-run — second reverse-relay for the broker port

**Files:**
- Modify: `workers/microvm-run/src/main.rs` (parse `--broker-uds`/`--broker-vsock-port`; spawn a second relay before boot)
- Modify: `workers/microvm-run/src/egress_relay.rs` (add a unit test proving two channels bind distinct suffix paths)

**Interfaces:**
- Consumes: existing `egress_relay::parse_egress_relay_args(Option<String>, Option<String>) -> Option<(String, u32)>` and `egress_relay::spawn_egress_relay(base_uds: &str, port: u32, target_uds: String) -> io::Result<String>` (both already channel-neutral — reuse as-is).

- [ ] **Step 1: Write the failing test** in `workers/microvm-run/src/egress_relay.rs` `mod tests`

```rust
#[test]
fn two_relays_bind_distinct_suffix_paths() {
    // Egress (1025) and broker (1026) reverse-relays share the vsock base UDS but
    // must bind DISTINCT host listener paths (`<base>_<port>`), so neither hides
    // the other. Proves the generic relay supports a second channel.
    let dir = std::env::temp_dir().join(format!("kastellan-tworelay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("vsock.sock");
    let egress_target = dir.join("egress-proxy.sock");
    let broker_target = dir.join("broker.sock");
    let e = spawn_egress_relay(&base.to_string_lossy(), 1025, egress_target.to_string_lossy().into_owned()).unwrap();
    let b = spawn_egress_relay(&base.to_string_lossy(), 1026, broker_target.to_string_lossy().into_owned()).unwrap();
    assert_eq!(e, format!("{}_1025", base.to_string_lossy()));
    assert_eq!(b, format!("{}_1026", base.to_string_lossy()));
    assert_ne!(e, b, "the two channels must bind distinct listener paths");
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run to verify it passes immediately** (this pins existing generic behaviour — no code change yet)

Run: `cargo test -p kastellan-microvm-run two_relays_bind_distinct_suffix_paths`
Expected: PASS (the relay is already generic; this is a contract-lock guarding the reuse Task 3 depends on).

- [ ] **Step 3: Wire the broker relay in `workers/microvm-run/src/main.rs`**

After the egress-relay block (lines ~62-66), add:

```rust
    // VM × broker: start a SECOND reverse-relay for the embed-broker channel
    // (port 1026), forwarding guest-initiated connections to the host broker UDS.
    // Same generic relay as egress; started before boot so its listener exists
    // before the guest dials. Independent of egress (different port + target).
    if let Some((broker_uds, broker_port)) =
        egress_relay::parse_egress_relay_args(arg("--broker-uds"), arg("--broker-vsock-port"))
    {
        egress_relay::spawn_egress_relay(&vsock_uds, broker_port, broker_uds)?;
    }
```

- [ ] **Step 4: Build + full crate tests on the Mac**

Run: `cargo test -p kastellan-microvm-run && cargo clippy -p kastellan-microvm-run --all-targets -- -D warnings`
Expected: PASS + clean (native Mac build; this crate is cross-platform std).

- [ ] **Step 5: Commit**

```bash
git add workers/microvm-run/src/main.rs workers/microvm-run/src/egress_relay.rs
git commit -m "feat(microvm-run): second reverse-relay for the broker vsock channel

Parse --broker-uds/--broker-vsock-port and spawn a second generic reverse-relay
(port 1026 -> host broker UDS) before booting firecracker, alongside the egress
relay. + a test pinning that two channels bind distinct <base>_<port> paths.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: sandbox FC — broker channel in the launch plan, launcher argv, and VMM jail

**Files:**
- Modify: `sandbox/src/linux_firecracker/plan.rs` (constants, plan fields, detection, value-match env override, cmdline token, plan construction, unit tests)
- Modify: `sandbox/src/linux_firecracker.rs` (`launcher_argv` broker pair)
- Modify: `sandbox/src/linux_firecracker/confine.rs` (`build_vmm_jail_argv` broker-UDS bind)

**Interfaces:**
- Consumes: `SandboxPolicy::broker_uds: Option<PathBuf>` (already exists), `SandboxPolicy::proxy_uds`.
- Produces: `pub const BROKER_VSOCK_PORT: u32 = 1026;`; `FirecrackerLaunchPlan { broker_vsock_port: Option<u32>, broker_host_uds: Option<PathBuf>, .. }`.

> **All of Task 4 is `#[cfg(target_os="linux")]`.** Mac gate = cross-clippy only; the unit tests run on the DGX (Task 7).

- [ ] **Step 1: Write the failing unit tests** in `sandbox/src/linux_firecracker/plan.rs` `mod tests`

```rust
#[test]
fn broker_uds_sets_broker_channel_fields_and_cmdline() {
    let mut policy = net_client_allowlist_policy(); // helper: Net::Allowlist(["searx.example.org:443"]) + proxy_uds set
    policy.broker_uds = Some(PathBuf::from("/tmp/embed-77-0/embed.sock"));
    policy.env = vec![
        ("KASTELLAN_EMBED_BROKER_UDS".into(), "/tmp/embed-77-0/embed.sock".into()),
        ("KASTELLAN_WEB_RESEARCH_EMBED_MODEL".into(), "embeddinggemma".into()),
    ];
    let plan = build_launch_plan(&policy, &test_image(), "/usr/local/bin/w", &[]).unwrap();
    // Channel fields set from broker_uds.
    assert_eq!(plan.broker_vsock_port, Some(BROKER_VSOCK_PORT));
    assert_eq!(plan.broker_host_uds, Some(PathBuf::from("/tmp/embed-77-0/embed.sock")));
    // Cmdline token present.
    assert!(plan.boot_args.contains(" kastellan.broker=1"), "boot_args: {}", plan.boot_args);
    // Value-match env override -> the worker sees the GUEST path, not the host path.
    let uds = plan.env.iter().find(|(k, _)| k == "KASTELLAN_EMBED_BROKER_UDS").map(|(_, v)| v.as_str());
    assert_eq!(uds, Some("/run/kastellan-broker.sock"));
    // The unrelated env var is untouched.
    assert!(plan.env.iter().any(|(k, v)| k == "KASTELLAN_WEB_RESEARCH_EMBED_MODEL" && v == "embeddinggemma"));
}

#[test]
fn no_broker_uds_leaves_broker_channel_unset() {
    let policy = net_client_allowlist_policy(); // no broker_uds
    let plan = build_launch_plan(&policy, &test_image(), "/usr/local/bin/w", &[]).unwrap();
    assert_eq!(plan.broker_vsock_port, None);
    assert_eq!(plan.broker_host_uds, None);
    assert!(!plan.boot_args.contains("kastellan.broker"));
}

#[test]
fn broker_uds_does_not_change_net_or_nic_decision() {
    // broker_uds must not flip net_enabled or the egress fields (orthogonal channel).
    let mut with = net_client_allowlist_policy();
    with.broker_uds = Some(PathBuf::from("/tmp/embed-77-0/embed.sock"));
    let plan = build_launch_plan(&with, &test_image(), "/usr/local/bin/w", &[]).unwrap();
    assert!(!plan.net_enabled, "force-routed net worker still has no NIC");
    assert_eq!(plan.egress_proxy_vsock_port, Some(EGRESS_VSOCK_PORT));
}
```

(If `net_client_allowlist_policy()`/`test_image()` helpers don't already exist in the test module, add small ones alongside the existing egress plan tests, matching the pattern at `plan.rs:~940`.)

- [ ] **Step 2: Add constants** in `plan.rs` after `EGRESS_VSOCK_PORT`/`GUEST_EGRESS_UDS` (line ~83)

```rust
/// Fixed vsock port for the guest→host embed-broker channel (VM × broker). A
/// broker-backed VM worker (`policy.broker_uds` set) reaches its host-side broker
/// over this THIRD guest-initiated vsock port; egress keeps `EGRESS_VSOCK_PORT`
/// (1025) and the JSON-RPC bridge keeps `WORKER_VSOCK_PORT` (1024). Shared with
/// `kastellan-microvm-init` (manual cross-crate contract, same as the others).
pub const BROKER_VSOCK_PORT: u32 = 1026;
/// In-guest path the worker dials for its broker and the init binds the broker
/// relay listener at. One generic path suffices (a worker binds at most one broker
/// socket). Shared with `kastellan-microvm-init`.
const GUEST_BROKER_UDS: &str = "/run/kastellan-broker.sock";
```

- [ ] **Step 3: Add plan fields** in `FirecrackerLaunchPlan` after `egress_host_uds` (line ~51)

```rust
    /// VM × broker: the guest-initiated broker vsock port, `Some(BROKER_VSOCK_PORT)`
    /// iff the worker declares a broker (`policy.broker_uds` set). Drives the
    /// ` kastellan.broker=1` cmdline token and the launcher's second reverse-relay.
    pub broker_vsock_port: Option<u32>,
    /// VM × broker: the **host** broker UDS (from `policy.broker_uds`) the launcher
    /// relays the guest's broker connections to. `Some` iff broker-backed.
    pub broker_host_uds: Option<std::path::PathBuf>,
```

- [ ] **Step 4: Detection** in `build_launch_plan`, right after the egress `net_enabled` match (after line ~250)

```rust
    // VM × broker: a broker-backed worker (`broker_uds` set) reaches its host-side
    // broker over a THIRD guest-initiated vsock port (1026). Independent of egress
    // (proxy_uds / port 1025) and of the net/NIC decision — the broker UDS never
    // changes the netns (mirrors bwrap's `broker_uds_is_bound_without_touching_netns`).
    // Both channels can be present at once.
    let (broker_vsock_port, broker_host_uds) = match &policy.broker_uds {
        Some(uds) => (Some(BROKER_VSOCK_PORT), Some(uds.clone())),
        None => (None, None),
    };
```

- [ ] **Step 5: Value-match env override** in `build_launch_plan`, immediately after the egress `KASTELLAN_EGRESS_PROXY_UDS` override block (after line ~345)

```rust
    // VM × broker: the worker's `*_BROKER_UDS` env carries the HOST broker UDS path
    // (injected by core's `rewrite_policy_for_broker`), which is unreachable from
    // inside the VM. Rewrite it to the in-guest relay path. KIND-AGNOSTIC: match by
    // VALUE (the unique per-worker host UDS path) rather than a hardcoded broker-kind
    // env key, so this crate stays broker-kind-agnostic and a future search-broker VM
    // is free plumbing. `rewrite_policy_for_broker` guarantees exactly one env entry
    // whose value equals `broker_uds` (a unique `<scratch>/<sock>` path — no collision).
    if let Some(host_uds) = &broker_host_uds {
        let host_str = host_uds.to_string_lossy();
        for (_, v) in env.iter_mut() {
            if *v == host_str {
                *v = GUEST_BROKER_UDS.to_string();
            }
        }
    }
```

- [ ] **Step 6: Cmdline token** in `build_launch_plan`, after the egress-token block (after line ~363)

```rust
    if broker_vsock_port.is_some() {
        boot_args.push_str(" kastellan.broker=1");
    }
```

- [ ] **Step 7: Plan construction** — add the two fields to the returned `FirecrackerLaunchPlan { .. }` literal (after `egress_host_uds,` at line ~404)

```rust
        broker_vsock_port,
        broker_host_uds,
```

- [ ] **Step 8: Launcher argv** in `sandbox/src/linux_firecracker.rs::launcher_argv`, after the egress pair (after line ~91)

```rust
    // VM × broker: when the worker declares a broker, the launcher also runs the
    // broker reverse-relay (listen on `<vsock_uds>_<broker_port>`, forward to the
    // host broker UDS).
    if let (Some(uds), Some(port)) = (&plan.broker_host_uds, plan.broker_vsock_port) {
        argv.push("--broker-uds".into());
        argv.push(uds.to_string_lossy().into_owned());
        argv.push("--broker-vsock-port".into());
        argv.push(port.to_string());
    }
```

- [ ] **Step 9: VMM jail bind** in `sandbox/src/linux_firecracker/confine.rs::build_vmm_jail_argv`, after the egress-UDS bind (after line ~120)

```rust
    // VM × broker: bind the host broker UDS rw so the confined launcher's broker
    // reverse-relay can reach it (same as the egress-proxy UDS above). The broker
    // rides its own vsock port, so --unshare-all's private netns is unaffected.
    if let Some(uds) = &plan.broker_host_uds {
        let s = uds.display().to_string();
        a.extend(["--bind".into(), s.clone(), s]);
    }
```

- [ ] **Step 10: Cross-clippy on the Mac (compile + lint all cfg(linux) sandbox code)**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings`
Expected: clean. (The plan.rs unit tests from Step 1 compile here but run on the DGX in Task 7.)

- [ ] **Step 11: Commit**

```bash
git add sandbox/src/linux_firecracker/plan.rs sandbox/src/linux_firecracker.rs sandbox/src/linux_firecracker/confine.rs
git commit -m "feat(sandbox): broker vsock channel (port 1026) in the FC launch plan

policy.broker_uds now drives a second guest->host vsock channel: plan fields
broker_vsock_port/broker_host_uds, a kastellan.broker=1 cmdline token, a
kind-agnostic value-match rewrite of the worker's *_BROKER_UDS env to the guest
path, the launcher's --broker-uds/--broker-vsock-port argv, and the VMM-jail bind
of the host broker UDS. Independent of egress; no NIC/netns effect.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: core web-research — VM × broker resolve branch + entry constructor

**Files:**
- Modify: `core/src/workers/web_research.rs` (new `web_research_firecracker_broker_entry`; `resolve()` VM×broker branch replacing the warn-and-ignore; new `#[cfg(target_os="linux")]` tests)

**Interfaces:**
- Consumes: `broker_env(endpoint, embed_model, allowlist)`, `net_entries(endpoint, embed_endpoint, allowlist)`, `crate::broker::BrokerSpec::embed`, `SandboxBackendKind::FirecrackerVm` (all present).
- Produces: `#[cfg(target_os = "linux")] pub fn web_research_firecracker_broker_entry(binary: PathBuf, image_dir: String, endpoint: &str, embed_endpoint: &str, embed_model: Option<&str>, allowlist: &[String]) -> ToolEntry`.

> **All new code here is `#[cfg(target_os="linux")]` and CANNOT be compiled on the Mac** (#144). It is first compiled + tested on the DGX (Task 7). Write carefully.

- [ ] **Step 1: Write the failing tests** (add to the `#[cfg(target_os = "linux")]` test region in `web_research.rs`)

```rust
#[cfg(target_os = "linux")]
#[test]
fn resolve_vm_broker_drops_embed_host_sets_vm_backend_and_broker_spec() {
    let get_env = |k: &str| match k {
        "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
        USE_EMBED_BROKER_ENV => Some("1".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
        EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
        _ => None,
    };
    let exists = |_p: &std::path::Path| true;
    let allowlist = |_t: &str| vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebResearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            // VM backend AND a broker spec (the two combined).
            assert!(matches!(entry.sandbox_backend, Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)));
            let spec = entry.broker.as_ref().expect("VM broker mode declares a broker spec");
            assert_eq!(spec.kind, crate::broker::BrokerKind::Embed);
            assert_eq!(spec.endpoint, "http://127.0.0.1:11434/v1/embeddings");
            // Embed host ABSENT from egress; VM fs_read empty.
            assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty");
            match &entry.policy.net {
                Net::Allowlist(hosts) => assert!(hosts.iter().all(|h| !h.starts_with("127.0.0.1")),
                    "embed host must be absent from net: {hosts:?}"),
                other => panic!("expected Net::Allowlist, got {other:?}"),
            }
            // Direct embed-endpoint env omitted; model present; broker_uds set at spawn.
            assert!(!entry.policy.env.iter().any(|(k, _)| k == EMBED_ENDPOINT_ENV));
            assert!(entry.policy.env.iter().any(|(k, v)| k == EMBED_MODEL_ENV && v == "embeddinggemma"));
            assert!(entry.policy.broker_uds.is_none());
        }
        other => panic!("expected Register(VM broker entry), got {}", outcome_label(&other)),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_vm_without_broker_stays_direct_vm_entry() {
    // USE_MICROVM without the broker gate => the existing direct/degrade VM entry.
    let get_env = |k: &str| match k {
        "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
        ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
        _ => None,
    };
    let exists = |_p: &std::path::Path| true;
    let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
    let c = ctx(&get_env, &exists, &allowlist);
    match WebResearchManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert!(matches!(entry.sandbox_backend, Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)));
            assert!(entry.broker.is_none(), "no broker without the gate + endpoint");
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}
```

- [ ] **Step 2: Add the VM broker entry constructor** in `web_research.rs`, after `web_research_firecracker_entry` (after line ~302)

```rust
/// Build the [`ToolEntry`] for web-research running inside a Firecracker micro-VM
/// **AND** reaching a host-side embed broker (VM × broker; opt-in via
/// `USE_MICROVM=1` + `USE_EMBED_BROKER=1` + an embed endpoint). Combines the VM
/// entry (empty `fs_read`, `FirecrackerVm` backend, force-routable) with broker
/// mode: the embed host is **dropped** from `Net::Allowlist`, only the embed model
/// env is injected (not the endpoint), and `broker: Some(Embed)` tells core's
/// chokepoint to spawn the broker + bind its UDS. In the VM the broker rides a
/// second vsock channel (port 1026); the FC plan rewrites the injected
/// `KASTELLAN_EMBED_BROKER_UDS` to the in-guest relay path.
///
/// Because the broker runs host-side, this is the ONLY way a VM worker reaches a
/// *loopback/local* embed backend for hybrid ranking (the egress proxy SSRF-blocks
/// loopback). Linux-only.
#[cfg(target_os = "linux")]
pub fn web_research_firecracker_broker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    embed_endpoint: &str,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let mut env = broker_env(endpoint, embed_model, allowlist);
    env.push(("KASTELLAN_MICROVM_DIR".to_string(), image_dir));
    env.push((
        "KASTELLAN_MICROVM_ROOTFS".to_string(),
        "web-research.ext4".to_string(),
    ));
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        // NO embed host — the worker reaches the backend only through the broker.
        net: Net::Allowlist(net_entries(endpoint, None, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,   // set at spawn (force-routing)
        broker_uds: None,  // set at spawn (rewrite_policy_for_broker)
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: Some(crate::broker::BrokerSpec::embed(embed_endpoint)),
    }
}
```

- [ ] **Step 3: Replace the warn-and-ignore with the broker branch** in `resolve()` (the `#[cfg(target_os = "linux")]` block, lines ~353-381)

```rust
        #[cfg(target_os = "linux")]
        {
            let use_microvm = (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
            if use_microvm {
                let binary = PathBuf::from(MICROVM_WORKER_BIN);
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                // VM × broker: the broker runs host-side and the VM worker reaches it
                // over the slice-4a vsock UDS (port 1026), so a loopback embed backend
                // works in VM mode. `use_broker` guarantees an embed endpoint.
                if use_broker {
                    let embed_endpoint = embed_endpoint.as_deref().expect("use_broker implies Some");
                    return Resolution::Register(web_research_firecracker_broker_entry(
                        binary,
                        image_dir,
                        &endpoint,
                        embed_endpoint,
                        embed_model.as_deref(),
                        &allowlist,
                    ));
                }
                return Resolution::Register(web_research_firecracker_entry(
                    binary,
                    image_dir,
                    &endpoint,
                    embed_endpoint.as_deref(),
                    embed_model.as_deref(),
                    &allowlist,
                ));
            }
        }
```

- [ ] **Step 4: Update the `#[cfg(target_os="linux")] const USE_MICROVM_ENV` doc + the module doc** — remove any "VM × broker unsupported / ignored" wording (lines ~356-368 old comment is gone with Step 3). Grep to be sure:

Run: `grep -n "VM × broker\|VM x broker\|unsupported in v1\|ignored because" core/src/workers/web_research.rs`
Expected: no stale "unsupported"/"ignored" matches remain.

- [ ] **Step 5: Mac sanity — the macOS build is unaffected** (the new code is `cfg(linux)`, so it must not break the macOS compile of the crate)

Run: `cargo build -p kastellan-core`
Expected: PASS (compiles the macOS target; the new `cfg(linux)` code is excluded — this only proves no macOS-visible breakage, NOT that the Linux code compiles; that is Task 7).

- [ ] **Step 6: Commit**

```bash
git add core/src/workers/web_research.rs
git commit -m "feat(web-research): VM x embed-broker resolve branch + entry

resolve() now emits a VM broker entry when USE_MICROVM=1 AND USE_EMBED_BROKER=1
(+ embed endpoint): FirecrackerVm backend + broker: Some(Embed) + embed host
dropped from Net::Allowlist + embed model env only. Replaces the prior
warn-and-ignore. New web_research_firecracker_broker_entry constructor.
cfg(linux); compiled + tested on the DGX (Task 7).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: e2e — VM × broker (hermetic policy pin + live full-stack hybrid)

**Files:**
- Create: `core/tests/web_research_firecracker_broker_e2e.rs`

**Interfaces:**
- Consumes: `web_research::web_research_firecracker_broker_entry` (Task 5), `broker::{spawn_broker, BrokerConfig, BrokerKind}`, `worker_lifecycle::force_route::rewrite_policy_for_broker`, `tool_host::{spawn_worker, dispatch, WorkerSpec}`, the FC backend + PG/microvm test helpers.

> Two tiers, mirroring `embed_broker_egress_e2e.rs` (Slice C). **Both are Linux-only** (the entry is `cfg(linux)`); the hermetic pin runs fast on the DGX unit pass, the live tier is `#[ignore]` and needs real KVM + vsock + a live egress path + a live embed backend.

- [ ] **Step 1: Write the hermetic policy-pin test** (full code) — a fresh file `core/tests/web_research_firecracker_broker_e2e.rs`:

```rust
#![cfg(target_os = "linux")]
//! VM × embed-broker e2e: a web-research worker in a Firecracker VM embeds through
//! the host-side broker over the second vsock channel (port 1026), with the embed
//! host absent from its egress — the VM analogue of Slice C's zero-embed-egress
//! hybrid property (`embed_broker_egress_e2e.rs`).

use std::path::PathBuf;

use kastellan_core::broker::BrokerKind;
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_research::web_research_firecracker_broker_entry;
use kastellan_sandbox::{Net, SandboxBackendKind};

/// Hermetic (no KVM/network): pin the post-rewrite VM broker policy the live tier
/// depends on — VM backend, broker UDS bound + injected, embed host absent from
/// egress, direct embed-endpoint env omitted, embed model present.
#[test]
fn vm_broker_policy_has_broker_uds_and_zero_embed_egress() {
    let worker = PathBuf::from("/usr/local/bin/kastellan-worker-web-research");
    let searx = "https://searx.example.org/search";
    let embed = "http://127.0.0.1:11434/v1/embeddings";
    let allowlist = vec!["searx.example.org".to_string(), "en.wikipedia.org".to_string()];

    let entry = web_research_firecracker_broker_entry(
        worker,
        "/var/lib/kastellan/microvm".to_string(),
        searx,
        embed,
        None,
        &allowlist,
    );
    // VM backend + broker spec present.
    assert!(matches!(entry.sandbox_backend, Some(SandboxBackendKind::FirecrackerVm)));
    let spec = entry.broker.as_ref().expect("VM broker entry declares a broker");
    assert_eq!(spec.kind, BrokerKind::Embed);
    assert_eq!(spec.endpoint, embed);

    // Simulate core's spawn-time rewrite onto the bound broker UDS.
    let uds = PathBuf::from("/tmp/embed-vm-test/embed.sock");
    let policy = rewrite_policy_for_broker(entry.policy, &uds, BrokerKind::Embed);
    assert_eq!(policy.broker_uds.as_deref(), Some(uds.as_path()));
    let injected = policy
        .env
        .iter()
        .find(|(k, _)| k == BrokerKind::Embed.uds_env())
        .map(|(_, v)| v.as_str());
    assert_eq!(injected, Some(uds.to_string_lossy().as_ref()));
    // Zero embed egress: the loopback embed host is absent from the allowlist.
    match &policy.net {
        Net::Allowlist(entries) => assert!(
            entries.iter().all(|e| !e.starts_with("127.0.0.1")),
            "embed host must be absent from egress; got {entries:?}"
        ),
        other => panic!("expected Net::Allowlist, got {other:?}"),
    }
    assert!(!policy.env.iter().any(|(k, _)| k == "KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT"));
}
```

- [ ] **Step 2: Add the live full-stack tier** (`#[ignore]`, DGX). Compose it from the two existing harnesses — read both first:
  - `core/tests/web_research_firecracker_egress_e2e.rs` — how to boot a **force-routed web-research VM** (real egress path). For the *live* tier this needs a **real** egress-proxy sidecar forwarding to live SearxNG + content (not the CONNECT-stub used there); reuse the force-routing spawn assembly from `core/tests/net_demo_firecracker_egress_e2e.rs` (real egress proxy over vsock).
  - `core/tests/embed_broker_egress_e2e.rs::brokered_worker_ranks_hybrid_with_zero_embed_egress` — how to `spawn_broker` + `rewrite_policy_for_broker` and assert `ranking == "hybrid"`.

  The live test must, in order: (a) bring up PG + resolve the web-research + embed-broker binaries (`[SKIP]` if missing); (b) `spawn_broker` (`BrokerKind::Embed`) pointed at the live embed backend → host broker UDS; (c) build `web_research_firecracker_broker_entry`, then `rewrite_policy_for_broker(entry.policy, &uds, Embed)`; (d) set up force-routing so the SearxNG search + content fetch ride the egress proxy (real proxy → live SearxNG), setting `proxy_uds` + CA on the policy; (e) `spawn_worker(FirecrackerVm backend, &spec)`; (f) `dispatch("web.research", {query, max_sources:2})`; (g) assert `result["ranking"] == "hybrid"` (the broker embed succeeded through the 1026 tunnel) **and** re-assert the embed host is absent from the live policy's `Net::Allowlist`. `[SKIP]` if no live SearxNG/embed backend.

  Key novel assertion (beyond the two source tests): hybrid ranking holds **from inside a VM with zero embed egress** — the embed reached the host broker over vsock port 1026.

- [ ] **Step 3: Cross-clippy the test target on the Mac (compile + lint; does not run KVM)**

Run: `cargo clippy -p kastellan-core --target aarch64-unknown-linux-gnu --test web_research_firecracker_broker_e2e -- -D warnings` — **NOTE: this will hit the #144 `ring` cross wall** and cannot complete on the Mac. Instead, verify compilation on the DGX (Task 7). On the Mac, only confirm the file is syntactically consistent by review.

- [ ] **Step 4: Commit**

```bash
git add core/tests/web_research_firecracker_broker_e2e.rs
git commit -m "test(web-research): VM x embed-broker e2e (policy pin + live hybrid)

Hermetic pin of the VM broker policy (broker UDS bound, embed host absent) +
an #[ignore] live full-stack tier asserting hybrid ranking from inside a VM with
zero embed egress (embed reaches the host broker over vsock port 1026). DGX-gated.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: DGX gate — build, compile the Linux code, run everything

**Files:** none (verification only). Push the branch first so the DGX can fetch it.

- [ ] **Step 1: Push the branch + fetch on the DGX**

```bash
git push -u origin feat/web-research-vm-embed-broker
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout feat/web-research-vm-embed-broker && git reset --hard origin/feat/web-research-vm-embed-broker'
```

- [ ] **Step 2: Build workspace + rebuild the release launcher + the rootfs (bakes the new microvm-init)**

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && cargo build --workspace 2>&1 | tail -20'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --release -p kastellan-microvm-run -p kastellan-microvm-init 2>&1 | tail -5'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && bash scripts/workers/microvm/build-web-research-rootfs.sh 2>&1 | tail -5'
```
Expected: all exit 0 (this is the FIRST compile of the `cfg(linux)` sandbox + core code — fix any compile errors here, then re-commit + re-push + re-fetch).

- [ ] **Step 3: Run the new unit tests (sandbox plan + core resolve) + workspace clippy**

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-sandbox linux_firecracker::plan 2>&1 | tail -15'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib workers::web_research 2>&1 | tail -15'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15'
```
Expected: the new broker plan tests + resolve tests PASS; clippy clean.

- [ ] **Step 4: Run the VM × broker e2e (hermetic tier always; live tier `--ignored`)**

Ensure a live embed backend + SearxNG are up on the DGX (Ollama `embeddinggemma` + `kastellan-searxng` :8888 — as used by Slice C). Then:

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && setsid bash -lc "cargo test -p kastellan-core --test web_research_firecracker_broker_e2e -- --ignored --nocapture > ~/vmbroker-e2e.log 2>&1" </dev/null & echo started'
# poll:
ssh dgx 'tail -30 ~/vmbroker-e2e.log'
```
Expected: `vm_broker_policy_has_broker_uds_and_zero_embed_egress` PASS; `vm_broker_ranks_hybrid_with_zero_embed_egress` PASS (or a clean `[SKIP]` if a live service is genuinely absent — but the gate requires it GREEN, so bring the services up).

- [ ] **Step 5: Full-workspace regression + record the new baseline**

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && export PATH=$HOME/.local/bin:$PATH && setsid bash -lc "cargo test --workspace > ~/dgx-full.log 2>&1; echo DONE_EXIT=$? >> ~/dgx-full.log" </dev/null & echo started'
ssh dgx 'grep -E "test result|DONE_EXIT" ~/dgx-full.log | tail -20'
```
Expected: 0 failed, 0 unexpected `[SKIP]`; record `passed/failed/ignored` as the new baseline (was 2416/0/40 at Slice C; expect +new unit tests passed, +1 ignored e2e).

- [ ] **Step 6: (no commit — verification task).** If Step 2/3 surfaced compile/test fixes, they were committed on the branch during iteration; ensure the branch is pushed.

---

### Task 8: docs + PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`
- Modify (if it names a stale VM-loopback caveat): the issue #429 note in `core/src/workers/web_research.rs` doc (the loopback-embed caveat now has a VM×broker escape hatch)

- [ ] **Step 1: Update HANDOVER.md** — move the VM×broker item from "Next TODO" to a new "Recently completed" header entry with the file paths, the port-1026 design, the value-match decision, the `/run`-single-mount gotcha, the Mac vs DGX verification split, and the new DGX baseline. Bump `Last updated`, `Current state` HEAD (after merge), and the session-end verification line.

- [ ] **Step 2: Update ROADMAP.md** — tick the embed-broker arc's VM×broker follow-up with the commit hash(es); note the arc is now fully complete (host + VM).

- [ ] **Step 3: Update the web_research.rs #429 caveat** — the VM loopback-embed caveat now has a remedy (VM×broker reaches a loopback embed backend host-side). One-line doc update, no behaviour change.

- [ ] **Step 4: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md core/src/workers/web_research.rs
git commit -m "docs(handover): VM x embed-broker complete; update ROADMAP + #429 caveat

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Open the PR** (link to the embed-broker arc; note the DGX gate is discharged with the new baseline)

```bash
git push
gh pr create --base main --head feat/web-research-vm-embed-broker \
  --title "feat(web-research): VM × embed-broker — second vsock channel (port 1026)" \
  --body "$(cat <<'EOF'
Completes the embed-broker arc: a USE_MICROVM web-research worker now reaches the
host-side embed broker over a second vsock channel (port 1026), mirroring the
slice-4a egress channel (1025). A VM worker gets hybrid ranking against a
local/loopback embed backend with **zero embed egress** — the Slice-C property,
now for VMs.

## What changed
- sandbox FC plan: `policy.broker_uds` → broker vsock channel (fields, detection,
  kind-agnostic value-match env override to the guest path, `kastellan.broker=1`
  token, launcher argv, VMM-jail bind).
- microvm-run: second reverse-relay for the broker port.
- microvm-init: `/run` tmpfs mounted once; generic `setup_relay(uds, port)` for
  egress + broker; `parse_broker_config`.
- core web_research: `resolve()` VM×broker branch + `web_research_firecracker_broker_entry`
  (replaces the prior warn-and-ignore); worker binary + `rewrite_policy_for_broker`
  unchanged.

## Verification
- Mac: cross-clippy (sandbox + microvm-init) clean; microvm-run + microvm-init
  cmdline unit tests green; workspace macOS build clean.
- DGX (real KVM + vsock + live PG + live SearxNG + live Ollama): new plan +
  resolve unit tests green, `web_research_firecracker_broker_e2e` (hermetic pin +
  live hybrid `--ignored`) green, full-workspace `cargo test` + clippy green;
  new baseline <fill in>.

Spec: docs/superpowers/specs/2026-07-13-web-research-vm-broker-embed-channel-design.md
Plan: docs/superpowers/plans/2026-07-13-web-research-vm-embed-broker.md

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review

**Spec coverage:** §3.1 plan.rs → Task 4; §3.2 launcher_argv → Task 4 Step 8; §3.3 confine bind → Task 4 Step 9; §3.4 microvm-run → Task 3; §3.5 microvm-init → Tasks 1+2; §3.6 web_research resolve → Task 5; §3.7 rootfs → Task 7 Step 2 (rebuild); §5 testing → Tasks 1-6 + Task 7 DGX gate; §6 risks (the `/run` double-mount) → Task 2 note + the mount-once wiring; §7 checklist → Task 8. All covered.

**Placeholder scan:** the only deliberately-open item is Task 6 Step 2's live tier, which is *composed* from two named existing harnesses with an ordered recipe + the exact novel assertion — an integration scoped to the DGX where the live services run, not a logic hand-wave. Every other step has complete code.

**Type consistency:** `setup_relay(guest_uds: &str, vsock_port: u32)` / `relay_loop(RawFd, u32)` / `relay_one_connection(RawFd, u32)` consistent (Task 2). `parse_broker_config -> BrokerConfig{enabled}` consistent (Tasks 1↔2). Plan fields `broker_vsock_port: Option<u32>` / `broker_host_uds: Option<PathBuf>` consistent across Task 4 (plan.rs) ↔ launcher_argv ↔ confine. `web_research_firecracker_broker_entry(PathBuf, String, &str, &str, Option<&str>, &[String])` consistent Task 5 ↔ Task 6. `BROKER_VSOCK_PORT = 1026` / `GUEST_BROKER_UDS = "/run/kastellan-broker.sock"` identical in sandbox (Task 4) and microvm-init (Task 1).
