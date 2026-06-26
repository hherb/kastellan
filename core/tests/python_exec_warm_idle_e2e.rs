//! End-to-end: python-exec under the macOS micro-VM with the warm/idle
//! lifecycle (`KASTELLAN_PYTHON_EXEC_IDLE_SECONDS > 0`).
//!
//! Pins the three properties warm reuse must hold:
//!   1. **Warm reuse** — N acquire→dispatch→release cycles boot the VM ONCE
//!      (asserted via a spawn-counting backend).
//!   2. **/tmp wipe across reuse** — a sentinel file written under /tmp by call
//!      1 is GONE for call 2 on the same warm VM (the isolation guarantee).
//!   3. **Idle teardown** — after `idle_seconds` with no call, the warm slot
//!      clears.
//!
//! `[SKIP]`s when Apple `container` / its service / the python-exec image are
//! missing. Build the image first:
//!     scripts/workers/python-exec/build-image.sh

#![cfg(target_os = "macos")]

use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, AuditSink};
use kastellan_core::worker_lifecycle::{
    Contract, IdleTimeoutCaps, IdleTimeoutLifecycle, Lifecycle, WorkerHandle,
    WorkerLifecycleManager,
};
use kastellan_core::workers::python_exec::{
    container_mode_entry, CONTAINER_WORKER_BIN, DEFAULT_IMAGE,
};
use kastellan_db::DbError;
use kastellan_sandbox::macos_container::MacosContainer;
use kastellan_sandbox::{
    SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxError, SandboxPolicy,
};

const TOOL_NAME: &str = "python-exec";

/// A no-op audit sink so the test needs no Postgres cluster — the container is
/// the only external dependency.
struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    async fn insert(
        &self,
        _actor: &str,
        _action: &str,
        _payload: serde_json::Value,
    ) -> Result<i64, DbError> {
        Ok(1)
    }
}

/// Spawn-counting wrapper over the real Container backend. The warm-reuse +
/// teardown tests assert against the counter so a regression that boots a fresh
/// VM per call fails loudly.
struct CountingBackend {
    inner: Arc<dyn SandboxBackend>,
    count: Arc<AtomicUsize>,
}

impl SandboxBackend for CountingBackend {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<Child, SandboxError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn_under_policy(policy, program, args)
    }
}

/// Skip (early-return `true`) when Apple `container` isn't usable on this host
/// or the python-exec image is absent.
fn skip_if_no_container_image() -> bool {
    if let Err(e) = MacosContainer::probe() {
        eprintln!("\n[SKIP] container probe failed: {e}\n");
        return true;
    }
    let listed = std::process::Command::new("container")
        .args(["image", "list"])
        .output();
    let has_image = matches!(
        listed,
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains("python-exec")
    );
    if !has_image {
        eprintln!(
            "\n[SKIP] {DEFAULT_IMAGE} image not present; run \
             scripts/workers/python-exec/build-image.sh\n"
        );
        return true;
    }
    false
}

/// Build an idle-timeout lifecycle whose Container slot is the counting backend.
fn lifecycle_with_counter(count: Arc<AtomicUsize>) -> IdleTimeoutLifecycle {
    let real = SandboxBackends::default_for_current_os()
        .resolve(Some(SandboxBackendKind::Container), Some(DEFAULT_IMAGE));
    let counting: Arc<dyn SandboxBackend> = Arc::new(CountingBackend { inner: real, count });
    // The python-exec entry sets sandbox_backend: Some(Container), so only the
    // container slot is consulted; the seatbelt slot is unused by this entry but
    // must be present — reuse the same arc.
    let bundle = Arc::new(SandboxBackends {
        seatbelt: Arc::clone(&counting),
        container: counting,
    });
    IdleTimeoutLifecycle::new(bundle)
}

/// A container entry with an explicit idle window (built directly so the test
/// controls the window without env plumbing).
fn warm_entry(idle_seconds: u64) -> kastellan_core::scheduler::ToolEntry {
    let lifecycle = Lifecycle::idle_timeout(
        IdleTimeoutCaps {
            idle_seconds,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        },
        Contract { stateless: true },
    )
    .expect("valid lifecycle");
    let mut entry = container_mode_entry(
        std::path::PathBuf::from(CONTAINER_WORKER_BIN),
        DEFAULT_IMAGE.to_string(),
        None,
        lifecycle,
    );
    // `SandboxBackends::resolve(Some(Container), Some(tag))` builds a FRESH
    // MacosContainer for the tag, which would bypass our spawn-counting wrapper.
    // Null the entry's image so resolve returns the *stored* container slot
    // (our CountingBackend) — that backend already carries DEFAULT_IMAGE because
    // `lifecycle_with_counter` built its inner via resolve(.., Some(DEFAULT_IMAGE)).
    entry.container_image = None;
    entry
}

/// Dispatch one `python.exec` over an already-acquired warm handle.
async fn dispatch_over_handle(handle: &mut WorkerHandle, code: &str) -> serde_json::Value {
    dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        handle.worker_mut(),
        TOOL_NAME,
        "python.exec",
        serde_json::json!({ "code": code }),
    )
    .await
    .expect("dispatch python.exec")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_reuse_three_calls_boot_vm_once() {
    if skip_if_no_container_image() {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(60);

    for cycle in 1..=3 {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire");
        let out = dispatch_over_handle(&mut handle, "print(6*7)").await;
        assert_eq!(
            out["stdout"].as_str().unwrap_or_default().trim(),
            "42",
            "cycle {cycle}: expected 42, got {out}"
        );
        assert_eq!(out["exit_code"], 0);
        drop(handle);
        assert!(
            lifecycle._test_slot_has_warm(TOOL_NAME).await,
            "cycle {cycle}: slot should be warm after release"
        );
    }
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "three warm calls must boot the VM exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tmp_is_wiped_between_warm_calls() {
    if skip_if_no_container_image() {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(60);

    // Call 1: write a sentinel under /tmp (the in-VM scratch tmpfs).
    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 1");
        let out = dispatch_over_handle(
            &mut handle,
            "open('/tmp/leak','w').write('secret'); print('wrote')",
        )
        .await;
        assert_eq!(out["exit_code"], 0, "call 1 should write the sentinel: {out}");
        drop(handle);
    }

    // Call 2 on the SAME warm VM: the sentinel must be gone (wiped at call start).
    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 2");
        let out = dispatch_over_handle(
            &mut handle,
            "import os; print('EXISTS' if os.path.exists('/tmp/leak') else 'GONE')",
        )
        .await;
        let stdout = out["stdout"].as_str().unwrap_or_default();
        assert!(
            stdout.contains("GONE"),
            "call 2 must not see call 1's /tmp sentinel (per-call wipe), got: {out}"
        );
        drop(handle);
    }

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "both calls ran on one warm VM (else the wipe assertion is vacuous)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_teardown_clears_warm_slot() {
    if skip_if_no_container_image() {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(1); // 1-second idle window

    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire");
        let _ = dispatch_over_handle(&mut handle, "print('ok')").await;
        drop(handle);
    }
    assert!(
        lifecycle._test_slot_has_warm(TOOL_NAME).await,
        "warm right after release"
    );

    tokio::time::sleep(Duration::from_millis(2_000)).await;

    assert!(
        !lifecycle._test_slot_has_warm(TOOL_NAME).await,
        "after the idle window the warm slot must be torn down"
    );
}
