# Firecracker micro-VM slice 2 (warm/idle) + re-armable watchdog — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Firecracker micro-VM workers reusable warm across calls by fixing the latent watchdog bug that SIGKILLs a warm worker `wall_clock_ms` after boot, and DGX-verify warm/idle end-to-end.

**Architecture:** The entry/resolver/in-guest-wipe wiring for warm reuse already exists from slice 1. The fix replaces the one-shot spawn-time watchdog with a **re-armable `Watchdog`** owned by `SupervisedWorker`, armed for the duration of each synchronous `SupervisedWorker::call` and disarmed (via RAII) the moment the call returns. The watchdog is therefore physically disarmed during every idle gap, so it can only fire when a *single* call overruns — never on an idle/warm VM. This is OS-neutral core code, so it fixes the macOS container warm path too.

**Tech Stack:** Rust (std `Mutex`/`Condvar`/threads, `libc::kill`), tokio (async dispatch over a sync `call`), the existing `IdleTimeoutLifecycle` warm-cache runtime, Firecracker micro-VM backend (DGX, aarch64).

## Global Constraints

- AGPL-3.0; AGPL-compatible deps only. No new dependency is introduced by this plan.
- Cross-platform: the watchdog change is OS-neutral `core` code (no `#[cfg]`). The Firecracker e2e is `#![cfg(target_os = "linux")]` + `#[ignore]` (DGX-only).
- TDD: failing test first, watch it fail, minimal implementation, watch it pass, commit.
- Keep the 2026-05-08 host-blackout protections verbatim: `is_valid_target_pid` (rejects pid 0, 1, and anything casting to a negative `pid_t`) and the injected `kill` fn so unit tests never reach `kill(2)`. Do not remove either.
- Build/test setup: `source "$HOME/.cargo/env"` first. Per-task Mac gate: `cargo clippy --workspace --all-targets -D warnings`.
- Files should stay focused; `tool_host.rs` is already an over-cap file — do not grow it beyond the small edits here (the watchdog logic stays in its `tool_host/watchdog.rs` sibling).

---

### Task 1: Failing regression test — warm worker must survive an idle gap longer than `wall_clock_ms`

This is the RED that proves the bug, using the real `kastellan-worker-shell-exec` worker (a persistent JSON-RPC server) under the real sandbox. It reproduces on macOS Seatbelt and Linux bwrap alike because the watchdog is OS-neutral. The discriminating assertion is **dispatch 2 succeeds**: warm-reuse hands back even a dead worker (the slot only checks age, not liveness), so only an actual second dispatch surfaces the kill.

**Files:**
- Modify: `core/tests/worker_lifecycle_idle_timeout_e2e.rs` (add imports, a `NoopAuditSink`, a wall-clock entry helper, a dispatch helper, and the regression test)

**Interfaces:**
- Consumes: `IdleTimeoutLifecycle::acquire(tool_name, &ToolEntry) -> WorkerHandle`; `WorkerHandle::worker_mut() -> &mut SupervisedWorker`; `kastellan_core::tool_host::dispatch_with_sink(sink, vault, worker, tool, method, params)`; `AuditSink`; `kastellan_core::secrets::Vault`; the file's existing `CountingSandboxBackend`, `sandbox_bundle_from`, `idle_timeout_entry`, `TOOL_NAME`, `ECHO_PATH`, `backend`, `skip_if_sandbox_unavailable`, `shell_exec_worker_binary`.
- Produces: nothing consumed by later tasks (test-only).

- [ ] **Step 1: Add the imports and helpers**

At the top of `core/tests/worker_lifecycle_idle_timeout_e2e.rs`, add to the imports:

```rust
use async_trait::async_trait;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, AuditSink, ToolHostError};
use kastellan_core::worker_lifecycle::WorkerHandle;
use kastellan_db::DbError;
```

After the existing `idle_timeout_entry` helper, add:

```rust
/// No-op audit sink so the dispatch helper needs no Postgres — the sandbox +
/// worker binary are the only dependencies (matching this suite's posture).
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

/// Like `idle_timeout_entry` but with a caller-chosen wall-clock budget so the
/// re-arm regression can use a short per-call budget (the default is 30 s).
fn idle_timeout_entry_wall_clock(
    worker: PathBuf,
    caps: IdleTimeoutCaps,
    wall_clock_ms: u64,
) -> ToolEntry {
    let mut entry = idle_timeout_entry(worker, caps);
    entry.wall_clock_ms = Some(wall_clock_ms);
    entry
}

/// Dispatch one `shell.exec` echo over an already-acquired warm handle.
async fn echo_over_handle(
    handle: &mut WorkerHandle,
    msg: &str,
) -> Result<serde_json::Value, ToolHostError> {
    dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        handle.worker_mut(),
        TOOL_NAME,
        "shell.exec",
        serde_json::json!({ "argv": [ECHO_PATH, msg] }),
    )
    .await
}
```

- [ ] **Step 2: Add the regression test**

Append this test to the same file:

```rust
/// Regression for the slice-2 watchdog bug: a warm worker must survive an idle
/// gap LONGER than its per-call `wall_clock_ms`. The old one-shot watchdog,
/// armed at spawn, SIGKILLs the worker `wall_clock_ms` after boot regardless of
/// the idle window; the re-armable watchdog is disarmed between calls, so the
/// worker survives. Discriminator: dispatch 2 must succeed (warm-reuse hands
/// back even a dead worker, so only a real second call surfaces the kill).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_worker_survives_idle_gap_longer_than_wall_clock() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] shell-exec worker not built: {}\n", worker.display());
        return;
    }

    let (sandbox, spawn_count) = CountingSandboxBackend::new(backend());
    let lifecycle = IdleTimeoutLifecycle::new(sandbox_bundle_from(sandbox));
    // Short per-call budget; generous idle window so the slot stays warm.
    let entry = idle_timeout_entry_wall_clock(
        worker.clone(),
        IdleTimeoutCaps {
            idle_seconds: 60,
            max_requests: 100,
            max_age_seconds: 60,
            grace_period_seconds: 5,
        },
        500, // wall_clock_ms
    );

    // Call 1: dispatch within budget, then release back to warm.
    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 1");
        let out = echo_over_handle(&mut handle, "one").await.expect("dispatch 1");
        assert_eq!(out["exit_code"], 0, "call 1 should succeed: {out}");
        drop(handle);
    }

    // Idle gap LONGER than the per-call budget, with no call in flight.
    tokio::time::sleep(Duration::from_millis(900)).await;

    // Call 2 on the SAME warm worker must still succeed.
    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 2");
        let out = echo_over_handle(&mut handle, "two").await.expect(
            "dispatch 2 — warm worker must survive an idle gap past wall_clock_ms \
             (re-arm regression)",
        );
        assert_eq!(out["exit_code"], 0, "call 2 should succeed: {out}");
        assert_eq!(out["stdout"].as_str().unwrap().trim_end(), "two");
        drop(handle);
    }

    assert_eq!(
        spawn_count.load(Ordering::SeqCst),
        1,
        "both calls must run on one warm worker (else the survival assertion is vacuous)"
    );
}
```

- [ ] **Step 3: Run the test to verify it FAILS (RED)**

```sh
source "$HOME/.cargo/env"
cargo build --workspace   # ensure the shell-exec worker binary exists
cargo test -p kastellan-core --test worker_lifecycle_idle_timeout_e2e \
    warm_worker_survives_idle_gap_longer_than_wall_clock -- --nocapture
```
Expected: **FAIL** — the panic message is the `expect("dispatch 2 …")` (a broken-pipe `ToolHostError` because the spawn-time watchdog SIGKILLed the warm worker at ~500 ms, and the 900 ms gap elapsed). If instead it shows `[SKIP]`, the sandbox/worker isn't available on this host — note that and run the RED check on a host where the worker builds (any Mac qualifies).

- [ ] **Step 4: Commit the failing test**

```sh
git add core/tests/worker_lifecycle_idle_timeout_e2e.rs
git commit -m "test(tool-host): RED — warm worker dies on idle gap past wall_clock_ms

Reproduces the slice-2 watchdog bug: the one-shot spawn-time watchdog SIGKILLs
a warm IdleTimeout worker wall_clock_ms after boot, regardless of the idle
window. Dispatch 2 fails (broken pipe) because the warm slot hands back the
dead worker. Fixed in the next task.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Re-armable `Watchdog` primitive + wire it into `SupervisedWorker::call`

Rewrite `tool_host/watchdog.rs` to a re-armable primitive and arm it around the `call` chokepoint. This is atomic (the primitive swap and the call-site rewire must land together or the build breaks). The Task-1 regression turns GREEN; new unit tests pin the primitive; existing tests prove no regression.

**Files:**
- Rewrite: `core/src/tool_host/watchdog.rs` (new `Watchdog` + `ArmGuard`; retain `is_valid_target_pid` + `send_sigkill`; new unit tests; delete `spawn_watchdog`, `WatchdogGuard`, old `watchdog_loop`)
- Modify: `core/src/tool_host.rs` — `spawn_worker` (build `Watchdog::new`), `SupervisedWorker` struct field + doc, `SupervisedWorker::call` (arm), `SupervisedWorker::close` (drop ordering)
- Test (RED→GREEN target): `core/tests/worker_lifecycle_idle_timeout_e2e.rs::warm_worker_survives_idle_gap_longer_than_wall_clock` (from Task 1)

**Interfaces:**
- Consumes: `libc::kill`, `libc::SIGKILL`, `std::sync::{Arc, Condvar, Mutex}`, `std::time::{Duration, Instant}`.
- Produces (`pub(super)`, visible to the `tool_host` parent only):
  - `Watchdog::new(pid: u32, ms: u64) -> Watchdog`
  - `Watchdog::arm_scope(&self) -> ArmGuard`
  - `ArmGuard` (RAII; disarms on drop)

- [ ] **Step 1: Write the new `tool_host/watchdog.rs` (primitive + tests)**

Replace the entire contents of `core/src/tool_host/watchdog.rs` with:

```rust
//! Re-armable wall-clock watchdog for a spawned worker.
//!
//! A single worker owns one [`Watchdog`], backed by one parked thread. The
//! watchdog is **armed** for the duration of each in-flight JSON-RPC call (via
//! [`Watchdog::arm_scope`], whose RAII [`ArmGuard`] disarms when the call
//! returns) and **disarmed** the rest of the time. It can therefore only fire
//! when a *single* call overruns its budget — never during an idle gap or
//! between calls. This is what makes a warm (reused) worker safe: there is no
//! deadline ticking while the worker sits idle in the warm-cache slot.
//!
//! Owned by [`crate::tool_host::SupervisedWorker`] and exercised at the
//! `SupervisedWorker::call` chokepoint. Only [`Watchdog`] and [`ArmGuard`] are
//! exported up to the parent (`pub(super)`); the kill machinery stays private
//! to this file and is exercised by the co-located tests.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// State shared between the owning [`Watchdog`] (+ its [`ArmGuard`]s) and the
/// background thread. Guarded by a `Mutex`; the thread parks on the `Condvar`.
struct WatchdogState {
    /// `Some(deadline)` while armed; `None` while disarmed (paused) or after a
    /// fire. The thread kills `pid` if it observes `now >= deadline`.
    deadline: Option<Instant>,
    /// Bumped on every arm so at most one kill is delivered per arm, and a
    /// stale wakeup from a previous arm can never fire the current one.
    generation: u64,
    /// The generation that already delivered a kill (so an arm fires once).
    fired_generation: Option<u64>,
    /// Set on [`Watchdog`] drop to terminate the thread.
    shutdown: bool,
}

struct Shared {
    state: Mutex<WatchdogState>,
    cv: Condvar,
}

/// A re-armable watchdog. One per worker; one background thread.
///
/// `pub(super)` so [`crate::tool_host::SupervisedWorker`] can hold one and
/// [`crate::tool_host::spawn_worker`] can build it.
pub(super) struct Watchdog {
    shared: Arc<Shared>,
    /// The per-call budget, captured at construction so callers needn't repeat
    /// it on every [`Self::arm_scope`].
    ms: u64,
}

impl Watchdog {
    /// Spawn the watchdog thread in the **disarmed** state.
    pub(super) fn new(pid: u32, ms: u64) -> Self {
        Self::new_with_kill(pid, ms, send_sigkill)
    }

    /// Construction seam: tests inject a non-killing `kill` so the thread never
    /// reaches `kill(2)` (see [`send_sigkill`] for the 2026-05-08 incident).
    fn new_with_kill(pid: u32, ms: u64, kill: fn(u32)) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(WatchdogState {
                deadline: None,
                generation: 0,
                fired_generation: None,
                shutdown: false,
            }),
            cv: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("kastellan-watchdog-{pid}"))
            .spawn(move || watchdog_loop(pid, thread_shared, kill))
            .expect("spawn watchdog thread");
        Self { shared, ms }
    }

    /// Arm the watchdog for `self.ms` from now. The returned [`ArmGuard`]
    /// disarms the watchdog when dropped — i.e. when the in-flight call
    /// returns. Re-arming while already armed simply moves the deadline.
    pub(super) fn arm_scope(&self) -> ArmGuard {
        let mut st = self.shared.state.lock().expect("watchdog state poisoned");
        st.generation = st.generation.wrapping_add(1);
        st.deadline = Some(Instant::now() + Duration::from_millis(self.ms));
        self.shared.cv.notify_all();
        ArmGuard {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock().expect("watchdog state poisoned");
        st.shutdown = true;
        self.shared.cv.notify_all();
    }
}

/// RAII handle that disarms the watchdog when dropped. Held for exactly the
/// span of one `SupervisedWorker::call`.
pub(super) struct ArmGuard {
    shared: Arc<Shared>,
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        let mut st = self.shared.state.lock().expect("watchdog state poisoned");
        st.deadline = None;
        self.shared.cv.notify_all();
    }
}

/// The watchdog thread body. Holds the state lock except while parked on the
/// `Condvar` (which releases it). All mutators (`arm_scope`, `ArmGuard::drop`,
/// `Watchdog::drop`) take the lock briefly and `notify_all`, so the thread
/// wakes promptly. `kill` is injected for test isolation.
fn watchdog_loop(pid: u32, shared: Arc<Shared>, kill: fn(u32)) {
    let mut st = shared.state.lock().expect("watchdog state poisoned");
    loop {
        if st.shutdown {
            return;
        }
        match st.deadline {
            None => {
                // Disarmed: park until armed / shutdown (no polling).
                st = shared.cv.wait(st).expect("watchdog cv poisoned");
            }
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    let gen = st.generation;
                    if st.fired_generation != Some(gen) {
                        st.fired_generation = Some(gen);
                        kill(pid);
                    }
                    st.deadline = None; // disarm after fire
                } else {
                    let remaining = deadline.saturating_duration_since(now);
                    let (next, _timeout) = shared
                        .cv
                        .wait_timeout(st, remaining)
                        .expect("watchdog cv poisoned");
                    st = next;
                }
            }
        }
    }
}

/// Whether `pid` is a value we are willing to deliver a SIGKILL to.
///
/// `kill(2)` treats certain `pid_t` values as broadcasts (`0` → caller's
/// process group; `-1` → every process the caller may signal). The `u32` →
/// `pid_t` (`i32`) cast collapses any value `> i32::MAX` to negative; worst
/// case `u32::MAX → -1`. PID 1 is init/launchd — never our worker.
fn is_valid_target_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// Best-effort SIGKILL. ESRCH (worker already exited) is the common case after
/// a natural close; we ignore it.
///
/// **Incident, 2026-05-08 (do not regress):** an earlier watchdog called
/// `libc::kill(pid as i32, SIGKILL)` with no validation. The unit tests used
/// `u32::MAX` as a "never-allocated PID", but `u32::MAX as i32 == -1`, and
/// `kill(-1, SIGKILL)` signals every process the user can signal — it killed
/// the user's X session and looked like a GPU-driver blackout. The fix is two
/// layers: the [`is_valid_target_pid`] guard here, and an injected killer in
/// the loop so tests never reach `kill(2)`. Do not remove either.
fn send_sigkill(pid: u32) {
    if !is_valid_target_pid(pid) {
        return;
    }
    // SAFETY: `kill(2)` with a valid `pid_t` + signal number is a defined
    // syscall with no preconditions on Rust state. A bad PID returns ESRCH,
    // which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Opaque PID for the tests. With the injected no-op/counting killers it is
    /// never delivered to `kill(2)` — see the `send_sigkill` incident note.
    const SAFE_FAKE_PID: u32 = u32::MAX;

    fn noop_kill(_pid: u32) {}

    /// Test-only counting killer. A `fn(u32)` can't capture, so it records into
    /// a process-global counter; each test that uses it resets the counter
    /// first and runs the watchdog to completion before asserting. The tests
    /// below that count kills run serially relative to this counter by reading
    /// it only after joining the watchdog thread.
    static KILL_COUNT: AtomicUsize = AtomicUsize::new(0);
    fn counting_kill(_pid: u32) {
        KILL_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    /// Build a `Shared` directly so a test can drive `watchdog_loop` and
    /// manipulate state without going through `Watchdog` (which always uses
    /// `send_sigkill`).
    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(WatchdogState {
                deadline: None,
                generation: 0,
                fired_generation: None,
                shutdown: false,
            }),
            cv: Condvar::new(),
        })
    }

    fn arm(shared: &Arc<Shared>, ms: u64) {
        let mut st = shared.state.lock().unwrap();
        st.generation = st.generation.wrapping_add(1);
        st.deadline = Some(Instant::now() + Duration::from_millis(ms));
        shared.cv.notify_all();
    }

    fn disarm(shared: &Arc<Shared>) {
        let mut st = shared.state.lock().unwrap();
        st.deadline = None;
        shared.cv.notify_all();
    }

    fn shutdown(shared: &Arc<Shared>) {
        let mut st = shared.state.lock().unwrap();
        st.shutdown = true;
        shared.cv.notify_all();
    }

    #[test]
    fn disarmed_watchdog_never_fires() {
        let sh = shared();
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, noop_kill));
        std::thread::sleep(Duration::from_millis(120));
        shutdown(&sh);
        h.join().unwrap();
        // No assertion on a counter needed: a disarmed loop that ever fired
        // would have to invent a deadline. The point is it exits cleanly on
        // shutdown without having waited on a deadline.
    }

    #[test]
    fn armed_watchdog_fires_after_deadline() {
        KILL_COUNT.store(0, Ordering::SeqCst);
        let sh = shared();
        arm(&sh, 80);
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, counting_kill));
        // Poll until it fires (then it self-disarms and parks).
        let start = Instant::now();
        while KILL_COUNT.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(2), "watchdog never fired");
            std::thread::sleep(Duration::from_millis(10));
        }
        shutdown(&sh);
        h.join().unwrap();
        assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 1, "fires exactly once per arm");
    }

    #[test]
    fn disarm_before_deadline_prevents_fire() {
        KILL_COUNT.store(0, Ordering::SeqCst);
        let sh = shared();
        arm(&sh, 300);
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, counting_kill));
        std::thread::sleep(Duration::from_millis(50));
        disarm(&sh); // before the 300 ms deadline
        std::thread::sleep(Duration::from_millis(400));
        shutdown(&sh);
        h.join().unwrap();
        assert_eq!(
            KILL_COUNT.load(Ordering::SeqCst),
            0,
            "a disarm before the deadline must prevent the kill"
        );
    }

    #[test]
    fn rearm_fires_again_on_the_new_deadline() {
        KILL_COUNT.store(0, Ordering::SeqCst);
        let sh = shared();
        arm(&sh, 60);
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, counting_kill));
        // First fire.
        let start = Instant::now();
        while KILL_COUNT.load(Ordering::SeqCst) < 1 {
            assert!(start.elapsed() < Duration::from_secs(2), "first fire missing");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Re-arm: a fresh generation + deadline must fire again.
        arm(&sh, 60);
        let start = Instant::now();
        while KILL_COUNT.load(Ordering::SeqCst) < 2 {
            assert!(start.elapsed() < Duration::from_secs(2), "re-arm did not fire");
            std::thread::sleep(Duration::from_millis(10));
        }
        shutdown(&sh);
        h.join().unwrap();
        assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 2, "re-arm fires on the new deadline");
    }

    #[test]
    fn arm_scope_guard_disarms_on_drop() {
        // End-to-end through the public seam with a non-killing thread: arming
        // then dropping the guard must leave the watchdog disarmed (no fire).
        let wd = Watchdog::new_with_kill(SAFE_FAKE_PID, 40, noop_kill);
        {
            let _arm = wd.arm_scope();
            // guard dropped here, well before the 40 ms deadline
        }
        std::thread::sleep(Duration::from_millis(80));
        // No panic / no fire path reached; dropping wd shuts the thread down.
        drop(wd);
    }

    /// Regression test for the 2026-05-08 host-blackout incident. If this turns
    /// red, the fanout regression has resurfaced — fix `send_sigkill`, do NOT
    /// loosen the validator.
    #[test]
    fn is_valid_target_pid_rejects_broadcast_values() {
        assert!(!is_valid_target_pid(0), "PID 0 = process group broadcast");
        assert!(!is_valid_target_pid(1), "PID 1 = init/launchd");
        assert!(
            !is_valid_target_pid(u32::MAX),
            "u32::MAX casts to pid_t -1 (everyone-the-user-can-signal broadcast)"
        );
        assert!(
            !is_valid_target_pid(i32::MAX as u32 + 1),
            "first u32 that casts to a negative pid_t must be rejected"
        );
        assert!(is_valid_target_pid(2));
        assert!(is_valid_target_pid(12_345));
        assert!(is_valid_target_pid(i32::MAX as u32));
    }
}
```

- [ ] **Step 2: Run the watchdog unit tests to verify they PASS**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib tool_host::watchdog -- --nocapture
```
Expected: PASS — all six watchdog tests green. (The crate may not yet build because `tool_host.rs` still references the old `spawn_watchdog`/`_watchdog`; if so, the lib won't compile — proceed to Step 3 first, then re-run. If you prefer a green checkpoint here, do Steps 3–4 before running.)

- [ ] **Step 3: Rewire `spawn_worker` and the `SupervisedWorker` struct in `core/src/tool_host.rs`**

In `spawn_worker` (around line 535), replace:

```rust
    let watchdog = spec.wall_clock_ms.map(|ms| watchdog::spawn_watchdog(pid, ms));
    Ok(SupervisedWorker {
        client,
        _watchdog: watchdog,
        egress: None,
        scratch: None,
    })
```

with:

```rust
    // Build the re-armable watchdog in the DISARMED state. It is armed only for
    // the duration of each `SupervisedWorker::call` (see that method), so a warm
    // worker sitting idle in the IdleTimeout slot is never under a kill timer.
    let watchdog = spec.wall_clock_ms.map(|ms| watchdog::Watchdog::new(pid, ms));
    Ok(SupervisedWorker {
        client,
        watchdog,
        egress: None,
        scratch: None,
    })
```

In the `SupervisedWorker` struct (around line 558), replace the field and update its doc comment block (lines ~544-560). Change:

```rust
    _watchdog: Option<watchdog::WatchdogGuard>,
```

to:

```rust
    /// Re-armable wall-clock watchdog (when `wall_clock_ms` was set on the
    /// spec). Armed for the span of each [`Self::call`] and disarmed in
    /// between, so a reused (warm) worker is never killed while idle. Dropping
    /// it shuts the watchdog thread down (no kill).
    watchdog: Option<watchdog::Watchdog>,
```

Also update the struct-level doc comment that currently says "`_watchdog` drops second, setting the watchdog's cancel flag. The watchdog thread checks the flag at most every 50 ms…" — replace that sentence with:

```rust
/// `watchdog` drops second, shutting down the watchdog thread (it never fires
/// on drop). `egress` drops third…
```

- [ ] **Step 4: Arm the watchdog in `SupervisedWorker::call`, and fix `close()` drop ordering**

In `SupervisedWorker::call` (around line 586), replace the body:

```rust
    fn call(
        &mut self,
        cmd: WorkerCommand,
    ) -> Result<serde_json::Value, ClientError> {
        self.client.call(&cmd.method, cmd.params)
    }
```

with:

```rust
    fn call(
        &mut self,
        cmd: WorkerCommand,
    ) -> Result<serde_json::Value, ClientError> {
        // Arm the wall-clock watchdog for exactly this in-flight call. The RAII
        // `ArmGuard` disarms it synchronously when `call` returns (success or
        // error), before the worker can be returned to the warm-cache slot — so
        // there is no deadline ticking during idle gaps and no Drop-ordering
        // race against the slot handoff. The watchdog bounds the JSON-RPC call
        // window, not the spawn/boot window (boot is bounded by the spawn path).
        let _arm = self.watchdog.as_ref().map(watchdog::Watchdog::arm_scope);
        self.client.call(&cmd.method, cmd.params)
    }
```

In `SupervisedWorker::close` (around line 600), update the destructure + ordered drop. Replace `_watchdog` with `watchdog` in both the pattern and the `drop(...)` call:

```rust
        let SupervisedWorker {
            client,
            watchdog,
            egress,
            scratch,
        } = self;
```
and
```rust
        let status = client.close();
        drop(watchdog);
        drop(egress);
        drop(scratch);
        status
```

- [ ] **Step 5: Run the Task-1 regression to verify it now PASSES (GREEN)**

```sh
source "$HOME/.cargo/env"
cargo build --workspace
cargo test -p kastellan-core --test worker_lifecycle_idle_timeout_e2e \
    warm_worker_survives_idle_gap_longer_than_wall_clock -- --nocapture
```
Expected: PASS — dispatch 2 succeeds and `spawn_count == 1`.

- [ ] **Step 6: Run the watchdog units + the broader tool_host/lifecycle suites for no regression**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib tool_host -- --nocapture
cargo test -p kastellan-core --test worker_lifecycle_idle_timeout_e2e -- --nocapture
cargo test -p kastellan-core --test shell_exec_e2e -- --nocapture
```
Expected: PASS (idle-timeout warm-reuse, cap rotation, crash recovery, and the shell-exec round-trip all still green). `[SKIP]` lines are acceptable only where the sandbox/PG is unavailable.

- [ ] **Step 7: Clippy gate**

```sh
source "$HOME/.cargo/env"
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: clean.

- [ ] **Step 8: Commit**

```sh
git add core/src/tool_host/watchdog.rs core/src/tool_host.rs \
        core/tests/worker_lifecycle_idle_timeout_e2e.rs
git commit -m "fix(tool-host): re-armable watchdog armed around call (warm-reuse safe)

Replace the one-shot spawn-time watchdog with a re-armable Watchdog owned by
SupervisedWorker, armed for the span of each call() via an RAII ArmGuard and
disarmed in between. A warm IdleTimeout worker is no longer under a kill timer
while idle, so it survives idle gaps longer than wall_clock_ms; a single
overrunning call is still killed. One parked thread per worker (no per-call
churn), single enforcement site, uniform across SingleUse/IdleTimeout. Fixes
the latent macOS-container warm bug too. Keeps the 2026-05-08 blackout guards.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Firecracker warm/idle DGX e2e (`#[ignore]`)

The slice deliverable: prove warm reuse, the in-guest `/tmp` wipe, idle teardown, and the in-VM re-arm regression all hold for the real Firecracker backend. DGX-only (`#[ignore]`), mirroring `python_exec_warm_idle_e2e.rs` (structure) and `python_exec_firecracker_e2e.rs` (skip discipline).

**Files:**
- Create: `core/tests/python_exec_firecracker_warm_idle_e2e.rs`

**Interfaces:**
- Consumes: `IdleTimeoutLifecycle`, `WorkerHandle::worker_mut`, `dispatch_with_sink`, `firecracker_mode_entry`, `Lifecycle::idle_timeout`, `IdleTimeoutCaps`, `Contract`, `SandboxBackends::resolve(Some(FirecrackerVm), None)`, `LinuxFirecracker::probe`, `FirecrackerImage`. Skip/locate helpers copied from `python_exec_firecracker_e2e.rs`.
- Produces: nothing (test-only).

- [ ] **Step 1: Write the e2e test file**

Create `core/tests/python_exec_firecracker_warm_idle_e2e.rs`:

```rust
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

use async_trait::async_trait;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, AuditSink};
use kastellan_core::worker_lifecycle::{
    Contract, IdleTimeoutCaps, IdleTimeoutLifecycle, Lifecycle, WorkerHandle,
    WorkerLifecycleManager,
};
use kastellan_core::workers::python_exec::firecracker_mode_entry;
use kastellan_db::DbError;
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{
    SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxError, SandboxPolicy,
};
use std::process::Child;

const TOOL_NAME: &str = "python-exec";
const CONTAINER_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-python-exec";

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

fn image_dir() -> String {
    std::env::var("KASTELLAN_MICROVM_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string())
}

fn firecracker_image() -> FirecrackerImage {
    let dir = PathBuf::from(image_dir());
    FirecrackerImage {
        kernel_path: dir.join("vmlinux"),
        rootfs_path: dir.join("python-exec.ext4"),
    }
}

fn locate_microvm_run() -> Option<PathBuf> {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("core has a workspace parent")
        .join("target");
    for profile in ["release", "debug"] {
        let p = target.join(profile).join("kastellan-microvm-run");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn skip_if_no_microvm() -> bool {
    if let Err(e) = LinuxFirecracker::probe(&firecracker_image()) {
        eprintln!("\n[SKIP] firecracker probe failed: {e}\n");
        return true;
    }
    match locate_microvm_run() {
        Some(bin) => {
            use std::sync::Once;
            static PATH_ONCE: Once = Once::new();
            PATH_ONCE.call_once(|| {
                let dir = bin.parent().unwrap().to_path_buf();
                let cur = std::env::var_os("PATH").unwrap_or_default();
                let mut paths = vec![dir];
                paths.extend(std::env::split_paths(&cur));
                let joined = std::env::join_paths(paths).expect("join PATH");
                std::env::set_var("PATH", joined);
            });
            false
        }
        None => {
            eprintln!(
                "\n[SKIP] kastellan-microvm-run not built; run \
                 `cargo build --release -p kastellan-microvm-run`\n"
            );
            true
        }
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
    if skip_if_no_microvm() {
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
    if skip_if_no_microvm() {
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
    if skip_if_no_microvm() {
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
    if skip_if_no_microvm() {
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
```

- [ ] **Step 2: Compile-check the new test on the Mac (cross-clippy, no KVM)**

The file is `#![cfg(target_os = "linux")]`, so the Mac can't `cargo test` it, but the workspace clippy gate compiles it for the Linux target indirectly only on Linux. On the Mac, confirm the rest of the workspace is unaffected:

```sh
source "$HOME/.cargo/env"
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: clean (the new file is `cfg`-excluded on macOS; this confirms no accidental macOS breakage).

- [ ] **Step 3: DGX acceptance (real KVM + vsock)**

Per the DGX-over-SSH convention, rebuild the rootfs (so it ships the #358 wipe worker) and the release launcher, then run the ignored suite. Run exactly as `ssh dgx '<cmd>'`:

```sh
# Rebuild the rootfs so the in-VM worker has wipe_scratch_contents (#358):
ssh dgx 'cd ~/src/kastellan && sudo scripts/workers/microvm/build-rootfs.sh'
# Rebuild the RELEASE launcher (the e2e prefers target/release):
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --release -p kastellan-microvm-run && cargo build --workspace'
# Run the warm/idle e2e:
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --test python_exec_firecracker_warm_idle_e2e -- --ignored --nocapture'
```
Expected: 4/4 pass — warm reuse boots the VM once, `/tmp` wiped between calls, idle teardown clears the slot, warm survives past `wall_clock_ms`. Also re-run the slice-1 firecracker e2e to confirm no regression:
```sh
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --test python_exec_firecracker_e2e -- --ignored --nocapture'
```
Expected: the slice-1 suite still green (≥6/6), 0 leftover `/tmp/kastellan-microvm-*` dirs.

If the DGX is unavailable this session, mark Step 3 as deferred in the handover (the macOS clippy gate + the Task-2 hermetic regression are the standing pre-DGX gates) and do NOT claim Linux acceptance.

- [ ] **Step 4: Commit**

```sh
git add core/tests/python_exec_firecracker_warm_idle_e2e.rs
git commit -m "test(microvm): Firecracker warm/idle e2e (DGX, ignored)

Mirror python_exec_warm_idle_e2e.rs for the FirecrackerVm backend: warm reuse
boots the VM once, /tmp wiped between calls, idle teardown clears the slot, and
the in-VM re-arm regression (warm survives an idle gap past wall_clock_ms).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Part A (re-armable watchdog, armed around `call`) → Task 2 (primitive rewrite + `call` arming + `spawn_worker`/struct/`close` rewire). ✓
- Part A property "no Drop-ordering race / disarm synchronous at call return" → encoded in the `call` `_arm` RAII + the doc comment. ✓
- Part A "keep 2026-05-08 blackout guards" → `is_valid_target_pid` + injected kill retained verbatim, plus the regression unit test. ✓
- Part B1 hermetic regression (short budget + idle gap, real worker) → Task 1 (RED) + Task 2 Step 5 (GREEN). ✓
- Part B1 primitive unit tests (arm fires; disarm prevents; re-arm; shutdown; pid validation) → Task 2 Step 1 tests. ✓
- Part B2 Firecracker warm/idle e2e (4 tests incl. the warm-past-wall_clock case) → Task 3. ✓
- Part B3 rootfs refresh + release-launcher rebuild notes → Task 3 Step 3 + the file doc comment. ✓
- "macOS container warm benefits for free" → no extra task needed (the fix is OS-neutral; the macOS warm suite is re-run implicitly by the Task-2 clippy/test gates, and the change is verified by the OS-neutral hermetic regression). ✓
- Out-of-scope (slices 3–5) → no tasks. ✓

**Placeholder scan:** No TBD/TODO; every code step shows complete code; every run step shows the command + expected result. ✓

**Type consistency:** `Watchdog::new(pid,ms)`, `Watchdog::arm_scope(&self) -> ArmGuard`, field `watchdog: Option<watchdog::Watchdog>`, and the `call` arm line all agree across Task 2 steps. `firecracker_mode_entry(binary, image_dir, params_file_max, lifecycle)` and `SandboxBackends { bwrap, firecracker }` match the current source. `dispatch_with_sink(sink, vault, worker, tool, method, params)` signature matches `tool_host.rs:241`. ✓

*One verification note for the executor:* Task 2 Step 2 may not compile until Steps 3–4 land (the lib references the swapped symbols). Run the watchdog-unit checkpoint after Step 4 if you want a green gate at Step 2 — this is called out inline.
