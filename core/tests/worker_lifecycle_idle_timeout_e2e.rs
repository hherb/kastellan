//! Integration tests for `IdleTimeoutLifecycle` (slice-2 runtime).
//!
//! Drives `IdleTimeoutLifecycle::acquire` against the real `hhagent-worker-shell-exec`
//! binary under the real sandbox backend. Each test constructs its own `ToolEntry`
//! declaring `Lifecycle::IdleTimeout` — the production `shell_exec_entry()` stays
//! single-use per the slice-1 pin.
//!
//! ## What this test pins
//!
//! 1. **Warm reuse** — 3 sequential acquire+release cycles for the same tool spawn
//!    exactly one worker process (via the `CountingSandboxBackend` wrapper).
//! 2. **`max_requests` rotation** — when `max_requests = 2`, the third acquire is a
//!    fresh spawn (counter = 2 after 3 cycles).
//! 3. **`max_age_seconds` rotation** — when `max_age_seconds = 1` and we sleep 1.5 s
//!    between acquires, the second acquire is a fresh spawn (counter = 2).
//! 4. **Idle teardown** — when `idle_seconds = 1`, after acquire+release+sleep(2 s)
//!    the warm slot is empty.
//! 5. **Crash recovery + backoff** — `handle.report_crash()` clears the warm slot,
//!    bumps `consecutive_restarts`, and sets `next_spawn_allowed_at`. The next acquire
//!    after the backoff elapses succeeds.
//! 6. **Concurrent serialisation** — two parallel tokio tasks acquiring the same tool
//!    don't overlap; the second's acquire-completion timestamp comes after the first's
//!    release.
//!
//! ## Skip behaviour
//!
//! Skips with `[SKIP]` lines on hosts missing the sandbox backend or the worker
//! binary. macOS hosts without bwrap (i.e. all of them) use Seatbelt automatically.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hhagent_core::scheduler::ToolEntry;
use hhagent_core::worker_lifecycle::{
    IdleTimeoutCaps, IdleTimeoutLifecycle, Lifecycle, RestartBackoff, WorkerLifecycleManager,
};
use hhagent_sandbox::{SandboxBackend, SandboxError, SandboxPolicy};
use hhagent_tests_common::sandbox::{backend, policy_for_shell_exec, skip_if_sandbox_unavailable};
use hhagent_tests_common::binaries::shell_exec_worker_binary;

#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";
#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

/// Sandbox-backend wrapper that counts every `spawn_under_policy` call.
///
/// The warm-reuse + cap-rotation tests assert against this counter to prove that
/// `IdleTimeoutLifecycle` *only* invokes `spawn_worker` when the cache miss demands
/// it. Wraps the real backend so the spawned worker is identical to production.
struct CountingSandboxBackend {
    inner: Box<dyn SandboxBackend>,
    count: Arc<AtomicUsize>,
}

impl CountingSandboxBackend {
    fn new(inner: Box<dyn SandboxBackend>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let wrapper = Arc::new(Self {
            inner,
            count: Arc::clone(&count),
        });
        (wrapper, count)
    }
}

impl SandboxBackend for CountingSandboxBackend {
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

/// Build a `ToolEntry` declaring `Lifecycle::IdleTimeout` against the shell-exec
/// worker. The production `shell_exec_entry()` declares `SingleUse` and stays that
/// way (slice-1 pin); tests need a fresh entry to opt-in to warm-keeping.
fn idle_timeout_entry(worker: PathBuf, caps: IdleTimeoutCaps) -> ToolEntry {
    let policy = policy_for_shell_exec(&worker, &[ECHO_PATH]);
    let contract = hhagent_core::worker_lifecycle::Contract { stateless: true };
    let lifecycle = Lifecycle::idle_timeout(caps, contract).expect("valid lifecycle");
    ToolEntry {
        binary: worker,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle,
    }
}

/// Tool name used in slot lookups — must match `acquire_impl`'s
/// `entry.binary.file_name()` derivation.
fn tool_name_for_binary(worker: &std::path::Path) -> String {
    worker
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| worker.to_string_lossy().into_owned())
}

#[tokio::test]
async fn warm_reuse_three_acquires_yield_one_spawn() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] shell-exec worker not built: {}\n", worker.display());
        return;
    }

    let (sandbox, spawn_count) = CountingSandboxBackend::new(backend());
    let lifecycle = IdleTimeoutLifecycle::new(sandbox);
    let tool_name = tool_name_for_binary(&worker);
    let entry = idle_timeout_entry(
        worker.clone(),
        IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 60,
            grace_period_seconds: 5,
        },
    );

    for cycle in 1..=3 {
        let handle = lifecycle.acquire(&entry).await.expect("acquire");
        drop(handle);
        // After release the slot should be warm (well within caps).
        assert!(
            lifecycle._test_slot_has_warm(&tool_name).await,
            "cycle {cycle}: slot should be warm after release"
        );
    }

    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        1,
        "three cycles should yield exactly one spawn (warm reuse)"
    );
}

#[tokio::test]
async fn max_requests_cap_forces_respawn() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] shell-exec worker not built: {}\n", worker.display());
        return;
    }

    let (sandbox, spawn_count) = CountingSandboxBackend::new(backend());
    let lifecycle = IdleTimeoutLifecycle::new(sandbox);
    let tool_name = tool_name_for_binary(&worker);
    let entry = idle_timeout_entry(
        worker.clone(),
        IdleTimeoutCaps {
            idle_seconds: 60,
            // Cap fires after the second release (request_count 2 == cap).
            max_requests: 2,
            max_age_seconds: 60,
            grace_period_seconds: 5,
        },
    );

    // Cycle 1: spawn, release → warm.
    let h1 = lifecycle.acquire(&entry).await.expect("acquire 1");
    drop(h1);
    assert!(lifecycle._test_slot_has_warm(&tool_name).await);
    assert_eq!(spawn_count.load(Ordering::SeqCst), 1);

    // Cycle 2: warm-reuse, release → max_requests hits, slot cleared.
    let h2 = lifecycle.acquire(&entry).await.expect("acquire 2");
    drop(h2);
    assert!(
        !lifecycle._test_slot_has_warm(&tool_name).await,
        "max_requests cap should have terminated the warm worker"
    );
    assert_eq!(spawn_count.load(Ordering::SeqCst), 1);

    // Cycle 3: fresh spawn.
    let h3 = lifecycle.acquire(&entry).await.expect("acquire 3");
    drop(h3);
    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        2,
        "third acquire after cap should be a fresh spawn"
    );
}

#[tokio::test]
async fn max_age_cap_forces_respawn_when_aged_out() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] shell-exec worker not built: {}\n", worker.display());
        return;
    }

    let (sandbox, spawn_count) = CountingSandboxBackend::new(backend());
    let lifecycle = IdleTimeoutLifecycle::new(sandbox);
    let tool_name = tool_name_for_binary(&worker);
    let entry = idle_timeout_entry(
        worker.clone(),
        IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 1, // 1-second age cap
            grace_period_seconds: 5,
        },
    );

    // Cycle 1: spawn, release → warm.
    let h1 = lifecycle.acquire(&entry).await.expect("acquire 1");
    drop(h1);
    assert_eq!(spawn_count.load(Ordering::SeqCst), 1);

    // Sleep past max_age. Use 1500ms to give plenty of margin against CI variance.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Cycle 2: warm-but-aged-out should drop the existing worker on acquire and
    // spawn a fresh one.
    let h2 = lifecycle.acquire(&entry).await.expect("acquire 2");
    drop(h2);

    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        2,
        "aged-out warm worker should be replaced on next acquire"
    );
    // Slot must be repopulated by the fresh spawn — but it'll age out again shortly.
    // We don't assert on slot state here because the idle-teardown task scheduled
    // earlier may fire concurrently; the only invariant is the spawn counter.
    let _ = tool_name;
}

#[tokio::test]
async fn idle_seconds_teardown_clears_warm_slot() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] shell-exec worker not built: {}\n", worker.display());
        return;
    }

    let (sandbox, _spawn_count) = CountingSandboxBackend::new(backend());
    let lifecycle = IdleTimeoutLifecycle::new(sandbox);
    let tool_name = tool_name_for_binary(&worker);
    let entry = idle_timeout_entry(
        worker.clone(),
        IdleTimeoutCaps {
            idle_seconds: 1, // 1-second idle teardown
            max_requests: 100,
            max_age_seconds: 60,
            grace_period_seconds: 5,
        },
    );

    let handle = lifecycle.acquire(&entry).await.expect("acquire");
    drop(handle);

    // Right after release: slot is warm.
    assert!(
        lifecycle._test_slot_has_warm(&tool_name).await,
        "slot should be warm immediately after release"
    );

    // Sleep past idle_seconds. The one-shot teardown task should fire.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        !lifecycle._test_slot_has_warm(&tool_name).await,
        "slot should be empty after idle_seconds teardown"
    );
}

#[tokio::test]
async fn crash_recovery_bumps_consecutive_restarts_and_clears_slot() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] shell-exec worker not built: {}\n", worker.display());
        return;
    }

    // Tight backoff so the test runs fast — 50 ms base, 100 ms cap.
    let backoff = RestartBackoff {
        base: Duration::from_millis(50),
        factor_num: 2,
        factor_den: 1,
        cap: Duration::from_millis(100),
    };
    let (sandbox, spawn_count) = CountingSandboxBackend::new(backend());
    let lifecycle = IdleTimeoutLifecycle::with_backoff(sandbox, backoff);
    let tool_name = tool_name_for_binary(&worker);
    let entry = idle_timeout_entry(
        worker.clone(),
        IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 60,
            grace_period_seconds: 5,
        },
    );

    // Acquire + report crash + release. Release path should: drop worker, clear
    // warm, bump consecutive_restarts to 1, set next_spawn_allowed_at = now + 50ms.
    let mut handle = lifecycle.acquire(&entry).await.expect("acquire 1");
    handle.report_crash();
    drop(handle);

    assert!(
        !lifecycle._test_slot_has_warm(&tool_name).await,
        "crashed worker should not be returned to slot"
    );
    assert_eq!(
        lifecycle._test_slot_consecutive_restarts(&tool_name).await,
        1,
        "consecutive_restarts should bump on crash"
    );

    // Sleep past backoff. Next acquire should succeed; the spawn counter goes up.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let h2 = lifecycle.acquire(&entry).await.expect("acquire 2");
    drop(h2);

    // One clean release resets consecutive_restarts to 0.
    assert_eq!(
        lifecycle._test_slot_consecutive_restarts(&tool_name).await,
        0,
        "successful release should reset consecutive_restarts"
    );
    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        2,
        "crash + successful retry should yield two spawns"
    );
}

#[tokio::test]
async fn concurrent_acquires_for_same_tool_serialize() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] shell-exec worker not built: {}\n", worker.display());
        return;
    }

    let (sandbox, _spawn_count) = CountingSandboxBackend::new(backend());
    let lifecycle = Arc::new(IdleTimeoutLifecycle::new(sandbox));
    let entry = idle_timeout_entry(
        worker.clone(),
        IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 60,
            grace_period_seconds: 5,
        },
    );

    // Task 1: acquire, sleep, release.
    let mgr1 = Arc::clone(&lifecycle);
    let entry1 = entry.clone();
    let start = Instant::now();
    let t1 = tokio::spawn(async move {
        let acquired_at = Instant::now() - start;
        let handle = mgr1.acquire(&entry1).await.expect("acquire 1");
        let held = Duration::from_millis(150);
        tokio::time::sleep(held).await;
        drop(handle);
        let released_at = Instant::now() - start;
        (acquired_at, released_at)
    });

    // Small delay to ensure task 1 wins the race for the slot.
    tokio::time::sleep(Duration::from_millis(25)).await;

    let mgr2 = Arc::clone(&lifecycle);
    let entry2 = entry.clone();
    let t2 = tokio::spawn(async move {
        let acquired_at = Instant::now() - start;
        let handle = mgr2.acquire(&entry2).await.expect("acquire 2");
        let acquired_completed_at = Instant::now() - start;
        drop(handle);
        (acquired_at, acquired_completed_at)
    });

    let (t1_acquired, t1_released) = t1.await.expect("task 1");
    let (t2_started, t2_acquired) = t2.await.expect("task 2");

    // Task 2 must have started before task 1 released (we delayed 25 ms; task 1's
    // hold is 150 ms — task 2 enters the await well before release).
    assert!(
        t2_started < t1_released,
        "task 2 should have started awaiting before task 1 released"
    );
    // Task 2's acquire must complete *after* task 1's release (serialised through
    // the per-slot mutex).
    assert!(
        t2_acquired >= t1_released,
        "task 2's acquire must complete after task 1's release (got t2_acquired={t2_acquired:?}, t1_released={t1_released:?})"
    );
    let _ = t1_acquired;
}
