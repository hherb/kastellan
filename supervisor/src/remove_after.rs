//! Remove a service's unit file (a plist, a `.service`) only once the
//! service is out of its manager.
//!
//! Lifted out of `launchd_agents.rs` (#894) when the systemd backend needed
//! the same rule (#896), so the two cannot drift. Each backend's own driver
//! tests exercise it with that backend's real answers.

use std::fs;
use std::path::Path;

use crate::SupervisorError;

/// Remove the unit file at `path` — but only once `stopped` (the backend's
/// verdict on its stop: launchd's bootout, systemd's stop + disable) says
/// the service is out of the manager.
///
/// After a failed stop the service may still be loaded or running; deleting
/// its file then would leave a live service with no file on disk while
/// `uninstall` reported success (#894). So the file is kept, a
/// `Backend`/`Io` error says so, and the operator can simply retry
/// `uninstall`.
pub(crate) fn remove_after_stop(
    stopped: Result<(), SupervisorError>,
    path: &Path,
) -> Result<(), SupervisorError> {
    let kept = |m: String| format!("{m}; kept {} so `uninstall` can be retried", path.display());
    match stopped {
        Ok(()) => {}
        Err(SupervisorError::Backend(m)) => return Err(SupervisorError::Backend(kept(m))),
        Err(SupervisorError::Io(m)) => return Err(SupervisorError::Io(kept(m))),
        Err(e) => return Err(e),
    }
    // Only "already absent" is success; any other failure leaves the file.
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(SupervisorError::Io(format!("remove {}: {e}", path.display())))
        }
        _ => Ok(()),
    }
}
