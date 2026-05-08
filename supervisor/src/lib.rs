//! hhagent-supervisor: emit and manage user-level service units across OSes.
//!
//! Linux  -> systemd `--user` unit files in  `~/.config/systemd/user/`
//! macOS  -> launchd LaunchAgents plists in   `~/Library/LaunchAgents/`
//!
//! Both backends share one trait ([`Supervisor`]) and one declarative spec
//! ([`ServiceSpec`]). The Linux backend ([`systemd_user::SystemdUser`]) and
//! the macOS backend ([`launchd_agents::LaunchAgents`]) are both real;
//! [`default_supervisor`] picks the right one for the current OS, and falls
//! back to a `NotYetImplemented` placeholder only on other Unixes.
//!
//! Why user-level only:
//!   - hhagent runs entirely in one OS user's account; system-level units
//!     would need root and would expand the attack surface.
//!   - `systemctl --user` and `launchctl bootstrap gui/<uid>` are the
//!     standard cross-platform pair for per-user always-on services.

#[cfg(target_os = "linux")]
pub mod systemd_user;

#[cfg(target_os = "macos")]
pub mod launchd_agents;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Declarative description of one supervised service.
///
/// Backend-neutral: every field has an obvious mapping to both a systemd
/// `[Service]` directive and a launchd plist key (mapped where each backend
/// implements [`Supervisor::install`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceSpec {
    /// Unit/agent name. Used as the file stem (`<name>.service` on
    /// Linux, `<name>.plist` on macOS) and as the launchd `Label`.
    /// The caller chooses any naming scheme they want (e.g.
    /// `hhagent-core` or reverse-DNS `org.hhagent.core`) — the
    /// backends only enforce character-class validation, not a forced
    /// prefix. Validated by the backend on install.
    pub name: String,
    /// Absolute path to the executable.
    pub program: PathBuf,
    /// Argv tail (does not include `program`).
    pub args: Vec<String>,
    /// Environment to set for the service. The backend always starts from
    /// a clean environment and applies these on top — no host env leaks.
    pub env: Vec<(String, String)>,
    /// Optional working directory. Must be absolute when set.
    pub working_dir: Option<PathBuf>,
    /// When `true`, ask the supervisor to restart the service if it exits.
    /// systemd: `Restart=on-failure`. launchd: `KeepAlive=true`.
    pub keep_alive: bool,
    /// Optional file to append stdout to. Parent dir must exist.
    pub stdout_log: Option<PathBuf>,
    /// Optional file to append stderr to. Parent dir must exist.
    pub stderr_log: Option<PathBuf>,
}

/// Coarse runtime state of a service, normalized across backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ServiceStatus {
    /// The service is currently running (`systemctl is-active` == "active").
    Active,
    /// The service exists but is not running (`systemctl is-active` == "inactive").
    Inactive,
    /// The service exists and entered the failed state.
    Failed,
    /// The service is not known to the supervisor (no installed unit / agent).
    NotInstalled,
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    /// Backend isn't implemented on this OS yet.
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
    /// Service name failed validation (slashes, traversal, empty, …).
    #[error("invalid service name: {0}")]
    InvalidName(String),
    /// Pre-flight probe failed; the supervisor cannot operate on this host.
    #[error("supervisor probe failed: {0}")]
    Probe(String),
    /// Underlying I/O error (file write, exec, etc.).
    #[error("supervisor I/O error: {0}")]
    Io(String),
    /// The supervisor's own command (systemctl, launchctl) returned a
    /// non-zero exit. The wrapped string is the captured stderr (trimmed).
    #[error("supervisor backend command failed: {0}")]
    Backend(String),
}

/// Common backend interface — `dyn`-safe.
///
/// Lifecycle: `install` writes the unit file and reloads the daemon,
/// `start`/`stop` toggle the running state, `uninstall` stops, removes the
/// unit, and reloads. `status` is read-only and never errors when the
/// service is missing — it returns [`ServiceStatus::NotInstalled`].
pub trait Supervisor {
    fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError>;
    fn start(&self, name: &str) -> Result<(), SupervisorError>;
    fn stop(&self, name: &str) -> Result<(), SupervisorError>;
    fn uninstall(&self, name: &str) -> Result<(), SupervisorError>;
    fn status(&self, name: &str) -> Result<ServiceStatus, SupervisorError>;
}

/// Pick the default supervisor for the current OS.
///
/// On Linux this is [`systemd_user::SystemdUser`] writing into
/// `~/.config/systemd/user/`. On macOS this is
/// [`launchd_agents::LaunchAgents`] writing into
/// `~/Library/LaunchAgents/`. On any other Unix this is a
/// placeholder that returns [`SupervisorError::NotImplemented`].
pub fn default_supervisor() -> Box<dyn Supervisor> {
    #[cfg(target_os = "linux")]
    {
        Box::new(systemd_user::SystemdUser::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(launchd_agents::LaunchAgents::new())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Box::new(NotYetImplemented)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct NotYetImplemented;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Supervisor for NotYetImplemented {
    fn install(&self, _: &ServiceSpec) -> Result<(), SupervisorError> {
        Err(SupervisorError::NotImplemented("install — Phase 0 work item"))
    }
    fn start(&self, _: &str) -> Result<(), SupervisorError> {
        Err(SupervisorError::NotImplemented("start — Phase 0 work item"))
    }
    fn stop(&self, _: &str) -> Result<(), SupervisorError> {
        Err(SupervisorError::NotImplemented("stop — Phase 0 work item"))
    }
    fn uninstall(&self, _: &str) -> Result<(), SupervisorError> {
        Err(SupervisorError::NotImplemented("uninstall — Phase 0 work item"))
    }
    fn status(&self, _: &str) -> Result<ServiceStatus, SupervisorError> {
        Err(SupervisorError::NotImplemented("status — Phase 0 work item"))
    }
}
