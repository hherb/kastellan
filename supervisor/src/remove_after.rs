//! Remove a service's unit file (a plist, a `.service`) only once the
//! service is out of its manager.
//!
//! Lifted out of `launchd_agents.rs` (#894) when the systemd backend needed
//! the same rule (#896), so the two cannot drift. The backend-neutral arms
//! are tested here; each backend's tests feed it that backend's stop readers.

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
    // Only claim "kept" for a file that is there (a never-installed service
    // on a host with no user bus has none).
    let kept = |m: String| match path.try_exists() {
        Ok(false) => m,
        _ => format!("{m}; kept {} so `uninstall` can be retried", path.display()),
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh, empty dir under the system temp dir, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(label: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("kastellan-remove-after-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("mkdir scratch");
            Self(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A unit file that cannot be removed (here: a directory in its place)
    /// is an error, never a silent success that leaves it to start at boot.
    #[test]
    fn a_file_that_cannot_be_removed_is_an_error() {
        let dir = Scratch::new("unremovable");
        let path = dir.0.join("svc.service");
        fs::create_dir(&path).expect("mkdir in the unit's place");
        let err = remove_after_stop(Ok(()), &path).expect_err("not removed");
        assert!(matches!(&err, SupervisorError::Io(m) if m.starts_with("remove ")), "{err:?}");
        assert!(path.exists());
    }

    /// A failed stop with no file on disk says nothing about keeping one.
    #[test]
    fn a_failed_stop_claims_kept_only_for_a_file_that_is_there() {
        let dir = Scratch::new("kept-note");
        let path = dir.0.join("svc.service");
        let no_bus = || Err(SupervisorError::Backend("no bus".into()));
        let err = remove_after_stop(no_bus(), &path).expect_err("stop failed");
        assert!(!err.to_string().contains("kept"), "{err}");
        fs::write(&path, "x").expect("write unit");
        let err = remove_after_stop(no_bus(), &path).expect_err("stop failed");
        assert!(err.to_string().contains("kept"), "{err}");
        assert!(path.exists());
    }
}
