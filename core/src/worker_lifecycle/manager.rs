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
/// manager.
///
/// Slice 1 shipped this as a thin newtype around `SupervisedWorker`. Slice 2 widens it
/// to an enum because idle-timeout drop semantics differ structurally from single-use:
///   - `SingleUse`: Drop terminates the worker (default behaviour of `SupervisedWorker`).
///   - `IdleTimeout`: Drop returns the worker to its warm slot (or terminates if the
///     worker died, the request cap fired, or the worker aged out).
///
/// The variant is private; consumers only see the `worker_mut` and `report_crash`
/// methods.
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
        slot: Option<Arc<super::idle_timeout::ToolSlot>>,
        spawned_at: std::time::Instant,
        request_count_so_far: u64,
        caps: super::types::IdleTimeoutCaps,
        died: bool,
        backoff: super::idle_timeout::RestartBackoff,
    },
}

impl WorkerHandle {
    /// Construct a single-use handle. Module-private — only the lifecycle implementations
    /// in this file (and the slice-2 runtime in `super::idle_timeout`) can build one.
    pub(crate) fn single_use(worker: SupervisedWorker) -> Self {
        Self {
            kind: WorkerHandleKind::SingleUse {
                worker: Some(worker),
            },
        }
    }

    /// Construct an idle-timeout handle. Module-private. Called only from
    /// `super::idle_timeout::acquire_impl` once it has the slot guard + bookkeeping.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn idle_timeout(
        worker: SupervisedWorker,
        slot_guard: tokio::sync::OwnedMutexGuard<super::idle_timeout::ToolState>,
        slot: Arc<super::idle_timeout::ToolSlot>,
        spawned_at: std::time::Instant,
        request_count_so_far: u64,
        caps: super::types::IdleTimeoutCaps,
        backoff: super::idle_timeout::RestartBackoff,
    ) -> Self {
        Self {
            kind: WorkerHandleKind::IdleTimeout {
                worker: Some(worker),
                slot_guard: Some(slot_guard),
                slot: Some(slot),
                spawned_at,
                request_count_so_far,
                caps,
                died: false,
                backoff,
            },
        }
    }

    /// Exclusive `&mut` to the live worker. The intended caller is
    /// `tool_host::dispatch(pool, handle.worker_mut(), tool, method, params)`; the
    /// chokepoint seal (issue #16) is unchanged because `SupervisedWorker::call` itself
    /// stays module-private to `tool_host`.
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

    /// Caller signals the dispatch error indicated worker death.
    ///
    /// For `SingleUse` this is a no-op (the worker exits on Drop regardless). For
    /// `IdleTimeout` this suppresses the worker-return path so the dead worker isn't
    /// put back into the slot, and bumps the restart-backoff counter on the slot's
    /// state so the next acquire waits.
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
                slot,
                spawned_at,
                request_count_so_far,
                caps,
                died,
                backoff,
            } => {
                let worker_opt = worker.take();
                let guard = slot_guard
                    .take()
                    .expect("slot_guard absent in idle-timeout Drop");
                let slot_opt = slot.take();
                super::idle_timeout::release_idle_timeout_worker(
                    worker_opt,
                    guard,
                    slot_opt,
                    *spawned_at,
                    *request_count_so_far,
                    caps.clone(),
                    *died,
                    *backoff,
                );
            }
        }
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
    async fn acquire(&self, entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        super::idle_timeout::acquire_impl(
            self.sandbox.as_ref(),
            self.backoff,
            &self.registry,
            entry,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker_lifecycle::types::Lifecycle;

    #[test]
    fn single_use_lifecycle_constructor_holds_the_sandbox_backend() {
        // The presence of a constructor that compiles is the assertion; the manager's
        // production spawn path is exercised end-to-end by `scheduler_step_dispatch_e2e`
        // (Task 6) and `cli_ask_e2e` after slice 1's wiring lands.
        let sandbox: Arc<dyn SandboxBackend> = Arc::from(hhagent_sandbox::default_backend());
        let _mgr = SingleUseLifecycle::new(sandbox);
    }

    #[tokio::test]
    async fn idle_timeout_acquire_on_single_use_entry_returns_wiring_error() {
        // Defensive: an idle-timeout manager called with a single-use entry is a
        // wiring bug. The manager returns an `Io(InvalidInput)` error rather than
        // panicking so the dispatcher's `step.spawn_failed` audit row still fires.
        let sandbox: Arc<dyn SandboxBackend> = Arc::from(hhagent_sandbox::default_backend());
        let mgr = IdleTimeoutLifecycle::new(sandbox);
        let entry = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/nope"),
            policy: hhagent_sandbox::SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: Lifecycle::SingleUse,
        };
        let r = mgr.acquire(&entry).await;
        assert!(r.is_err(), "must return Err on wiring bug");
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
