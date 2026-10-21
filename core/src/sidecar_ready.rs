//! The bounded readiness wait both sidecar spawns run — the egress proxy
//! ([`crate::egress::spawn`]) and the brokers ([`crate::broker`]).
//!
//! One copy because #754 was fixed in two hand-written loops, and the egress one
//! was inline in `spawn_sidecar`, where no test could reach it: reverting its fix
//! left every test green. Here the loop takes its probes as closures, so the
//! contract is hermetically testable without a real process.

use std::process::ExitStatus;
use std::time::{Duration, Instant};

const READY_POLL: Duration = Duration::from_millis(25);

/// Outcome of waiting for a sidecar to become ready. Distinguishes a clean bind
/// from an early process exit (so the caller can surface the real exit status
/// instead of a misleading bind-timeout) and from a genuine timeout.
pub(crate) enum SidecarReady {
    /// `ready` answered true — the sidecar is (about to be) serving.
    Ready,
    /// The sidecar exited before becoming ready — fail fast with its status.
    Exited(ExitStatus),
    /// Neither happened within the deadline.
    TimedOut,
    /// Our own `try_wait` failed, so we cannot tell whether the sidecar is alive
    /// (#754). A fact about us, not the sidecar — never reported as a bind timeout.
    Unpollable(std::io::Error),
}

/// Poll `ready`, up to `timeout`, while watching for the sidecar to exit early
/// via `exited` (its `try_wait`). Returns as soon as the sidecar is ready, dies,
/// or cannot be polled, so a sidecar that fails at startup surfaces its exit
/// status immediately rather than blocking the full `timeout`.
pub(crate) fn wait_for_sidecar_ready(
    timeout: Duration,
    mut ready: impl FnMut() -> bool,
    mut exited: impl FnMut() -> std::io::Result<Option<ExitStatus>>,
) -> SidecarReady {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return SidecarReady::Ready;
        }
        match exited() {
            Ok(Some(status)) => return SidecarReady::Exited(status),
            Ok(None) => {}
            Err(e) => return SidecarReady::Unpollable(e),
        }
        if Instant::now() >= deadline {
            return SidecarReady::TimedOut;
        }
        std::thread::sleep(READY_POLL);
    }
}

/// Pure: may we still kill a child whose `try_wait` failed with `e`?
///
/// Not on `ECHILD`: the pid is no longer our unreaped child, so it may already
/// belong to another process. A `WNOHANG` `waitpid` only says that once the child
/// has exited, so skipping the kill strands nothing. Any other errno: kill and
/// reap, fail closed — we cannot see the sidecar, so we must not keep it.
pub(crate) fn may_kill_after_failed_poll(e: &std::io::Error) -> bool {
    e.raw_os_error() != Some(libc::ECHILD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_out_when_never_ready_and_alive() {
        let ready = wait_for_sidecar_ready(Duration::from_millis(60), || false, || Ok(None));
        assert!(matches!(ready, SidecarReady::TimedOut));
    }

    #[test]
    fn ready_wins_over_the_exit_poll() {
        let ready = wait_for_sidecar_ready(Duration::from_secs(10), || true, || {
            panic!("a ready sidecar must not be polled for exit")
        });
        assert!(matches!(ready, SidecarReady::Ready));
    }

    #[test]
    fn reports_early_exit_without_waiting_full_timeout() {
        // A sidecar that exits before binding is reported as `Exited` at once,
        // NOT after the (here 10s) timeout — the whole point of watching liveness.
        let status = std::process::Command::new("true").status().expect("run `true`");
        let start = Instant::now();
        let ready = wait_for_sidecar_ready(Duration::from_secs(10), || false, || Ok(Some(status)));
        assert!(matches!(ready, SidecarReady::Exited(_)));
        assert!(start.elapsed() < Duration::from_secs(1), "must not block the full timeout");
    }

    #[test]
    fn reports_a_failed_exit_poll_instead_of_timing_out() {
        // #754's shape: a `try_wait` that ERRORS was read as "still running", so
        // the loop spun to the timeout and blamed the UDS bind — a fact about our
        // own `waitpid` reported as one about the sidecar.
        let start = Instant::now();
        let ready = wait_for_sidecar_ready(Duration::from_secs(10), || false, || {
            Err(std::io::Error::from_raw_os_error(libc::ECHILD))
        });
        match ready {
            SidecarReady::Unpollable(e) => assert_eq!(e.raw_os_error(), Some(libc::ECHILD)),
            _ => panic!("a failed exit poll must be reported as such, not as a bind outcome"),
        }
        assert!(start.elapsed() < Duration::from_secs(1), "must not wait out the timeout");
    }

    #[test]
    fn echild_forbids_the_kill_and_every_other_errno_demands_it() {
        let err = std::io::Error::from_raw_os_error;
        assert!(!may_kill_after_failed_poll(&err(libc::ECHILD)));
        for errno in [libc::EINTR, libc::EINVAL, libc::EIO] {
            assert!(may_kill_after_failed_poll(&err(errno)), "errno {errno}");
        }
        // No errno at all (a synthesised error) is not ECHILD: fail closed.
        assert!(may_kill_after_failed_poll(&std::io::Error::other("synthetic")));
    }
}
