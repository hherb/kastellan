# Firecracker micro-VM slice 5b-4 — Matrix adopts the foundation (matrix-in-a-VM)

**Date:** 2026-07-02
**Status:** draft for operator review (core decisions brainstormed + approved
2026-07-02; supervision trade-offs in
`2026-07-02-slice5b4-supervision-tradeoffs.md`, decision = option A)
**Closes:** [#380](https://github.com/hherb/kastellan/issues/380)
**Composes:** slice 5b-1 (`PersistentWorker`), 5b-2 (`persistent_store`),
5c (`spawn_net_transport` transparent-tunnel net-in-VM), 5a (VMM confinement).

## Goal

The live Matrix channel worker runs inside a Firecracker micro-VM on Linux:
long-lived, respawn-supervised by the **shared** `PersistentWorker` (not the
bespoke copy in `channel/matrix.rs`), reaching the homeserver only through a
per-worker transparent-tunnel egress sidecar, with its E2E crypto store on a
`persistent_store` that survives VM respawns.

## Non-goals

- True `jailer` (root chroot + uid drop) — stays a future
  `VmmConfinement::Jailer` tier.
- Protocol multiplexing (concurrent in-flight JSON-RPC requests) — the one-pipe
  poll/send latency coupling is accepted, unchanged from today.
- MITM of matrix traffic — matrix cannot trust the ephemeral proxy CA;
  transparent tunnel is the permanent posture (5c spec).
- IMAP/Telegram channels — but the new driver component is written for them to
  reuse (see §5b-4a.1).
- macOS matrix-in-a-VM — `FirecrackerVm` is Linux-only; macOS keeps
  Seatbelt (+ sidecar under force-routing) as today.

## Operator decisions (locked in brainstorm, 2026-07-02)

1. **Scope = one spec, two sub-slices / two PRs.**
   **5b-4a** — channel restructure on the current backends (cross-platform,
   Mac-testable): `PersistentWorker` adoption + sidecar egress. Lands green
   alone.
   **5b-4b** — VM platform work (DGX-gated): matrix rootfs + guest-loopback
   init fix + `persistent_store` + `FirecrackerVm` switch.
2. **Egress gating: ride the global `KASTELLAN_EGRESS_FORCE_ROUTING`** (default
   ON in the supervised deployment) — no matrix-specific flag. Consequence: the
   next DGX deploy flips live Matrix to sidecar-routed, so the live DGX round
   trip through a sidecar is a hard pre-deploy gate (known risk: the DGX
   resolver has made the sidecar's own DNS flaky in real-net *tests*; verify
   live against `matrix.kastellan.dev` explicitly).
3. **Supervision structure: option A — a reusable channel-generic driver
   layered over an untouched `PersistentWorker`** (trade-off doc above).

---

# Sub-slice 5b-4a — channel restructure (cross-platform)

## 5b-4a.1 `PolledWorkerDriver` — the reusable channel driver

New module `core/src/channel/polled_driver.rs`. Owns everything push-shaped
that `PersistentWorker` deliberately does not:

```rust
/// What a channel-shaped worker looks like to the driver.
/// Matrix today; IMAP/Telegram instantiate this later.
pub struct PolledWorkerSpec {
    pub label: &'static str,        // supervisor label ("matrix")
    pub init_method: &'static str,  // "matrix.init"  — login proof, returns identity
    pub poll_method: &'static str,  // "matrix.poll"  — long-poll, {timeout_ms}
    pub send_method: &'static str,  // "matrix.send"  — one outbound message
    pub poll_timeout: Duration,     // worker-side long-poll wait (POLL_MS today)
}

pub struct PolledWorkerDriver { /* driver thread + channel endpoints */ }

impl PolledWorkerDriver {
    /// Spawns PersistentWorker::spawn(label, factory) and the driver thread.
    /// Blocks until the first `init_method` call returns — the identity JSON
    /// is the login proof (daemon gates ChannelBus on it, as today).
    pub fn spawn(
        spec: PolledWorkerSpec,
        factory: PersistentFactory,             // unchanged 5b-1 type
        parse_poll: fn(Value) -> Vec<IncomingMessage>,
        encode_send: fn(&OutgoingMessage) -> Value,
    ) -> anyhow::Result<(Self, Value /* identity */)>;

    pub fn endpoints(&mut self) -> (tok_mpsc::Receiver<IncomingMessage>,
                                    std_mpsc::Sender<OutgoingMessage>);
}
```

Driver-thread loop (direct port of today's `drive()` semantics, minus the
respawn state machine which `PersistentWorker` now owns):

1. Drain queued outbound messages (`try_recv`) into `pending: VecDeque`.
2. Flush `pending` front-first via `handle.call(send_method, …)`; stop at the
   first error, **keeping unacked messages in `pending`** — the no-dropped-
   reply guarantee survives a respawn exactly as today.
3. `handle.call(poll_method, {timeout_ms})` → `parse_poll` → forward each
   `IncomingMessage` over the bounded tokio mpsc (`blocking_send`).
4. On any `Err` (worker died / `"persistent worker is restarting"`): log once,
   sleep one short retry slice (reuse `RESPAWN_POLL_SLICE`-style 200 ms),
   check for shutdown (`inbound_tx.is_closed()`), loop. The supervisor is
   respawning underneath; the driver just retries through the window.
5. Shutdown: both channel endpoints dropped → driver returns →
   `PersistentHandle::shutdown` (RAII) tears the transport + sidecar down.

Identity surfacing: `spawn` performs the first `handle.call(init_method, …)`
on the caller thread after `PersistentWorker::spawn` returns; the factory does
**not** call init (respawn re-inits — see §5b-4a.3).

Constants: `RESPAWN_ALARM_*` duplication is resolved by deletion — matrix's
copy of the backoff/alarm loop goes away; `RespawnRateAlarm` (5/300 s) is
defined once in `worker_lifecycle` and used only by `PersistentWorker`
(#380 acceptance item 2).

## 5b-4a.2 Matrix adopts it — what changes in `channel/matrix.rs`

- **Deleted:** `drive()`, `supervised_self_spawn`, `WorkerFactory`,
  `spawn_worker_client`, `ProtocolWorkerClient` (+ its `WorkerClient` trait if
  no other impl remains). This removes both duplicated state machines *and*
  the duplicated spawn/lockdown/stderr sequence (#380 acceptance items 1 & 3 —
  `ClientTransport::spawn` becomes the single canonical sequence).
- **Kept:** `MatrixChannel` (now a thin façade over the driver's endpoints —
  `Channel` impl unchanged), `build_matrix_policy`, `MatrixSpawnConfig`,
  `spawn_matrix_worker` (rewired), pairing/bus/route code untouched.
- `spawn_matrix_worker` builds a `PersistentFactory` closure that calls
  `spawn_net_transport` (§5b-4a.3) and hands it to
  `PolledWorkerDriver::spawn(MATRIX_SPEC, factory, parse_poll, encode_send)`.
  `parse_poll`/`encode_send` are pure fns over the existing
  `kastellan-matrix-wire` types (unit-testable without a worker).

## 5b-4a.3 Egress: matrix joins force-routing via `spawn_net_transport`

- When force-routing resolves ON (`KASTELLAN_EGRESS_FORCE_ROUTING`, default ON
  in the supervised deployment; fail-closed if the proxy binary is missing):
  the factory calls `spawn_net_transport(NetTransportSpawn { backend,
  sidecar_backend, proxy_bin, program, args, base_policy, allowlist:
  ["homeserver:port"], worker_name: "matrix", extra_ca: None }, scratch)`.
  - Transparent tunnel is inherent (`disable_mitm=true`, no CA injected) —
    matches the worker's existing `ProxyBridge` + own-TLS posture; the worker
    side needs **zero changes**.
  - `forced_transparent_policy` drops `/etc/resolv.conf` (sidecar resolves
    DNS) but **preserves the system CA trust-store fs_read entries** —
    matrix-sdk 0.18 validates homeserver TLS against native certs
    (load-bearing; `build_matrix_policy` keeps them).
  - Sidecar + worker respawn 1:1 (`NetClientTransport` field order) —
    per-respawn login/init is re-proven because the driver's step-4 retry ends
    with the *next successful call*; the worker re-inits itself from the
    persisted session on boot (restore-or-login), so no explicit re-init call
    is needed. `matrix.poll`/`matrix.send` on a fresh worker are valid
    immediately after its `LiveSdk::connect` (which `main.rs` completes before
    serving stdio).
- Force-routing OFF (dev): factory falls back to plain
  `ClientTransport::spawn` with the un-rewritten policy (direct
  `Net::Allowlist`), preserving today's dev behaviour.
- On bwrap/Seatbelt the store stays a plain `fs_write` host dir (persists
  across respawns natively) — `persistent_store` arrives only with the VM
  backend in 5b-4b.

## 5b-4a.4 Tests (5b-4a gate)

- Unit: driver state machine against a scripted fake `PersistentTransport`
  (pending retention across a simulated death; init-identity surfacing;
  retry-through-restarting; shutdown-during-retry; poll→inbound forwarding).
- Existing hermetic `matrix_channel_e2e` (fake worker process) migrates to the
  new spawn path and must stay green on both platforms — it is the proof the
  restructure preserves `Channel` semantics.
- `core` egress force-routing e2e already covers `spawn_net_transport`
  (5c `net_demo_egress_e2e`); matrix adds no new mechanism on this path.
- **Live DGX gate before merge:** `matrix_live_e2e` round-trip +
  `matrix_restart_recovers_downtime_message` (#321) with the daemon spawn path
  running sidecar-routed (force-routing ON) against `matrix.kastellan.dev` —
  explicitly verifying the sidecar-DNS risk from decision 2.

---

# Sub-slice 5b-4b — VM platform work (Linux/DGX)

## 5b-4b.1 `scripts/workers/microvm/build-matrix-rootfs.sh`

Mirrors `build-net-demo-rootfs.sh` with three deltas:

- Bakes `kastellan-worker-matrix` (built `--release --features live-matrix`)
  + its `ldd` lib closure. `bundled-sqlite` ⇒ no host libsqlite3 needed.
- **Bakes the OS CA trust store** (`/etc/ssl/certs`, `/etc/ssl/cert.pem`,
  `/usr/share/ca-certificates`, staged into `$WORK` directly) — matrix-sdk
  reads the *system* store; `/etc`/`/usr` are not share anchors so this cannot
  ride `fs_read`. This is the one rootfs that ships a CA bundle, by design.
- `ROOTFS_MIB=512` (matrix-sdk + tokio + crypto closure; net-demo's 128 is far
  too small). Emits `matrix.ext4` next to the shared `vmlinux`.
- Includes `/run` (egress relay mountpoint) + the standard share anchors.

## 5b-4b.2 `kastellan-microvm-init`: bring guest loopback up

`ProxyBridge` binds/dials `127.0.0.1:<port>` inside the guest; the minimal VM
boots with `lo` DOWN. Add a `bring_loopback_up()` step to init (a
`SIOCSIFFLAGS IFF_UP` ioctl on `lo` via the existing libc dependency,
fail-loud), unconditionally — it is harmless for workers that don't use it and
removes a per-worker conditional. Unit-testable as a pure argv/ioctl-shape
where feasible; proven end-to-end by the 5b-4b e2e.

## 5b-4b.3 Persistent store: `fs_write` dir → `persistent_store` image

- `build_matrix_policy` (VM mode) sets `persistent_store:
  Some(PersistentStore { host_backing: <state_dir>/matrix-state.ext4,
  guest_mount: "/data", size_mib: 256 })` and `KASTELLAN_MATRIX_STORE=/data`.
  `size_mib=256` is generous for a single-user crypto store; note it is fixed
  at first mkfs (resize = open [#381]).
- Host backing lives at the stable per-deployment state dir (the same
  `store_dir` root used today), NOT the run dir — mkfs-once, survives respawn
  (5b-2 mechanism; the VMM jail already binds `persistent_image_path` RW).
- This is what preserves E2E device identity, `session.json`, and the #321
  sync-token downtime recovery across VM respawns (under FC, `fs_write` is
  wiped per spawn — the load-bearing reason the store must move).
- **Password-file delivery (first login only):** `.login-password` currently
  rides the host store dir; a VM can't see it and the host can't write into
  the ext4 image. VM mode instead passes the one-time password file via an
  `fs_read` RO-share path under `/tmp` (a share anchor); the worker's existing
  read-then-delete keeps working except the delete fails on the RO mount
  (harmless warn) — the host-side spawn code deletes its copy right after the
  init handshake returns (RAII). Steady-state daemon spawns are password-less
  (session restore), so this path is bootstrap-only.

## 5b-4b.4 Backend switch

- Opt-in env `KASTELLAN_MATRIX_USE_MICROVM=1` (Linux only), following the
  python-exec/web-fetch precedent: `spawn_matrix_worker` resolves
  `SandboxBackendKind::FirecrackerVm` for the **worker** backend and injects
  `KASTELLAN_MICROVM_DIR`/`KASTELLAN_MICROVM_ROOTFS=matrix.ext4` into the
  policy env; the **sidecar backend stays the host bwrap** (5c invariant — the
  egress proxy is the real-network boundary; passing FC here boots a proxy
  with no route). Fail-closed: flag set but `LinuxFirecracker::probe` fails ⇒
  refuse to spawn (no silent bwrap fallback), matching the microvm
  convention.
- In-VM, force-routing is **mandatory**: `Net::Allowlist` without `proxy_uds`
  is already rejected fail-closed by `build_launch_plan` (no virtio-net
  device exists). `mem_mb` stays 512 (KVM-enforced).
- Non-VM Linux (flag unset) and macOS keep the 5b-4a bwrap/Seatbelt path
  byte-identical.

## 5b-4b.5 Tests (5b-4b gate, DGX)

- New `#[ignore]` `matrix_firecracker_live_e2e` (or a VM mode of the existing
  live e2e): daemon-shaped spawn with `KASTELLAN_MATRIX_USE_MICROVM=1` against
  the live homeserver — round-trip + a genuine `pkill -f
  kastellan-microvm-run` (15-char comm gotcha) → `PersistentWorker` respawns a
  fresh VM + sidecar → downtime message recovered (#321 + `persistent_store`
  composed; the kv-demo respawn test is the template).
- Hermetic sandbox additions where pure: init loopback step, rootfs script
  shellcheck-style self-checks, policy derivation units (persistent_store +
  rootfs env injection).
- Full DGX `cargo test --workspace` + clippy `-D warnings`; rebuild the
  **release** launcher + `matrix.ext4` before the e2e (stale-launcher gotcha).

---

# Known-good evidence & risks

- **Guest wall clock / TLS validity:** proven sane — the 5c
  `net_demo_firecracker_egress_e2e` already performs full webpki cert-window
  validation inside a DGX VM.
- **Entropy/devtmpfs:** all VM workers already rely on the pinned kernel's
  devtmpfs auto-mount for `/dev/vd*`; `/dev/urandom` rides the same mechanism
  (rustls/ring + matrix crypto need it). No init change required.
- **vsock ports:** stdio (1024) + egress reverse-channel (1025) already
  coexist; the ProxyBridge hop is intra-guest loopback TCP, not a third vsock
  port.
- **Risk — sidecar DNS on the DGX** (decision 2): verified live before any
  deploy flips; if the sidecar cannot resolve `matrix.kastellan.dev` in that
  environment, that is an environment fix (resolver config), not a design
  change — the worker keeps working direct until force-routing is enabled for
  it.
- **Risk — live-channel regression:** 5b-4a is the risky half (it rewrites the
  supervision of a working production channel); it is deliberately isolated,
  cross-platform-testable, and gated on the live DGX round-trip before merge.

# Deferred / follow-ups

- Promote `PolledWorkerDriver` into `worker_lifecycle/` if a non-channel
  streaming consumer ever appears (two-consumer refactor).
- IMAP/Telegram channels instantiate the driver (Phase 2).
- `persistent_store` resize ([#381]).
- True `jailer` tier; MITM-of-matrix (not planned — see Non-goals).
