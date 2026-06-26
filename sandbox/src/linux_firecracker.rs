//! Linux micro-VM backend for [`SandboxBackend`]: boots a Firecracker guest
//! and bridges the worker's JSON-RPC stdio over vsock.
//!
//! Defense-in-depth on top of (not instead of) bwrap/seccomp/Landlock/cgroup:
//! a throwaway guest kernel is the blast wall. The backend itself is a thin
//! pure-fn-then-spawn shell (mirrors [`crate::linux_bwrap`]); the boot + vsock
//! bridge live in the `kastellan-microvm-run` launcher binary that this
//! backend spawns as the `Child`.
//!
//! All of this module is `#[cfg(target_os = "linux")]`-gated (see lib.rs).

use std::process::Child;

use crate::{SandboxBackend, SandboxError, SandboxPolicy};

/// Boots workers inside a Firecracker micro-VM. Holds no mutable state
/// (`Send + Sync` via the empty struct), matching the other backends.
#[derive(Default)]
pub struct LinuxFirecracker;

impl LinuxFirecracker {
    pub fn new() -> Self {
        Self
    }
}

impl SandboxBackend for LinuxFirecracker {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<Child, SandboxError> {
        Err(SandboxError::Backend(
            "linux_firecracker: spawn not implemented yet (Task 2)".into(),
        ))
    }
}
