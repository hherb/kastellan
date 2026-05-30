//! macOS-only serialisation lock for tests that touch the launchd
//! `gui/<uid>` domain.
//!
//! launchd's GUI domain is a process-global resource (one per uid),
//! so two tests that both `launchctl bootstrap`/`bootout` against it
//! can race even though they install distinct service labels. This
//! lock is taken by each daemon-spawning test on macOS — and by
//! `bring_up_pg_cluster` (issue #130) — for the duration of its
//! `install` → `start` → `stop` → `uninstall` cycle.
//!
//! # Reentrant on purpose
//!
//! The lock is a reentrant mutex, not a plain `Mutex`. Some callers
//! (`supervisor_e2e`, `observation_capture`) take the lock for their
//! whole test and *then* call `bring_up_pg_cluster`, which now takes the
//! lock itself. With a non-reentrant `Mutex` that second acquire on the
//! same thread would self-deadlock; a reentrant mutex lets the same
//! thread re-acquire freely while still excluding *other* threads.
//! (Different test threads contend exactly as before — the cross-thread
//! guarantee is unchanged.)
//!
//! We use [`parking_lot::ReentrantMutex`] because `std::sync::ReentrantLock`
//! is still unstable on the 1.96 toolchain. `parking_lot` is already in
//! the build graph (MIT/Apache-2.0, AGPL-compatible) and its mutex does
//! not poison, so there is no poison-recovery branch to maintain.
//!
//! On Linux the function is a no-op (the systemd `--user` manager
//! handles concurrent service operations natively).

#[cfg(target_os = "macos")]
use parking_lot::{ReentrantMutex, ReentrantMutexGuard};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;

#[cfg(target_os = "macos")]
fn serial_mutex() -> &'static ReentrantMutex<()> {
    static M: OnceLock<ReentrantMutex<()>> = OnceLock::new();
    M.get_or_init(|| ReentrantMutex::new(()))
}

/// Take the macOS-only launchd serial lock. The returned guard
/// releases the lock on drop. Reentrant on the same thread (see the
/// module docs); blocks across threads.
///
/// On Linux this returns `()` (no synchronisation needed).
#[cfg(target_os = "macos")]
pub fn serial_lock() -> ReentrantMutexGuard<'static, ()> {
    serial_mutex().lock()
}

#[cfg(not(target_os = "macos"))]
pub fn serial_lock() {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    //! Behavioural pins for the macOS serial lock.
    //!
    //! These are gated to macOS because on Linux `serial_lock()` returns
    //! `()` (a no-op) and there is no guard to reason about.

    use super::serial_lock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::Duration;

    /// The serial lock must be **reentrant on a single thread**: a test
    /// that already holds it (e.g. `supervisor_e2e` / `observation_capture`
    /// take it before spawning a daemon) and then calls
    /// `bring_up_pg_cluster` — which also takes the lock (issue #130) —
    /// must not self-deadlock. A spawned worker acquires the lock twice
    /// and signals completion; a non-reentrant `Mutex` deadlocks on the
    /// second acquire and the signal never arrives, so the 5 s
    /// `recv_timeout` fires and the assertion fails.
    #[test]
    fn serial_lock_is_reentrant_on_same_thread() {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outer = serial_lock();
            let inner = serial_lock(); // re-acquire on the same thread
            drop(inner);
            drop(outer);
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "serial_lock() must be reentrant on the same thread; the double \
             acquire did not complete within 5 s, which means it deadlocked"
        );
    }

    /// Switching the lock to `ReentrantLock` must not weaken its core
    /// guarantee: two *different* threads still mutually exclude. Four
    /// threads each enter the critical section, bump a shared counter,
    /// and record the running maximum; if exclusion holds, no more than
    /// one thread is ever inside at once, so the max observed is exactly 1.
    #[test]
    fn serial_lock_excludes_across_threads() {
        let inside = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let inside = Arc::clone(&inside);
            let max_seen = Arc::clone(&max_seen);
            handles.push(thread::spawn(move || {
                let _guard = serial_lock();
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(20));
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "serial_lock() must serialize across threads; observed more than \
             one thread inside the critical section simultaneously"
        );
    }
}
