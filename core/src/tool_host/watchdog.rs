//! Wall-clock watchdog for a spawned worker.
//!
//! Lifted out of `tool_host.rs` (HANDOVER Next-TODO item 5, the file-size
//! sibling-lift) as a self-contained subsystem: a single worker can be given
//! a wall-clock budget, after which a background thread SIGKILLs it. The
//! watchdog is *cancelled* — never fired — when the owning
//! [`crate::tool_host::SupervisedWorker`] is dropped or closed first, so a
//! normal close races ahead of any kill.
//!
//! This module is a **descendant of `tool_host`**, so it can see
//! `tool_host`'s module-private items — but it has no reason to touch the
//! [`crate::tool_host::WorkerCommand`] seal and does not. Only
//! [`WatchdogGuard`] and [`spawn_watchdog`] are exported up to the parent
//! (`pub(super)`); the kill machinery below stays private to this file and is
//! exercised by the co-located tests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cancellation handle for the watchdog thread. When this guard is
/// dropped, the watchdog thread observes the cancel flag on its next
/// poll and exits without sending SIGKILL.
///
/// `pub(super)` so [`crate::tool_host::SupervisedWorker`] can hold one as a
/// field; the type is not part of the crate's public surface.
pub(super) struct WatchdogGuard {
    cancel: Arc<AtomicBool>,
}

impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        // Release ordering pairs with the Acquire load inside the
        // watchdog loop.
        self.cancel.store(true, Ordering::Release);
    }
}

/// Spawn the watchdog thread for a single worker.
///
/// Polling cadence is 50 ms — fine-grained enough that a normal close
/// races ahead of any kill (cancel flag is checked once per tick), and
/// coarse enough that the thread is essentially free.
///
/// Sending SIGKILL to a PID is a fundamentally racy operation if the PID
/// can be reused; the cancel flag closes that race for the *normal* exit
/// path. For pathological cases (worker exits naturally exactly at the
/// deadline) a SIGKILL to ESRCH is harmless. We rely on the fact that
/// Linux/macOS PID reuse is not instantaneous: short polling intervals
/// plus the fast-cancel-on-drop close any practical window.
///
/// `pub(super)` so [`crate::tool_host::spawn_worker`] can start it.
pub(super) fn spawn_watchdog(pid: u32, wall_clock_ms: u64) -> WatchdogGuard {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();
    let deadline = Instant::now() + Duration::from_millis(wall_clock_ms);

    std::thread::Builder::new()
        .name(format!("hhagent-watchdog-{pid}"))
        .spawn(move || watchdog_loop(pid, deadline, cancel_clone, send_sigkill))
        .expect("spawn watchdog thread");

    WatchdogGuard { cancel }
}

/// Pure-ish helper that the watchdog thread runs. Extracted so the loop
/// body is unit-testable without spawning a thread (see tests below).
///
/// `kill` is injected so tests can verify the control flow (cancel-flag
/// handling, deadline observance) without ever reaching `kill(2)`.
/// Production passes [`send_sigkill`]; tests pass a no-op. This is a
/// load-bearing test isolation — see the [`send_sigkill`] doc comment
/// for the 2026-05-08 host-blackout incident that motivated it.
fn watchdog_loop(pid: u32, deadline: Instant, cancel: Arc<AtomicBool>, kill: fn(u32)) {
    let tick = Duration::from_millis(50);
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            // Re-check the cancel flag right before firing — closes the
            // race where Drop set the flag while we were in the
            // pre-deadline branch.
            if !cancel.load(Ordering::Acquire) {
                kill(pid);
            }
            return;
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(std::cmp::min(remaining, tick));
    }
}

/// Whether `pid` is a value we are willing to deliver a SIGKILL to.
///
/// `kill(2)` treats certain `pid_t` values as broadcast operations:
///   - `0`  → signal every process in the caller's process group
///   - `-1` → signal every process the caller has permission to signal
///     (excluding init and the caller itself)
///
/// The Rust API here takes a `u32`; `pid as libc::pid_t` (an `i32` on
/// both Linux and macOS) collapses any value `> i32::MAX` to a negative
/// `pid_t`. The worst case is `u32::MAX → -1`.
///
/// PID 1 is init / launchd. We never spawn it; refusing to signal it
/// catches caller-bookkeeping bugs cheaply.
fn is_valid_target_pid(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// Best-effort SIGKILL. ESRCH (worker already exited) is the common case
/// after a natural close; we ignore it.
///
/// **Incident, 2026-05-08 (do not regress):** an earlier version of this
/// function called `libc::kill(pid as i32, SIGKILL)` with no validation.
/// The watchdog unit tests used `SAFE_FAKE_PID = u32::MAX` as a
/// "never-allocated PID" — but `u32::MAX as i32 == -1`, and
/// `kill(-1, SIGKILL)` signals every process the user can signal.
/// Running `watchdog_loop_runs_until_deadline_when_not_cancelled`
/// therefore SIGKILLed the user's X session, gnome-shell, and
/// per-session sshd children, producing what looked like a GPU-driver
/// display blackout. The fix is two-layered:
///   1. defensive guard here via [`is_valid_target_pid`] (PID 0, 1, and
///      anything that would cast to a negative `pid_t` are rejected),
///   2. an injected killer in [`watchdog_loop`] so tests never reach
///      `kill(2)` at all.
///
/// Do not remove either layer.
fn send_sigkill(pid: u32) {
    if !is_valid_target_pid(pid) {
        return;
    }
    // SAFETY: `kill(2)` with a valid `pid_t` and a signal number is a
    // defined syscall with no preconditions on Rust state. A bad PID
    // returns ESRCH which we ignore.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opaque PID for the watchdog tests. With the injected [`noop_kill`]
    /// it is never delivered to `kill(2)` — the value is just plumbed
    /// through `watchdog_loop` to exercise the control flow (cancel
    /// flag, deadline observance). The previous version of these tests
    /// used `u32::MAX` *and* a real `kill(2)`, which casts to `-1` and
    /// SIGKILLs every process the user owns; see the `send_sigkill` doc
    /// comment for the incident write-up.
    const SAFE_FAKE_PID: u32 = u32::MAX;

    /// No-op killer injected into [`watchdog_loop`] from tests.
    /// Whatever PID is passed in is simply discarded.
    fn noop_kill(_pid: u32) {}

    #[test]
    fn watchdog_loop_returns_immediately_when_cancelled_before_start() {
        let cancel = Arc::new(AtomicBool::new(true));
        let deadline = Instant::now() + Duration::from_secs(60);
        let started = Instant::now();
        watchdog_loop(SAFE_FAKE_PID, deadline, cancel, noop_kill);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "pre-cancelled watchdog must not wait for the deadline"
        );
    }

    #[test]
    fn watchdog_loop_runs_until_deadline_when_not_cancelled() {
        let cancel = Arc::new(AtomicBool::new(false));
        let budget = Duration::from_millis(150);
        let deadline = Instant::now() + budget;
        let started = Instant::now();
        watchdog_loop(SAFE_FAKE_PID, deadline, cancel, noop_kill);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= budget,
            "watchdog returned before deadline: elapsed={elapsed:?}, budget={budget:?}"
        );
        assert!(
            elapsed < budget + Duration::from_millis(200),
            "watchdog overshot the deadline by more than tick+slop: elapsed={elapsed:?}"
        );
    }

    #[test]
    fn watchdog_loop_observes_cancel_during_polling() {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        let deadline = Instant::now() + Duration::from_secs(60);

        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            watchdog_loop(SAFE_FAKE_PID, deadline, cancel_clone, noop_kill)
        });
        // Give the loop time to enter its sleep, then cancel.
        std::thread::sleep(Duration::from_millis(30));
        cancel.store(true, Ordering::Release);
        handle.join().expect("watchdog thread joined");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "cancelled watchdog must exit on the next poll, not wait the full deadline"
        );
    }

    /// Regression test for the 2026-05-08 host-blackout incident.
    ///
    /// `is_valid_target_pid` must reject every PID value that `kill(2)`
    /// would interpret as a broadcast: `0` (caller's process group),
    /// `1` (init / launchd, never our worker), and anything `> i32::MAX`
    /// — all of which collapse to a non-positive `pid_t`. The worst
    /// offender is `u32::MAX`, which casts to `-1` (every process the
    /// user can signal). If this test ever turns red, the fanout
    /// regression has resurfaced — fix `send_sigkill`, do **not**
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
        // Realistic worker PIDs accepted.
        assert!(is_valid_target_pid(2));
        assert!(is_valid_target_pid(12_345));
        assert!(is_valid_target_pid(i32::MAX as u32));
    }

    #[test]
    fn watchdog_guard_drop_sets_cancel_flag() {
        let cancel = Arc::new(AtomicBool::new(false));
        let guard = WatchdogGuard {
            cancel: cancel.clone(),
        };
        assert!(!cancel.load(Ordering::Acquire));
        drop(guard);
        assert!(
            cancel.load(Ordering::Acquire),
            "WatchdogGuard::Drop must set the cancel flag so the watchdog thread exits cleanly"
        );
    }
}
