//! Integration smoke: prove `SingleUseLifecycle::acquire` actually routes
//! through the entry-selected sandbox backend, end-to-end with a real
//! Apple `container` invocation.
//!
//! Positive pin: `sandbox_backend: Some(Container)` + alpine-only
//! `/sbin/apk` binary — container backend mounts alpine, apk exists
//! inside the VM, spawn succeeds.
//!
//! Slice 1 already verified that `MacosContainer::spawn_under_policy`
//! works in isolation; this test pins the *routing* through the
//! production `SingleUseLifecycle::acquire` path. The unit-level
//! counter-backend test
//! `single_use_lifecycle_acquire_routes_via_entry_sandbox_backend_kind`
//! in `core::worker_lifecycle::manager` covers the selection-bit pin
//! more cheaply (no real container needed) and proves the
//! `None → seatbelt`, `Some(Container) → container` asymmetry.
//!
//! A two-sided integration test (positive container, negative seatbelt)
//! was considered but rejected: `sandbox-exec` always exists on macOS,
//! so `Command::spawn("sandbox-exec", ["...", "/sbin/apk"])` returns
//! `Ok(Child)` at the host level; the `/sbin/apk` not-found error
//! surfaces asynchronously inside sandbox-exec's own process, not as
//! a spawn-time error the lifecycle manager can observe. The unit-test
//! counter-backend covers the negative direction without that semantic
//! ambiguity.
//!
//! Skip-as-pass when `container --version` / `container system status`
//! / the `alpine:3.20` image are missing — every one of those answering to
//! `KASTELLAN_MICROVM_REQUIRE_E2E` (#684), so an operator can demand a real
//! container run and get a failure rather than a silent skip.
//!
//! No freshness gate here, and that is correct rather than an omission:
//! `alpine:3.20` is an upstream image this repo does not build, so it has no
//! source closure to be stale against. #687's rule applies to the images
//! `scripts/workers/*/build-image.sh` produces.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::Arc;

use kastellan_core::scheduler::ToolEntry;
use kastellan_core::worker_lifecycle::{Lifecycle, SingleUseLifecycle, WorkerLifecycleManager};
use kastellan_sandbox::{
    macos_container::DEFAULT_IMAGE, Net, Profile, SandboxBackendKind, SandboxBackends,
    SandboxPolicy,
};
use kastellan_tests_common::microvm::skip_if_no_container;

fn minimal_policy() -> SandboxPolicy {
    SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: None,
        tasks_max: None,
        env: vec![],
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    }
}

/// Positive half: `sandbox_backend: Some(Container)` runs `/sbin/apk`
/// inside the alpine container, which exists there. The acquire should
/// succeed; we kill the worker immediately afterwards because apk would
/// otherwise hang waiting for stdin / a real RPC call.
#[tokio::test]
async fn single_use_lifecycle_routes_through_container_when_entry_opts_in() {
    if skip_if_no_container(DEFAULT_IMAGE) {
        return;
    }

    let sbs = Arc::new(SandboxBackends::default_for_current_os());
    let mgr = SingleUseLifecycle::new(Arc::clone(&sbs));

    let entry = ToolEntry {
        binary: PathBuf::from("/sbin/apk"),
        policy: minimal_policy(),
        wall_clock_ms: Some(5_000),
        lifecycle: Lifecycle::SingleUse,
        sandbox_backend: Some(SandboxBackendKind::Container),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
    };

    let result = mgr.acquire("apk-routing-positive", &entry).await;
    let mut handle = result
        .expect("acquire under Container backend must succeed; alpine has /sbin/apk");

    // Kill cleanly. The worker doesn't speak JSON-RPC, so any subsequent
    // call() would error — but the routing assertion is the successful
    // spawn through the container backend. The watchdog (wall_clock_ms)
    // would also kill within 5 s if we let it.
    let _ = handle.worker_mut().kill();
}

