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
pub mod macos_container;
#[cfg(target_os = "macos")]
pub mod macos_seatbelt;

use std::path::PathBuf;
use std::sync::Arc;

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
    /// Hard CPU-time limit (milliseconds). Enforced via
    /// `setrlimit(RLIMIT_CPU)` from the worker prelude (POSIX, so applies
    /// on Linux and macOS). `0` means "unset, no rlimit applied".
    pub cpu_ms: u64,
    /// Hard memory limit (megabytes).
    ///
    /// * **Linux:** enforced via cgroup `MemoryMax` by [`crate::linux_cgroup`].
    /// * **macOS Seatbelt:** **not enforced** (Seatbelt has no memory
    ///   primitive; `RLIMIT_AS` has high false-positive risk for
    ///   malloc-heavy workers and is intentionally deferred).
    /// * **macOS Apple `container` backend** ([`crate::macos_container`]):
    ///   enforced via `container run -m <N>M` with SIGKILL on overrun.
    ///   Note the **200 MiB floor** — `container` rejects smaller values;
    ///   the backend clamps and emits a `tracing::warn!` so operators see
    ///   the silent widening. Opt-in per-worker (Slice 2 wiring), not the
    ///   default macOS backend.
    pub mem_mb: u64,
    /// Profile preset.
    pub profile: Profile,
    /// Per-worker CPU bandwidth ceiling (percent of one CPU). `None`
    /// falls back to the backend's defense-in-depth default.
    ///
    /// * **Linux cgroup:** enforced; default 200%, hardcoded in
    ///   [`crate::linux_cgroup`].
    /// * **macOS Seatbelt:** no effect (no equivalent primitive).
    /// * **macOS Apple `container` backend:** enforced via
    ///   `container run -c <fractional vCPUs>`; `None` lets `container`
    ///   pick up its host `--default-cpus` configuration (no
    ///   backend-emitted default, deliberately diverging from
    ///   `linux_cgroup` to avoid silently capping the per-host setting).
    #[serde(default)]
    pub cpu_quota_pct: Option<u32>,
    /// Per-worker max task count. `None` falls back to the backend's
    /// defense-in-depth default.
    ///
    /// * **Linux cgroup:** enforced via `pids.max` (per-cgroup process
    ///   count, kernel-enforced); default 64.
    /// * **macOS Seatbelt:** no effect (no equivalent primitive).
    /// * **macOS Apple `container` backend:** enforced via `container
    ///   run --ulimit nproc=<N>:<N>`, which becomes per-real-UID
    ///   `RLIMIT_NPROC` inside the Linux VM. **Semantic gap worth
    ///   knowing:** the Linux cgroup form is per-cgroup; the container
    ///   form is per-UID across the VM. Inside a one-worker container
    ///   running as a single UID the practical effect is similar, but
    ///   the guarantees are not identical.
    #[serde(default)]
    pub tasks_max: Option<u64>,
    /// Environment variables to set inside the jail. Empty by default
    /// — the host environment is **always** cleared before this is
    /// applied, so the jail sees only what's listed here.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl Default for SandboxPolicy {
    /// Conservative defaults: no FS access, no network, strict profile,
    /// 1-second CPU budget, 64 MiB memory, no cgroup overrides. Production
    /// callers (e.g. `shell_exec_entry`) override the limits explicitly;
    /// the `Default` impl exists so tests and future field additions can
    /// use `..Default::default()` without churning every fixture.
    fn default() -> Self {
        Self {
            fs_read: Vec::new(),
            fs_write: Vec::new(),
            net: Net::default(),
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::default(),
            cpu_quota_pct: None,
            tasks_max: None,
            env: Vec::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Operator-facing identifier for selecting a specific sandbox backend
/// per-worker. Cfg-gated per-OS so cross-OS mis-config (e.g. declaring
/// `Container` on Linux) is a compile-time error rather than a runtime
/// surprise.
///
/// `None` on a `ToolEntry.sandbox_backend` means "use the per-OS
/// default" — today darwin → `Seatbelt`, linux → `Bwrap`. Only opt in
/// here when a worker has a concrete reason to diverge (e.g. needs
/// memory enforcement on macOS, which `Seatbelt` can't provide).
///
/// See `docs/superpowers/specs/2026-05-21-macos-container-slice-2-design.md`
/// for the rationale behind OS-specific variant names vs an abstract
/// `MicroVm` category.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SandboxBackendKind {
    #[cfg(target_os = "linux")]
    Bwrap,
    #[cfg(target_os = "macos")]
    Seatbelt,
    #[cfg(target_os = "macos")]
    Container,
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
///
/// Kept for direct-spawn callers (e.g. `tests-common::sandbox::backend()`)
/// that don't need per-entry selection. Daemon-backed call sites
/// construct [`SandboxBackends::default_for_current_os`] instead — that
/// bundle supports the per-worker `sandbox_backend` opt-in introduced
/// by Slice 2.
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

/// Per-OS bundle of constructed sandbox backends, used by the lifecycle
/// managers to resolve a per-worker [`SandboxBackendKind`] to a concrete
/// `Arc<dyn SandboxBackend>`.
///
/// Fields are cfg-gated to match `SandboxBackendKind` — every variant
/// of the enum that exists at compile time has a backing field, so
/// [`SandboxBackends::resolve`] is total (no runtime panic path for
/// "unknown variant").
///
/// Constructed once at daemon startup via
/// [`SandboxBackends::default_for_current_os`] (cheap — backends hold
/// no mutable state) and threaded through the lifecycle managers as
/// `Arc<SandboxBackends>`. Tests build a custom instance directly via
/// struct-literal syntax with their own counter / stub backends.
///
/// `Clone` is provided so consumers that thread the bundle through
/// async boundaries can copy the per-field `Arc`s cheaply.
#[derive(Clone)]
pub struct SandboxBackends {
    #[cfg(target_os = "linux")]
    pub bwrap: Arc<dyn SandboxBackend>,
    #[cfg(target_os = "macos")]
    pub seatbelt: Arc<dyn SandboxBackend>,
    #[cfg(target_os = "macos")]
    pub container: Arc<dyn SandboxBackend>,
}

impl SandboxBackends {
    /// Construct the per-OS default bundle. On Linux this is a single
    /// `LinuxBwrap`; on darwin it is `MacosSeatbelt` (the per-OS
    /// default) plus a `MacosContainer` for opt-in workers. Cheap —
    /// each backend is a unit-like struct with no I/O at construction.
    pub fn default_for_current_os() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                bwrap: Arc::new(linux_bwrap::LinuxBwrap::new()),
            }
        }
        #[cfg(target_os = "macos")]
        {
            Self {
                seatbelt: Arc::new(macos_seatbelt::MacosSeatbelt::new()),
                container: Arc::new(macos_container::MacosContainer::new()),
            }
        }
    }

    /// Resolve a per-worker [`SandboxBackendKind`] to a concrete backend.
    ///
    /// `None` returns the per-OS default (linux → `bwrap`, darwin →
    /// `seatbelt`). `Some(K)` returns the matching field. The returned
    /// `Arc` is a refcount bump; callers hold it for the lifetime of
    /// one acquire call (single-use lifecycle) or one warm-slot fill
    /// (idle-timeout lifecycle).
    pub fn resolve(&self, kind: Option<SandboxBackendKind>) -> Arc<dyn SandboxBackend> {
        match kind {
            None => {
                #[cfg(target_os = "linux")]
                {
                    Arc::clone(&self.bwrap)
                }
                #[cfg(target_os = "macos")]
                {
                    Arc::clone(&self.seatbelt)
                }
            }
            #[cfg(target_os = "linux")]
            Some(SandboxBackendKind::Bwrap) => Arc::clone(&self.bwrap),
            #[cfg(target_os = "macos")]
            Some(SandboxBackendKind::Seatbelt) => Arc::clone(&self.seatbelt),
            #[cfg(target_os = "macos")]
            Some(SandboxBackendKind::Container) => Arc::clone(&self.container),
        }
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
    /// memory. The intent is that adding a future field to
    /// [`SandboxPolicy`] doesn't require touching every test fixture;
    /// production callers must override the limits explicitly. Pinned
    /// so a future change to the defaults is a deliberate audit-trail
    /// edit.
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

    /// Both new tunables default to `None`, which falls back to the
    /// hardcoded defense-in-depth ceilings in `linux_cgroup`. Production
    /// policies override explicitly when they need tighter caps.
    #[test]
    fn sandbox_policy_default_leaves_cpu_quota_and_tasks_max_unset() {
        let p = SandboxPolicy::default();
        assert_eq!(p.cpu_quota_pct, None);
        assert_eq!(p.tasks_max, None);
    }

    #[test]
    fn net_default_is_deny() {
        assert!(matches!(Net::default(), Net::Deny));
    }

    #[test]
    fn profile_default_is_worker_strict() {
        assert_eq!(Profile::default(), Profile::WorkerStrict);
    }

    /// `SandboxBackendKind` is `Copy + Eq` so it can be threaded through
    /// per-call dispatch without lifetime gymnastics. Cfg-gating means
    /// the variant set is OS-specific by design — cross-OS mis-config
    /// is a compile-time error rather than a runtime surprise.
    #[test]
    fn sandbox_backend_kind_is_copy_and_eq() {
        #[cfg(target_os = "linux")]
        {
            let a = SandboxBackendKind::Bwrap;
            let b = a;
            assert_eq!(a, b);
        }
        #[cfg(target_os = "macos")]
        {
            let a = SandboxBackendKind::Seatbelt;
            let b = a;
            assert_eq!(a, b);
            let c = SandboxBackendKind::Container;
            assert_ne!(a, c);
        }
    }

    /// `resolve(None)` returns the per-OS default backend. The test pins
    /// pointer identity against the struct's own per-OS default slot —
    /// if a future refactor swaps the default to a different slot, this
    /// trips deliberately.
    #[test]
    fn sandbox_backends_resolve_none_returns_per_os_default() {
        let sbs = SandboxBackends::default_for_current_os();
        let got = sbs.resolve(None);
        #[cfg(target_os = "linux")]
        assert!(Arc::ptr_eq(&got, &sbs.bwrap));
        #[cfg(target_os = "macos")]
        assert!(Arc::ptr_eq(&got, &sbs.seatbelt));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_backends_resolve_some_seatbelt_on_darwin() {
        let sbs = SandboxBackends::default_for_current_os();
        let got = sbs.resolve(Some(SandboxBackendKind::Seatbelt));
        assert!(Arc::ptr_eq(&got, &sbs.seatbelt));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_backends_resolve_some_container_on_darwin() {
        let sbs = SandboxBackends::default_for_current_os();
        let got = sbs.resolve(Some(SandboxBackendKind::Container));
        assert!(Arc::ptr_eq(&got, &sbs.container));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_backends_resolve_some_bwrap_on_linux() {
        let sbs = SandboxBackends::default_for_current_os();
        let got = sbs.resolve(Some(SandboxBackendKind::Bwrap));
        assert!(Arc::ptr_eq(&got, &sbs.bwrap));
    }
}
