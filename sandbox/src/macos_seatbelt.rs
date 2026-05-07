//! macOS backend for [`SandboxBackend`]: shells out to `/usr/bin/sandbox-exec`
//! (Seatbelt). Mirrors the Linux `linux_bwrap` backend's shape:
//!   - `build_profile(policy)` is a pure function returning the TinyScheme
//!     `.sb` profile we hand to `sandbox-exec -p`.
//!   - `MacosSeatbelt::probe()` runs a minimal `sandbox-exec /usr/bin/true`
//!     to verify Seatbelt is healthy on this host.
//!   - `MacosSeatbelt::spawn_under_policy()` validates the policy paths,
//!     builds the profile, and spawns the worker.
//!
//! What this backend gives you (Phase 0b):
//!   - Mandatory Access Control (MAC) via Seatbelt: default-deny FS, default-deny
//!     network, explicit allowlists for /usr/lib, /System/Library, /dev's safe
//!     nodes, and per-policy fs_read / fs_write paths.
//!   - Environment cleared via `Command::env_clear()` before exec (analogue of
//!     bwrap's `--clearenv`); `policy.env` re-applied on top.
//!   - `Command::process_group(0)` so the worker is in its own session
//!     (analogue of `--new-session`).
//!
//! Not yet (deferred to supervisor work):
//!   - `setrlimit` for `policy.cpu_ms` / `policy.mem_mb`.
//!   - A `--die-with-parent` equivalent. macOS has no `PR_SET_PDEATHSIG`;
//!     either a `kqueue(EVFILT_PROC, NOTE_EXIT)` watcher or supervisor lifecycle
//!     handles this. Today the worker can outlive a crashed parent — caught by
//!     the supervisor in Phase 0 cont.
//!
//! See [`docs/superpowers/specs/2026-05-07-macos-seatbelt-backend-design.md`].

use std::process::{Child, Command, Stdio};

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

/// Shell out to `/usr/bin/sandbox-exec` for sandboxing.
#[derive(Default)]
pub struct MacosSeatbelt;

impl MacosSeatbelt {
    pub fn new() -> Self {
        Self
    }
}

impl SandboxBackend for MacosSeatbelt {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<Child, SandboxError> {
        Err(SandboxError::Backend(
            "MacosSeatbelt::spawn_under_policy not yet implemented".into(),
        ))
    }
}

// Avoid unused-import warnings until later tasks fill these in.
#[allow(dead_code)]
fn _unused_imports_marker(_c: Command, _s: Stdio) {}
