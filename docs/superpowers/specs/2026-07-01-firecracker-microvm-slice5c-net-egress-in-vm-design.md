# Firecracker micro-VM slice 5c — network egress in a VM (long-lived, transparent-tunnel) — design

**Date:** 2026-07-01
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation) / Phase-3 hardening
**Precedent:** the Firecracker arc — slice-1 backend
(`2026-06-26-linux-firecracker-microvm-design.md`), slice-2 warm/idle, slice-3
host-dir sharing, **slice-4a egress vsock reverse-channel**
(`2026-06-28-firecracker-microvm-slice4a-egress-transport-design.md`),
**slice-4b web-fetch in a VM**
(`2026-06-28-firecracker-microvm-slice4b-web-fetch-design.md`), slice-5a VMM
confinement, **slice-5b-1/5b-2 persistent-VM lifecycle + persistent store**
(`2026-06-29-firecracker-microvm-slice5b1-5b2-persistent-vm-lifecycle-design.md`);
the egress sidecar (`core/src/egress/{spawn,net_worker}.rs`,
`workers/egress-proxy/`); the live Matrix channel
(`core/src/channel/matrix.rs` — transparent-tunnel `ProxyBridge`).

## Problem

The Firecracker arc has, so far, shipped two disjoint capabilities:

- **Net workers in a VM, but single-use** (slice 4a transport + 4b web-fetch): a
  force-routed `Net::Allowlist + proxy_uds` worker boots in a guest with **no
  virtio-net device**, dials the in-guest UDS `/run/kastellan-egress.sock`, and
  that relays over a guest-initiated vsock channel (port 1025) → launcher
  reverse-relay → the host egress-proxy UDS. web-fetch serves **one** dispatch,
  then the VM tears down. It also uses **MITM** egress (trusts only the
  per-instance proxy CA delivered per-spawn).

- **Long-lived workers in a VM, but network-free** (slice 5b-1/5b-2): the reusable
  `PersistentWorker` supervisor boots a VM **once**, serves **many** JSON-RPC
  calls, respawns on crash (backoff + rate alarm), and keeps a persistent RW ext4
  store — proven with the `Net::Deny` `kv-demo` worker.

The next piece toward **Matrix-in-a-VM** is the intersection those two never
covered: a **long-lived worker with network egress**, where the worker does its
**own end-to-end TLS** (so the egress proxy **cannot** MITM it — a
transparent-tunnel), and the egress-proxy **sidecar** is **long-lived and
respawns 1:1 with the VM**.

Two concrete gaps block this today:

1. **The egress sidecar is coupled to a single dispatch.**
   `spawn_forced_net_worker` (`core/src/egress/net_worker.rs`) mints a per-worker
   scratch, spawns the sidecar, rewrites the policy onto its UDS, spawns the
   worker, and tears the sidecar down 1:1 when the (single-use) worker exits.
   There is no path that keeps a sidecar alive alongside a **persistently-alive**
   VM worker and rebuilds it when the VM respawns.

2. **All existing net-in-VM egress is MITM.** `rewrite_worker_policy`
   unconditionally injects the per-instance proxy CA into the worker
   (`KASTELLAN_EGRESS_PROXY_CA` + `fs_read`), and web-fetch trusts *only* that CA.
   A worker that must validate a **real** origin with its **own** trust store —
   because the proxy transparently tunnels its bytes and never terminates the TLS
   — has no supported spawn path. (`disable_mitm` exists as a `NetWorkerSpawn`
   field and the proxy honours it — `workers/egress-proxy/src/proxy.rs` — but no
   caller wires the transparent-tunnel + no-CA rewrite together for a long-lived
   worker.)

## Goal

Prove end-to-end that a **long-lived `Net::Allowlist` worker** can:

- run inside a Firecracker VM (no virtio-net), booting **once** and serving
  **many** calls over that boot;
- reach an allowlisted origin through the host egress proxy over the slice-4a
  vsock reverse-channel, doing its **own end-to-end TLS** (**transparent-tunnel /
  no-MITM**);
- be **respawn-supervised**, with the egress **sidecar respawning 1:1 with the
  VM**;

…delivered as **reusable, cross-platform, backend-agnostic** primitives, **without
modifying the live Matrix code** (Matrix adopts them in slice 5b-4).

### What composes unchanged (the reuse story)

- **The vsock reverse-channel (4a)** is transport-only and already carries a
  long-lived stream (`workers/microvm-run/src/egress_relay.rs`,
  `workers/microvm-init` relay child): each in-guest UDS connection maps 1:1 to an
  independent vsock connection → host relay → sidecar UDS. No change.
- **`PersistentWorker` (5b-1)** already accepts a
  `factory: FnMut() -> Result<Box<dyn PersistentTransport>>`, spawns it on a
  persistent PDEATHSIG-safe OS thread, drives serialized calls, and on death drops
  the old transport **off-thread** (reaping its child) before re-running the
  factory. **1:1 respawn falls out for free** if the transport owns the sidecar.
- **The Firecracker plan (4a/4b)** already turns `Net::Allowlist + proxy_uds` into
  `net_enabled=false` + egress vsock 1025 + the
  `KASTELLAN_EGRESS_PROXY_UDS → /run/kastellan-egress.sock` override, and the
  slice-4b resolver already selects a rootfs by `KASTELLAN_MICROVM_ROOTFS`.
- **The egress proxy** honours `disable_mitm` (peek-first-byte skipped ⇒ always a
  transparent tunnel, even for a TLS ClientHello — `proxy.rs`). No change.

The genuinely new surface is small: one transport type, one factory, one
`mitm`-conditional in the policy rewrite, and a demonstrator worker + rootfs.

## Architecture & data flow

```
daemon thread ── PersistentWorker::spawn(net_client_factory) ──▶ persistent OS thread
                                                                    │ factory()
        ┌────────────────────────────────────────────────────────────┘
        ▼
  make scratch dir  (/tmp/egress-<pid>-<seq>/, RAII-owned by the sidecar)
  spawn egress sidecar  (disable_mitm=TRUE, Net::Allowlist)  → host proxy UDS + ca.pem
  rewrite policy (mitm=FALSE): proxy_uds=<host UDS>, Net::Allowlist,
      backend=FirecrackerVm ;  NO KASTELLAN_EGRESS_PROXY_CA injected
  LinuxFirecracker::spawn_under_policy
      → build_launch_plan (4a/4b): net_enabled=false, egress vsock 1025,
        KASTELLAN_EGRESS_PROXY_UDS→/run/kastellan-egress.sock,
        rootfs = net-demo.ext4 (KASTELLAN_MICROVM_ROOTFS)
      → microvm-run (confined, 5a): pre-bind <uds>_1025 reverse-relay → sidecar UDS; boot FC
      → connect protocol Client over the vsock stdio bridge (bridge.rs::pump)
  return NetClientTransport { client, egress: EgressSidecar }  ── Box<dyn PersistentTransport>
        │
        ▼   (many calls, one boot)
PersistentHandle::call("net.tls_probe", {host, port}) ─▶ net-demo in guest
      → CONNECT-over-UDS to /run/kastellan-egress.sock → vsock 1025 → launcher → sidecar UDS
      → sidecar: allowlist + SSRF check on host:port ; disable_mitm ⇒ transparent tunnel
      → end-to-end TLS  net-demo ↔ origin  (proxy never terminates) → handshake OK
      → {ok:true, peer_subject, alpn}
PersistentHandle::call("net.stats")  ─▶ {calls_served, pid}   (same process, one boot)

  VM/worker crash → PersistentWorker drops old NetClientTransport off-thread
      (EgressSidecar::Drop reaps old sidecar child + removes scratch; Client Drop reaps
       old VMM child) → factory() again → fresh sidecar + fresh VM
      → next net.tls_probe succeeds ⇒ sidecar respawns 1:1 with the VM
```

The worker reaches the network **only** through the proxy relay; the guest kernel
has no NIC — strictly stronger containment than the bwrap private-netns path,
with the worker's egress code unchanged from any other proxy-dialing worker.

## Component changes

### 1. `net-demo` worker crate — `workers/net-demo/` (new)

A minimal long-lived `Net::Allowlist` demonstrator (Rust; `prelude::serve_stdio`,
so it locks itself down **in-process** — seccomp `NetClient` + Landlock — at
startup, defense-in-depth even inside the VM, exactly the kv-demo posture). The
net-facing analog of kv-demo; a permanent integration-test fixture (the
`fake_matrix_worker`/`kv-demo` precedent), **not** registered as a `tool_host`
tool (spawned directly via `PersistentWorker`, like Matrix).

Methods:
- `net.tls_probe {host, port=443}` → open an **end-to-end TLS** connection to the
  origin through `KASTELLAN_EGRESS_PROXY_UDS` (issue a CONNECT over the UDS, then
  a rustls handshake) and return `{ok, peer_subject, alpn}`. **Trust config =
  system roots (baked `ca-certificates` bundle) + optional
  `KASTELLAN_NETDEMO_EXTRA_CA` (test-only path added to `fs_read`)** so a
  self-signed loopback origin validates hermetically. Failure returns
  `{ok:false, error}`, not an RPC error (a probe result, like a fetch result).
- `net.stats` → `{calls_served, pid}` from in-process counters (proves the
  **same** process serves many calls over one boot — the `kv.stats` analog).
- `net.crash` → deterministic `process::exit`/panic (the `kv.crash` analog; drives
  the respawn e2e).

*Transport reuse:* prefer reusing the CONNECT-over-UDS plumbing from
`web-common::ProxyConnectGet` if it can be given a **system-roots + extra-CA**
trust config without weakening its existing MITM-CA-only mode; otherwise the
net-demo rolls a thin (~60-line) CONNECT-over-UDS + rustls-handshake probe and
leaves `web-common` byte-unchanged. Decide during TDD; either keeps `web-common`
non-regressed. (Matrix's real client is `matrix-sdk`, which already validates
against the system trust store over a transparent tunnel — this demonstrator
mirrors that trust posture without pulling in the SDK.)

### 2. Rootfs — `scripts/workers/microvm/build-net-demo-rootfs.sh` (new)

Sibling of `build-web-fetch-rootfs.sh`. Stages `kastellan-worker-net-demo` at
`/usr/local/bin/` + its `ldd` closure + `kastellan-microvm-init` (baked PID1) +
the shared pseudo-fs/anchor dirs + **`/run`** mountpoint (egress relay, 4a) +
**the `ca-certificates` system CA bundle** (`/etc/ssl/certs/…`). The CA bundle is
the one real rootfs difference from web-fetch (4b deliberately baked *no* bundle —
MITM-only, per-instance CA — whereas a transparent-tunnel worker must validate a
real origin with system roots). Journal-less ext4 (`-O ^has_journal`, mounted RO,
shared across concurrent VMs). Emits **`net-demo.ext4`** into the shared image dir
(default `KASTELLAN_MICROVM_DIR=/var/lib/kastellan/microvm`, sharing the pinned
`vmlinux`); selected at spawn by `KASTELLAN_MICROVM_ROOTFS=net-demo.ext4` (the
slice-4b resolver, reused unchanged). Factor genuinely-shared rootfs setup into a
sourced helper if cheap; otherwise duplicate with a kept-in-sync comment (house
style).

### 3. Net-aware persistent transport + factory — `core/src/worker_lifecycle/persistent_net.rs` (new)

Kept a sibling of `persistent.rs` so both stay under the 500-LOC cap.

- **`NetClientTransport`** implements `PersistentTransport`, owning the protocol
  `Client` **and** the `EgressSidecar`:
  - `call` forwards to `client.call` (the `ClientTransport` behaviour).
  - `death_report` snapshots the worker stderr tail + a non-blocking `try_wait`
    (reuse `ClientTransport`'s helper).
  - **`Drop`** reaps **both** children — `EgressSidecar::Drop` already kills+reaps
    the sidecar and removes the scratch dir; the `Client`/VMM child is reaped as
    `ClientTransport::Drop` does today (`kill()` + blocking `wait()`), which
    `PersistentWorker` already detaches off the driver thread. Carries the
    zombie-reap lesson (`ClientTransport::Drop`, the recurring-daemon-zombie fix).
- **`net_client_factory(params) -> PersistentFactory`**: returns a
  `Box<dyn FnMut() -> Result<Box<dyn PersistentTransport>>>` that on each call:
  1. makes a unique scratch dir (`make_worker_scratch_dir`, reused — the
     socket-path-overflow guard included);
  2. spawns the egress sidecar in **transparent-tunnel** mode
     (`NetWorkerSpawn { disable_mitm: true, .. }`), sidecar-first fail-closed;
  3. rewrites the worker policy with **`mitm=false`** (see §4): sets `proxy_uds`,
     leaves `Net::Allowlist`, injects **no** CA;
  4. spawns the VM worker under the (already `FirecrackerVm`) backend + connects
     the `Client` — the `ClientTransport::spawn` shape, but against the
     rewritten, sidecar-bound policy;
  5. bundles `{ client, egress }` into a `NetClientTransport` and returns it.

  This closure **is** the `factory` handed to `PersistentWorker::spawn`; the
  supervisor's existing off-thread drop + re-run gives 1:1 sidecar+VM respawn with
  no new supervision code.

### 4. Transparent-tunnel policy rewrite — `core/src/egress/net_worker.rs`

`rewrite_worker_policy` gains a **`mitm: bool`** parameter (or a sibling
`rewrite_worker_policy_transparent`): the per-instance CA is added to `fs_read`
and injected as `KASTELLAN_EGRESS_PROXY_CA` **only when `mitm == true`**. A
transparent-tunnel worker (`mitm=false`) gets `proxy_uds` set and the
`/etc/resolv.conf` removal (it still doesn't resolve — the sidecar does), but
**no CA** — it validates origins with its own roots. All existing callers pass
`mitm=true` (byte-identical MITM path); only `net_client_factory` passes
`mitm=false`. `disable_mitm` is already carried into the sidecar via the existing
`NetWorkerSpawn` field, so no proxy-side change.

## Testing & TDD discipline (rules #1–#2)

**Pure units (macOS + Linux, no VM, no sandbox):**
- `rewrite_worker_policy(mitm=false)` injects **no** CA (`fs_read` unchanged, no
  `KASTELLAN_EGRESS_PROXY_CA`) and still sets `proxy_uds` + drops
  `/etc/resolv.conf`; `mitm=true` is byte-identical to today.
- `NetClientTransport` bundling/Drop with a **fake** sidecar + **fake** client:
  `call`/`death_report` delegate correctly; `Drop` tears down **both** (assert via
  drop-order flags).
- `net-demo` probe-result shaping (ok/err JSON) against a fake transport.

**Hermetic, always-on, cross-platform e2e (runs on the Mac under Seatbelt, no KVM)
— `core/tests/net_demo_egress_e2e.rs`:**
- Stand up a **loopback self-signed TLS origin** (rustls server bound to
  `127.0.0.1:<port>`) + a **real transparent-tunnel sidecar** (the operator
  allowlist contains that literal loopback address; the SSRF **literal-IP
  carve-out** admits it — exactly as `egress_force_routing_e2e` already does for
  its allowed literal-loopback round-trip, so **no DNS is needed**).
- `PersistentWorker::spawn(net_client_factory{ backend: Seatbelt/bwrap })` →
  `net.tls_probe{127.0.0.1:<port>}` validates against the injected
  `KASTELLAN_NETDEMO_EXTRA_CA` ⇒ `ok:true` → **TLS through the transparent tunnel
  proven** + many-calls-one-boot via `net.stats`.
- `net.crash` → `PersistentWorker` respawns → next `net.tls_probe` succeeds ⇒
  **1:1 sidecar+VM respawn proven** (the fresh sidecar has a fresh UDS/scratch).
- This is the cross-platform abstraction proof without a VM (the kv-demo
  Mac-Seatbelt-e2e precedent).

**DGX real-KVM e2e (gated, `#[ignore]` or `/dev/kvm`-skip-as-pass) —
`core/tests/net_demo_firecracker_egress_e2e.rs`:**
- The same loopback-TLS probe, but net-demo runs **in a Firecracker VM** over the
  slice-4a vsock reverse-channel (`backend: FirecrackerVm`,
  `KASTELLAN_MICROVM_ROOTFS=net-demo.ext4`), under **default-ON slice-5a
  confinement**. Because the origin is a loopback literal IP, the test is fully
  **hermetic** (no real-net, no DNS) — a real CI-able Linux gate, dodging the DGX
  public-DNS/anycast resolver caveat (memory `dgx-realnet-egress-tests-fail`).
- Assertions: `net.tls_probe` `ok:true`; `net.stats` shows one boot across many
  calls; **SIGKILL the VM → `PersistentWorker` respawns → `net.tls_probe` succeeds
  again**. Slice-1/2/3/4a/4b/5b e2e show no regression; **0 orphan run-dirs**.

**`#[ignore]` real-net (Mac operator, not a CI gate):** `net.tls_probe` to a real
public HTTPS host the operator allowlists, validated against the **baked system CA
bundle** — the last-mile "validates a real CA chain over the transparent tunnel"
proof the loopback origin cannot give. Mirrors `real_mitm_fetch_through_sidecar`.

**Discipline / gates:** Mac = `cargo build --workspace`, the Mac-runnable units +
the Seatbelt e2e, cross-clippy `--target aarch64-unknown-linux-gnu --all-targets
-D warnings` for the sandbox/microvm-init/core linux-cfg touchpoints (there are
few — most 5c code is cross-platform). DGX = full native `cargo test --workspace`
+ `clippy --workspace --all-targets -D warnings` + the KVM e2e. **FC e2e gotchas**
(carried from 4a/4b/5b): rebuild the **release** launcher (`cargo build --release
-p kastellan-microvm-run`) **and** the `net-demo` rootfs before the e2e (a stale
release launcher or rootfs silently shadows source — #362 false-leak class);
`export PATH=$HOME/.local/bin:$PATH` so `firecracker` is on the non-interactive
ssh PATH (else the e2e skip-as-passes silently).

## Security posture

- **No virtio-net device** in the guest (`net_enabled=false`): the worker's only
  egress is in-guest UDS → vsock → host sidecar. The guest kernel exposes no
  network interface to the worker — strictly stronger than the bwrap
  private-netns path.
- **Allowlist + SSRF unchanged.** The egress proxy still resolves DNS itself,
  rejects private/loopback/link-local/ULA/CGNAT/multicast (with the operator
  literal-IP carve-out), and enforces the host:port allowlist on the visible
  CONNECT line. Force-routing stays **sidecar-first fail-closed** — no sidecar,
  no VM.
- **Explicit transparent-tunnel trade-off (documented in the worker + spec).** In
  transparent-tunnel mode the proxy tunnels the worker's TLS **opaquely** — it
  **cannot** leak-scan or MITM this worker's payload. That is the deliberate price
  of letting the worker do its own end-to-end TLS (matrix's hard requirement: it
  cannot trust our ephemeral CA). The boundary for a transparent-tunnel worker is
  **allowlist + SSRF on the CONNECT target**, not payload inspection. A worker
  that *needs* payload leak-scan must stay on the MITM path (web-fetch); this is a
  per-worker choice, keyed by `disable_mitm`/`mitm=false` at spawn, never a global
  weakening.
- **No CA delivered to a transparent-tunnel worker.** `rewrite_worker_policy(mitm
  =false)` injects no proxy CA, so a compromised net-demo cannot even *see* the
  per-instance CA private material's public half via its own env — it trusts only
  system roots (+ a test-only CA in e2e). The proxy CA private key never leaves
  the sidecar (unchanged).
- **1:1 respawn + zombie-reap.** Each respawn tears down the old sidecar (child
  reaped, scratch removed) and old VMM child before the new pair spawns; the
  `NetClientTransport::Drop` + `PersistentWorker`'s off-thread detach carry the
  recurring-daemon-zombie fix forward.
- **`SandboxPolicy` + the bwrap/Seatbelt `None`/MITM paths stay byte-unchanged.**
  Every 5c addition is additive; existing workers and the whole non-transparent
  egress path are untouched.

## Cross-platform posture

| Concern | Linux (DGX) | macOS (dev) |
| --- | --- | --- |
| `NetClientTransport` + `net_client_factory` | cross-platform | same |
| `net-demo` worker | runs in FC VM **or** bwrap | runs under Seatbelt |
| transparent-tunnel sidecar | egress-proxy sidecar (bwrap) | egress-proxy sidecar (Seatbelt) |
| egress transport | vsock reverse-channel (4a) in a VM; UDS bind under bwrap | UDS under Seatbelt |
| acceptance e2e | real KVM: loopback-TLS probe + respawn | Seatbelt: loopback-TLS probe + respawn |

The VM-specific mechanism is Linux/Firecracker-only, but every reusable
abstraction (`NetClientTransport`, `net_client_factory`, the `mitm` rewrite flag,
`net-demo`) compiles and unit-tests on both, and each backend provides an
equivalent transparent-tunnel egress guarantee — satisfying the hard
cross-platform constraint.

## Non-goals (deferred)

- **Matrix adoption (5b-4):** a matrix rootfs (baked `kastellan-worker-matrix` +
  deps, bigger `mem_mb`), Matrix switching its backend to `FirecrackerVm`, and
  replacing its bespoke `supervised_self_spawn`/`drive` with the shared
  `PersistentWorker` + `NetClientTransport`. Composes 5b-1 + 5b-2 + 5c
  (also folds in issue #380 — sharing the supervisor/spawn/lockdown code with
  matrix.rs).
- **Persistent store in the net demo:** 5b-2 already provides it; the net demo is
  **state-free** (respawn survival is proven by "egress works again after
  respawn," not by surviving state). Matrix will compose both in 5b-4.
- **Leak-scan / MITM of the tunneled bytes:** impossible by definition in a
  transparent tunnel; out of scope for this worker class.
- **True `jailer`** (root chroot + uid-drop): deferred to the privileged-tier
  `VmmConfinement::Jailer` sibling seam (slice 5a).
- **A generalized "long-lived net worker in a VM" registry/config** for arbitrary
  workers (browser-driver/web-search long-lived variants): defer until a second
  real consumer beyond matrix exists (YAGNI).
- **x86_64 acceptance** (only aarch64/DGX is verified).

## Files (anticipated)

- new: `workers/net-demo/` (crate) + `scripts/workers/microvm/build-net-demo-rootfs.sh`
- new: `core/src/worker_lifecycle/persistent_net.rs` (+ `persistent_net/tests.rs` if over cap)
- new: `core/tests/net_demo_egress_e2e.rs` (hermetic, cross-platform) +
  `core/tests/net_demo_firecracker_egress_e2e.rs` (DGX real-KVM)
- edit: `core/src/egress/net_worker.rs` (`mitm: bool` on `rewrite_worker_policy`;
  transparent-tunnel factory hook / `disable_mitm` wiring)
- edit: `core/src/worker_lifecycle/mod.rs` (register `persistent_net`)
- edit: `Cargo.toml` workspace members (+ `workers/net-demo`)
- edit: HANDOVER.md + ROADMAP.md (5c ticked; 5b-4 framed as next)

## Open implementation details (resolve during the plan/TDD, not now)

- **CONNECT-over-UDS trust config reuse vs. thin roll-your-own** in `net-demo`
  (§1): whether `web-common::ProxyConnectGet` can accept a system-roots + extra-CA
  trust config without regressing its MITM-CA-only mode, or the demo rolls a small
  probe. Either is acceptable; pick the one that leaves `web-common` unchanged.
- **`net-demo` `ldd` closure** (rustls/ring are largely static; the closure should
  be thin) and confirming the exact system-CA bundle path(s) to bake.
- **Where `NetClientTransport::spawn` shares code with `ClientTransport::spawn`**
  (stderr-tail drain, `Client` connect, `death_report`): factor a shared helper if
  cheap, else duplicate the ~small connect block with a comment — do **not**
  modify the 5b-1 `ClientTransport` contract.
- **`mitm: bool` vs. a sibling `rewrite_worker_policy_transparent`** (§4): choose
  whichever keeps every existing caller byte-identical with the least churn.
