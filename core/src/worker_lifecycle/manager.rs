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
    sandboxes: Arc<hhagent_sandbox::SandboxBackends>,
}

impl SingleUseLifecycle {
    pub fn new(sandboxes: Arc<hhagent_sandbox::SandboxBackends>) -> Self {
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
        // Resolution is an Arc::clone (refcount bump, nanoseconds).
        let backend = self.sandboxes.resolve(entry.sandbox_backend);
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
    sandboxes: Arc<hhagent_sandbox::SandboxBackends>,
    backoff: super::idle_timeout::RestartBackoff,
    registry: super::idle_timeout::WarmRegistry,
}

impl IdleTimeoutLifecycle {
    /// Construct with default exponential backoff (1s, 2s, 4s, 8s, …, capped at 60s).
    pub fn new(sandboxes: Arc<hhagent_sandbox::SandboxBackends>) -> Self {
        Self::with_backoff(sandboxes, super::idle_timeout::RestartBackoff::default())
    }

    /// Construct with operator-supplied backoff configuration.
    pub fn with_backoff(
        sandboxes: Arc<hhagent_sandbox::SandboxBackends>,
        backoff: super::idle_timeout::RestartBackoff,
    ) -> Self {
        Self {
            sandboxes,
            backoff,
            registry: super::idle_timeout::empty_registry(),
        }
    }

    /// Test-only inspector: returns whether the slot for `tool_name` has a warm worker.
    /// Used by `worker_lifecycle_idle_timeout_e2e.rs` to pin idle teardown + crash
    /// recovery semantics without depending on PID introspection. The lookup is
    /// async-friendly: it briefly takes the outer std-mutex on the registry, then
    /// takes the per-slot tokio mutex.
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

    /// Test-only inspector: returns the warm slot's `consecutive_restarts` counter.
    /// Used by the crash-recovery e2e to assert that `report_crash` flowed through to
    /// the restart-backoff bookkeeping. Returns 0 if the slot is absent.
    #[doc(hidden)]
    pub async fn _test_slot_consecutive_restarts(&self, tool_name: &str) -> u32 {
        let map = self.registry.lock().expect("warm-registry mutex poisoned");
        let Some(slot) = map.get(tool_name) else {
            return 0;
        };
        let slot = Arc::clone(slot);
        drop(map);
        let state = slot.state.lock().await;
        state.consecutive_restarts
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
        // reused for a different tool with a different backend.
        let backend = self.sandboxes.resolve(entry.sandbox_backend);
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
mod tests {
    use super::*;
    use crate::worker_lifecycle::types::Lifecycle;

    #[test]
    fn single_use_lifecycle_constructor_holds_the_sandbox_backend() {
        // The presence of a constructor that compiles is the assertion; the manager's
        // production spawn path is exercised end-to-end by `scheduler_step_dispatch_e2e`
        // (Task 6) and `cli_ask_e2e` after slice 1's wiring lands.
        let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
        let _mgr = SingleUseLifecycle::new(sandboxes);
    }

    #[tokio::test]
    async fn idle_timeout_acquire_on_single_use_entry_returns_wiring_error() {
        // Defensive: an idle-timeout manager called with a single-use entry is a
        // wiring bug. The manager returns an `Io(InvalidInput)` error rather than
        // panicking so the dispatcher's `step.spawn_failed` audit row still fires.
        let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
        let mgr = IdleTimeoutLifecycle::new(sandboxes);
        let entry = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/nope"),
            policy: hhagent_sandbox::SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: Lifecycle::SingleUse,
            sandbox_backend: None,
            container_image: None,
        };
        let r = mgr.acquire("test-tool", &entry).await;
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

    /// `SingleUseLifecycle::acquire` resolves `entry.sandbox_backend`
    /// against its `SandboxBackends` bundle and reaches *that* backend,
    /// not a hardcoded one. We verify by injecting two counter-backends
    /// and asserting only the per-entry-selected counter ticks.
    ///
    /// `SandboxBackends` fields are `pub`, so tests can build a custom
    /// instance directly with stub backends. No production constructor
    /// is exposed for this — the field-visible-to-callers shape is
    /// deliberate.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn single_use_lifecycle_acquire_routes_via_entry_sandbox_backend_kind() {
        use hhagent_sandbox::{
            SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxError, SandboxPolicy,
        };
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingBackend {
            counter: Arc<AtomicU32>,
        }
        impl SandboxBackend for CountingBackend {
            fn spawn_under_policy(
                &self,
                _policy: &SandboxPolicy,
                _program: &str,
                _args: &[&str],
            ) -> Result<std::process::Child, SandboxError> {
                self.counter.fetch_add(1, Ordering::Relaxed);
                // Stub: never actually spawn. The routing assertion
                // fires before any real I/O — we only care which
                // backend's `spawn_under_policy` was reached.
                Err(SandboxError::Backend(
                    "counted, intentionally unspawned".to_string(),
                ))
            }
        }

        let seatbelt_calls = Arc::new(AtomicU32::new(0));
        let container_calls = Arc::new(AtomicU32::new(0));

        let sbs = Arc::new(SandboxBackends {
            seatbelt: Arc::new(CountingBackend {
                counter: Arc::clone(&seatbelt_calls),
            }),
            container: Arc::new(CountingBackend {
                counter: Arc::clone(&container_calls),
            }),
        });

        let mgr = SingleUseLifecycle::new(Arc::clone(&sbs));

        let entry_container = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/dev/null"),
            policy: SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: Lifecycle::SingleUse,
            sandbox_backend: Some(SandboxBackendKind::Container),
            container_image: None,
        };
        let _ = mgr.acquire("test", &entry_container).await;
        assert_eq!(
            container_calls.load(Ordering::Relaxed),
            1,
            "container backend should be called when entry opts in"
        );
        assert_eq!(
            seatbelt_calls.load(Ordering::Relaxed),
            0,
            "seatbelt backend should be untouched"
        );

        let entry_default = crate::scheduler::tool_dispatch::ToolEntry {
            sandbox_backend: None,
            ..entry_container
        };
        let _ = mgr.acquire("test", &entry_default).await;
        assert_eq!(
            container_calls.load(Ordering::Relaxed),
            1,
            "container backend should not be re-called for default entry"
        );
        assert_eq!(
            seatbelt_calls.load(Ordering::Relaxed),
            1,
            "seatbelt backend should be called for sandbox_backend: None"
        );
    }

    /// `IdleTimeoutLifecycle::acquire` resolves `entry.sandbox_backend`
    /// against its `SandboxBackends` bundle on every cold-spawn path.
    /// Mirrors the `SingleUseLifecycle` counter-backend pin so a future
    /// refactor that drops the resolve from one manager but not the
    /// other trips deliberately. We use distinct tool names per call so
    /// each acquire takes the cold-spawn path (warm slots are keyed by
    /// tool name).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn idle_timeout_lifecycle_acquire_routes_via_entry_sandbox_backend_kind() {
        use crate::worker_lifecycle::{Contract, IdleTimeoutCaps};
        use hhagent_sandbox::{
            SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxError, SandboxPolicy,
        };
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingBackend {
            counter: Arc<AtomicU32>,
        }
        impl SandboxBackend for CountingBackend {
            fn spawn_under_policy(
                &self,
                _policy: &SandboxPolicy,
                _program: &str,
                _args: &[&str],
            ) -> Result<std::process::Child, SandboxError> {
                self.counter.fetch_add(1, Ordering::Relaxed);
                Err(SandboxError::Backend(
                    "counted, intentionally unspawned".to_string(),
                ))
            }
        }

        let seatbelt_calls = Arc::new(AtomicU32::new(0));
        let container_calls = Arc::new(AtomicU32::new(0));

        let sbs = Arc::new(SandboxBackends {
            seatbelt: Arc::new(CountingBackend {
                counter: Arc::clone(&seatbelt_calls),
            }),
            container: Arc::new(CountingBackend {
                counter: Arc::clone(&container_calls),
            }),
        });

        let mgr = IdleTimeoutLifecycle::new(Arc::clone(&sbs));

        let caps = IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 10,
            max_age_seconds: 3600,
            grace_period_seconds: 5,
        };
        let lifecycle = Lifecycle::idle_timeout(caps, Contract { stateless: true })
            .expect("valid lifecycle");
        let entry_container = crate::scheduler::tool_dispatch::ToolEntry {
            binary: std::path::PathBuf::from("/dev/null"),
            policy: SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle,
            sandbox_backend: Some(SandboxBackendKind::Container),
            container_image: None,
        };
        // Distinct tool name from the default-entry call below — warm
        // slots are keyed by tool name, so reusing the name would race
        // the two cold-spawn paths against shared warm-cache state.
        // Each spawn here returns Err so no warm worker is stashed, but
        // distinct names make the routing isolation explicit.
        let _ = mgr.acquire("routing-container", &entry_container).await;
        assert_eq!(
            container_calls.load(Ordering::Relaxed),
            1,
            "container backend should be called when entry opts in"
        );
        assert_eq!(
            seatbelt_calls.load(Ordering::Relaxed),
            0,
            "seatbelt backend should be untouched"
        );

        let entry_default = crate::scheduler::tool_dispatch::ToolEntry {
            sandbox_backend: None,
            ..entry_container
        };
        let _ = mgr.acquire("routing-default", &entry_default).await;
        assert_eq!(
            container_calls.load(Ordering::Relaxed),
            1,
            "container backend should not be re-called for default entry"
        );
        assert_eq!(
            seatbelt_calls.load(Ordering::Relaxed),
            1,
            "seatbelt backend should be called for sandbox_backend: None"
        );
    }
}
