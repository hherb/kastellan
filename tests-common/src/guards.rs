//! RAII cleanup guards: `ServiceGuard` stops + uninstalls a supervisor
//! service on drop; `PathGuard` recursively wipes a directory on drop.
//!
//! Both are intentionally best-effort: `stop` / `uninstall` /
//! `remove_dir_all` errors are swallowed because the alternative — a
//! panic from `Drop` while another panic is unwinding — would abort
//! the whole test process and lose the original failure message.

use std::path::PathBuf;

use hhagent_supervisor::Supervisor;

/// Owns a supervisor handle + the service name to clean up. On drop,
/// calls `stop` then `uninstall`. Both are idempotent in the trait
/// contract, so a service that was never started, or was already
/// stopped, will drop cleanly.
pub struct ServiceGuard {
    /// Boxed because the trait is `dyn`-safe and we want to pass an
    /// owned handle in (constructing a fresh `default_supervisor()`
    /// for cleanup is wrong — Linux's `SystemdUser` carries a
    /// pre-resolved `XDG_RUNTIME_DIR` that would re-probe).
    pub sup: Box<dyn Supervisor>,
    pub name: String,
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = self.sup.stop(&self.name);
        let _ = self.sup.uninstall(&self.name);
    }
}

/// Owns a directory path. On drop, recursively removes the directory.
/// Best-effort: a missing path or a permissions failure is swallowed.
pub struct PathGuard {
    pub path: PathBuf,
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
