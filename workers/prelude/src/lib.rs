//! `hhagent-worker-prelude`: defence-in-depth primitives that every tool
//! worker calls **from inside its own process** before it serves any request.
//!
//! ## Why a separate crate
//!
//! The parent (`core::tool_host`) already wraps every worker in `bwrap` from
//! the outside (Linux) or Seatbelt (macOS, future). That gives namespace +
//! coarse FS isolation. **This crate adds a second, finer-grained layer
//! applied from inside the worker:**
//!
//!   * **Landlock** — a kernel LSM filter that the worker installs on itself,
//!     restricting which paths it may read, write, or execute. Survives even
//!     if bwrap's mount setup is somehow circumvented.
//!   * **seccomp-bpf** — a BPF syscall filter that kills the process if it
//!     attempts a catastrophically dangerous syscall (mount, unshare, bpf,
//!     ptrace, kexec, …).
//!
//! Both layers are *worker-side*: even a worker compromised by malicious
//! tool input cannot lift them — `landlock::restrict_self()` and
//! `seccomp::apply_filter()` are one-way operations enforced by the kernel.
//!
//! ## Usage
//!
//! ```ignore
//! fn main() -> anyhow::Result<()> {
//!     let mut handler = MyHandler::from_env()?;
//!     hhagent_worker_prelude::serve_stdio(&mut handler)?;
//!     Ok(())
//! }
//! ```
//!
//! `serve_stdio` calls [`lock_down`] before dispatching any JSON-RPC traffic.
//! Workers that need finer-grained control may call [`lock_down`] directly
//! and then use `hhagent_protocol::server::serve_stdio` — but they are then
//! responsible for ensuring no I/O happens between dynamic-linker resolution
//! and the lock-down call.
//!
//! ## Cross-platform contract
//!
//! On non-Linux targets, [`lock_down`] is a no-op (returns
//! [`LockdownReport::SkippedNonLinux`]). The cross-platform contract is
//! preserved because the macOS Seatbelt backend (Phase 0b) installs the
//! equivalent containment from the parent side — the worker process on
//! macOS is launched already inside the Seatbelt profile.

#![deny(missing_debug_implementations)]

#[cfg(target_os = "linux")]
pub mod landlock_lock;
#[cfg(target_os = "linux")]
pub mod seccomp_lock;

use std::io;

use hhagent_protocol::server::Handler;

/// What `lock_down` actually managed to install. Returned so the worker can
/// log it (and tests can assert on it).
#[derive(Debug)]
pub enum LockdownReport {
    /// Both layers installed and enforcing.
    Linux {
        landlock: LandlockReport,
        seccomp: SeccompReport,
    },
    /// Non-Linux target — both layers are no-ops here. Containment is the
    /// parent's job (Seatbelt).
    SkippedNonLinux,
}

/// Status of the Landlock layer after `lock_down`.
#[derive(Debug, Clone, Copy)]
pub enum LandlockReport {
    /// Filter installed; kernel reports the requested ABI is fully supported.
    FullyEnforced,
    /// Filter installed but the kernel only supports a partial subset of
    /// the access rights we asked for. Logged but not fatal.
    PartiallyEnforced,
    /// Kernel is too old (no Landlock support, < 5.13) — filter not
    /// installed. The bwrap mount layer is still in effect, so isolation is
    /// degraded but not absent.
    KernelTooOld,
}

/// Status of the seccomp-bpf layer after `lock_down`.
#[derive(Debug, Clone, Copy)]
pub enum SeccompReport {
    /// BPF filter loaded and active.
    Installed,
    /// `HHAGENT_SECCOMP_PROFILE` env var was missing or set to `"none"`,
    /// so no filter was applied. Useful in tests; not recommended in prod.
    Disabled,
}

#[derive(Debug, thiserror::Error)]
pub enum LockdownError {
    #[error("landlock: {0}")]
    Landlock(String),
    #[error("seccomp: {0}")]
    Seccomp(String),
    #[error("env: {0}")]
    Env(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Apply both kernel layers, in order: Landlock first (it's a one-way FS
/// restriction), then seccomp (one-way syscall restriction).
///
/// Reads its policy from environment variables set by the parent process
/// (`core::tool_host`):
///
///   * `HHAGENT_LANDLOCK_RW`  — JSON array of absolute paths the worker may
///     write to (its scratch dir). Read-only access to `/usr`, `/lib*`,
///     `/etc/ld.so.cache` is implicit so dynamic-linker + libc still work.
///   * `HHAGENT_SECCOMP_PROFILE` — `"strict"`, `"net_client"`, or `"none"`.
///     `"none"` disables seccomp entirely (used in tests).
///
/// The function only fails on programmer error (malformed env, kernel ABI
/// returns an error). A kernel that lacks Landlock support is reported via
/// [`LandlockReport::KernelTooOld`], not via an error — callers should still
/// proceed, since bwrap is the primary containment layer.
pub fn lock_down() -> Result<LockdownReport, LockdownError> {
    #[cfg(target_os = "linux")]
    {
        let landlock = landlock_lock::apply_from_env()?;
        let seccomp = seccomp_lock::apply_from_env()?;
        Ok(LockdownReport::Linux { landlock, seccomp })
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(LockdownReport::SkippedNonLinux)
    }
}

/// Drop-in replacement for `hhagent_protocol::server::serve_stdio` that
/// applies [`lock_down`] before entering the request loop. This is the
/// recommended entry point for tool workers.
///
/// Errors from `lock_down` are returned as `io::Error` (kind `Other`) so the
/// caller can use the same `?` chain it was already using for the protocol
/// loop.
pub fn serve_stdio<H: Handler>(handler: &mut H) -> io::Result<()> {
    match lock_down() {
        Ok(report) => {
            // Single, structured line on stderr so the parent can capture it
            // for the audit log without parsing JSON. Workers that want
            // richer logging can call `lock_down` themselves and skip this.
            eprintln!("hhagent-worker-prelude: lockdown {report:?}");
        }
        Err(e) => {
            return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
        }
    }
    hhagent_protocol::server::serve_stdio(handler)
}
