//! [`ServiceStatus`]: the backend-neutral reading of a service's state.

use serde::{Deserialize, Serialize};

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
