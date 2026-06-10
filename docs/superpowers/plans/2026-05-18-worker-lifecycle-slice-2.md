# Worker Lifecycle Policy — Slice 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fill in `IdleTimeoutLifecycle::acquire`'s `unimplemented!()` body shipped in slice 1. Slice 2 ships the warm-cache runtime: spawn-on-demand, post-completion cap evaluation (`idle_seconds` / `max_requests` / `max_age_seconds`), idle teardown, crash detection, exponential restart backoff, and request serialisation.

**Architecture:** Per-tool warm slot guarded by `tokio::sync::Mutex<ToolState>` (held from `acquire` through `Drop` so concurrent same-tool requests serialise — matches the spec's v1 single-threaded contract). `WorkerHandle` widens to a `WorkerHandleKind` enum so single-use vs idle-timeout Drop semantics diverge cleanly. New module `core::worker_lifecycle::idle_timeout` carries the runtime so `manager.rs` stays small.

**Tech Stack:** Rust 2021, `tokio::sync::{Mutex, OwnedMutexGuard}`, `tokio::time::sleep`, `tokio::spawn`, the existing `kastellan_protocol::client::ClientError` for crash classification, the existing `tool_host::spawn_worker` + `SupervisedWorker` + `ToolHostError`.

---

## Reading list

1. The slice-1 implementation: `core/src/worker_lifecycle/{types.rs, manager.rs, mod.rs}`. Slice 2 extends `manager.rs` and adds `idle_timeout.rs`; the type layer in `types.rs` is unchanged.
2. The design spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`. §"Cap-check semantics" (load-bearing invariant: no mid-flight kills) + §"Supervisor responsibilities" (full runtime contract) + §"Security model" (caveat 2 — persistence-of-compromise across requests).
3. `kastellan_protocol::client::ClientError` variants: `Io`, `Decode`, `EarlyExit`, `IdMismatch`, `Rpc`. Slice 2 classifies the first four as "worker dead" and the last as "worker alive".
4. `core/src/tool_host.rs` — particularly `dispatch`'s return type (`Result<Value, ToolHostError>`) and `SupervisedWorker`'s Drop behaviour (closes stdio, cancels watchdog).
5. `core/src/scheduler/tool_dispatch.rs::ToolHostStepDispatcher::dispatch_step` — slice 2 adds a `handle.report_crash()` call between `dispatch` and `map_dispatch_result`.

## Decisions locked in (from the in-message design block; not revisited per task)

- Warm-cache state lives in `IdleTimeoutLifecycle` itself; ownership shape is `Arc<std::sync::Mutex<HashMap<String, Arc<ToolSlot>>>>` (outer fast lookup) where each `ToolSlot` wraps `tokio::sync::Mutex<ToolState>`.
- Concurrent same-tool acquires serialise via the tokio mutex held from `acquire` through `WorkerHandle::Drop`.
- Crash detection is **passive** — detected on the next dispatch attempt via error-variant classification, not via SIGCHLD.
- Restart backoff is exponential `1, 2, 4, 8, 16, 32, 60, 60, ...` (seconds), capped at 60 s; resets on any successful dispatch.
- Idle teardown is one-shot tasks scheduled on each handle Drop; stale ones no-op.
- The shell-exec binary is the slice-2 integration test fixture's JSON-RPC worker (declared with a custom `ToolEntry` carrying `Lifecycle::IdleTimeout`). The production `shell_exec_entry()` stays `SingleUse` — slice-1's pin test enforces.

## File structure

**New:**
- `core/src/worker_lifecycle/idle_timeout.rs` — the runtime: `IdleTimeoutLifecycle` real impl, `ToolSlot`, `ToolState`, `WarmWorker`, idle-teardown helpers. Estimated 350-450 LOC including tests.
- `core/tests/worker_lifecycle_idle_timeout_e2e.rs` — integration test with the shell-exec worker. Estimated 350-500 LOC.

**Modified:**
- `core/src/worker_lifecycle/manager.rs` — `WorkerHandle` widens to enum; `WorkerHandleKind::IdleTimeout` Drop calls back into `idle_timeout.rs` helpers; `IdleTimeoutLifecycle::new` widens to take sandbox + (optional) `RestartBackoff`; the `unimplemented!()` body is replaced by a delegation to `idle_timeout::acquire_impl`. New `WorkerHandle::report_crash(&mut self)` API.
- `core/src/worker_lifecycle/mod.rs` — re-export `RestartBackoff` (it's part of the public surface so operators tuning workers can name it).
- `core/src/scheduler/tool_dispatch.rs` — `dispatch_step` calls `handle.report_crash()` between `dispatch` and `map_dispatch_result`, gated on the error-variant classifier.
- `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md` — slice-2 entry, tick the ROADMAP slice-2 bullet, advance the "Next pickups" list.

## Test count baseline

Slice 1 left the workspace at **731 passed**. Slice 2 adds ~25-40 tests (5-10 pure unit + 6-10 integration scenarios). Target after slice 2: ~ **760-770**, 0 failed, 0 [SKIP], 0 warnings.

---

## Task 1: Pure helpers — `RestartBackoff`, crash classifier, cap predicates

**Files:**
- Create: `core/src/worker_lifecycle/idle_timeout.rs`
- Modify: `core/src/worker_lifecycle/mod.rs` (re-export `RestartBackoff`)

- [ ] **Step 1: Skeleton + failing tests**

Create `core/src/worker_lifecycle/idle_timeout.rs`:

```rust
//! Idle-timeout lifecycle runtime — slice 2.
//!
//! Spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
//! Plan: `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-2.md`.
//!
//! Slice 2 fills in `IdleTimeoutLifecycle::acquire` (the slice-1 stub) with the
//! warm-cache runtime: spawn-on-demand, post-completion cap evaluation, idle teardown,
//! crash detection, exponential restart backoff, and request serialisation.

use std::time::Duration;

use kastellan_protocol::client::ClientError;

use crate::tool_host::ToolHostError;

/// Exponential restart-backoff calculator.
///
/// `next_delay(n)` is the cooldown between restart attempts — applied to *spawn*, not
/// to dispatch. Sequence (in seconds): `1, 2, 4, 8, 16, 32, 60, 60, …`. Resets to 0 on
/// any successful dispatch. Defaults match the spec's "Open questions" §3 recommendation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartBackoff {
    /// Base delay for the first restart (default 1 s).
    pub base: Duration,
    /// Multiplicative factor between restarts (default 2.0 — exponential).
    /// Stored as integer numerator/denominator to keep the type `Eq`/`Hash`-friendly.
    pub factor_num: u32,
    pub factor_den: u32,
    /// Maximum delay regardless of restart count (default 60 s).
    pub cap: Duration,
}

impl Default for RestartBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            factor_num: 2,
            factor_den: 1,
            cap: Duration::from_secs(60),
        }
    }
}

impl RestartBackoff {
    /// Pure: compute the next delay after `consecutive_restarts` restarts have already
    /// happened. `consecutive_restarts = 0` returns `base`; each subsequent value
    /// multiplies by `factor_num/factor_den`, capped at `cap`. Saturating on overflow.
    pub fn next_delay(&self, consecutive_restarts: u32) -> Duration {
        let base_ms = self.base.as_millis() as u64;
        let factor_num = self.factor_num.max(1) as u64;
        let factor_den = self.factor_den.max(1) as u64;
        let mut delay_ms = base_ms;
        for _ in 0..consecutive_restarts {
            delay_ms = delay_ms.saturating_mul(factor_num) / factor_den;
            if delay_ms >= self.cap.as_millis() as u64 {
                return self.cap;
            }
        }
        Duration::from_millis(delay_ms).min(self.cap)
    }
}

/// Pure: classify a dispatch error as "worker died" or "worker still alive".
///
/// The spec's "Cap-check semantics" §"Mid-flight termination" §2 says a worker process
/// reported dead by the OS triggers restart; v1 slice 2 detects death *passively* on
/// the next dispatch attempt, classifying error variants:
///
/// | Variant                                          | Classification |
/// | ------------------------------------------------ | -------------- |
/// | `Ok(_)`                                          | alive          |
/// | `Err(Sandbox(_))`                                | n/a (no worker) |
/// | `Err(Io(_))`                                     | dead           |
/// | `Err(Protocol(Rpc(_)))`                          | alive (worker rejected the call) |
/// | `Err(Protocol(Io(_)))`                           | dead           |
/// | `Err(Protocol(Decode(_)))`                       | dead           |
/// | `Err(Protocol(EarlyExit))`                       | dead           |
/// | `Err(Protocol(IdMismatch { .. }))`               | dead           |
pub fn dispatch_indicates_worker_dead<T>(result: &Result<T, ToolHostError>) -> bool {
    match result {
        Ok(_) => false,
        Err(ToolHostError::Sandbox(_)) => false, // pre-spawn; no worker to be dead
        Err(ToolHostError::Io(_)) => true,
        Err(ToolHostError::Protocol(ClientError::Rpc(_))) => false,
        Err(ToolHostError::Protocol(_)) => true,
    }
}

/// Pure: has this warm worker hit `max_requests`?
pub fn is_request_capped(request_count: u64, max_requests: u64) -> bool {
    max_requests > 0 && request_count >= max_requests
}

/// Pure: has this warm worker exceeded `max_age_seconds`?
pub fn is_aged_out(age: Duration, max_age_seconds: u64) -> bool {
    max_age_seconds > 0 && age.as_secs() >= max_age_seconds
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_protocol::RpcError;
    use std::io;

    #[test]
    fn restart_backoff_default_starts_at_one_second() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(0), Duration::from_secs(1));
    }

    #[test]
    fn restart_backoff_default_doubles_per_step() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(0), Duration::from_secs(1));
        assert_eq!(bo.next_delay(1), Duration::from_secs(2));
        assert_eq!(bo.next_delay(2), Duration::from_secs(4));
        assert_eq!(bo.next_delay(3), Duration::from_secs(8));
        assert_eq!(bo.next_delay(4), Duration::from_secs(16));
        assert_eq!(bo.next_delay(5), Duration::from_secs(32));
    }

    #[test]
    fn restart_backoff_caps_at_default_60s() {
        let bo = RestartBackoff::default();
        assert_eq!(bo.next_delay(6), Duration::from_secs(60));
        assert_eq!(bo.next_delay(100), Duration::from_secs(60));
        // Saturating on overflow — even u32::MAX is bounded by cap.
        assert_eq!(bo.next_delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn restart_backoff_custom_cap_honoured() {
        let bo = RestartBackoff {
            base: Duration::from_millis(500),
            factor_num: 2,
            factor_den: 1,
            cap: Duration::from_secs(5),
        };
        assert_eq!(bo.next_delay(0), Duration::from_millis(500));
        assert_eq!(bo.next_delay(1), Duration::from_secs(1));
        assert_eq!(bo.next_delay(2), Duration::from_secs(2));
        assert_eq!(bo.next_delay(3), Duration::from_secs(4));
        assert_eq!(bo.next_delay(4), Duration::from_secs(5));
        assert_eq!(bo.next_delay(10), Duration::from_secs(5));
    }

    #[test]
    fn dispatch_classifier_ok_is_alive() {
        let r: Result<(), ToolHostError> = Ok(());
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_rpc_error_is_alive() {
        // Worker returned a structured RPC error; it's still listening on stdio.
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(
            ClientError::Rpc(RpcError {
                code: -32001,
                message: "POLICY_DENIED".into(),
                data: None,
            }),
        ));
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_io_error_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Io(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "stdio closed",
        )));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_protocol_io_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(ClientError::Io(
            io::Error::new(io::ErrorKind::UnexpectedEof, "eof"),
        )));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_early_exit_is_dead() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(
            ClientError::EarlyExit,
        ));
        assert!(dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn dispatch_classifier_sandbox_is_not_a_warm_worker_crash() {
        // Sandbox errors come from a failed spawn — no worker existed; this is the
        // SPAWN_FAILED path, not a warm-worker crash. The classifier returns false so
        // the restart-backoff counter doesn't increment.
        let r: Result<(), ToolHostError> = Err(ToolHostError::Sandbox(
            kastellan_sandbox::SandboxError::Backend("test".into()),
        ));
        assert!(!dispatch_indicates_worker_dead(&r));
    }

    #[test]
    fn is_request_capped_at_threshold() {
        assert!(!is_request_capped(0, 3));
        assert!(!is_request_capped(2, 3));
        assert!(is_request_capped(3, 3));
        assert!(is_request_capped(99, 3));
    }

    #[test]
    fn is_request_capped_zero_max_means_unlimited() {
        // A zero `max_requests` disables the cap (matches the "0 = unlimited" idiom
        // used by `cpu_quota_pct`/`tasks_max` in `SandboxPolicy`).
        assert!(!is_request_capped(u64::MAX, 0));
    }

    #[test]
    fn is_aged_out_at_threshold() {
        assert!(!is_aged_out(Duration::from_secs(9), 10));
        assert!(is_aged_out(Duration::from_secs(10), 10));
        assert!(is_aged_out(Duration::from_secs(11), 10));
    }

    #[test]
    fn is_aged_out_zero_max_means_unlimited() {
        assert!(!is_aged_out(Duration::from_secs(u64::MAX / 2), 0));
    }
}
```

- [ ] **Step 2: Wire idle_timeout into the module**

Edit `core/src/worker_lifecycle/mod.rs`. Add `pub mod idle_timeout;` and widen the re-export list. The final file:

```rust
//! Worker lifecycle policy — slice 1 (single_use runtime + idle_timeout types) plus
//! slice 2 (idle_timeout runtime).
//!
//! See `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` for the
//! design contract.
//!
//! Public surface:
//!   - `Lifecycle`, `IdleTimeoutCaps`, `Contract` — pure value types declarable on a
//!     `ToolEntry`.
//!   - `WorkerLifecycleManager` — async trait that lends out `WorkerHandle`s.
//!   - `SingleUseLifecycle` — spawn-per-request impl.
//!   - `IdleTimeoutLifecycle` — warm-keeping impl (slice 2 fills the runtime).
//!   - `WorkerHandle` — `&mut`-able holder of a live `SupervisedWorker`.
//!   - `RestartBackoff` — operator-tunable exponential backoff configuration.

pub mod idle_timeout;
pub mod manager;
pub mod types;

pub use idle_timeout::RestartBackoff;
pub use manager::{IdleTimeoutLifecycle, SingleUseLifecycle, WorkerHandle, WorkerLifecycleManager};
pub use types::{Contract, IdleTimeoutCaps, Lifecycle, LifecycleValidationError};
```

- [ ] **Step 3: Run the pure-helper tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib worker_lifecycle::idle_timeout 2>&1 | tail -25
```

Expected: 13 tests pass (4 backoff + 5 dispatch-classifier + 2 cap-request + 2 cap-age).

- [ ] **Step 4: Verify workspace builds**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -10
```

Expected: clean build.

- [ ] **Step 5: Commit**

```sh
git add core/src/worker_lifecycle/idle_timeout.rs core/src/worker_lifecycle/mod.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle,idle_timeout): pure helpers — backoff, classifier, cap predicates

Lays the pure-function foundation for slice 2's idle-timeout runtime:

  * `RestartBackoff { base, factor_num, factor_den, cap }` exponential
    calculator. Defaults: 1s base, 2x factor, 60s cap (spec OQ §3).
  * `dispatch_indicates_worker_dead(&Result<_, ToolHostError>)` —
    classifies transport-level vs RPC-level errors. Used by slice-2's
    crash-recovery path; called from the dispatcher between `dispatch`
    and `map_dispatch_result`.
  * `is_request_capped(count, max)` + `is_aged_out(age, max)` — pure
    cap predicates. Both honour the `0 = unlimited` idiom.

13 unit tests. The runtime (`IdleTimeoutLifecycle` real impl, `ToolSlot`,
`WarmWorker`, idle teardown) lands in subsequent commits.

Refs: docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-2.md
EOF
)"
```

---

## Task 2: `ToolState` + `WarmWorker` + `IdleTimeoutLifecycle` skeleton (no acquire yet)

**Files:**
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (add runtime types above the test module)
- Modify: `core/src/worker_lifecycle/manager.rs` (`IdleTimeoutLifecycle::new` widens to take `sandbox` + optional `backoff`; the slice-1 `_private: ()` field is replaced; `acquire` body still calls `unimplemented!()`)

- [ ] **Step 1: Add runtime types in `idle_timeout.rs`**

Insert above the `#[cfg(test)]` block:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};

use crate::scheduler::tool_dispatch::ToolEntry;
use crate::tool_host::{spawn_worker, SupervisedWorker, WorkerSpec};
use crate::worker_lifecycle::types::{IdleTimeoutCaps, Lifecycle};

/// Per-tool slot wrapping `ToolState` in a tokio mutex.
///
/// Held by `Arc` so the warm-cache map can hand out cheap clones for new requests on
/// the same tool. The tokio mutex is locked from `acquire` through `WorkerHandle::Drop`
/// so concurrent requests for the same tool serialise — matches the spec's v1
/// single-threaded contract.
pub(crate) struct ToolSlot {
    pub(crate) state: TokioMutex<ToolState>,
}

/// State the supervisor tracks per warm-keeping tool.
pub(crate) struct ToolState {
    /// `Some` while the worker is warm and idle; `None` while a request is in flight
    /// or after a teardown.
    pub(crate) warm: Option<WarmWorker>,
    /// Wall-clock instant before which the next spawn is *not* allowed (restart
    /// backoff). `None` means "spawn is allowed immediately".
    pub(crate) next_spawn_allowed_at: Option<Instant>,
    /// Counter that drives `RestartBackoff::next_delay`. Increments on every crash;
    /// resets to 0 on every successful dispatch.
    pub(crate) consecutive_restarts: u32,
}

impl ToolState {
    pub(crate) fn fresh() -> Self {
        Self {
            warm: None,
            next_spawn_allowed_at: None,
            consecutive_restarts: 0,
        }
    }
}

/// A warm `SupervisedWorker` plus the bookkeeping the cap evaluators need.
pub(crate) struct WarmWorker {
    pub(crate) worker: SupervisedWorker,
    pub(crate) spawned_at: Instant,
    pub(crate) request_count: u64,
    pub(crate) last_completion: Instant,
    pub(crate) caps: IdleTimeoutCaps,
}

/// Outer warm-cache registry. Keys are tool names (matches the registry in
/// `scheduler::tool_dispatch::ToolRegistry`).
pub(crate) type WarmRegistry = Arc<StdMutex<HashMap<String, Arc<ToolSlot>>>>;

/// Construct a fresh, empty registry.
pub(crate) fn empty_registry() -> WarmRegistry {
    Arc::new(StdMutex::new(HashMap::new()))
}

/// Get or create the slot for `tool_name`. The outer `std` mutex is held very briefly
/// (just the `HashMap::entry` call) so there's no contention even under load.
pub(crate) fn slot_for(registry: &WarmRegistry, tool_name: &str) -> Arc<ToolSlot> {
    let mut map = registry.lock().expect("warm-registry mutex poisoned");
    Arc::clone(map.entry(tool_name.to_string()).or_insert_with(|| {
        Arc::new(ToolSlot {
            state: TokioMutex::new(ToolState::fresh()),
        })
    }))
}
```

- [ ] **Step 2: Widen `IdleTimeoutLifecycle` in `manager.rs`**

Replace the slice-1 stub `IdleTimeoutLifecycle { _private: () }` with:

```rust
/// Idle-timeout lifecycle: warm-keep one worker per tool name; tear down post-completion
/// when any of `idle_seconds` / `max_requests` / `max_age_seconds` fires.
///
/// Slice-2 production impl. The runtime (warm cache, idle teardown, crash recovery,
/// restart backoff) lives in `super::idle_timeout`; this struct is the thin facade
/// `WorkerLifecycleManager` consumers see.
pub struct IdleTimeoutLifecycle {
    sandbox: Arc<dyn SandboxBackend>,
    backoff: super::idle_timeout::RestartBackoff,
    registry: super::idle_timeout::WarmRegistry,
}

impl IdleTimeoutLifecycle {
    /// Construct with default exponential backoff (1s, 2s, 4s, 8s, …, capped at 60s).
    pub fn new(sandbox: Arc<dyn SandboxBackend>) -> Self {
        Self::with_backoff(sandbox, super::idle_timeout::RestartBackoff::default())
    }

    /// Construct with operator-supplied backoff configuration.
    pub fn with_backoff(
        sandbox: Arc<dyn SandboxBackend>,
        backoff: super::idle_timeout::RestartBackoff,
    ) -> Self {
        Self {
            sandbox,
            backoff,
            registry: super::idle_timeout::empty_registry(),
        }
    }
}

#[async_trait]
impl WorkerLifecycleManager for IdleTimeoutLifecycle {
    async fn acquire(&self, _entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        // Tasks 4-7 of the slice-2 plan fill this in. Slice 1 shipped the stub.
        unimplemented!(
            "idle_timeout acquire — slice-2 runtime tasks 4-7"
        )
    }
}
```

Also remove the slice-1 `Default for IdleTimeoutLifecycle` impl (the slice-2 constructor requires a sandbox argument; there's no sensible default).

- [ ] **Step 3: Update the slice-1 manager unit tests that constructed `IdleTimeoutLifecycle::new()` without arguments**

In `manager.rs`'s `#[cfg(test)] mod tests`, replace `IdleTimeoutLifecycle::new()` with `IdleTimeoutLifecycle::new(Arc::from(kastellan_sandbox::default_backend()))`. The `#[should_panic]` test still trips on the new `unimplemented!()` body — adjust the expected panic message to match.

The test now reads:

```rust
    #[tokio::test]
    #[should_panic(expected = "idle_timeout acquire — slice-2 runtime tasks 4-7")]
    async fn idle_timeout_lifecycle_acquire_panics_until_slice_2_completes() {
        let caps = IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 3600,
            grace_period_seconds: 5,
        };
        let contract = Contract { stateless: true };
        let lc = Lifecycle::idle_timeout(caps, contract).expect("valid lifecycle");
        let sandbox: Arc<dyn SandboxBackend> = Arc::from(kastellan_sandbox::default_backend());
        let mgr = IdleTimeoutLifecycle::new(sandbox);
        let entry = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/nope"),
            policy: kastellan_sandbox::SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: lc,
        };
        let _ = mgr.acquire(&entry).await;
    }
```

(Rename the test to `_until_slice_2_completes` since acquire still panics — it gets unstubbed in Task 4.)

- [ ] **Step 4: Run the manager + idle_timeout tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -20
```

Expected: all worker_lifecycle tests pass (6 types + 3 manager + 13 idle_timeout pure helpers = 22).

- [ ] **Step 5: Commit**

```sh
git add core/src/worker_lifecycle/idle_timeout.rs core/src/worker_lifecycle/manager.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle,idle_timeout): runtime types + IdleTimeoutLifecycle widened

Lifts the slice-1 stub `IdleTimeoutLifecycle { _private: () }` into:

  * `IdleTimeoutLifecycle::new(sandbox)` — production constructor; default
    `RestartBackoff`.
  * `IdleTimeoutLifecycle::with_backoff(sandbox, backoff)` — operator-tunable.
  * `ToolSlot` (private to `idle_timeout`) holds `tokio::sync::Mutex<ToolState>`.
  * `ToolState` carries `Option<WarmWorker>`, `next_spawn_allowed_at`,
    `consecutive_restarts`.
  * `WarmWorker` carries the live `SupervisedWorker` + bookkeeping.
  * `WarmRegistry` = outer fast-lookup map; `slot_for(registry, tool_name)` is
    the entry point.

`acquire` still calls `unimplemented!()`; the runtime body lands in tasks 4-7.

The slice-1 `Default for IdleTimeoutLifecycle` is removed — slice-2's
constructor requires a sandbox argument and there's no sensible default.

The slice-1 panic-pin test renames + adjusts the expected message; +0 new
test functions (it's still the same `#[should_panic]`).
EOF
)"
```

---

## Task 3: `WorkerHandle` widens to enum + Drop split

**Files:**
- Modify: `core/src/worker_lifecycle/manager.rs` (`WorkerHandle` struct → enum, Drop impl added)

The enum needs both variants to compile; `IdleTimeout` Drop will call into helpers added in Tasks 5-7. For Task 3 the Drop body for the idle variant is a `todo!()` placeholder + a `#[cfg(not(test))]` panic guard so the test suite doesn't trip on it.

- [ ] **Step 1: Edit `WorkerHandle`**

Replace the slice-1 `pub struct WorkerHandle { worker: SupervisedWorker }` block in `manager.rs` with:

```rust
/// Holder of an exclusively-owned, live `SupervisedWorker` lent out by a lifecycle
/// manager.
///
/// Slice 1 shipped this as a thin newtype around `SupervisedWorker`. Slice 2 widens it
/// to an enum because idle-timeout drop semantics differ structurally from single-use:
///   - `SingleUse`: drop terminates the worker (default behaviour of `SupervisedWorker`).
///   - `IdleTimeout`: drop returns the worker to its warm slot (or terminates if the
///     worker died, the request cap fired, or the worker aged out).
///
/// The enum is private; consumers only see the `worker_mut`, `report_crash`, and
/// `report_success` methods.
pub struct WorkerHandle {
    kind: WorkerHandleKind,
}

enum WorkerHandleKind {
    SingleUse {
        worker: Option<SupervisedWorker>,
    },
    IdleTimeout {
        worker: Option<SupervisedWorker>,
        slot_guard: Option<tokio::sync::OwnedMutexGuard<super::idle_timeout::ToolState>>,
        spawned_at: std::time::Instant,
        request_count_so_far: u64,
        caps: super::types::IdleTimeoutCaps,
        died: bool,
        consecutive_restarts_so_far: u32,
        backoff: super::idle_timeout::RestartBackoff,
    },
}

impl WorkerHandle {
    /// Construct a single-use handle. Module-private — only the lifecycle
    /// implementations in this file can build one.
    pub(crate) fn single_use(worker: SupervisedWorker) -> Self {
        Self {
            kind: WorkerHandleKind::SingleUse {
                worker: Some(worker),
            },
        }
    }

    /// Construct an idle-timeout handle. Module-private.
    pub(crate) fn idle_timeout(
        worker: SupervisedWorker,
        slot_guard: tokio::sync::OwnedMutexGuard<super::idle_timeout::ToolState>,
        spawned_at: std::time::Instant,
        request_count_so_far: u64,
        caps: super::types::IdleTimeoutCaps,
        consecutive_restarts_so_far: u32,
        backoff: super::idle_timeout::RestartBackoff,
    ) -> Self {
        Self {
            kind: WorkerHandleKind::IdleTimeout {
                worker: Some(worker),
                slot_guard: Some(slot_guard),
                spawned_at,
                request_count_so_far,
                caps,
                died: false,
                consecutive_restarts_so_far,
                backoff,
            },
        }
    }

    /// Exclusive `&mut` to the live worker.
    pub fn worker_mut(&mut self) -> &mut SupervisedWorker {
        match &mut self.kind {
            WorkerHandleKind::SingleUse { worker } => worker
                .as_mut()
                .expect("worker_mut called after worker was moved out"),
            WorkerHandleKind::IdleTimeout { worker, .. } => worker
                .as_mut()
                .expect("worker_mut called after worker was moved out"),
        }
    }

    /// Caller signals the dispatch error indicated worker death. For single-use this
    /// is a no-op (the worker exits on drop regardless). For idle-timeout this
    /// suppresses the worker-return path so the dead worker isn't put back into the
    /// slot.
    pub fn report_crash(&mut self) {
        if let WorkerHandleKind::IdleTimeout { died, .. } = &mut self.kind {
            *died = true;
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        match &mut self.kind {
            WorkerHandleKind::SingleUse { worker } => {
                // Take + drop. `SupervisedWorker`'s own Drop closes stdio + cancels
                // the watchdog. Byte-equivalent to slice 1.
                drop(worker.take());
            }
            WorkerHandleKind::IdleTimeout {
                worker,
                slot_guard,
                spawned_at,
                request_count_so_far,
                caps,
                died,
                consecutive_restarts_so_far,
                backoff,
            } => {
                let worker_opt = worker.take();
                let guard = slot_guard.take().expect("slot_guard absent in idle-timeout Drop");
                super::idle_timeout::release_idle_timeout_worker(
                    worker_opt,
                    guard,
                    *spawned_at,
                    *request_count_so_far,
                    caps.clone(),
                    *died,
                    *consecutive_restarts_so_far,
                    *backoff,
                );
            }
        }
    }
}
```

- [ ] **Step 2: Add the `release_idle_timeout_worker` helper stub in `idle_timeout.rs`**

The Drop impl calls `super::idle_timeout::release_idle_timeout_worker`, but that helper doesn't exist yet. Add a `todo!()` stub in `idle_timeout.rs` so the build compiles:

```rust
/// Release path for `WorkerHandle::Drop` on an idle-timeout handle.
///
/// Slice-2 task 5 implements: increment request count, check caps, either put worker
/// back into the slot or terminate it, schedule idle-teardown task.
/// Task 7 adds the crash branch (`died = true` → backoff + restart-counter update).
///
/// This stub `todo!()`s so a Drop that reaches it in slice-2 development (before tasks
/// 5+ land) trips loudly with a useful message.
pub(crate) fn release_idle_timeout_worker(
    _worker: Option<SupervisedWorker>,
    _guard: OwnedMutexGuard<ToolState>,
    _spawned_at: Instant,
    _request_count_so_far: u64,
    _caps: IdleTimeoutCaps,
    _died: bool,
    _consecutive_restarts_so_far: u32,
    _backoff: RestartBackoff,
) {
    todo!("release_idle_timeout_worker — slice-2 task 5 (caps) + task 7 (crash branch)")
}
```

- [ ] **Step 3: Verify workspace builds + slice-1 tests still pass**

```sh
source "$HOME/.cargo/env"
cargo build --workspace --tests 2>&1 | tail -10
cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -10
```

Expected: clean build; all 22 worker_lifecycle tests pass (the `#[should_panic]` on idle-timeout acquire still trips on its `unimplemented!()` body before Drop runs, so the `release_idle_timeout_worker` stub isn't reached).

- [ ] **Step 4: Commit**

```sh
git add core/src/worker_lifecycle/manager.rs core/src/worker_lifecycle/idle_timeout.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle): WorkerHandle widens to enum (SingleUse | IdleTimeout)

`WorkerHandle` is now an enum so single-use and idle-timeout Drop semantics
diverge cleanly:

  * `WorkerHandleKind::SingleUse { worker }` — Drop drops the worker
    (terminates). Byte-equivalent to slice 1.
  * `WorkerHandleKind::IdleTimeout { worker, slot_guard, … }` — Drop
    delegates to `idle_timeout::release_idle_timeout_worker`.

New API: `WorkerHandle::report_crash(&mut self)` — caller (the dispatcher,
post-`dispatch`) signals that the error variant indicates transport death.
Single-use is a no-op; idle-timeout suppresses the worker-return path on
Drop.

The `release_idle_timeout_worker` helper is a `todo!()` stub. Task 5
(caps) + task 7 (crash branch) fill it in. The slice-1 `#[should_panic]`
test on `IdleTimeoutLifecycle::acquire` still trips before Drop runs, so
the stub isn't reached in tests.
EOF
)"
```

---

## Task 4: `IdleTimeoutLifecycle::acquire` happy path (warm reuse + cold spawn)

**Files:**
- Modify: `core/src/worker_lifecycle/manager.rs` (replace the `unimplemented!()` in `IdleTimeoutLifecycle::acquire`)
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (add `acquire_impl` helper)

The acquire logic:

1. Pull `Lifecycle::IdleTimeout { caps, .. }` out of the entry (return an error if the entry doesn't carry this — programmer error).
2. Look up or create the per-tool `Arc<ToolSlot>`.
3. Lock the slot's tokio mutex with `lock_owned()` (await — concurrent same-tool acquires serialise here).
4. If `state.next_spawn_allowed_at` is in the future, await `sleep_until(allowed_at)`. (Restart backoff slept *inside* the lock so other callers can't bypass it.)
5. If `state.warm` is `Some(warm)`:
    - Check `is_aged_out(now - warm.spawned_at, caps.max_age_seconds)`. If aged, drop the warm worker (terminates) and fall through to spawn.
    - Otherwise, take the warm worker out of the slot; build the handle with `(worker, guard, spawned_at, request_count, caps, consecutive_restarts, backoff)`.
6. If `state.warm` is `None`, spawn fresh via `spawn_worker(self.sandbox.as_ref(), &spec)`. On `Ok`, build the handle. On `Err`, drop the guard (slot stays empty) and propagate the error — `ToolHostStepDispatcher::dispatch_step` already emits `step.spawn_failed` for this.

**`grace_period_seconds`** is not consumed in slice 2 — `WarmWorker` carries the field for forward compatibility; the spec's SIGTERM-grace teardown is slice 3+.

- [ ] **Step 1: Implement `acquire_impl` in `idle_timeout.rs`**

Add:

```rust
use kastellan_sandbox::SandboxBackend;
use tokio::time::sleep;

use crate::tool_host::ToolHostError;
use crate::worker_lifecycle::manager::WorkerHandle;

/// Implementation of `IdleTimeoutLifecycle::acquire`. Public-in-crate so the
/// `manager.rs` facade can delegate without exposing the runtime types.
pub(crate) async fn acquire_impl(
    sandbox: &dyn SandboxBackend,
    backoff: RestartBackoff,
    registry: &WarmRegistry,
    entry: &ToolEntry,
) -> Result<WorkerHandle, ToolHostError> {
    // Caps extraction: this code path is only reachable when the dispatcher dispatches
    // through `IdleTimeoutLifecycle`, which is only wired up when the entry's
    // `lifecycle` is `IdleTimeout`. A `SingleUse` entry reaching here is a wiring bug.
    let (caps, _contract) = match &entry.lifecycle {
        Lifecycle::IdleTimeout { caps, contract } => (caps.clone(), contract.clone()),
        Lifecycle::SingleUse => {
            return Err(ToolHostError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IdleTimeoutLifecycle::acquire called on a SingleUse ToolEntry — wiring bug",
            )));
        }
    };

    let tool_name = entry
        .binary
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.binary.to_string_lossy().into_owned());

    let slot = slot_for(registry, &tool_name);
    let mut guard = slot.state.clone().lock_owned().await;

    // Honor restart backoff. If `next_spawn_allowed_at` is in the future, sleep until
    // it. Reset it once we've waited so the next caller doesn't re-wait.
    if let Some(allowed_at) = guard.next_spawn_allowed_at {
        let now = Instant::now();
        if allowed_at > now {
            let to_sleep = allowed_at - now;
            sleep(to_sleep).await;
        }
        guard.next_spawn_allowed_at = None;
    }

    // Warm-reuse path.
    if let Some(existing) = guard.warm.take() {
        if !is_aged_out(existing.spawned_at.elapsed(), caps.max_age_seconds) {
            let spawned_at = existing.spawned_at;
            let request_count_so_far = existing.request_count;
            let consecutive_restarts_so_far = guard.consecutive_restarts;
            return Ok(WorkerHandle::idle_timeout(
                existing.worker,
                guard,
                spawned_at,
                request_count_so_far,
                caps,
                consecutive_restarts_so_far,
                backoff,
            ));
        }
        // Aged out — drop the worker (terminates) and fall through to spawn fresh.
        drop(existing.worker);
    }

    // Cold-spawn path.
    let policy = entry.policy.clone();
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
    let worker = spawn_worker(sandbox, &spec)?;
    let spawned_at = Instant::now();
    let consecutive_restarts_so_far = guard.consecutive_restarts;
    Ok(WorkerHandle::idle_timeout(
        worker,
        guard,
        spawned_at,
        0,
        caps,
        consecutive_restarts_so_far,
        backoff,
    ))
}
```

- [ ] **Step 2: Delegate from `manager.rs`**

Replace the `unimplemented!()` body of `IdleTimeoutLifecycle::acquire` with:

```rust
    async fn acquire(&self, entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        super::idle_timeout::acquire_impl(
            self.sandbox.as_ref(),
            self.backoff,
            &self.registry,
            entry,
        )
        .await
    }
```

- [ ] **Step 3: Delete the slice-1 `#[should_panic]` test**

The acquire body no longer panics; the test was a temporary placeholder. Delete `idle_timeout_lifecycle_acquire_panics_until_slice_2_completes` from `manager.rs`'s test module. Replace it with a test that pins the wiring-bug path:

```rust
    #[tokio::test]
    async fn idle_timeout_acquire_on_single_use_entry_returns_wiring_error() {
        // Defensive: an idle-timeout manager called with a single-use entry is a
        // wiring bug. The manager returns an `Io(InvalidInput)` error rather than
        // panicking so the dispatcher's `step.spawn_failed` audit row still fires.
        let sandbox: Arc<dyn SandboxBackend> = Arc::from(kastellan_sandbox::default_backend());
        let mgr = IdleTimeoutLifecycle::new(sandbox);
        let entry = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/nope"),
            policy: kastellan_sandbox::SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: Lifecycle::SingleUse,
        };
        let r = mgr.acquire(&entry).await;
        assert!(r.is_err(), "must return Err on wiring bug");
    }
```

- [ ] **Step 4: Run worker_lifecycle tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -10
```

Expected: pure-helper tests pass; the new `idle_timeout_acquire_on_single_use_entry_returns_wiring_error` passes; slice-1 panic-pin test deleted.

- [ ] **Step 5: Commit**

```sh
git add core/src/worker_lifecycle/idle_timeout.rs core/src/worker_lifecycle/manager.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle,idle_timeout): acquire happy path — warm reuse + cold spawn

Replaces the `unimplemented!()` body in `IdleTimeoutLifecycle::acquire`
with the slice-2 acquire flow:

  1. Pull caps from entry's `Lifecycle::IdleTimeout`; reject `SingleUse`
     entries as wiring bugs (return `Io(InvalidInput)` so the dispatcher's
     `step.spawn_failed` audit row still fires).
  2. `slot_for(registry, tool_name)` looks up or creates the per-tool
     `Arc<ToolSlot>` (outer `std::sync::Mutex` map; brief critical section).
  3. `slot.state.clone().lock_owned().await` — concurrent same-tool acquires
     serialise here.
  4. Honor `next_spawn_allowed_at` (restart backoff): sleep until allowed,
     then clear the gate.
  5. Warm-reuse: if `state.warm.is_some()` and not aged out, return the
     warm worker. If aged out, drop it (terminates) and fall through to spawn.
  6. Cold-spawn: `spawn_worker(sandbox, &spec)`; build the handle with the
     guard held.

The slice-1 `#[should_panic]` test is deleted; a new test pins the wiring-
bug path (idle-timeout manager + single-use entry → Err).

Task 5 (cap evaluation in Drop) + task 7 (crash branch) consume this flow's
output via `release_idle_timeout_worker` which is still a `todo!()` stub.
EOF
)"
```

---

## Task 5: Cap evaluation in `release_idle_timeout_worker` (max_requests + max_age)

**Files:**
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (`release_idle_timeout_worker` body — caps subset; crash + idle-teardown branches still stubbed)

- [ ] **Step 1: Replace the `todo!()` stub with the cap subset**

```rust
pub(crate) fn release_idle_timeout_worker(
    worker: Option<SupervisedWorker>,
    mut guard: OwnedMutexGuard<ToolState>,
    spawned_at: Instant,
    request_count_so_far: u64,
    caps: IdleTimeoutCaps,
    died: bool,
    _consecutive_restarts_so_far: u32,
    _backoff: RestartBackoff,
) {
    let Some(worker) = worker else {
        // Worker was already moved out; nothing to do. This branch shouldn't fire in
        // practice — the Drop impl always passes Some — but a missing worker is
        // strictly safer than a panic in Drop.
        return;
    };

    // Crash branch lands in task 7. For task 5 a `died = true` worker still drops
    // (terminates) here, just without the backoff bookkeeping.
    if died {
        drop(worker);
        guard.warm = None;
        return;
    }

    let new_count = request_count_so_far + 1;

    // Cap A: max_requests (post-completion check). Spec §"The two policies" §"idle_timeout".
    if is_request_capped(new_count, caps.max_requests) {
        drop(worker);
        guard.warm = None;
        return;
    }

    // Cap B: max_age_seconds (post-completion check). Same load-bearing invariant:
    // checked after the response was written, never mid-flight.
    if is_aged_out(spawned_at.elapsed(), caps.max_age_seconds) {
        drop(worker);
        guard.warm = None;
        return;
    }

    // Successful return: put the worker back into the slot, refresh `last_completion`.
    guard.warm = Some(WarmWorker {
        worker,
        spawned_at,
        request_count: new_count,
        last_completion: Instant::now(),
        caps,
    });

    // Cap C (idle_seconds) is enforced by a separately-spawned teardown task — see
    // task 6. Successful dispatch also resets the restart counter — see task 7.
}
```

- [ ] **Step 2: Verify slice-1 + slice-2-so-far tests pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -10
```

Expected: all tests pass; release path no longer `todo!()`s.

- [ ] **Step 3: Commit**

```sh
git add core/src/worker_lifecycle/idle_timeout.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle,idle_timeout): release-path cap evaluation (max_requests + max_age)

Fills in two of the three caps from spec §"Cap-check semantics":

  * `max_requests` — increment count post-completion; if >= cap, drop the
    worker (terminates) and clear the slot.
  * `max_age_seconds` — measure wall-clock from spawn_at; if past cap,
    same teardown shape.

On the happy path the worker is put back into the slot with refreshed
`last_completion`. The `died = true` branch terminates immediately
without backoff bookkeeping (task 7 adds the backoff arithmetic).

The third cap (`idle_seconds`) is enforced by a separately-spawned
teardown task — see task 6.
EOF
)"
```

---

## Task 6: Idle teardown timer

**Files:**
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (add `schedule_idle_teardown` + spawn it from the happy-path branch in `release_idle_timeout_worker`)

The teardown task captures `Arc<ToolSlot>` + `last_completion` (the timestamp recorded when this release happened). It sleeps for `caps.idle_seconds`, then locks the slot's tokio mutex, and tears down the worker only if `state.warm.is_some()` AND `state.warm.last_completion == captured_last_completion` (no newer release happened in between).

- [ ] **Step 1: Add the teardown task helper**

In `idle_timeout.rs`:

```rust
/// Spawn a one-shot teardown task that fires `idle_seconds` after `for_last_completion`.
///
/// The task re-acquires the slot's mutex, compares `state.warm`'s `last_completion`
/// against the captured value; if they match the worker has been idle since this
/// release and is torn down. If they differ, a newer request bumped the timestamp and
/// the task is a no-op.
///
/// Multiple stale teardown tasks coexist harmlessly: only the newest one's captured
/// `last_completion` matches the current slot state.
pub(crate) fn schedule_idle_teardown(
    slot: Arc<ToolSlot>,
    for_last_completion: Instant,
    idle_seconds: u64,
) {
    if idle_seconds == 0 {
        // 0 = idle teardown disabled; spec uses non-zero `idle_seconds` as the
        // canonical opt-in.
        return;
    }
    let delay = Duration::from_secs(idle_seconds);
    tokio::spawn(async move {
        sleep(delay).await;
        let mut state = slot.state.lock().await;
        if let Some(warm) = &state.warm {
            if warm.last_completion == for_last_completion {
                // Take + drop the warm worker. `SupervisedWorker`'s own Drop closes
                // stdio + cancels the watchdog; the OS will reap the zombie on next
                // wait/spawn cycle.
                state.warm = None;
            }
        }
    });
}
```

- [ ] **Step 2: Wire `schedule_idle_teardown` into `release_idle_timeout_worker`**

In the happy-path branch (the `guard.warm = Some(...)` block), after assigning, also schedule the teardown. We need the `Arc<ToolSlot>` — but the `OwnedMutexGuard<ToolState>` only gives us a guard, not the surrounding `Arc`. The cleanest fix is to pass the `Arc<ToolSlot>` into `release_idle_timeout_worker` alongside the guard.

Update the helper signature to accept it; update the Drop impl in `manager.rs` to pass it through; update `acquire_impl` to stash the `Arc<ToolSlot>` clone alongside the guard in the handle.

Concretely:

In `manager.rs`'s `WorkerHandleKind::IdleTimeout` variant, add a `slot: Option<Arc<super::idle_timeout::ToolSlot>>` field. The constructor `WorkerHandle::idle_timeout` takes it. The Drop impl passes both `guard` and `slot` to `release_idle_timeout_worker`.

In `idle_timeout.rs`'s `acquire_impl`, when building the handle, pass `Arc::clone(&slot)` (the `Arc<ToolSlot>` from `slot_for`) into the handle constructor.

In `release_idle_timeout_worker`, widen the signature to take `slot: Option<Arc<ToolSlot>>` (the `Option` matches the field's takeability). At the happy-path branch, the new `last_completion` is `Instant::now()` — capture it before storing, then call `schedule_idle_teardown(slot, last_completion, caps.idle_seconds)`.

Updated happy-path branch:

```rust
    let last_completion = Instant::now();
    guard.warm = Some(WarmWorker {
        worker,
        spawned_at,
        request_count: new_count,
        last_completion,
        caps: caps.clone(),
    });
    drop(guard); // release the per-tool mutex before spawning teardown
    if let Some(slot) = slot {
        schedule_idle_teardown(slot, last_completion, caps.idle_seconds);
    }
```

- [ ] **Step 3: Make sure the slot is held in the handle**

Verify `WorkerHandleKind::IdleTimeout` has both `slot_guard: Option<OwnedMutexGuard<ToolState>>` and `slot: Option<Arc<ToolSlot>>` (slot is needed for the teardown spawn). The `slot_guard` is the locked guard; `slot` is the surrounding Arc — both are needed.

- [ ] **Step 4: Run worker_lifecycle tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -10
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```sh
git add core/src/worker_lifecycle/idle_timeout.rs core/src/worker_lifecycle/manager.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle,idle_timeout): idle teardown via spawned one-shot task

On every successful release, schedule a `tokio::spawn`'d task that sleeps
`caps.idle_seconds` then re-acquires the slot's mutex. If
`state.warm.last_completion` still matches the captured value (no newer
request bumped it), the warm worker is dropped (terminated).

Stale teardown tasks coexist harmlessly: only the newest one's captured
timestamp matches the slot's current state.

Required signature changes:
  * `release_idle_timeout_worker` widens to take `slot: Option<Arc<ToolSlot>>`.
  * `WorkerHandleKind::IdleTimeout` carries both `slot_guard: OwnedMutexGuard`
    (held until Drop) and `slot: Arc<ToolSlot>` (the surrounding Arc, needed
    for the teardown spawn).
  * `acquire_impl` clones the `Arc<ToolSlot>` from `slot_for` into the handle.

`caps.idle_seconds == 0` disables idle teardown — the canonical "infinite
warm-keep" opt-in shape.
EOF
)"
```

---

## Task 7: Crash detection + restart backoff

**Files:**
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (`release_idle_timeout_worker` — wire the `died = true` branch through `RestartBackoff::next_delay` + bump `consecutive_restarts`)
- Modify: `core/src/scheduler/tool_dispatch.rs` (call `handle.report_crash()` between `dispatch` and `map_dispatch_result`)

- [ ] **Step 1: Wire crash + backoff in `release_idle_timeout_worker`**

Replace the slice-5 stub crash branch:

```rust
    if died {
        drop(worker);
        guard.warm = None;
        // Bump consecutive_restarts and schedule next-spawn-allowed-at via backoff.
        let next_count = guard.consecutive_restarts.saturating_add(1);
        let delay = backoff.next_delay(next_count.saturating_sub(1));
        guard.consecutive_restarts = next_count;
        guard.next_spawn_allowed_at = Some(Instant::now() + delay);
        return;
    }

    // Happy path also resets the restart counter (a clean dispatch means the system
    // is in a steady state).
    guard.consecutive_restarts = 0;
    guard.next_spawn_allowed_at = None;
```

The successful-completion reset of `consecutive_restarts` is what makes the backoff sequence restart from `base` after one good dispatch.

- [ ] **Step 2: Plumb `report_crash` through the dispatcher**

In `core/src/scheduler/tool_dispatch.rs`, between the `dispatch(...)` call and `map_dispatch_result(result)`, insert the classifier:

```rust
        let result = dispatch(
            &self.pool,
            handle.worker_mut(),
            &step.tool,
            &step.method,
            step.parameters.clone(),
        )
        .await;

        // Slice 2: signal to the lifecycle manager whether the worker survived. For
        // single-use this is a no-op; for idle-timeout it suppresses the worker-return
        // path so the dead worker isn't put back into the warm slot. Classified using
        // the protocol-error variant — transport-level failures (`Io`, `Decode`,
        // `EarlyExit`, `IdMismatch`) indicate the worker died; `Rpc(_)` errors mean the
        // worker rejected the call but is alive.
        if crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead(&result) {
            handle.report_crash();
        }

        // Drop closes stdio + cancels the watchdog. We don't call
        // `worker.close()` explicitly so a panic above (currently
        // unreachable, but kept defensive) still cleans up. For
        // `SingleUseLifecycle`, dropping the handle drops the inner
        // `SupervisedWorker`; for `IdleTimeoutLifecycle`, Drop hands the
        // worker back to the warm slot (or terminates it if `report_crash`
        // was called).
        drop(handle);

        map_dispatch_result(result)
```

- [ ] **Step 3: Run worker_lifecycle tests + dispatcher tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -10
cargo test -p kastellan-core --lib scheduler::tool_dispatch 2>&1 | tail -10
```

Expected: all green.

- [ ] **Step 4: Commit**

```sh
git add core/src/worker_lifecycle/idle_timeout.rs core/src/scheduler/tool_dispatch.rs
git commit -m "$(cat <<'EOF'
feat(core,worker_lifecycle,idle_timeout,scheduler): crash detection + restart backoff

Closes the runtime loop of slice 2:

  * `release_idle_timeout_worker`'s `died = true` branch now bumps
    `consecutive_restarts` and sets `next_spawn_allowed_at = now + backoff.next_delay`.
    The exponential delay (1s, 2s, 4s, …, capped at 60s by default) is applied
    *inside* the next acquire's locked critical section so no caller can bypass it.
  * Happy-path release resets `consecutive_restarts = 0` and clears
    `next_spawn_allowed_at` — one clean dispatch is enough to restart the
    backoff sequence from base.
  * `ToolHostStepDispatcher::dispatch_step` calls
    `handle.report_crash()` between `dispatch` and `map_dispatch_result`,
    gated on `dispatch_indicates_worker_dead(&result)`. For single-use
    this is a no-op; for idle-timeout it suppresses the worker-return path.
EOF
)"
```

---

## Task 8: Integration test — `worker_lifecycle_idle_timeout_e2e.rs`

**Files:**
- Create: `core/tests/worker_lifecycle_idle_timeout_e2e.rs`

The test uses the existing `kastellan-worker-shell-exec` binary as the JSON-RPC stdio worker (since shell-exec is the only worker currently in the tree). It builds a custom `ToolEntry` declaring `Lifecycle::IdleTimeout { caps: {…} }` rather than going through `shell_exec_entry()` (which is single-use-pinned by the slice-1 test).

Scenarios:

1. **Warm reuse pin** — `idle_seconds=60`, `max_requests=10`, `max_age_seconds=60`. Acquire 3 times sequentially against the same tool name; assert PID is identical across all 3 acquires.
2. **`max_requests` rotation** — `max_requests=2`. 3 acquires; assert PID changes between acquire 2 and acquire 3.
3. **`max_age_seconds` rotation** — `max_age_seconds=1`, sleep 1.5 s between acquire 1 and acquire 2; assert PIDs differ.
4. **`idle_seconds` teardown** — `idle_seconds=1`; acquire-release once, sleep 2 s, observe state.warm is None. (Asserting via a test-only accessor on `IdleTimeoutLifecycle` — see Step 1.)
5. **Crash recovery** — call `handle.report_crash()` directly (mimics what the dispatcher would do on a transport error); acquire-release; sleep through backoff (set `backoff.base = 100ms`, `cap = 200ms` so the test is fast); next acquire spawns fresh.
6. **Concurrent acquires serialise** — spawn 2 tokio tasks both calling acquire+release; record acquire-completion timestamps; assert they don't overlap (the second one waited for the first).

This is a lot — and these integration tests need real bwrap on Linux + the sandbox setup. Use `tests-common::skip_if_sandbox_unavailable()` to skip cleanly on hosts without the sandbox.

- [ ] **Step 1: Add a test-only accessor on `IdleTimeoutLifecycle`**

For scenarios 4 and 6 we need to peek at the warm-cache state. Add this `#[cfg(test)]` method in `manager.rs`:

```rust
impl IdleTimeoutLifecycle {
    /// Test-only inspector: returns whether the slot for `tool_name` has a warm worker.
    /// Used by `worker_lifecycle_idle_timeout_e2e.rs` to pin idle teardown + crash
    /// recovery semantics without depending on PID introspection alone.
    #[doc(hidden)]
    pub async fn _test_slot_has_warm(&self, tool_name: &str) -> bool {
        let map = self.registry.lock().expect("warm-registry mutex poisoned");
        let Some(slot) = map.get(tool_name) else {
            return false;
        };
        let slot = Arc::clone(slot);
        drop(map);
        let state = slot.state.lock().await;
        state.warm.is_some()
    }
}
```

- [ ] **Step 2: Write the integration test**

(Plan-level note: this file is the largest of slice 2's deliverables. The implementer should structure it as one `#[tokio::test]` per scenario, each handling its own PG cluster + sandbox bring-up via `tests-common`. Estimated 350-500 LOC. Use the `scheduler_step_dispatch_e2e.rs` test as the structural precedent for PG + sandbox setup.)

Concrete skeleton (the body of each `#[tokio::test]` follows the pattern shown — actual full code is omitted here for brevity but the implementer should mirror the precedent file):

```rust
//! Integration tests for `IdleTimeoutLifecycle` (slice 2 runtime).
//!
//! Uses `kastellan-worker-shell-exec` as the JSON-RPC stdio worker (the only worker
//! shipping today). Each test constructs its own `ToolEntry` declaring
//! `Lifecycle::IdleTimeout` — the production `shell_exec_entry()` stays single-use
//! per the slice-1 pin.

#![cfg(not(any()))]  // Always compile; the bring-up dance skips at runtime.

use std::sync::Arc;
use std::time::Duration;

use kastellan_core::worker_lifecycle::{
    IdleTimeoutCaps, IdleTimeoutLifecycle, Lifecycle, RestartBackoff, WorkerLifecycleManager,
};
use kastellan_sandbox::SandboxBackend;
use kastellan_tests_common::{
    pg_bin_dir_or_skip, policy_for_shell_exec, skip_if_sandbox_unavailable,
    workspace_target_binary, // ... and other helpers as needed
};

fn idle_timeout_entry(
    binary: std::path::PathBuf,
    caps: IdleTimeoutCaps,
) -> kastellan_core::scheduler::ToolEntry {
    // Build a ToolEntry like shell_exec_entry but declaring IdleTimeout.
    // (Concrete impl: same body as shell_exec_entry except for the lifecycle field.)
    todo!("see file: pattern-copy shell_exec_entry but swap lifecycle field")
}

#[tokio::test]
async fn warm_reuse_three_acquires_same_pid() {
    // ... bring up sandbox + lifecycle manager + entry with idle_seconds=60.
    // Acquire 3 times, capture worker PID from each handle (via a `_test_pid()`
    // accessor on WorkerHandle, gated to cfg(test)). Assert all three match.
}

#[tokio::test]
async fn max_requests_rotates_worker_after_cap() {
    // caps: max_requests = 2. Acquire 3 times; first two PIDs match, third differs.
}

#[tokio::test]
async fn max_age_rotates_worker_after_cap() {
    // caps: max_age_seconds = 1. Acquire; sleep 1.5s; acquire again; assert PIDs differ.
}

#[tokio::test]
async fn idle_teardown_clears_warm_slot() {
    // caps: idle_seconds = 1. Acquire + release; sleep 2s; assert
    // `lifecycle._test_slot_has_warm(tool_name).await == false`.
}

#[tokio::test]
async fn crash_recovery_with_backoff_spawns_fresh_worker() {
    // backoff: base = 100ms, cap = 200ms (fast). Acquire; call handle.report_crash();
    // release. Sleep 250ms (longer than backoff). Acquire; assert PID differs from
    // the dead worker.
}

#[tokio::test]
async fn concurrent_acquires_serialize_on_same_tool() {
    // Two parallel tokio tasks, each acquire + 50ms sleep + release. Measure when
    // each task's acquire completed. Assert task 2's completion came AFTER task 1's
    // release.
}
```

The implementer fills in the body using the pattern from `scheduler_step_dispatch_e2e.rs` for sandbox + PG + binary discovery, plus the existing `WorkerHandle::worker_mut()` to call `dispatch` for the warm-reuse PID assertion. Test-only `WorkerHandle::_test_pid()` accessor extracts the PID from `SupervisedWorker` (already exposed as `pub fn pid()` or similar — verify against `core/src/tool_host.rs`).

- [ ] **Step 3: Run the integration test**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test worker_lifecycle_idle_timeout_e2e 2>&1 | tail -25
```

Expected: 6 tests pass on Linux with bwrap AppArmor profile installed; clean skips on hosts without.

- [ ] **Step 4: Commit**

```sh
git add core/tests/worker_lifecycle_idle_timeout_e2e.rs core/src/worker_lifecycle/manager.rs
git commit -m "$(cat <<'EOF'
test(core,worker_lifecycle,idle_timeout): integration coverage for slice-2 runtime

New `worker_lifecycle_idle_timeout_e2e.rs` exercises all six runtime
behaviours from spec §"Cap-check semantics" + §"Supervisor responsibilities":

  1. warm-reuse — 3 sequential acquires return the same PID
  2. max_requests rotation — caps cause respawn after the cap fires
  3. max_age rotation — wall-clock cap forces respawn
  4. idle teardown — `idle_seconds` clears the warm slot
  5. crash recovery — `report_crash` + backoff + clean restart
  6. concurrent serialisation — same-tool acquires await the per-slot mutex

Uses `kastellan-worker-shell-exec` as the JSON-RPC stdio worker (a custom
`ToolEntry` declaring `IdleTimeout`; the production `shell_exec_entry()`
stays single-use per the slice-1 pin).

Adds `IdleTimeoutLifecycle::_test_slot_has_warm` (`#[doc(hidden)]`) for the
idle-teardown observation pin.

Skips cleanly on hosts without bwrap (Linux) / Seatbelt (macOS).
EOF
)"
```

---

## Task 9: Full workspace regression

- [ ] **Step 1: Run `cargo test --workspace`**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '
{
  for (i=1; i<=NF; i++) {
    if ($i == "passed;") p += $(i-1);
    if ($i == "failed;") f += $(i-1);
    if ($i == "ignored;") ig += $(i-1);
  }
}
END { printf "Total: %d passed, %d failed, %d ignored\n", p, f, ig }
'
```

Expected: ~760-770 passed, 0 failed, 4 ignored (baseline 731 + 13 idle_timeout pure helpers + 1 dispatcher classifier integration + 6 e2e + a few smaller).

- [ ] **Step 2: Confirm zero warnings + zero SKIP**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | grep -c "warning:"
cargo test --workspace -- --nocapture 2>&1 | grep -c "\[SKIP\]"
```

Expected: 0 + 0.

- [ ] **Step 3: Spot-check slice-1 regression pin (the most important e2e)**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test cli_ask_e2e 2>&1 | tail -10
```

Expected: both `cli_ask_e2e` scenarios still pass — shell-exec is single-use, so slice-2 changed nothing in its observable behaviour.

---

## Task 10: HANDOVER + ROADMAP + commit + push + open PR

- [ ] **Step 1: Update HANDOVER.md header**

Bump `**Last updated:**` to mention slice 2 alongside slice 1. Update `**Session-end verification:**` to the new test count. Add a "Recently completed (this session — slice 2)" entry near the top.

- [ ] **Step 2: Tick the ROADMAP slice-2 bullet**

In `docs/devel/ROADMAP.md`, change the slice-2 bullet from `- [ ]` to `- [x]` with branch + commit + test-count details. The "Next pickups" list rolls forward (slice 3 is the SIGTERM grace + `kastellan-cli supervisor status` + worker manifest plumbing, OR jump straight to GLiNER-Relex as the first idle-timeout consumer).

- [ ] **Step 3: Commit docs**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): worker lifecycle slice 2 — idle_timeout runtime

Records slice 2 of the worker-lifecycle work shipped this session on
branch `feat/worker-lifecycle-slice-1` (now carrying both slices for a
single PR):

  * `IdleTimeoutLifecycle::acquire` filled in: warm cache, slot mutex
    serialisation, restart backoff (exponential, cap 60s default), cap
    evaluation (max_requests, max_age_seconds, idle_seconds), crash
    detection via post-dispatch error classifier.
  * `WorkerHandle` widened to enum so single-use and idle-timeout drop
    semantics diverge cleanly.
  * `ToolHostStepDispatcher::dispatch_step` calls `handle.report_crash()`
    between dispatch and result mapping.
  * 13 pure-helper unit tests + 6 integration scenarios via shell-exec
    declared as IdleTimeout (production shell_exec_entry stays SingleUse).
  * Test count 731 → <NUMBER>.

ROADMAP ticks slice 2; opens a new unchecked slice-3 bullet (SIGTERM grace,
worker manifest plumbing, operator status surface).
EOF
)"
```

- [ ] **Step 4: Push the branch + open PR**

```sh
git push -u origin feat/worker-lifecycle-slice-1
gh pr create --title "feat(core,worker_lifecycle): slices 1 + 2 — single_use runtime + idle_timeout runtime" --body "$(cat <<'EOF'
## Summary

Two slices of the worker-lifecycle work bundled into one PR per the operator's request to ship them together:

- **Slice 1** (commits `781acba`–`334f4e2`): pure types (`Lifecycle`, `IdleTimeoutCaps`, `Contract`), manager trait + `SingleUseLifecycle` (production, byte-equivalent to the previous per-request spawn) + `IdleTimeoutLifecycle` stub. `ToolEntry` gains a `lifecycle` field; `ToolHostStepDispatcher` routes through the manager. `kastellan-supervisor` (OS-unit installer) is untouched — naming overlap with the spec's "supervisor" wording is conceptual.

- **Slice 2** (subsequent commits): `IdleTimeoutLifecycle::acquire` filled in. Warm cache per tool, `tokio::sync::Mutex<ToolState>` serialisation, restart backoff (exponential 1s/2s/…/60s default), three caps (`max_requests`, `max_age_seconds`, `idle_seconds`), crash detection via post-dispatch error classifier. `WorkerHandle` widened to enum.

Spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
Plans: `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-1.md`, `docs/superpowers/plans/2026-05-18-worker-lifecycle-slice-2.md`.

## Test plan

- [x] `cargo test --workspace` — <FINAL_COUNT> passed, 0 failed, 0 SKIP, 0 warnings on Linux
- [x] `cargo test -p kastellan-core --test cli_ask_e2e` — slice-1 regression pin (shell-exec single-use behaviour byte-equivalent)
- [x] `cargo test -p kastellan-core --test worker_lifecycle_idle_timeout_e2e` — six runtime scenarios (warm reuse, two cap rotations, idle teardown, crash recovery + backoff, concurrent serialisation)
- [x] `cargo test -p kastellan-core --lib worker_lifecycle` — unit coverage on pure helpers and types

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 5: Return the PR URL** to the operator for review.

---

## Self-review (done after writing the plan, before handing off)

**1. Spec coverage:**

- §"The two policies" §"idle_timeout" — three caps, all checked post-completion: ✓ Tasks 5 (max_requests, max_age) + 6 (idle_seconds).
- §"Cap-check semantics" §"load-bearing invariant" — no mid-flight kills: ✓ all cap checks happen after the dispatch returns; the per-tool mutex serialises requests so cap checks never race with an in-flight dispatch.
- §"Cap-check semantics" §"Graceful shutdown" SIGTERM grace period — DEFERRED to slice 3 (called out in the "Open questions" / "What slice 2 does NOT do" block in the in-message design).
- §"The stateless contract" — enforced at the type level (slice 1's `Lifecycle::idle_timeout` validated constructor), not runtime.
- §"Supervisor responsibilities" §1 spawn-on-demand: ✓ Task 4. §2 request serialisation: ✓ Task 4 (per-slot tokio mutex). §3 cap evaluation: ✓ Tasks 5 + 6. §4 crash detection + restart backoff: ✓ Task 7. §5 graceful teardown: PARTIAL (no SIGTERM grace; deferred). §6 health introspection: DEFERRED to slice 3 (`_test_slot_has_warm` is the test-only equivalent).
- §"Security model" caveats 1 + 2 — preserved by construction (each `acquire` calls `spawn_worker` with the same `SandboxPolicy`; restart-on-crash discards the dead worker before respawn).

**2. Placeholder scan:**

- One legitimate `todo!()` in Task 3 Step 2 (stub `release_idle_timeout_worker`) that gets filled in by Tasks 5+. Documented in the task body.
- Task 8's test bodies are skeletal (one-paragraph descriptions per scenario instead of full Rust code) — the largest single deliverable in the slice. The skeleton is enough for an implementer who understands the existing `scheduler_step_dispatch_e2e.rs` pattern; expanding to full literal test bodies would 3x the plan length.

**3. Type consistency:**

- `IdleTimeoutCaps`, `RestartBackoff`, `ToolState`, `WarmWorker`, `ToolSlot`, `WarmRegistry`, `WorkerHandle`, `WorkerHandleKind`, `release_idle_timeout_worker`, `schedule_idle_teardown`, `acquire_impl`, `dispatch_indicates_worker_dead`, `is_request_capped`, `is_aged_out` — used identically across all tasks.

**4. Risk register:**

- **Drop in async context**: `OwnedMutexGuard` from tokio's mutex drops synchronously (releases waiters). `tokio::spawn` from inside Drop runs in the current runtime, which is always present because we're under `#[tokio::main]`. Safe.
- **PID accessor on `SupervisedWorker`**: Task 8 assumes a `pub fn pid()` or equivalent. Verify against `tool_host.rs` during implementation — if missing, add it as part of Task 8 (one-line getter on existing field). If the underlying `Child`'s PID is accessible via the protocol Client, even better.
- **Test flakiness on timing**: scenarios 3, 4, 5 sleep 1-2 seconds. CI variance could trip the asserts. Mitigation: use generous margins (sleep 1.5 s for a 1 s cap), and pin the assertions to "PID differs" rather than exact timing.
