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
//! So `stop_target` waits until each agent is really gone (why there and
//! not in `stop`: see `LaunchAgents::stop_target`). The polling loop is
//! kept pure here — the probe and the sleep are passed in — so it can be
//! tested without a launchd domain.

use std::time::Duration;

use super::is_loaded_in_domain;
use crate::SupervisorError;

/// How long `stop_target` waits for launchd to finish removing an agent.
///
/// Well above the slowest agent we ship: launchd itself SIGKILLs an
/// agent `ExitTimeOut` seconds after its SIGTERM (10 s in our plists),
/// so a removal still pending after 30 s means something else is wrong.
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How often `stop_target` re-checks whether the agent has left the domain.
const UNLOAD_POLL: Duration = Duration::from_millis(100);

/// Wait until launchd has really removed `target` (a `gui/<uid>/<label>`)
/// after a `bootout`.
///
/// Still in the domain after [`UNLOAD_TIMEOUT`] is an error: a `start`
/// now would skip its `bootstrap` and leave the agent unloaded.
pub(super) fn wait_until_unloaded(target: &str) -> Result<(), SupervisorError> {
    let gone = wait_until_gone(
        || !is_loaded_in_domain(target),
        UNLOAD_TIMEOUT,
        UNLOAD_POLL,
        std::thread::sleep,
    );
    if gone {
        Ok(())
    } else {
        Err(SupervisorError::Backend(format!(
            "{target} is still loaded {}s after `launchctl bootout`; starting it now \
             would be skipped as already running",
            UNLOAD_TIMEOUT.as_secs()
        )))
    }
}

/// Call `is_gone` until it returns `true` or `timeout` has been spent
/// sleeping in steps of `poll`.
///
/// Returns `true` once `is_gone` said so, `false` if it never did. The
/// probe always runs once more after the last sleep, so a removal that
/// finishes during the final step still counts.
fn wait_until_gone(
    mut is_gone: impl FnMut() -> bool,
    timeout: Duration,
    poll: Duration,
    mut sleep: impl FnMut(Duration),
) -> bool {
    // Number of sleeps that fit in the budget, at least one.
    let steps = (timeout.as_millis() / poll.as_millis().max(1)).max(1);
    for _ in 0..steps {
        if is_gone() {
            return true;
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
        assert!(wait_until_gone(|| true, 10 * MS, MS, |_| slept += 1));
        assert_eq!(slept, 0);
    }

    #[test]
    fn waits_until_the_probe_flips() {
        let mut calls = 0;
        let mut slept = 0;
        let gone = wait_until_gone(
            || {
                calls += 1;
                calls == 4
            },
            10 * MS,
            MS,
            |_| slept += 1,
        );
        assert!(gone);
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
                false
            },
            10 * MS,
            2 * MS,
            |d| slept += d,
        );
        assert!(!gone);
        assert_eq!(slept, 10 * MS, "sleeps exactly the budget");
        assert_eq!(calls, 6, "one probe per step plus a final one");
    }

    #[test]
    fn a_removal_finishing_in_the_last_step_counts() {
        let mut calls = 0;
        let gone = wait_until_gone(
            || {
                calls += 1;
                calls == 6
            },
            10 * MS,
            2 * MS,
            |_| {},
        );
        assert!(gone);
    }
}
