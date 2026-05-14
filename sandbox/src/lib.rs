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
#[cfg(target_os = "linux")]
pub mod linux_cgroup;
#[cfg(target_os = "macos")]
pub mod macos_seatbelt;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Coarse profile presets that map to backend-specific defaults.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Profile {
    /// Strictest: no net by default, scratch FS only, minimal syscall set.
    #[default]
    WorkerStrict,
    /// Slightly relaxed for workers that need outbound HTTPS via the egress proxy.
    WorkerNetClient,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum Net {
    /// Deny all network access.
    #[default]
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

impl Default for SandboxPolicy {
    /// Conservative defaults: no FS access, no network, strict profile,
    /// 1-second CPU budget, 64 MiB memory. Production callers (e.g.
    /// `shell_exec_entry`) override the limits explicitly; the `Default`
    /// impl exists so tests and future field additions can use
    /// `..Default::default()` without churning every fixture.
    fn default() -> Self {
        Self {
            fs_read: Vec::new(),
            fs_write: Vec::new(),
            net: Net::default(),
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::default(),
            env: Vec::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Common backend interface. To be implemented by [`linux_bwrap`], [`macos_seatbelt`],
/// and [`microvm`] in subsequent phases.
///
/// `Send + Sync` are required because backends are shared via `Arc<dyn SandboxBackend>`
/// across async tasks in the scheduler (one `Arc` per lane runner). Both concrete
/// implementations (`LinuxBwrap`, `MacosSeatbelt`) hold no mutable state and
/// satisfy these bounds automatically.
pub trait SandboxBackend: Send + Sync {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `Default` pins the most-restrictive sensible values: no FS access,
    /// no network, `WorkerStrict` profile, 1-second CPU budget, 64 MiB
    /// memory. The intent is that adding a field to [`SandboxPolicy`]
    /// (e.g. issue #6's `cpu_quota_pct`) doesn't require touching every
    /// test fixture; production callers must override the limits
    /// explicitly. Pinned so a future change to the defaults is a
    /// deliberate audit-trail edit.
    #[test]
    fn sandbox_policy_default_is_strict_deny_with_one_second_budget() {
        let p = SandboxPolicy::default();
        assert!(p.fs_read.is_empty());
        assert!(p.fs_write.is_empty());
        assert!(matches!(p.net, Net::Deny));
        assert_eq!(p.cpu_ms, 1_000);
        assert_eq!(p.mem_mb, 64);
        assert_eq!(p.profile, Profile::WorkerStrict);
        assert!(p.env.is_empty());
    }

    #[test]
    fn net_default_is_deny() {
        assert!(matches!(Net::default(), Net::Deny));
    }

    #[test]
    fn profile_default_is_worker_strict() {
        assert_eq!(Profile::default(), Profile::WorkerStrict);
    }
}
