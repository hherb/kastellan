//! Composite lifecycle manager: dispatches by `ToolEntry.lifecycle`.
//!
//! Before this module existed, the daemon held a single
//! [`crate::worker_lifecycle::WorkerLifecycleManager`] of one concrete
//! type — [`crate::worker_lifecycle::SingleUseLifecycle`] in production,
//! [`crate::worker_lifecycle::IdleTimeoutLifecycle`] in
//! `worker_lifecycle_idle_timeout_e2e`. That worked while every
//! registered [`crate::scheduler::ToolEntry`] declared the same
//! lifecycle. The GLiNER-Relex worker (slice-2 of the gliner-relex
//! implementation plan) introduces the first mixed deployment:
//! `shell-exec` keeps `Lifecycle::SingleUse` (per-request isolation is
//! its security model — pinned by
//! `shell_exec_entry_declares_single_use_lifecycle`), while
//! `gliner-relex` declares `Lifecycle::IdleTimeout` so the ~1.3 GB
//! model stays resident across calls.
//!
//! `SingleUseLifecycle::acquire` ignores `entry.lifecycle` and always
//! spawns single-use; `IdleTimeoutLifecycle::acquire` rejects
//! `Lifecycle::SingleUse` entries with `Err(Io(InvalidInput))` (wiring
//! bug — see `idle_timeout::acquire_impl`). Composing them by entry
//! discriminant is the cheapest way to make the `Lifecycle` field
//! actually load-bearing in production.
//!
//! Both inner managers share the same sandbox backend Arc — concurrent
//! acquires across tools cost no extra spawn machinery; per-tool warm
//! caches live inside [`crate::worker_lifecycle::IdleTimeoutLifecycle`]
//! only.

use std::sync::Arc;

use async_trait::async_trait;
use hhagent_sandbox::SandboxBackend;

use crate::scheduler::ToolEntry;
use crate::tool_host::ToolHostError;
use crate::worker_lifecycle::{
    IdleTimeoutLifecycle, Lifecycle, SingleUseLifecycle, WorkerHandle, WorkerLifecycleManager,
};

/// Multi-policy manager. Holds one [`SingleUseLifecycle`] and one
/// [`IdleTimeoutLifecycle`] over the same sandbox backend, routes each
/// `acquire` call to the right one by inspecting `entry.lifecycle`.
///
/// A strict superset of [`SingleUseLifecycle`] for entries declaring
/// [`Lifecycle::SingleUse`] (same code path; same `WorkerHandle`
/// variant; byte-equivalent behaviour). Construct via [`Self::new`]
/// for the production default backoff, or [`Self::with_backoff`] for
/// operator-tuned restart backoff on the idle-timeout side.
pub struct CompositeLifecycle {
    single_use: SingleUseLifecycle,
    idle_timeout: IdleTimeoutLifecycle,
}

impl CompositeLifecycle {
    /// Build with default exponential restart backoff (1 s → 60 s cap).
    pub fn new(sandbox: Arc<dyn SandboxBackend>) -> Self {
        Self {
            single_use: SingleUseLifecycle::new(Arc::clone(&sandbox)),
            idle_timeout: IdleTimeoutLifecycle::new(sandbox),
        }
    }

    /// Build with operator-supplied restart backoff. The backoff
    /// applies to the idle-timeout side only — `SingleUseLifecycle`
    /// has no warm-cache to back off from.
    pub fn with_backoff(
        sandbox: Arc<dyn SandboxBackend>,
        backoff: super::idle_timeout::RestartBackoff,
    ) -> Self {
        Self {
            single_use: SingleUseLifecycle::new(Arc::clone(&sandbox)),
            idle_timeout: IdleTimeoutLifecycle::with_backoff(sandbox, backoff),
        }
    }
}

#[async_trait]
impl WorkerLifecycleManager for CompositeLifecycle {
    async fn acquire(
        &self,
        tool_name: &str,
        entry: &ToolEntry,
    ) -> Result<WorkerHandle, ToolHostError> {
        match entry.lifecycle {
            Lifecycle::SingleUse => self.single_use.acquire(tool_name, entry).await,
            Lifecycle::IdleTimeout { .. } => self.idle_timeout.acquire(tool_name, entry).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::worker_lifecycle::{Contract, IdleTimeoutCaps};
    use hhagent_sandbox::{SandboxError, SandboxPolicy};

    /// Stub sandbox that always errors on spawn so the tests don't need
    /// a real `bwrap`/Seatbelt environment. We only need to verify the
    /// dispatch path — `CompositeLifecycle::acquire` either delegates
    /// to `SingleUseLifecycle` (which calls `spawn_under_policy` and
    /// surfaces the error) or to `IdleTimeoutLifecycle` (which also
    /// calls `spawn_under_policy` on a cold slot and surfaces the
    /// error). A distinct error string per call wouldn't help here —
    /// what matters is *which* manager runs.
    struct NeverSpawnsBackend;

    impl SandboxBackend for NeverSpawnsBackend {
        fn spawn_under_policy(
            &self,
            _policy: &SandboxPolicy,
            _program: &str,
            _args: &[&str],
        ) -> Result<std::process::Child, SandboxError> {
            Err(SandboxError::Backend(
                "test stub: spawning is intentionally unavailable".to_string(),
            ))
        }
    }

    fn dummy_single_use_entry() -> ToolEntry {
        ToolEntry {
            binary: std::path::PathBuf::from("/dev/null"),
            policy: SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle: Lifecycle::SingleUse,
            sandbox_backend: None,
        }
    }

    fn dummy_idle_timeout_entry() -> ToolEntry {
        let caps = IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 10,
            max_age_seconds: 3600,
            grace_period_seconds: 5,
        };
        let lifecycle = Lifecycle::idle_timeout(caps, Contract { stateless: true })
            .expect("valid lifecycle");
        ToolEntry {
            binary: std::path::PathBuf::from("/dev/null"),
            policy: SandboxPolicy::default(),
            wall_clock_ms: None,
            lifecycle,
            sandbox_backend: None,
        }
    }

    #[tokio::test]
    async fn dispatches_single_use_entry_to_single_use_manager() {
        let composite = CompositeLifecycle::new(Arc::new(NeverSpawnsBackend));
        let entry = dummy_single_use_entry();
        let result = composite.acquire("any", &entry).await;
        // `SingleUseLifecycle::acquire` always calls spawn → SandboxError
        // → ToolHostError::Sandbox. The IdleTimeoutLifecycle path would
        // instead return `ToolHostError::Io(InvalidInput)` because it
        // rejects `Lifecycle::SingleUse` up front. The Sandbox-vs-Io
        // discriminant proves which manager ran without needing
        // process introspection.
        match result {
            Err(ToolHostError::Sandbox(_)) => {}
            Err(e) => panic!("expected Sandbox error (single-use path), got {e:?}"),
            Ok(_) => panic!("expected Sandbox error (single-use path), got Ok(_)"),
        }
    }

    #[tokio::test]
    async fn dispatches_idle_timeout_entry_to_idle_timeout_manager() {
        let composite = CompositeLifecycle::new(Arc::new(NeverSpawnsBackend));
        let entry = dummy_idle_timeout_entry();
        let result = composite.acquire("gliner-relex-test", &entry).await;
        // `IdleTimeoutLifecycle::acquire_impl` on a cold slot calls
        // spawn → SandboxError → ToolHostError::Sandbox. If the
        // SingleUseLifecycle path had run, the error would be the same
        // (single-use also spawns); that's structurally indistinguishable
        // here. So the meaningful signal is the *opposite* direction:
        // confirm we do NOT get the `InvalidInput` wiring-bug error
        // that idle-timeout returns when fed a SingleUse entry.
        match result {
            Err(ToolHostError::Sandbox(_)) => {}
            Err(ToolHostError::Io(io_err)) => {
                panic!(
                    "expected Sandbox error (idle-timeout cold-spawn path); \
                     got Io({io_err}) which suggests the single-use side wrongly handled \
                     an IdleTimeout entry"
                );
            }
            Err(e) => panic!("expected Sandbox error, got {e:?}"),
            Ok(_) => panic!("expected Sandbox error, got Ok(_)"),
        }
    }

    #[tokio::test]
    async fn with_backoff_constructor_compiles_and_dispatches() {
        // Pure compile-pin: the alternate constructor accepts the
        // operator-tuned backoff and dispatches identically to `new`.
        // We don't pin specific backoff timing here — that's the
        // idle_timeout::tests's job.
        let composite = CompositeLifecycle::with_backoff(
            Arc::new(NeverSpawnsBackend),
            super::super::idle_timeout::RestartBackoff::default(),
        );
        let entry = dummy_single_use_entry();
        assert!(composite.acquire("any", &entry).await.is_err());
    }
}
