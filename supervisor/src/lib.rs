//! hhagent-supervisor: emit and manage user-level service units across OSes.
//!
//! Linux  -> systemd `--user`  unit files in  `~/.config/systemd/user/`
//! macOS  -> launchd LaunchAgents plists in   `~/Library/LaunchAgents/`
//!
//! This crate generates the right unit/plist for a `ServiceSpec` and drives
//! `systemctl --user` / `launchctl bootstrap gui/<uid>` accordingly.
//! Implementation lands in Phase 0 / 0b.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    pub keep_alive: bool,
    pub stdout_log: Option<PathBuf>,
    pub stderr_log: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
    #[error("supervisor error: {0}")]
    Backend(String),
}

pub trait Supervisor {
    fn install(&self, spec: &ServiceSpec) -> Result<(), SupervisorError>;
    fn start(&self, name: &str) -> Result<(), SupervisorError>;
    fn stop(&self, name: &str) -> Result<(), SupervisorError>;
    fn uninstall(&self, name: &str) -> Result<(), SupervisorError>;
}

pub fn default_supervisor() -> Box<dyn Supervisor> {
    Box::new(NotYetImplemented)
}

struct NotYetImplemented;

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
}
