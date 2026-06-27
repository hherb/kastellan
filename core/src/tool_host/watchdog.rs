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

    /// Construction seam: tests inject a non-killing `kill` (a closure
    /// capturing a per-test counter) so the thread never reaches `kill(2)`
    /// (see [`send_sigkill`] for the 2026-05-08 incident). Generic over the
    /// killer so each test owns its own counter — no shared mutable state, so
    /// the unit tests are parallel-safe under cargo's default test harness.
    fn new_with_kill<K: Fn(u32) + Send + 'static>(pid: u32, ms: u64, kill: K) -> Self {
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
fn watchdog_loop<K: Fn(u32)>(pid: u32, shared: Arc<Shared>, kill: K) {
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

    /// Opaque PID for the tests. With the injected counting killers it is never
    /// delivered to `kill(2)` — see the `send_sigkill` incident note.
    const SAFE_FAKE_PID: u32 = u32::MAX;

    /// A per-test counting killer: returns a `(closure, counter)` pair. Each
    /// test owns its own `Arc<AtomicUsize>`, so there is no shared mutable
    /// state and the tests are parallel-safe. The closure is `Send + 'static`
    /// (captures only the Arc), satisfying the watchdog thread's bound.
    fn counting_kill() -> (impl Fn(u32) + Send + 'static, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        (move |_pid: u32| { c.fetch_add(1, Ordering::SeqCst); }, count)
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
        let (kill, count) = counting_kill();
        let sh = shared();
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, kill));
        std::thread::sleep(Duration::from_millis(120));
        shutdown(&sh);
        h.join().unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a watchdog that was never armed must never fire"
        );
    }

    #[test]
    fn armed_watchdog_fires_after_deadline() {
        let (kill, count) = counting_kill();
        let sh = shared();
        arm(&sh, 80);
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, kill));
        // Poll until it fires (then it self-disarms and parks).
        let start = Instant::now();
        while count.load(Ordering::SeqCst) == 0 {
            assert!(start.elapsed() < Duration::from_secs(2), "watchdog never fired");
            std::thread::sleep(Duration::from_millis(10));
        }
        shutdown(&sh);
        h.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1, "fires exactly once per arm");
    }

    #[test]
    fn disarm_before_deadline_prevents_fire() {
        let (kill, count) = counting_kill();
        let sh = shared();
        arm(&sh, 300);
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, kill));
        std::thread::sleep(Duration::from_millis(50));
        disarm(&sh); // before the 300 ms deadline
        std::thread::sleep(Duration::from_millis(400));
        shutdown(&sh);
        h.join().unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a disarm before the deadline must prevent the kill"
        );
    }

    #[test]
    fn rearm_fires_again_on_the_new_deadline() {
        let (kill, count) = counting_kill();
        let sh = shared();
        arm(&sh, 60);
        let sh2 = Arc::clone(&sh);
        let h = std::thread::spawn(move || watchdog_loop(SAFE_FAKE_PID, sh2, kill));
        // First fire.
        let start = Instant::now();
        while count.load(Ordering::SeqCst) < 1 {
            assert!(start.elapsed() < Duration::from_secs(2), "first fire missing");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Re-arm: a fresh generation + deadline must fire again.
        arm(&sh, 60);
        let start = Instant::now();
        while count.load(Ordering::SeqCst) < 2 {
            assert!(start.elapsed() < Duration::from_secs(2), "re-arm did not fire");
            std::thread::sleep(Duration::from_millis(10));
        }
        shutdown(&sh);
        h.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2, "re-arm fires on the new deadline");
    }

    #[test]
    fn arm_scope_guard_disarms_on_drop() {
        // End-to-end through the public seam: arming then dropping the guard
        // before the deadline must leave the watchdog disarmed (no fire).
        let (kill, count) = counting_kill();
        let wd = Watchdog::new_with_kill(SAFE_FAKE_PID, 40, kill);
        {
            let _arm = wd.arm_scope();
            // guard dropped here, well before the 40 ms deadline
        }
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "dropping the ArmGuard before the deadline must disarm (no fire)"
        );
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
