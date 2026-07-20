//! End-to-end: python-exec under the **Linux Firecracker micro-VM** with the
//! warm/idle lifecycle (`KASTELLAN_PYTHON_EXEC_IDLE_SECONDS > 0`). The Linux
//! counterpart of `python_exec_warm_idle_e2e.rs` (macOS `MacosContainer`).
//!
//! Pins the four properties warm reuse must hold for the VM backend:
//!   1. Warm reuse — N acquire→dispatch→release cycles boot the VM ONCE
//!      (spawn-counting backend; also proves the vsock bridge survives multiple
//!      sequential JSON-RPC calls on one connection).
//!   2. /tmp wipe across reuse — a sentinel under /tmp from call 1 is GONE for
//!      call 2 on the same warm VM (the in-guest #358 wipe).
//!   3. Idle teardown — after `idle_seconds` with no call, the warm slot clears.
//!   4. Warm reuse past wall_clock_ms — a short per-call budget with a longer
//!      idle gap; the warm VM survives (the slice-2 re-arm fix, in-VM).
//!
//! DGX-only / `#[ignore]`: needs /dev/kvm + /dev/vhost-vsock, a built
//! rootfs+kernel (`scripts/workers/microvm/build-rootfs.sh` — REBUILD so the
//! rootfs ships the current worker with the #358 /tmp wipe), firecracker on
//! $PATH, and the launcher built (`cargo build --release -p kastellan-microvm-run`
//! — the e2e prefers target/release, so a stale release binary shadows source
//! changes). Run with:
//!   cargo test -p kastellan-core --test python_exec_firecracker_warm_idle_e2e -- --ignored --nocapture

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::dispatch_with_sink;
use kastellan_core::worker_lifecycle::{
    Contract, IdleTimeoutCaps, IdleTimeoutLifecycle, Lifecycle, WorkerHandle,
    WorkerLifecycleManager,
};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_sandbox::{
    SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxError, SandboxPolicy,
};
use kastellan_tests_common::microvm::{image_dir, skip_if_no_microvm};
use kastellan_tests_common::NoopAuditSink;
use std::process::Child;

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "python-exec.ext4";

const TOOL_NAME: &str = "python-exec";
const CONTAINER_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-python-exec";

/// Spawn-counting wrapper over the real Firecracker backend.
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


/// IdleTimeout lifecycle whose Firecracker slot is the spawn-counting backend.
fn lifecycle_with_counter(count: Arc<AtomicUsize>) -> IdleTimeoutLifecycle {
    let real = SandboxBackends::default_for_current_os()
        .resolve(Some(SandboxBackendKind::FirecrackerVm), None);
    let counting: Arc<dyn SandboxBackend> = Arc::new(CountingBackend { inner: real, count });
    // The python-exec firecracker entry sets sandbox_backend: Some(FirecrackerVm),
    // so only the firecracker slot is consulted; the bwrap slot is unused by this
    // entry but must be present — reuse the same arc.
    let bundle = Arc::new(SandboxBackends {
        bwrap: Arc::clone(&counting),
        firecracker: counting,
    });
    IdleTimeoutLifecycle::new(bundle)
}

/// A firecracker entry with an explicit idle window + wall-clock budget.
fn warm_entry(idle_seconds: u64, wall_clock_ms: u64) -> kastellan_core::scheduler::ToolEntry {
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
    let mut entry = firecracker_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        image_dir(),
        None,
        lifecycle,
    );
    entry.wall_clock_ms = Some(wall_clock_ms);
    entry
}

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
#[ignore = "needs DGX: /dev/kvm + vhost_vsock + built rootfs + kastellan-microvm-run"]
async fn firecracker_warm_reuse_three_calls_boot_vm_once() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(60, 30_000);

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
#[ignore = "needs DGX"]
async fn firecracker_tmp_is_wiped_between_warm_calls() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(60, 30_000);

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
#[ignore = "needs DGX"]
async fn firecracker_idle_teardown_clears_warm_slot() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(1, 30_000); // 1-second idle window

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs DGX"]
async fn firecracker_warm_survives_idle_gap_past_wall_clock() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    // Short per-call budget; longer idle window. The old one-shot watchdog would
    // SIGKILL the launcher Child (and thus the VM) wall_clock_ms after boot; the
    // re-armable watchdog is disarmed between calls, so the VM survives.
    let entry = warm_entry(60, 2_000);

    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 1");
        let out = dispatch_over_handle(&mut handle, "print(1)").await;
        assert_eq!(out["exit_code"], 0, "call 1 should succeed: {out}");
        drop(handle);
    }

    // Idle gap longer than the per-call budget, no call in flight.
    tokio::time::sleep(Duration::from_millis(3_000)).await;

    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 2");
        let out = dispatch_over_handle(&mut handle, "print(2)").await;
        assert_eq!(
            out["exit_code"], 0,
            "call 2 — warm VM must survive an idle gap past wall_clock_ms: {out}"
        );
        assert_eq!(out["stdout"].as_str().unwrap_or_default().trim(), "2");
        drop(handle);
    }

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "both calls ran on one warm VM (else the survival assertion is vacuous)"
    );
}
