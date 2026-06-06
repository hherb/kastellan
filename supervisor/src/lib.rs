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

pub mod specs;

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
    /// Names of services that must start *before* this one. Maps to a
    /// systemd `After=<name>.service` line per entry. **Ignored on
    /// launchd** — launchd has no inter-agent ordering, so on macOS the
    /// equivalent guarantee comes from each service's own readiness
    /// behaviour (core fail-closed-restarts until Postgres is reachable).
    /// Default empty: a spec that sets nothing here emits exactly today's
    /// unit file — `build_unit_file` only adds ordering directives when
    /// this is non-empty.
    #[serde(default)]
    pub after: Vec<String>,
    /// The target bundle this service belongs to, if any. When `Some`,
    /// systemd emits `PartOf=<target>.target` (so stopping the target
    /// stops this service) and switches the `[Install] WantedBy=` to
    /// `<target>.target`. **Ignored on launchd.** Default `None`.
    #[serde(default)]
    pub part_of: Option<String>,
}

/// A named bundle of services brought up and torn down together.
///
/// `members` are service names listed in **start order** (dependencies
/// first); teardown reverses the order. On systemd this compiles to a
/// real `hhagent.target` unit; on launchd (which has no target concept)
/// the [`Supervisor`] default methods install and start the members in
/// this order, relying on each service's own readiness behaviour for
/// correctness.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetSpec {
    /// Bundle name. Becomes `<name>.target` on systemd; on launchd it is
    /// only an identifier for the member set (no file is written for it).
    pub name: String,
    /// Member service names, in start order (dependencies first).
    pub members: Vec<String>,
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

    /// Install every member of a [`TargetSpec`] (the generic bundle).
    ///
    /// Default implementation installs each member spec in order. The
    /// systemd backend overrides this to additionally write a native
    /// `.target` unit. The macOS/launchd backend uses this default —
    /// there is no target file on launchd.
    fn install_target(
        &self,
        _target: &TargetSpec,
        members: &[ServiceSpec],
    ) -> Result<(), SupervisorError> {
        for spec in members {
            self.install(spec)?;
        }
        Ok(())
    }

    /// Start every member in `target.members` order (dependencies first).
    ///
    /// Default implementation starts each member sequentially. There is
    /// **no explicit readiness wait** — on launchd, inter-service
    /// ordering is not enforced and correctness relies on each service's
    /// own readiness behaviour (core fail-closed-restarts until Postgres
    /// is reachable). The systemd backend overrides this to `systemctl
    /// start <name>.target`, letting systemd resolve ordering from
    /// `After=`.
    fn start_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        for name in &target.members {
            self.start(name)?;
        }
        Ok(())
    }

    /// Stop every member in **reverse** `target.members` order.
    fn stop_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        for name in target.members.iter().rev() {
            self.stop(name)?;
        }
        Ok(())
    }

    /// Uninstall every member in **reverse** order.
    fn uninstall_target(&self, target: &TargetSpec) -> Result<(), SupervisorError> {
        for name in target.members.iter().rev() {
            self.uninstall(name)?;
        }
        Ok(())
    }
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

/// Probe the default supervisor backend on this OS.
///
/// On Linux this delegates to [`systemd_user::probe`] (talks to the
/// per-user systemd manager). On macOS this delegates to
/// [`launchd_agents::probe`] (talks to the GUI launchd domain). On
/// any other Unix this returns [`SupervisorError::NotImplemented`].
///
/// The point: callers (notably integration tests) can do a single
/// "is the supervisor usable on this host?" check without per-OS
/// branching. A failed probe is the canonical signal to skip
/// supervisor-touching work — headless Linux without
/// `loginctl enable-linger` and SSH-only macOS sessions both fail
/// here, and both are environments where a `start` would otherwise
/// produce a confusing backend error.
pub fn default_probe() -> Result<(), SupervisorError> {
    #[cfg(target_os = "linux")]
    {
        systemd_user::probe()
    }
    #[cfg(target_os = "macos")]
    {
        launchd_agents::probe()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(SupervisorError::NotImplemented(
            "default_probe — Phase 0 work item",
        ))
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

#[cfg(test)]
mod default_target_tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingSupervisor {
        calls: RefCell<Vec<String>>,
    }
    impl Supervisor for RecordingSupervisor {
        fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("install:{}", spec.name));
            Ok(())
        }
        fn start(&self, name: &str) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("start:{name}"));
            Ok(())
        }
        fn stop(&self, name: &str) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("stop:{name}"));
            Ok(())
        }
        fn uninstall(&self, name: &str) -> Result<(), SupervisorError> {
            self.calls.borrow_mut().push(format!("uninstall:{name}"));
            Ok(())
        }
        fn status(&self, _name: &str) -> Result<ServiceStatus, SupervisorError> {
            Ok(ServiceStatus::Active)
        }
    }

    fn spec(name: &str) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            program: std::path::PathBuf::from("/bin/true"),
            args: vec![],
            env: vec![],
            working_dir: None,
            keep_alive: true,
            stdout_log: None,
            stderr_log: None,
            after: vec![],
            part_of: Some("hhagent".into()),
        }
    }

    #[test]
    fn default_bundle_installs_then_starts_in_member_order() {
        let sup = RecordingSupervisor::default();
        let target = TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        let members = [spec("hhagent-postgres"), spec("hhagent-core")];
        sup.install_target(&target, &members).unwrap();
        sup.start_target(&target).unwrap();
        let calls = sup.calls.borrow().clone();
        assert_eq!(
            calls,
            vec![
                "install:hhagent-postgres",
                "install:hhagent-core",
                "start:hhagent-postgres",
                "start:hhagent-core",
            ]
        );
    }

    #[test]
    fn default_bundle_stops_in_reverse_member_order() {
        let sup = RecordingSupervisor::default();
        let target = TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        sup.stop_target(&target).unwrap();
        assert_eq!(
            sup.calls.borrow().clone(),
            vec!["stop:hhagent-core", "stop:hhagent-postgres"]
        );
    }
}

#[cfg(test)]
mod spec_ordering_tests {
    use super::*;

    #[test]
    fn target_spec_holds_name_and_ordered_members() {
        let t = TargetSpec {
            name: "hhagent".into(),
            members: vec!["hhagent-postgres".into(), "hhagent-core".into()],
        };
        assert_eq!(t.name, "hhagent");
        assert_eq!(t.members, vec!["hhagent-postgres", "hhagent-core"]);
    }

    #[test]
    fn service_spec_ordering_fields_default_when_absent() {
        // Proves #[serde(default)] supplies empty/None when the JSON omits
        // the new ordering fields — i.e. an old serialised spec still loads.
        let json = r#"{
            "name": "svc",
            "program": "/bin/true",
            "args": [],
            "env": [],
            "working_dir": null,
            "keep_alive": false,
            "stdout_log": null,
            "stderr_log": null
        }"#;
        let s: ServiceSpec = serde_json::from_str(json).expect("deserialize without ordering fields");
        assert!(s.after.is_empty());
        assert!(s.part_of.is_none());
    }
}
