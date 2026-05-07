//! hhagent-sandbox: declarative, cross-platform sandbox for tool workers.
//!
//! One [`SandboxPolicy`] drives all backends. Backend selection is automatic
//! per OS, with an optional micro-VM backend for stronger isolation.
//!
//! Backends (Phase 0/0b):
//!   - linux_bwrap   — bubblewrap + Landlock + seccomp-bpf
//!   - macos_seatbelt — sandbox-exec (Seatbelt) + setrlimit
//!   - microvm       — Firecracker (Linux) / Apple `container` CLI (macOS Tahoe+)

#[cfg(target_os = "linux")]
pub mod linux_bwrap;
#[cfg(target_os = "macos")]
pub mod macos_seatbelt;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Coarse profile presets that map to backend-specific defaults.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Profile {
    /// Strictest: no net by default, scratch FS only, minimal syscall set.
    WorkerStrict,
    /// Slightly relaxed for workers that need outbound HTTPS via the egress proxy.
    WorkerNetClient,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Net {
    /// Deny all network access.
    Deny,
    /// Allowlist of "host:port" entries. Egress still flows through the egress proxy.
    Allowlist(Vec<String>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// Read-only mounts/paths.
    pub fs_read: Vec<PathBuf>,
    /// Writable paths (typically a per-worker scratch dir).
    pub fs_write: Vec<PathBuf>,
    /// Network policy.
    pub net: Net,
    /// Hard CPU-time limit (milliseconds).
    pub cpu_ms: u64,
    /// Hard memory limit (megabytes).
    pub mem_mb: u64,
    /// Profile preset.
    pub profile: Profile,
    /// Environment variables to set inside the jail. Empty by default — the
    /// host environment is **always** cleared before this is applied, so the
    /// jail sees only what's listed here.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    /// Reserved for future backends (e.g. micro-VM) that want to signal a
    /// recognised-but-unimplemented case with a more specific message than
    /// the catch-all `Backend(...)`. Currently no constructor.
    #[allow(dead_code)]
    #[error("backend not yet implemented: {0}")]
    NotImplemented(&'static str),
    #[error("backend error: {0}")]
    Backend(String),
}

/// Common backend interface. To be implemented by [`linux_bwrap`], [`macos_seatbelt`],
/// and [`microvm`] in subsequent phases.
pub trait SandboxBackend {
    /// Build the argv (or equivalent invocation) that runs `program` with `args`
    /// under `policy`. Implementation detail of the backend; not stable yet.
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<std::process::Child, SandboxError>;
}

/// Pick the default backend for the current OS.
pub fn default_backend() -> Box<dyn SandboxBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux_bwrap::LinuxBwrap::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos_seatbelt::MacosSeatbelt::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Box::new(NotYetImplemented)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct NotYetImplemented;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl SandboxBackend for NotYetImplemented {
    fn spawn_under_policy(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[&str],
    ) -> Result<std::process::Child, SandboxError> {
        Err(SandboxError::Backend(
            "no sandbox backend for this OS — only Linux and macOS are supported".into(),
        ))
    }
}
