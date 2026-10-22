//! RAII cleanup guards: `ServiceGuard` stops + uninstalls a supervisor
//! service on drop; `PathGuard` recursively wipes a directory on drop.
//!
//! Both are intentionally best-effort: `stop` / `uninstall` /
//! `remove_dir_all` errors never panic (an `uninstall` error is printed as a
//! `[WARN]` line) because the alternative — a
//! panic from `Drop` while another panic is unwinding — would abort
//! the whole test process and lose the original failure message.

use std::path::PathBuf;

use kastellan_supervisor::Supervisor;

/// Owns a supervisor handle + the service name to clean up. On drop,
/// calls `stop` then `uninstall`. Both are idempotent in the trait
/// contract, so a service that was never started, or was already
/// stopped, will drop cleanly.
pub struct ServiceGuard {
    /// Boxed because the trait is `dyn`-safe and the guard needs to
    /// own its own handle (cannot borrow the test's handle without
    /// pinning the guard's lifetime). Callers typically pass a fresh
    /// `default_supervisor()` here: that re-probes `XDG_RUNTIME_DIR`
    /// on Linux, which is wasteful but harmless — both probes resolve
    /// to the same path, and the cost is one syscall during cleanup.
    pub sup: Box<dyn Supervisor>,
    pub name: String,
}

impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = self.sup.stop(&self.name);
        // Not swallowed silently: a failed uninstall may keep the unit file
        // (launchd keeps the plist after a failed bootout, #894), which then
        // starts at the next login — a gate's zero-`[WARN]` check sees this.
        if let Err(e) = self.sup.uninstall(&self.name) {
            eprint!("{}", crate::skip::warn_line(&format!("uninstall {} in cleanup: {e}", self.name)));
        }
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
