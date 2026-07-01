# Firecracker micro-VM slice 5b-1 + 5b-2 — long-lived persistent-VM worker lifecycle + persistent RW store — design

**Date:** 2026-06-29
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation) / Phase-3 hardening
**Precedent:** the Firecracker arc — slice-1 backend
(`2026-06-26-linux-firecracker-microvm-design.md`), slice-2 warm/idle,
slice-3 host-dir sharing (`linux_firecracker/{images,mounts}.rs`), slice-4a/4b
net workers, slice-5a VMM confinement
(`2026-06-29-firecracker-microvm-slice5a-vmm-confinement-design.md`); the live
Matrix channel worker (`core/src/channel/matrix.rs` — `supervised_self_spawn` /
`drive` / `RespawnRateAlarm`).

## Problem

Every Firecracker worker shipped so far is **single-use and short-lived**:
python-exec (slices 1–3), egress transport (4a), web-fetch (4b) all boot a VM,
serve **one** dispatch, and tear it down. The original slice-5 deliverable was
split into **5a** (VMM confinement — DONE, merged `c6ddc3f`/PR #377) and **5b**
(this arc): run a **long-lived** worker — eventually the network-facing Matrix
channel worker — inside a VM that boots **once** and stays up for the worker's
lifetime, serving many calls.

The full "Matrix-in-a-VM" target decomposes into four composable pieces:
persistent-VM lifecycle → persistent RW store → transparent-tunnel vsock egress +
long-lived sidecar → matrix rootfs/channel adoption. **This spec covers only the
first two** (5b-1 + 5b-2), the `Net::Deny` foundation with no network. The egress
and matrix-adoption pieces are deferred to later slices (5c / 5b-4).

Two concrete gaps block a long-lived worker today:

1. **No reusable long-lived-worker supervision.** The only persistent-thread
   spawn + crash-respawn machinery lives **inside** the Matrix channel
   (`supervised_self_spawn` / `drive` in `core/src/channel/matrix.rs`), coupled to
   the `Channel` trait and the `matrix.poll`/`matrix.send` loop. There is no
   generic way to keep an arbitrary worker alive, dispatch many RPC calls to it,
   and respawn it on death. (The `SingleUse`/`IdleTimeout` lifecycle managers are
   a *different* model — idle-cache reuse between independent dispatches, not a
   persistently-alive service.)

2. **No persistent writable storage in a VM.** Slice-3 RW scratch is **per-spawn
   ephemeral**: `images.rs` runs `mkfs.ext4` fresh on every boot and RAII-cleans
   the image afterward. A long-lived worker that must keep state across a VM
   respawn (the eventual Matrix E2E SQLite crypto store + `session.json`) has
   nowhere durable to write — the rootfs is read-only and shared, `/tmp` is an
   ephemeral guest tmpfs.

### Transport is already long-lived-ready (no change needed)

Investigation confirmed the host↔guest vsock stdio bridge
(`workers/microvm-run/src/bridge.rs::pump`) is pure bidirectional byte-streaming
that lives until the **host** closes stdin. A supervisor that holds the protocol
`Client` open and issues many calls works as-is; the guest worker's
`serve_stdio` loop simply never exits. The guest-init `execv`-the-worker model
(`workers/microvm-init`) needs **no** "service multiplexer" rework — one VM = one
long-lived worker serving many calls. The `SandboxBackend::spawn_under_policy`
seam the channel path already uses (`backend.spawn_under_policy`) is polymorphic,
so backend selection (bwrap ↔ Firecracker) is a wiring choice, not a transport
change.

## Goal

Prove end-to-end on real KVM that a long-lived `Net::Deny` worker can:

- run inside a Firecracker VM, booting **once** and serving **many** JSON-RPC
  calls over that one boot;
- be **respawn-supervised** on crash (capped-exponential backoff + sliding-window
  rate alarm), driven from a persistent OS thread (PDEATHSIG-safe under the
  slice-5a confined launcher);
- keep a **persistent store** that survives a VM respawn.

…delivered as **reusable, cross-platform, backend-agnostic** primitives, **without
modifying the live Matrix code** (it stays green; Matrix adopts these primitives
in a later slice).

## Non-goals (deferred)

- **Network / egress in a VM** (5c): transparent-tunnel (no-MITM) vsock egress +
  a long-lived egress-proxy sidecar that respawns 1:1 with the VM. This slice is
  `Net::Deny` only.
- **Matrix adoption** (5b-4): a matrix rootfs (baked worker + deps), Matrix
  switching its backend to `FirecrackerVm`, and replacing
  `supervised_self_spawn` with the shared `PersistentWorker`.
- **Warm/idle-cache reuse** (slice 2 model) — orthogonal; this worker is
  *persistently alive*, not idle-cached.
- **True `jailer`** (root chroot + uid-drop) — already deferred to a
  privileged-tier `VmmConfinement::Jailer` sibling in slice 5a.

## Design

Three deliverables.

### 1. `PersistentWorker` supervisor — `core/src/worker_lifecycle/persistent.rs`

A reusable generalization of matrix's `supervised_self_spawn` + `drive`, minus
the channel/poll-send coupling. Backend-agnostic (supervises any worker under any
`SandboxBackend`), so it is the cross-platform long-lived-worker primitive.

- **`PersistentWorker::spawn(factory) -> Result<PersistentHandle>`**, where
  `factory: FnMut() -> Result<SupervisedWorker>` performs the actual
  `backend.spawn_under_policy` + a readiness check and returns the supervised
  worker. The factory is **invoked on a persistent OS thread** that outlives any
  caller — required because slice-5a wraps the FC launcher in bwrap
  `--die-with-parent` (`PR_SET_PDEATHSIG`), so the spawning thread must not be an
  ephemeral pool thread (the #348 lesson). `spawn` returns only after the first
  spawn + readiness succeed (failure surfaces as `Err`, not a silently-dead
  worker).

- **`PersistentHandle::call(method, params) -> Result<Value>`** forwards the
  request over an internal channel to the persistent thread, which **owns the
  `Client`** and runs `client.call()`. Calls are serialized (one worker). The
  handle stays valid across respawns because ownership is centralized — handing a
  `Client` out would stale it on respawn.

- **Supervision loop** on the persistent thread: detect worker death (reusing the
  existing crash-classification + `dispatch_indicates_worker_dead` patterns),
  respawn via `factory()` with **capped-exponential backoff** (1s→30s) and a
  **sliding-window `RespawnRateAlarm`** (≥5 respawns / 300s → one warn). A call
  in flight when the worker dies returns `Err` so the caller can retry.

- **`PersistentHandle::shutdown()`** signals the thread to stop respawning and
  drop the worker (RAII teardown of the `SupervisedWorker`, which tears down the
  VM).

*Reuse:* lift the backoff schedule + `RespawnRateAlarm` helpers if they are
already factored; otherwise share them via a small `worker_lifecycle` submodule
rather than duplicating. Do **not** modify `channel/matrix.rs`.

### 2. Firecracker persistent RW store (5b-2) — `linux_firecracker/{images,mounts}.rs`

New additive policy field on `SandboxPolicy`:

```rust
pub struct PersistentStore {
    pub host_backing: PathBuf, // stable host path; FC: ext4 image file, bwrap/Seatbelt: directory
    pub guest_mount: PathBuf,  // absolute in-guest mount point
    pub size_mib: u32,         // ext4 image size on first create (ignored by dir-backed backends)
}
// SandboxPolicy { …, #[serde(default)] persistent_store: Option<PersistentStore> }
```

`None` ⇒ byte-identical to today (no new behaviour on any backend). The
`host_backing` path is interpreted per-backend: an **ext4 image file** on
Firecracker, a **directory** on bwrap/Seatbelt.

- **Firecracker (the new mechanism).** `host_backing` is a stable ext4 image file.
  **mkfs-once:** if it does not exist, `mkfs.ext4` at `size_mib`; if it exists,
  reuse it untouched so contents survive. Attach it as an **RW drive** and encode
  an `rw` entry in the `kastellan.mounts` cmdline manifest mapping the block
  device → `guest_mount`. A host-side **`flock(LOCK_EX)`** on the image, acquired
  before boot and held for the VM's lifetime, is the **fail-closed** guard
  against two VMs mounting the same RW ext4 concurrently (page-cache → corruption).
  This is *distinct* from slice-3 RW scratch (re-mkfs per spawn, RAII-cleaned,
  unlocked): the persistent image is stable-path, mkfs-once, flock-guarded, and
  never auto-removed.

- **Cross-platform counterpart (equivalent guarantee).** On **bwrap** and
  **Seatbelt** there is no host-dir-into-VM barrier: a persistent store is simply
  a non-ephemeral `fs_write` host directory bound/granted at `guest_mount`, which
  already persists across respawns with no image and no mkfs. `persistent_store`
  resolves to that persistent `fs_write` on those backends. This is the
  cross-platform path the demo's macOS-dev e2e exercises.

*Why ext4-image, not virtio-fs / host-bind:* matches slice-3's deliberate
"per-spawn ext4 block device, no virtio-fs" precedent and reuses the existing
`images.rs` / `mounts.rs` machinery — minimal new surface. Crash consistency
relies on ext4 journaling (journal recovery on the next mount after a SIGKILL).

### 3. `kv-demo` worker + rootfs — `workers/kv-demo/`

A minimal long-lived `Net::Deny` demonstrator (Rust; uses `prelude::serve_stdio`
like shell-exec, so it locks itself down **in-process** — seccomp `Strict` +
Landlock `RW=[guest_mount]` — at startup; defense-in-depth even inside the VM).
Methods:

- `kv.put {key, value}` → atomic write (temp + rename) into
  `<guest_mount>/store.json`.
- `kv.get {key}` → read back (proves persistence across respawn).
- `kv.stats` → `{calls_served, pid}` from in-process counters (proves the **same**
  process serves many calls over one boot).

Plus `scripts/workers/kv-demo/build-kv-demo-rootfs.sh` (mirrors
`build-web-fetch-rootfs.sh`) baking the binary + `kastellan-microvm-init` into a
small rootfs; a small VM (256–512 MiB). The worker is **not** registered as a
normal tool (no `tool_host` dispatch) — it is spawned directly via
`PersistentWorker`, exactly as Matrix is spawned via the channel. It becomes a
permanent integration-test fixture (the `fake_matrix_worker` precedent).

## Data flow (DGX, real KVM)

```
daemon thread ── PersistentWorker::spawn(factory) ──▶ persistent OS thread
                                                          │ factory()
                                                          ▼
                              LinuxFirecracker::spawn_under_policy(policy w/ persistent_store)
                                  │  mkfs-once + flock(host_image); attach RW drive;
                                  │  encode kastellan.mounts rw → guest_mount
                                  ▼
                              kastellan-microvm-run (confined, slice 5a) ─▶ firecracker ─▶ guest
                                  │  vsock stdio bridge (bridge.rs::pump, unchanged)
                                  ▼
                              microvm-init ─ execv ─▶ kv-demo (serve_stdio loop, locked down)
                                                          ▲   writes <guest_mount>/store.json (RW ext4)
PersistentHandle::call("kv.put"/"kv.get"/"kv.stats") ─────┘   (many calls, one boot)

  crash → persistent thread respawns (backoff+alarm) → new VM, SAME host_image → store intact
```

## Error handling / failure modes

- **mkfs / flock failure** → `spawn_under_policy` returns `SandboxError`
  (fail-closed; the persistent thread surfaces it and retries with backoff).
- **flock contention** (another VM holds the image) → fail-closed refuse to boot.
- **Worker death** → classified as worker-dead, respawn with backoff; rate alarm
  on churn.
- **In-flight call during crash** → `Err` to the caller (retryable).
- **Dirty ext4 after SIGKILL** → ext4 journal recovery on next mount; the demo
  e2e asserts the last committed value survives (atomic temp+rename write makes
  the store self-consistent).

## Testing / verification (TDD)

- **Hermetic unit (macOS + Linux, no VM):**
  - `PersistentWorker` with a fake factory + fake worker — respawn-on-death,
    backoff schedule, rate-alarm fire-once, call-after-respawn validity,
    in-flight-call-on-crash error.
  - `kv-demo` store put/get/stats + atomic-write semantics.
  - `images.rs` mkfs-if-absent decision (pure) + `mounts.rs` rw-entry encoding
    (pure).
- **macOS dev e2e:** `kv-demo` under Seatbelt via `PersistentWorker` with a
  persistent `fs_write` dir — many calls + process-kill respawn + store survives.
  Proves the cross-platform abstraction without a VM.
- **DGX real-KVM e2e** (`core/tests/…firecracker…`, skip-as-pass without
  `/dev/kvm`): boot `kv-demo` in a VM with the persistent ext4 store; issue many
  `kv.put`/`get`/`stats` (liveness across calls); **SIGKILL the VM, assert
  `PersistentWorker` respawns it and `kv.get` returns the pre-crash value**
  (5b-1 + 5b-2 together), under default-ON slice-5a confinement.
- **Gate:** full `cargo test --workspace` (macOS skip-as-pass) + `clippy
  --all-targets -D warnings`; DGX native `cargo test --workspace` as the Linux
  acceptance gate.
- **FC e2e gotchas (DGX):** rebuild the `--release` launcher (`cargo build
  --release -p kastellan-microvm-run`) **and** the new `kv-demo` rootfs, and
  `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive
  ssh PATH).

## Cross-platform posture

| Concern | Linux (DGX) | macOS (dev) |
| --- | --- | --- |
| `PersistentWorker` supervisor | cross-platform (OS-agnostic) | same |
| `kv-demo` worker | runs in FC VM **or** bwrap | runs under Seatbelt |
| persistent store | ext4 image, mkfs-once, flock | persistent `fs_write` dir (natural) |
| acceptance e2e | real KVM respawn+survive | Seatbelt respawn+survive |

The VM-specific mechanism is Linux/Firecracker-only, but every reusable
abstraction (`PersistentWorker`, `PersistentStore` policy field, `kv-demo`)
compiles and unit-tests on both, and each backend provides an equivalent
persistence guarantee — satisfying the hard cross-platform constraint.

## Files (anticipated)

- new: `core/src/worker_lifecycle/persistent.rs` (+ `persistent/tests.rs` if over cap)
- new: `workers/kv-demo/` (crate) + `scripts/workers/kv-demo/build-kv-demo-rootfs.sh`
- new: `core/tests/kv_demo_persistent_vm_e2e.rs` (DGX) + a macOS-dev e2e
- edit: `sandbox/src/lib.rs` (add `PersistentStore` + `SandboxPolicy.persistent_store`)
- edit: `sandbox/src/linux_firecracker/{images.rs,mounts.rs,plan.rs}` (persistent
  image: mkfs-once, flock, rw mount entry)
- edit: backend mappings on bwrap/Seatbelt to honor `persistent_store` as a
  persistent `fs_write`
- edit: HANDOVER.md + ROADMAP.md (5b-1/5b-2 ticked; 5c/5b-4 framed)

## Open questions (resolved during brainstorming)

- Scope this session = **5b-1 + 5b-2** (lifecycle + persistent store, no network).
- Demonstrator = a **new minimal `kv-demo` worker** (not python-exec / not a
  matrix stub).
- Supervision = a **new reusable `PersistentWorker`**, leaving the live Matrix
  code untouched.
