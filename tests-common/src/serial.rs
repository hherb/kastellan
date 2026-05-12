//! macOS-only serialisation lock for tests that touch the launchd
//! `gui/<uid>` domain.
//!
//! launchd's GUI domain is a process-global resource (one per uid),
//! so two tests that both `launchctl bootstrap`/`bootout` against it
//! can race even though they install distinct service labels. This
//! `Mutex<()>` is taken by each daemon-spawning test on macOS for the
//! duration of its `install` → `start` → `stop` → `uninstall` cycle.
//!
//! On Linux the function is a no-op (the systemd `--user` manager
//! handles concurrent service operations natively).

#[cfg(target_os = "macos")]
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(target_os = "macos")]
fn serial_mutex() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

/// Take the macOS-only launchd serial lock. The returned guard
/// releases the lock on drop.
///
/// On Linux this returns `()` (no synchronisation needed).
#[cfg(target_os = "macos")]
pub fn serial_lock() -> MutexGuard<'static, ()> {
    serial_mutex()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(not(target_os = "macos"))]
pub fn serial_lock() {}
