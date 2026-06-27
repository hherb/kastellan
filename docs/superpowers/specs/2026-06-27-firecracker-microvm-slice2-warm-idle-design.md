# Firecracker micro-VM — slice 2 (warm/idle reuse) + re-armable watchdog — design

**Date:** 2026-06-27
**Status:** approved (brainstorming)
**Phase:** 4 (sandbox-backend continuation)
**Precedent:** slice-1 design `2026-06-26-linux-firecracker-microvm-design.md` (staging
table, row 2); macOS warm/idle `2026-06-26-python-exec-warm-idle-container-design.md`
(#358); the warm/idle runtime `core/src/worker_lifecycle/idle_timeout.rs`.

## Problem

Slice 1 shipped the Firecracker backend booting a `Net::Deny` python-exec worker
in a real KVM guest (PR #364 + follow-ups #360/#362/#363). Slice 2 in the staging
table is **warm/idle lifecycle parity**: keep the VM booted between `python.exec`
calls so a multi-call task pays the ~0.2 s boot once, mirroring the macOS
`MacosContainer` warm path (#358).

Investigating the current tree shows the **entry/resolver-level wiring already
exists**, built generically in slice 1:

- `firecracker_mode_entry(binary, image_dir, params_file_max, lifecycle)` already
  takes a `lifecycle` parameter.
- The python-exec resolver already parses `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS`
  (`parse_idle_caps`) and builds the lifecycle via `container_lifecycle(...)` for
  the Firecracker arm, identical to the container arm.
- The worker-side per-call `/tmp` wipe (`wipe_scratch_contents`, #358) runs inside
  the guest on every `run_code` call, **regardless of backend**.
- The `IdleTimeout` warm-cache runtime (`acquire_impl`) is **backend-agnostic** — it
  warm-reuses any `SupervisedWorker`, and the Firecracker `kastellan-microvm-run`
  launcher *is* that `Child`.

So slice 2 is not a big new-feature slice. It is **(A) fix a latent watchdog bug
that breaks warm reuse past `wall_clock_ms`** (surfaced — not introduced — by the
Firecracker path; it affects the macOS container warm path too), and **(B)
DGX-verify Firecracker warm/idle end-to-end** with a non-vacuous e2e.

## Part A — the latent watchdog bug

### Symptom

`idle_timeout::acquire_impl` cold-spawns the warm worker with
`wall_clock_ms: entry.wall_clock_ms` (= `Some(30_000)` for python-exec). Today's
watchdog (`tool_host/watchdog.rs::spawn_watchdog`) is **one-shot**: a thread armed
at spawn that SIGKILLs the worker `wall_clock_ms` after boot, cancelled only when
the owning `SupervisedWorker` is dropped. The warm-reuse path
(`idle_timeout.rs:444-457`) hands the existing worker back to a new `WorkerHandle`
**without resetting the watchdog**.

Consequence: a warm worker is SIGKILLed `wall_clock_ms` after **boot**, regardless
of the idle window. With `IDLE_SECONDS=300` but `wall_clock_ms=30_000`, the warm VM
dies at 30 s; the slot then holds a dead worker, the next acquire fails the
dead-worker check and cold-respawns — warmth is silently lost, and a kill can land
**mid-dispatch** on a healthy VM.

The macOS container warm e2e (`python_exec_warm_idle_e2e.rs`) never catches this:
all three tests run well within 30 s (`idle_seconds` 60 with sub-second calls, or a
1 s idle window). The bug is real but untested.

### Why the watchdog belongs around the call, not the worker

The wall-clock watchdog exists to bound a **hung dispatch**. For a `SingleUse`
worker, worker-lifetime ≈ one dispatch, so a spawn-time watchdog happens to be
correct. For a warm `IdleTimeout` worker, lifetime spans many dispatches plus idle
gaps, so a spawn-time watchdog is semantically wrong — it must bound each in-flight
call, not the whole warm lifetime.

`SupervisedWorker::call` is the synchronous JSON-RPC chokepoint (`fn call(&mut self,
cmd) -> Result<Value, ClientError>`, module-private per issue #16). It blocks on
`client.call` until the worker responds. That is the single, correct place to arm a
per-dispatch timer.

### The fix: a re-armable watchdog, armed around `call`

Replace the one-shot `WatchdogGuard` with a **re-armable `Watchdog`** owned by
`SupervisedWorker`, armed for the duration of each `call`:

**Primitive (`core/src/tool_host/watchdog.rs`, rewritten):**

- `Watchdog::new(pid, ms) -> Watchdog` — spawn **one** thread that parks on a
  condvar in the **disarmed** state. Stores `ms` so callers needn't repeat it.
- `Watchdog::arm_scope(&self) -> ArmGuard` — set `deadline = now + ms`, bump a
  generation counter, wake the thread. The returned RAII `ArmGuard` **disarms on
  drop** (clears the deadline + wakes the thread).
- Thread loop (shared state behind a `Mutex` + `Condvar`):
  - shutdown flag set → return (exit thread).
  - disarmed (no deadline) → `wait` on the condvar indefinitely (free; no polling).
  - armed → `wait_timeout(deadline - now)`; on wake, if `now >= deadline` and this
    generation has not already fired → `kill(pid)` once, then self-disarm.
- `Watchdog::Drop` → set shutdown flag + wake; the thread exits. **No kill on
  drop** — worker termination stays with `client.close()`/`kill()`, unchanged.
- **Keep the 2026-05-08 host-blackout protections verbatim:** `is_valid_target_pid`
  (rejects 0, 1, and any value casting to a negative `pid_t`) and the injected
  `kill` fn so tests never reach `kill(2)`. These are load-bearing — do not remove.

**Chokepoint (`SupervisedWorker::call`):**

```rust
fn call(&mut self, cmd: WorkerCommand) -> Result<serde_json::Value, ClientError> {
    // Arm the wall-clock watchdog for exactly this in-flight call; the RAII
    // ArmGuard disarms synchronously when `call` returns (success or error).
    let _arm = self.watchdog.as_ref().map(Watchdog::arm_scope);
    self.client.call(&cmd.method, cmd.params)
}
```

**Wiring (`core/src/tool_host.rs`):**

- `spawn_worker` builds `spec.wall_clock_ms.map(|ms| Watchdog::new(pid, ms))`
  (disarmed) instead of `spawn_watchdog(pid, ms)`.
- Field `_watchdog: Option<WatchdogGuard>` → `watchdog: Option<Watchdog>`.
- `close()` / `Drop` drop the `Watchdog` (thread shutdown), in the same documented
  order as today (after `client`, before `egress`/`scratch`).
- `idle_timeout.rs` is **unchanged** — it keeps passing `entry.wall_clock_ms` into
  the spawn; enforcement simply moved from spawn-time-arm into `call`.

### Properties this guarantees

- **No VM lost mid-process / no mis-timing.** The watchdog is armed *only* while a
  call is in flight, with a fresh `ms` budget per call, and is physically disarmed
  during every idle gap and between calls. There is no deadline ticking that can
  fire on an idle/warm VM. It fires only when a *single* call exceeds
  `wall_clock_ms` — the intended hang protection.
- **No Drop-ordering race.** Disarm is synchronous at `call` return, before any
  `WorkerHandle` drop / warm-slot handoff. (This is why the alternative of owning a
  one-shot guard on `WorkerHandle` was rejected — it would require an explicit
  cancel-before-handoff invariant that a future field reorder could silently break.)
- **No thread churn.** One parked condvar thread per worker, reused across all its
  calls (vs. spawning/cancelling a thread per dispatch).
- **One enforcement site, uniform** across `SingleUse`, `IdleTimeout` warm reuse,
  and the plain `dispatch` path — all funnel through `call`.

### Residual (accepted, documented)

The watchdog now bounds the JSON-RPC **call** window, not the spawn/boot window.
This is arguably more correct (bound work, not setup), and a worker that hangs
during cold boot is already bounded by the spawn path. Noted in the `call` doc
comment.

## Part B — verification

### B1. Hermetic regression (macOS + Linux dev, TDD RED→GREEN)

A fast test that pins the disarm-between-calls property **without** a real VM, using
a real long-lived child via the existing idle-timeout test seam
(`worker_lifecycle_idle_timeout_e2e.rs` patterns):

- Spawn a warm worker whose effective per-call budget is short (e.g. `wall_clock_ms
  ≈ 200 ms`) with a longer idle window.
- Call 1 completes within the budget; release (drop the handle) → worker returns to
  the warm slot.
- Sleep an idle gap **longer than the budget** (e.g. 300 ms) with no call in flight.
- Call 2 on the same warm slot → **worker is alive and reused** (spawn count stays
  1).

Under the **old** one-shot watchdog the worker is dead by call 2 (budget elapsed
from boot); under the re-armable watchdog it survives. This is the RED→GREEN proof
and is backend-agnostic (the bug + fix live in the lifecycle/tool_host layer).

Plus rewritten unit tests for the new `Watchdog` primitive in `watchdog.rs`:
arm→fire-after-budget; disarm-before-budget→no fire; re-arm after a disarm fires on
the *new* deadline; shutdown exits the thread; `is_valid_target_pid` rejections
(the blackout regression test) retained verbatim.

### B2. Firecracker warm/idle e2e (DGX, `#[ignore]`)

New `core/tests/python_exec_firecracker_warm_idle_e2e.rs`, mirroring
`python_exec_warm_idle_e2e.rs` but for `SandboxBackendKind::FirecrackerVm`
(`#![cfg(target_os = "linux")]`, `KASTELLAN_PYTHON_EXEC_USE_MICROVM` path), with a
spawn-counting backend wrapper:

1. **Warm reuse** — 3 acquire→dispatch→release cycles boot the VM **once** (proves
   the vsock bridge + launcher survive multiple sequential JSON-RPC calls on one
   connection).
2. **/tmp wipe across reuse** — a sentinel written under `/tmp` by call 1 is GONE
   for call 2 on the same warm VM (the in-guest #358 wipe).
3. **Idle teardown** — after `idle_seconds` the warm slot clears.
4. **(new) Warm reuse past `wall_clock_ms`** — short per-call budget + longer idle
   window; warm reuse survives across a gap longer than the budget (the Part-A
   regression, non-vacuous under a real VM).

`[SKIP]`-as-pass when KVM / vsock / the rootfs image are absent, matching the
slice-1 e2e's skip discipline.

### B3. Rootfs refresh (DGX setup step)

`scripts/workers/microvm/build-rootfs.sh` cross-builds the worker binary from the
workspace, so the DGX rootfs must be **rebuilt** before the e2e to ship the current
worker (with `wipe_scratch_contents`, #358). Codified as a setup note in the e2e's
skip message (point at `build-rootfs.sh`), same as slice 1's
`KASTELLAN_PYTHON_EXEC_USE_MICROVM` e2e.

**Firecracker e2e gotcha (carried from #362):** rebuild the **release** launcher
(`cargo build --release -p kastellan-microvm-run`) before running the e2e —
`locate_microvm_run()` prefers `target/release` and a stale binary silently shadows
source changes.

## Testing & TDD discipline (rules #1–#2)

- **Pure/hermetic, no KVM (Mac + Linux dev):** the `Watchdog` primitive unit tests
  and the B1 re-arm regression. These are the RED→GREEN gate.
- **DGX only (`#[ignore]`, real KVM + vsock):** the B2 warm/idle e2e.
- Per the codified rule, the per-task Mac gate is `cargo clippy --workspace
  --all-targets -D warnings` (+ Linux cross-clippy for the sandbox crate if any
  linux-cfg code changes — none expected here; the watchdog change is OS-neutral
  core code). Linux acceptance (B2) runs on the DGX.

## Out of scope

- Host-dir sharing (slice 3 — per-spawn RO/overlay ext4 block devices).
- Net workers (slice 4 — egress-proxy UDS over a 2nd vsock).
- Jailer + long-lived/channel workers (slice 5).
- Any change to `SingleUse` semantics beyond the mechanical watchdog-primitive swap
  (behavior stays equivalent: armed during its one call, the worker terminates on
  drop).
