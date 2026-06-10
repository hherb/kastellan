//! Lifecycle manager: spawns workers, lends out `WorkerHandle`s.
//!
//! Slice 1 ships `SingleUseLifecycle` (production, byte-equivalent to today's
//! per-request spawn) and `IdleTimeoutLifecycle` (stub — `acquire` panics).

use std::sync::Arc;

use async_trait::async_trait;

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
///
/// **Drop runtime contract:** the `IdleTimeout` variant's Drop impl calls
/// `tokio::spawn` to schedule the one-shot idle-teardown task, so it must run inside a
/// live tokio runtime. The production caller (`ToolHostStepDispatcher::dispatch_step`)
/// satisfies this trivially — Drop happens on the async stack. Tests that construct or
/// drop an idle-timeout handle must use `#[tokio::test]`. Dropping outside a runtime
/// panics from inside Drop.
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
    ///
    /// `tool_name` is the logical registry key (i.e. `PlannedStep::tool`). It is the
    /// warm-cache key for `IdleTimeoutLifecycle` — using the registry key rather than
    /// the binary basename means two tools whose binaries happen to share a `file_name`
    /// (e.g. `/opt/a/inference` and `/opt/b/inference`) get separate slots. The
    /// `SingleUseLifecycle` impl ignores it because it never caches.
    async fn acquire(
        &self,
        tool_name: &str,
        entry: &ToolEntry,
    ) -> Result<WorkerHandle, ToolHostError>;
}

/// Single-use lifecycle: spawn one worker per acquire, terminate on drop.
///
/// Production impl for slice 1. Behaviour is byte-equivalent to the spawn
/// path that used to live inline in
/// `scheduler::tool_dispatch::ToolHostStepDispatcher::dispatch_step`.
///
/// Slice 2 (this slice): holds an `Arc<SandboxBackends>` bundle instead
/// of a single `Arc<dyn SandboxBackend>`; resolves the entry's
/// `sandbox_backend` per call. Existing entries default to `None` so
/// the per-OS default backend keeps being used (byte-equivalent).
pub struct SingleUseLifecycle {
    sandboxes: Arc<kastellan_sandbox::SandboxBackends>,
}

impl SingleUseLifecycle {
    pub fn new(sandboxes: Arc<kastellan_sandbox::SandboxBackends>) -> Self {
        Self { sandboxes }
    }
}

#[async_trait]
impl WorkerLifecycleManager for SingleUseLifecycle {
    async fn acquire(
        &self,
        _tool_name: &str,
        entry: &ToolEntry,
    ) -> Result<WorkerHandle, ToolHostError> {
        // `_tool_name` is unused: single-use never caches, so there is no per-tool slot
        // to key by. The parameter exists on the trait for `IdleTimeoutLifecycle`'s
        // warm-cache key (see trait doc).
        //
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
        // Resolve per call: `entry.sandbox_backend == None` returns the
        // per-OS default; `Some(K)` returns the matching backend slot.
        // For Container kind, `entry.container_image.as_deref()` picks
        // the per-worker image tag (or `None` → default-image cached slot).
        // Cost: Arc::clone (refcount bump, nanoseconds) for the cached
        // paths, or a fresh Arc::new(MacosContainer::with_image) for
        // per-worker images (still cheap — String + Arc).
        let backend = self
            .sandboxes
            .resolve(entry.sandbox_backend, entry.container_image.as_deref());
        let worker = spawn_worker(backend.as_ref(), &spec)?;
        Ok(WorkerHandle::single_use(worker))
    }
}

/// Idle-timeout lifecycle: warm-keep one worker per tool name; tear down post-completion
/// when any of `idle_seconds` / `max_requests` / `max_age_seconds` fires.
///
/// Slice-2 production impl. The runtime (warm cache, idle teardown, crash recovery,
/// restart backoff) lives in `super::idle_timeout`; this struct is the thin facade
/// `WorkerLifecycleManager` consumers see.
///
/// Slice 2 (per-worker backend selection): holds an `Arc<SandboxBackends>`
/// bundle and resolves the entry's `sandbox_backend` at slot-fill time.
/// The warm-cache key remains the tool name, so two tools that select
/// different backends still get separate warm slots.
pub struct IdleTimeoutLifecycle {
    sandboxes: Arc<kastellan_sandbox::SandboxBackends>,
    backoff: super::idle_timeout::RestartBackoff,
    registry: super::idle_timeout::WarmRegistry,
}

impl IdleTimeoutLifecycle {
    /// Construct with default exponential backoff (1s, 2s, 4s, 8s, …, capped at 60s).
    pub fn new(sandboxes: Arc<kastellan_sandbox::SandboxBackends>) -> Self {
        Self::with_backoff(sandboxes, super::idle_timeout::RestartBackoff::default())
    }

    /// Construct with operator-supplied backoff configuration.
    pub fn with_backoff(
        sandboxes: Arc<kastellan_sandbox::SandboxBackends>,
        backoff: super::idle_timeout::RestartBackoff,
    ) -> Self {
        Self {
            sandboxes,
            backoff,
            registry: super::idle_timeout::empty_registry(),
        }
    }

    /// Look up an existing warm slot by name without creating one, cloning
    /// the `Arc<ToolSlot>` out from under the registry's std-mutex.
    ///
    /// This is a *synchronous* helper on purpose: the guard lives and dies
    /// entirely within this call, so a caller can `.await` on the returned
    /// slot's per-slot tokio mutex without holding the outer std-mutex
    /// across the await. (The inline form used an explicit `drop(map)` to
    /// the same effect, but `clippy::await_holding_lock` is not flow-
    /// sensitive to that drop and flagged it anyway; factoring the locked
    /// section into a non-async fn silences the lint legitimately.)
    fn lookup_slot(&self, tool_name: &str) -> Option<Arc<super::idle_timeout::ToolSlot>> {
        self.registry
            .lock()
            .expect("warm-registry mutex poisoned")
            .get(tool_name)
            .map(Arc::clone)
    }

    /// Test-only inspector: returns whether the slot for `tool_name` has a warm worker.
    /// Used by `worker_lifecycle_idle_timeout_e2e.rs` to pin idle teardown + crash
    /// recovery semantics without depending on PID introspection. The lookup is
    /// async-friendly: it briefly takes the outer std-mutex on the registry (via
    /// [`Self::lookup_slot`]), then takes the per-slot tokio mutex.
    #[doc(hidden)]
    pub async fn _test_slot_has_warm(&self, tool_name: &str) -> bool {
        let Some(slot) = self.lookup_slot(tool_name) else {
            return false;
        };
        let state = slot.state.lock().await;
        state.warm.is_some()
    }

    /// Test-only inspector: returns the warm slot's `consecutive_restarts` counter.
    /// Used by the crash-recovery e2e to assert that `report_crash` flowed through to
    /// the restart-backoff bookkeeping. Returns 0 if the slot is absent.
    #[doc(hidden)]
    pub async fn _test_slot_consecutive_restarts(&self, tool_name: &str) -> u32 {
        let Some(slot) = self.lookup_slot(tool_name) else {
            return 0;
        };
        let state = slot.state.lock().await;
        state.consecutive_restarts
    }

    /// Test-only inspector: returns the slot's current `pending_acquires` depth.
    ///
    /// Closes the observability half of issue #84. The atomic counter is incremented
    /// in `acquire_impl` before `lock_owned().await` (via `PendingAcquireGuard`) and
    /// decremented once the lock is acquired, so the returned depth reflects
    /// "callers waiting for the slot's tokio mutex" (queued), not "callers
    /// dispatching" (in flight).
    ///
    /// Sync, not async — the read is a plain atomic load, no tokio mutex involved.
    /// Returns 0 if the slot doesn't exist yet (no acquire has hit this tool name).
    ///
    /// **Not the production CLI surface.** Matches the `_test_slot_*` naming
    /// convention of its sibling inspectors above; the future
    /// `kastellan-cli supervisor status` plumbing will add a parallel `pub fn
    /// slot_pending_acquires` (or equivalent) wrapping the same atomic load.
    /// Inlining the production accessor here would have meant either renaming
    /// all three inspectors at once or breaking convention for just this one.
    #[doc(hidden)]
    pub fn _test_slot_pending_acquires(&self, tool_name: &str) -> u32 {
        let map = self.registry.lock().expect("warm-registry mutex poisoned");
        let Some(slot) = map.get(tool_name) else {
            return 0;
        };
        slot.pending_acquires
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

#[async_trait]
impl WorkerLifecycleManager for IdleTimeoutLifecycle {
    async fn acquire(
        &self,
        tool_name: &str,
        entry: &ToolEntry,
    ) -> Result<WorkerHandle, ToolHostError> {
        // Resolve per-acquire: cold-fill paths pick up the right backend
        // for the entry. The warm cache below in `acquire_impl` is keyed
        // by tool name, so a warm worker spawned under one backend isn't
        // reused for a different tool with a different backend. The
        // `entry.container_image.as_deref()` arg drives per-worker image
        // selection for the Container kind (see SandboxBackends::resolve
        // docs). Note: the warm-cache key does NOT include the image
        // tag — a runtime `container_image` swap on the same tool name
        // would not invalidate the warm slot. Today this is safe because
        // image tags are baked into the `ToolEntry` at daemon startup
        // and a restart flushes the WarmRegistry; if a future
        // operator-driven live-reconfigure path is added, the warm-cache
        // key must be widened to include the image tag.
        let backend = self
            .sandboxes
            .resolve(entry.sandbox_backend, entry.container_image.as_deref());
        super::idle_timeout::acquire_impl(
            backend.as_ref(),
            self.backoff,
            &self.registry,
            tool_name,
            entry,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
