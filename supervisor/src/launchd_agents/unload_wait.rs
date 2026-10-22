//! Waiting for an agent to leave the launchd domain after `bootout`.
//!
//! `launchctl bootout` can return while launchd is still removing a
//! running agent: the agent got its SIGTERM but has not exited yet (a
//! database flushing on shutdown takes a moment). During that window
//! `launchctl print` still finds the agent, so `LaunchAgents::start`'s
//! "already loaded → nothing to do" check skips the `bootstrap`, and
//! the agent then disappears for good. `kastellan-cli install` (stop,
//! then start) lost Postgres exactly this way on the live Mac.
//!
//! So `stop_target` waits until `launchctl print` no longer finds each
//! agent (why there and not in `stop`: see `LaunchAgents::stop_target`).
//! The loops are kept pure here — the probe, the stop and the sleep are
//! passed in — so they can be tested without a launchd domain.

use std::time::Duration;

use super::builders::DEFAULT_EXIT_TIMEOUT_SEC;
use super::is_loaded_in_domain;
use crate::SupervisorError;

/// How long `stop_target` sleeps, in total, waiting for launchd to finish
/// removing an agent. Probe time comes on top (each probe is capped at
/// `LAUNCHCTL_TIMEOUT`, and a probe that times out ends the wait).
///
/// Well above the slowest agent we ship: launchd itself SIGKILLs an agent
/// `ExitTimeOut` (`DEFAULT_EXIT_TIMEOUT_SEC`) seconds after its SIGTERM,
/// so a removal still pending after this means something else is wrong.
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const _: () = assert!(UNLOAD_TIMEOUT.as_secs() > DEFAULT_EXIT_TIMEOUT_SEC as u64);

/// How often `stop_target` re-checks whether the agent has left the domain.
const UNLOAD_POLL: Duration = Duration::from_millis(100);

/// Stop `members` in reverse order, waiting for each to be gone before
/// the next is stopped. Fail-fast like the trait default: on the first
/// error the members not yet reached are left untouched.
pub(super) fn stop_each_in_reverse<E>(
    members: &[String],
    mut stop: impl FnMut(&str) -> Result<(), E>,
    mut wait_gone: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    for name in members.iter().rev() {
        stop(name)?;
        wait_gone(name)?;
    }
    Ok(())
}

/// Wait until launchd has really removed `target` (a `gui/<uid>/<label>`)
/// after a `bootout`.
///
/// Still in the domain after [`UNLOAD_TIMEOUT`] is an error: a `start`
/// now would skip its `bootstrap` and leave the agent unloaded. So is a
/// probe that cannot answer — reading that as "gone" would be the very
/// race this wait exists to close.
pub(super) fn wait_until_unloaded(target: &str) -> Result<(), SupervisorError> {
    let gone = wait_until_gone(
        || is_loaded_in_domain(target).map(|present| !present),
        UNLOAD_TIMEOUT,
        UNLOAD_POLL,
        std::thread::sleep,
    )?;
    if gone {
        Ok(())
    } else {
        Err(SupervisorError::Backend(format!(
            "{target} is still in the launchd domain after {}s of waiting since \
             `launchctl bootout` (launchd should have SIGKILLed it after \
             ExitTimeOut={DEFAULT_EXIT_TIMEOUT_SEC}s); a `start` now would skip it as \
             already loaded. Inspect: `launchctl print {target}`; force it out: \
             `launchctl kill SIGKILL {target}`, then retry",
            UNLOAD_TIMEOUT.as_secs()
        )))
    }
}

/// Call `is_gone` until it returns `Ok(true)`, returns an error, or
/// `max(1, timeout / poll)` sleeps of `poll` (whole milliseconds) have
/// been spent.
///
/// Returns `Ok(true)` once `is_gone` said so, `Ok(false)` if it never
/// did. The probe always runs once more after the last sleep, so a
/// removal that finishes during the final step still counts.
fn wait_until_gone<E>(
    mut is_gone: impl FnMut() -> Result<bool, E>,
    timeout: Duration,
    poll: Duration,
    mut sleep: impl FnMut(Duration),
) -> Result<bool, E> {
    let steps = (timeout.as_millis() / poll.as_millis().max(1)).max(1);
    for _ in 0..steps {
        if is_gone()? {
            return Ok(true);
        }
        sleep(poll);
    }
    is_gone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn already_gone_returns_true_without_sleeping() {
        let mut slept = 0;
        assert_eq!(wait_until_gone(|| Ok::<_, ()>(true), 10 * MS, MS, |_| slept += 1), Ok(true));
        assert_eq!(slept, 0);
    }

    #[test]
    fn waits_until_the_probe_flips() {
        let mut calls = 0;
        let mut slept = 0;
        let gone = wait_until_gone(
            || {
                calls += 1;
                Ok::<_, ()>(calls == 4)
            },
            10 * MS,
            MS,
            |_| slept += 1,
        );
        assert_eq!(gone, Ok(true));
        assert_eq!(calls, 4, "stops probing once the agent is gone");
        assert_eq!(slept, 3);
    }

    #[test]
    fn gives_up_after_the_budget_and_says_so() {
        let mut calls = 0;
        let mut slept = Duration::ZERO;
        let gone = wait_until_gone(
            || {
                calls += 1;
                Ok::<_, ()>(false)
            },
            10 * MS,
            2 * MS,
            |d| slept += d,
        );
        assert_eq!(gone, Ok(false));
        assert_eq!(slept, 10 * MS, "a budget that is a multiple of the poll is slept in full");
        assert_eq!(calls, 6, "one probe per step plus a final one");
    }

    #[test]
    fn a_removal_finishing_in_the_last_step_counts() {
        let mut calls = 0;
        let gone = wait_until_gone(
            || {
                calls += 1;
                Ok::<_, ()>(calls == 6)
            },
            10 * MS,
            2 * MS,
            |_| {},
        );
        assert_eq!(gone, Ok(true));
    }

    /// A probe that cannot answer ends the wait with its error — it is
    /// never read as "gone".
    #[test]
    fn a_probe_error_ends_the_wait_as_an_error() {
        let mut calls = 0;
        let gone = wait_until_gone(
            || {
                calls += 1;
                if calls == 2 { Err("print timed out") } else { Ok(false) }
            },
            10 * MS,
            MS,
            |_| {},
        );
        assert_eq!(gone, Err("print timed out"));
        assert_eq!(calls, 2, "no probing after the error");
    }

    /// Core (last member) is stopped and gone before Postgres is told to
    /// stop: Postgres's SIGTERM shutdown waits for core's connections.
    #[test]
    fn stops_in_reverse_and_waits_for_each_before_the_next() {
        let members = vec!["pg".to_string(), "core".to_string()];
        let log = std::cell::RefCell::new(Vec::new());
        let r = stop_each_in_reverse::<()>(
            &members,
            |n| {
                log.borrow_mut().push(format!("stop {n}"));
                Ok(())
            },
            |n| {
                log.borrow_mut().push(format!("wait {n}"));
                Ok(())
            },
        );
        assert_eq!(r, Ok(()));
        assert_eq!(log.into_inner(), ["stop core", "wait core", "stop pg", "wait pg"]);
    }

    #[test]
    fn a_failed_wait_leaves_the_remaining_members_untouched() {
        let members = vec!["pg".to_string(), "core".to_string()];
        let log = std::cell::RefCell::new(Vec::new());
        let r = stop_each_in_reverse(
            &members,
            |n| {
                log.borrow_mut().push(format!("stop {n}"));
                Ok(())
            },
            |n| Err(format!("{n} still loaded")),
        );
        assert_eq!(r, Err("core still loaded".to_string()));
        assert_eq!(log.into_inner(), ["stop core"]);
    }
}
