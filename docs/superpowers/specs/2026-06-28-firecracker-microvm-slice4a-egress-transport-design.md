# Firecracker micro-VM — slice 4a: egress-proxy vsock reverse-channel transport

**Date:** 2026-06-28
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation) / Phase-3 hardening
**Parent:** [`2026-06-26-linux-firecracker-microvm-design.md`](2026-06-26-linux-firecracker-microvm-design.md) (slice table, row 4)
**Precedent:** slice 1 (`linux_firecracker.rs` + `microvm-run` + `microvm-init`, PR #364), slice 3 host-dir sharing (PR #371); bwrap force-routing (`sandbox/src/linux_bwrap.rs`, `core/src/egress/`)

## Problem

Firecracker micro-VM slices 1–3 are merged: a `Net::Deny` worker boots inside a
real KVM guest over vsock, with `policy.env` forwarding (slice 1), warm/idle
reuse (slice 2), and per-spawn host-dir RO/RW sharing (slice 3). The next worker
class is **net workers** — a force-routed `Net::Allowlist` worker (web-fetch,
web-search, …) that reaches the network **only** through the host egress proxy's
Unix-domain socket.

On bwrap this works because AF_UNIX sockets are mount-namespace-scoped, not
net-namespace-scoped: the backend gives the worker a private netns (no route to
anything) and `--bind`-mounts the proxy UDS into the jail at an identical path,
so the worker dials it directly while having no other egress
(`sandbox/src/linux_bwrap.rs`, `Net::Allowlist + proxy_uds` arm).

Inside a Firecracker guest there is **no shared mount namespace** with the host,
so the host proxy UDS cannot be bind-mounted in. The guest is a separate kernel
with its own filesystem. The proxy UDS is unreachable. This slice builds the
transport that closes that gap.

## Goal

Forward the host egress-proxy UDS into the Firecracker guest over a **second
vsock channel**, so a force-routed `Net::Allowlist` worker can run in a VM with
its **existing, unchanged** proxy-dialing code (it still just connects to
`KASTELLAN_EGRESS_PROXY_UDS`). The VM carries **no virtio-net device** — all
egress flows through the proxy relay — which is a *stronger* containment than the
bwrap path (the guest kernel has no network stack reachable from the worker at
all).

### Scope: 4a = transport only

Slice 4 is split. **4a (this spec)** builds and proves the transport in
isolation; **4b (later)** plugs in the first real net-worker consumer.

- **In 4a:** the vsock reverse channel end-to-end — plan field, launcher
  reverse-relay, guest-side in-guest-UDS↔vsock relay, backend UDS translation —
  with full host-side unit/integration tests and a real-KVM smoke that proves the
  guest-initiated vsock direction works.
- **Deferred to 4b:** any real net-worker rootfs (web-fetch / web-search binary +
  lib closure); CA-cert-into-guest for MITM TLS (slice-3 RO-share will carry it);
  the full "in-VM worker fetches an allowlisted host through the real egress
  proxy" e2e.

The split de-risks the genuinely novel part: this codebase has only ever used the
**host-initiated** hybrid-vsock direction (host dials the guest's port 1024 for
JSON-RPC). The reverse, **guest-initiated** direction is untested here and is the
one thing that can't be verified without real KVM.

## Background: the two vsock directions

Firecracker's hybrid vsock multiplexes over one device (one `guest_cid`, one
host-side base UDS). Two directions, confirmed in `workers/microvm-run/src/bridge.rs`:

- **Host-initiated (existing, slice 1):** the host connects the base UDS
  (`<run_dir>/vsock.sock`) and sends `CONNECT <port>\n`; firecracker forwards to a
  guest process listening on `<port>`. Used today for the JSON-RPC channel
  (`WORKER_VSOCK_PORT = 1024`; guest init binds `VMADDR_CID_ANY:1024`, the
  launcher dials and `OK …`-handshakes).
- **Guest-initiated (new, this slice):** a guest process connects
  `AF_VSOCK(CID=2 [host], port)`; firecracker connects, on the host, to a Unix
  socket at **`<base_uds>_<port>`** (e.g. `vsock.sock_1025`). The host must be
  **listening** on that path before the guest dials.

The egress flow is naturally guest-initiated (the worker originates the outbound
connection), so it uses the second direction.

## Architecture & data flow

```
                          ┌──────────────────── Firecracker guest ────────────────────┐
 worker (4b: web-fetch…)  │  dials KASTELLAN_EGRESS_PROXY_UDS = /run/kastellan-egress.sock
   (UNCHANGED code)  ──────┼──▶ in-guest UDS ──▶ [init relay child] ──▶ AF_VSOCK(CID=2, 1025)
                          └────────────────────────────────────────────────────│───────┘
                                                            (firecracker)       │
 host: launcher (microvm-run) listens on  <run_dir>/vsock.sock_1025  ◀──────────┘
        └──▶ relay each accepted conn ──▶ real host egress-proxy UDS (the sidecar)
                                              └──▶ proxy does DNS + outbound TCP + allowlist/SSRF/(MITM)
```

- The launcher **pre-binds** `vsock.sock_1025` *before* booting firecracker, so a
  fast-booting worker can never dial before the host listener exists.
- The guest init **binds the in-guest UDS in the parent, before `exec`**, so the
  worker can never dial before the in-guest listener exists.
- Each in-guest UDS connection maps 1:1 to an independent vsock connection →
  independent host-side accept → independent dial of the real proxy UDS.
  Concurrent and sequential tunnels scale with no shared state.

Control flow (force-routed `Net::Allowlist` worker, 4b consumer; 4a proves the
channel with the init self-test instead of a real worker):

```
tool_host force-routing → sidecar spawned (host egress proxy on <scratch>/egress.sock)
  → rewrite_worker_policy sets policy.proxy_uds = <scratch>/egress.sock   (UNCHANGED)
  → backend.spawn_under_policy(policy, …)                                 [FirecrackerVm]
      build_launch_plan: egress_proxy_vsock_port = Some(1025), net_enabled = false,
                         boot_args += " kastellan.egress=1"
      spawn microvm-run as Child:
        --egress-uds <scratch>/egress.sock      (host sidecar path)
        (guest env KASTELLAN_EGRESS_PROXY_UDS overridden → /run/kastellan-egress.sock)
        ├─ bind <run_dir>/vsock.sock_1025  (BEFORE boot)
        ├─ boot firecracker
        ├─ JSON-RPC pump on port 1024            (unchanged, slice 1)
        └─ accept loop on vsock.sock_1025 → relay each conn ↔ <scratch>/egress.sock

in-guest PID1 microvm-init (kastellan.egress=1):
  → mount /proc,/sys,/tmp,/run ; apply_host_mounts (slice 3)
  → bind /run/kastellan-egress.sock ; fork relay child (UDS ↔ AF_VSOCK(2,1025))
  → connect vsock 1024, dup2→fd0/1 ; exec worker (serve_stdio, unchanged)
```

## Component changes

### Shared constants (both crates, kept in sync by comment — the existing pattern)
`sandbox/src/linux_firecracker/plan.rs` and `workers/microvm-init`:
- `EGRESS_VSOCK_PORT: u32 = 1025` (mirrors `WORKER_VSOCK_PORT = 1024`).
- `GUEST_EGRESS_UDS: &str = "/run/kastellan-egress.sock"` — the in-guest path the
  worker dials and the init binds.
- `EGRESS_CMDLINE_KEY = "kastellan.egress"`, `EGRESS_SELFTEST_CMDLINE_KEY =
  "kastellan.egress.selftest"` (mirrors `ENV_CMDLINE_KEY`/`MOUNTS_CMDLINE_KEY`).

### 1. `sandbox/src/linux_firecracker/plan.rs` (pure, unit-tested)
- Add `egress_proxy_vsock_port: Option<u32>` to `FirecrackerLaunchPlan`.
- In `build_launch_plan`: set it to `Some(EGRESS_VSOCK_PORT)` **iff**
  `policy.net` is `Net::Allowlist(_)` **and** `policy.proxy_uds.is_some()`
  (force-routed). Otherwise `None`.
- Force-routed ⇒ `net_enabled = false` (no virtio-net device; egress is via the
  proxy vsock only). Document that a *legacy* direct-net `Net::Allowlist` (no
  `proxy_uds`) is **not supported in a VM** in 4a — it would need a virtio-net
  device this slice does not build; `build_launch_plan` may reject it explicitly
  or leave it `net_enabled=false` with a documented "force-routing required for
  net workers in a VM" note. (Decision: reject up front with a clear
  `SandboxError`, fail-closed — a net worker silently getting no egress is worse
  than a spawn error.)
- Append the ` kastellan.egress=1` token to `boot_args` when
  `egress_proxy_vsock_port.is_some()`; append ` kastellan.egress.selftest=1`
  additionally when the test knob is set (see Testing). Reuse the existing
  1024-byte `boot_args` fail-closed cap.
- `render_firecracker_config` is **unchanged** — one vsock device carries both
  ports; the reverse channel needs no extra config stanza.

### 2. `sandbox/src/linux_firecracker.rs` — `spawn_under_policy` (backend-local translation)
- When `plan.egress_proxy_vsock_port.is_some()`:
  - Pass `policy.proxy_uds` (the host sidecar path) to the launcher as
    `--egress-uds <path>`.
  - Override the guest's `KASTELLAN_EGRESS_PROXY_UDS` env value to
    `GUEST_EGRESS_UDS` **before** the env cmdline token is encoded, so the worker
    in-guest dials the in-guest path, not the (unreachable) host path.
- `SandboxPolicy` gains **no new field**; the bwrap backend is untouched (it
  keeps binding `policy.proxy_uds` at an identical path, env unchanged). The
  host-vs-guest UDS divergence is entirely a Firecracker-backend concern, handled
  at the spawn boundary.

### 3. `workers/microvm-run/` (launcher = the Child)
- New CLI flag `--egress-uds <host-proxy-uds>` (optional; absent ⇒ no reverse
  channel, byte-identical to slice 1–3 behaviour).
- When present: **before** `boot::firecracker_argv` spawns firecracker, bind a
  `UnixListener` at `format!("{}_{}", base_uds, EGRESS_VSOCK_PORT)`. Spawn an
  accept loop (detached thread); for each accepted connection, dial
  `--egress-uds` and run a bidirectional byte pump (two `try_clone`d halves,
  per-chunk flush — the same shape as `bridge::pump`). New module
  `workers/microvm-run/src/egress_relay.rs` holds the pure-ish relay loop +
  the host-side listener path helper, separately testable.
- Detached threads die on launcher exit (VM teardown); the listener socket lives
  inside `<run_dir>`, so the existing RAII `teardown_run_dir` reclaims it (no new
  teardown path).

### 4. `workers/microvm-init/` (guest PID1)
- New module-or-fn `egress.rs`: a pure cmdline parser (`kastellan.egress=1`,
  `kastellan.egress.selftest=1` → small struct) plus the `#[cfg(target_os =
  "linux")]` libc relay.
- When `kastellan.egress=1`, in `main` **before** `accept_host_bridge`/`exec_worker`:
  - Ensure `/run` is a writable tmpfs (mount tmpfs at `/run`; the rootfs is a
    read-only superblock so the mountpoint dir is pre-created in `build-rootfs.sh`).
  - Bind `AF_UNIX` listener at `GUEST_EGRESS_UDS` (parent — so the path exists
    before the worker runs).
  - `fork()` a relay child: accept loop on the in-guest UDS; for each connection,
    `AF_VSOCK(CID=2, EGRESS_VSOCK_PORT)` + bidirectional pump. The child runs for
    the VM's life; it is a child of the post-`exec` worker (PID1) and dies when
    the VM tears down (`panic=1`/`reboot=k`). Not reaped — acceptable, the VM is
    gone.
  - **Self-test (`kastellan.egress.selftest=1`)**: the parent additionally
    connects its own `GUEST_EGRESS_UDS`, writes `PING\n`, reads a line, and on
    `PONG\n` logs `EGRESS_CHANNEL_OK` to the kernel console (stderr → `fc.log`).
    This proves the full reverse path on real KVM with no net-worker dependency.
    Then the parent `exec`s the worker normally.
- Best-effort, never aborts PID1 (the slice-3 NUL-safe / log-and-continue posture):
  a failed bind/mount logs and continues to `exec_worker` (a 4b worker would then
  fail its first dial, surfaced as a normal worker error, not a dead guest).
- `build-rootfs.sh`: pre-create the `/run` mountpoint dir (one line, mirrors the
  existing `/proc /sys /tmp` mountpoints).

## Testing & TDD discipline (rules #1–#2)

**Hermetic (run on the Mac dev box, no KVM):**
- `plan.rs` units: `egress_proxy_vsock_port` set only when force-routed
  (`Net::Allowlist` + `proxy_uds`); `None` for `Net::Deny`, for `Net::Allowlist`
  without `proxy_uds` (and the fail-closed reject for that case in a VM), and for
  `Net::ProxyEgress`; `net_enabled == false` when force-routed; the
  ` kastellan.egress=1` token present/absent in `boot_args`; the selftest token
  gated on the test knob; relative-path rejection still fires.
- backend unit: `--egress-uds` launcher arg derived from `policy.proxy_uds`; the
  guest `KASTELLAN_EGRESS_PROXY_UDS` env overridden to `GUEST_EGRESS_UDS` in the
  encoded cmdline.
- launcher `egress_relay` integration (host-only, no VM): two `UnixListener`s
  stand in for "firecracker delivery" (`<base>_1025`) and "the real proxy UDS";
  assert a byte payload round-trips both directions and the relay tears down when
  either side closes. The host-side listener-path helper (`<base>_<port>`) is
  unit-pinned.
- init `egress` cmdline parser: pure unit tests (enabled/disabled/selftest,
  malformed → disabled, fail-safe). The libc relay/`fork` is
  `#[cfg(target_os="linux")]` and exercised on the DGX.

**DGX (real KVM, `#[ignore]`):** new e2e
`core/tests/python_exec_firecracker_egress_channel_e2e.rs` (or a sandbox-level
launcher e2e): a host echo listener (replies `PONG\n` to `PING\n`) is wired as
`--egress-uds`; a VM boots with `kastellan.egress.selftest=1` using the existing
python-exec rootfs; assert `fc.log` contains `EGRESS_CHANNEL_OK`. This is the
real-KVM proof of the guest-initiated vsock direction and a permanent regression
gate. Slice-1/2/3 e2e must show no regression; `0` orphan run-dirs after.

Per the standing rule: sandbox/microvm-init linux-cfg modules don't run under
`cargo test` on macOS → per-task Mac gate is cross-clippy
(`--target aarch64-unknown-linux-gnu --all-targets -D warnings`); `cargo test` +
the e2e run on the DGX. The pure parser/relay-helper units that *do* compile on
macOS run there too.

## Security posture

- **No virtio-net device** in a force-routed VM: the worker's only egress is the
  in-guest UDS → vsock → host proxy. The guest kernel exposes no network
  interface to the worker — strictly stronger than the bwrap private-netns path.
- The egress proxy (allowlist + SSRF + optional MITM/leak-scan/pin) is the
  **unchanged** real boundary; this slice only changes *how the worker reaches
  it*, not what it enforces.
- The self-test path is **test-gated** by `kastellan.egress.selftest=1`, emitted
  only when a test env knob is set; it is absent from a production `boot_args`.
- Fail-closed: a net worker requesting a VM without force-routing (no
  `proxy_uds`) is rejected at `build_launch_plan`, not silently left egress-less.

## Out of scope (→ slice 4b and beyond)

- Any real net-worker rootfs (web-fetch / web-search): binary + lib closure +
  opt-in flag + the full "fetch an allowlisted host through the proxy, in-VM" e2e.
- CA-cert-into-guest for MITM TLS (the per-instance proxy CA at `<scratch>/ca.pem`
  must appear in-guest for an MITM worker to trust the proxy leaf): reuse the
  slice-3 RO-share by adding the ca.pem path to the worker's `fs_read`; 4b.
- Long-lived / channel workers in a VM (matrix-style, loopback `ProxyBridge`) and
  jailer hardening: slice 5.
- x86_64 acceptance (Firecracker supports it; only aarch64/DGX is verified).
