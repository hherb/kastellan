//! Unit tests for the `worker_lifecycle` manager module.
//!
//! Lifted from an inline `#[cfg(test)] mod tests` block in `manager.rs` to
//! keep the production file under the 500-LOC soft cap. The body is
//! byte-identical to what it was inline; `use super::*` still resolves to
//! the parent `manager` module per the Rust 2018 sibling-directory module
//! pattern. End-to-end routing pins live in
//! `core/tests/lifecycle_container_routing_e2e.rs`.

use super::*;
use crate::worker_lifecycle::types::Lifecycle;

#[test]
fn single_use_lifecycle_constructor_holds_the_sandbox_backend() {
    // The presence of a constructor that compiles is the assertion; the manager's
    // production spawn path is exercised end-to-end by `scheduler_step_dispatch_e2e`
    // (Task 6) and `cli_ask_e2e` after slice 1's wiring lands.
    let sandboxes = Arc::new(kastellan_sandbox::SandboxBackends::default_for_current_os());
    let _mgr = SingleUseLifecycle::new(sandboxes);
}

#[tokio::test]
async fn idle_timeout_acquire_on_single_use_entry_returns_wiring_error() {
    // Defensive: an idle-timeout manager called with a single-use entry is a
    // wiring bug. The manager returns an `Io(InvalidInput)` error rather than
    // panicking so the dispatcher's `step.spawn_failed` audit row still fires.
    let sandboxes = Arc::new(kastellan_sandbox::SandboxBackends::default_for_current_os());
    let mgr = IdleTimeoutLifecycle::new(sandboxes);
    let entry = crate::scheduler::tool_dispatch::ToolEntry {
        binary: std::path::PathBuf::from("/nope"),
        policy: kastellan_sandbox::SandboxPolicy::default(),
        wall_clock_ms: None,
        lifecycle: Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
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

/// Issue #84: the queue-depth inspector returns 0 for a tool name that has
/// never been acquired (slot doesn't exist in the registry yet).
///
/// Sync, not async — the read is a plain atomic load and `_test_slot_pending_acquires`
/// has a sync signature (no tokio mutex involved on the read path). Pin the no-slot
/// case here; the WITH-slot case is implicitly covered by the `pending_acquire_guard_*`
/// pure tests + production wiring (see `acquire_impl` in `idle_timeout.rs`).
#[tokio::test]
async fn test_slot_pending_acquires_returns_zero_for_absent_tool() {
    let sandboxes = Arc::new(kastellan_sandbox::SandboxBackends::default_for_current_os());
    let mgr = IdleTimeoutLifecycle::new(sandboxes);
    assert_eq!(
        mgr._test_slot_pending_acquires("never-acquired"),
        0,
        "absent-slot lookup must return 0, not panic or block"
    );
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
    use kastellan_sandbox::{
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
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
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
    use kastellan_sandbox::{
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
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
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
