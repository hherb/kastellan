//! Lifecycle manager: spawns workers, lends out `WorkerHandle`s.
//!
//! Slice 1 ships `SingleUseLifecycle` (production, byte-equivalent to today's
//! per-request spawn) and `IdleTimeoutLifecycle` (stub — `acquire` panics).

use std::sync::Arc;

use async_trait::async_trait;
use hhagent_sandbox::SandboxBackend;

use crate::scheduler::tool_dispatch::ToolEntry;
use crate::tool_host::{spawn_worker, SupervisedWorker, ToolHostError, WorkerSpec};

/// Holder of an exclusively-owned, live `SupervisedWorker` lent out by a lifecycle
/// manager. The dispatcher calls `worker_mut()` to get the `&mut SupervisedWorker`
/// that `tool_host::dispatch` wants.
///
/// **Slice 1 Drop semantics:** the default `Drop` drops the inner `SupervisedWorker`,
/// whose own `Drop` closes stdio + cancels the watchdog. For `SingleUseLifecycle` this
/// is exactly the right behaviour — the worker exits.
///
/// **Slice 2 will replace this:** the handle will carry a back-channel to the manager
/// so `Drop` hands the worker back to the warm-pool instead of terminating it. Slice 1
/// keeps the type minimal so the slice-2 extension is additive.
pub struct WorkerHandle {
    worker: SupervisedWorker,
}

impl WorkerHandle {
    /// Construct a single-use handle. Module-private — only the lifecycle implementations
    /// in this file can build one.
    pub(crate) fn single_use(worker: SupervisedWorker) -> Self {
        Self { worker }
    }

    /// Exclusive `&mut` to the live worker. The intended caller is
    /// `tool_host::dispatch(pool, handle.worker_mut(), tool, method, params)`; the
    /// chokepoint seal (issue #16) is unchanged because `SupervisedWorker::call` itself
    /// stays module-private to `tool_host`.
    pub fn worker_mut(&mut self) -> &mut SupervisedWorker {
        &mut self.worker
    }
}

/// Lifecycle manager trait. `dyn`-safe (no generics, no associated types).
///
/// `acquire` is async because the `IdleTimeout` runtime (slice 2) will need to await
/// queue-slot availability when a request lands on a busy warm worker.
/// `SingleUseLifecycle` doesn't actually await anything inside `acquire`, but uses the
/// same trait shape so the dispatcher can hold an `Arc<dyn WorkerLifecycleManager>`
/// without per-policy branching.
#[async_trait]
pub trait WorkerLifecycleManager: Send + Sync {
    /// Acquire a `WorkerHandle` for `entry`'s tool. The handle's lifetime equals one
    /// JSON-RPC request: caller dispatches against it, then drops it. Slice 1 always
    /// terminates the underlying worker on drop; slice 2 may hand it back to a pool.
    async fn acquire(&self, entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError>;
}

/// Single-use lifecycle: spawn one worker per acquire, terminate on drop.
///
/// Production impl for slice 1. Behaviour is byte-equivalent to the spawn path that
/// used to live inline in `scheduler::tool_dispatch::ToolHostStepDispatcher::dispatch_step`.
pub struct SingleUseLifecycle {
    sandbox: Arc<dyn SandboxBackend>,
}

impl SingleUseLifecycle {
    pub fn new(sandbox: Arc<dyn SandboxBackend>) -> Self {
        Self { sandbox }
    }
}

#[async_trait]
impl WorkerLifecycleManager for SingleUseLifecycle {
    async fn acquire(&self, entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        // Per-call clone of the base policy so concurrent dispatches against the same
        // `ToolEntry` cannot mutate each other's policy. The clone matches the
        // discipline the pre-refactor inline path used.
        let policy = entry.policy.clone();
        let program = entry.binary.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &program,
            args: &[],
            wall_clock_ms: entry.wall_clock_ms,
        };
        let worker = spawn_worker(self.sandbox.as_ref(), &spec)?;
        Ok(WorkerHandle::single_use(worker))
    }
}

/// Idle-timeout lifecycle stub.
///
/// **Slice 1 declares this type so downstream code can name it; runtime invocation
/// panics with `unimplemented!()`.** The `acquire` body intentionally panics rather
/// than returning an error so any accidental wiring of an idle-timeout worker into
/// slice 1's daemon trips loudly on the first request rather than silently falling
/// through to a `SPAWN_FAILED` audit row.
///
/// Slice 2 (the GLiNER-Relex prereq) replaces this body with the spawn-on-demand /
/// post-completion-cap / crash-recovery runtime per the spec.
pub struct IdleTimeoutLifecycle {
    _private: (),
}

impl IdleTimeoutLifecycle {
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for IdleTimeoutLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WorkerLifecycleManager for IdleTimeoutLifecycle {
    async fn acquire(&self, _entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        unimplemented!(
            "idle_timeout lifecycle runtime — slice 2; \
             slice 1 ships SingleUseLifecycle only"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_lifecycle::types::{Contract, IdleTimeoutCaps, Lifecycle};

    #[test]
    fn single_use_lifecycle_constructor_holds_the_sandbox_backend() {
        // The presence of a constructor that compiles is the assertion; the manager's
        // production spawn path is exercised end-to-end by `scheduler_step_dispatch_e2e`
        // (Task 6) and `cli_ask_e2e` after slice 1's wiring lands.
        let sandbox: Arc<dyn SandboxBackend> = Arc::from(hhagent_sandbox::default_backend());
        let _mgr = SingleUseLifecycle::new(sandbox);
    }

    #[tokio::test]
    #[should_panic(expected = "idle_timeout lifecycle runtime — slice 2")]
    async fn idle_timeout_lifecycle_acquire_panics_until_slice_2() {
        // The stub exists at the type level so downstream code (a future
        // `WorkerManifest` parser, slice 2's runtime) can refer to it without
        // conditional compilation. Runtime invocation is deliberately wired to
        // `unimplemented!()` so a test that accidentally routes idle-timeout traffic
        // through slice 1's daemon trips loudly.
        let caps = IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 3600,
            grace_period_seconds: 5,
        };
        let contract = Contract { stateless: true };
        let lc = Lifecycle::idle_timeout(caps, contract).expect("valid lifecycle");
        let mgr = IdleTimeoutLifecycle::new();
        // We need a `ToolEntry` to call acquire — defer to a dummy. The acquire body
        // panics before reading any field of the entry, so the dummy is safe.
        let entry = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/nope"),
            policy: hhagent_sandbox::SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: lc,
        };
        let _ = mgr.acquire(&entry).await;
    }

    #[test]
    fn worker_handle_exposes_worker_mut() {
        // Type-level pin: `WorkerHandle::worker_mut` returns `&mut SupervisedWorker`,
        // which is what `dispatch_step` will pass into `tool_host::dispatch`. The
        // assertion is the signature; no runtime invocation here.
        fn _shape_pin(h: &mut WorkerHandle) -> &mut SupervisedWorker {
            h.worker_mut()
        }
    }
}
