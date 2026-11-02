//! The two limits that end a task's gathering: a plan count and, on the fast
//! lane, a wall-clock ceiling (#922 part A).
//!
//! Both are checked at the top of every loop iteration, before a plan is
//! formulated. Over either one, the loop spends its single forced-synthesis
//! turn when something was gathered, and fails otherwise — the same handling
//! for both, so a task that ran out of time answers exactly like one that ran
//! out of plans.
//!
//! The ceiling is checked only *between* plans. A plan that starts just before
//! it runs to completion (its steps are bounded by their own timeouts), and the
//! synthesis turn comes after that. Since #941 the planner call itself is also
//! bounded by the time left ([`call_timeout`], never below
//! [`CALL_TIMEOUT_FLOOR`]) and its reply by [`PLANNER_MAX_TOKENS`], so the
//! overrun is at most about one floor-length call plus one synthesis, rather
//! than a full `KASTELLAN_LLM_TIMEOUT_MS` per call.

use std::time::{Duration, Instant};

/// The most tokens one planner reply may generate (#941). A plan is a few
/// hundred tokens; thinking can run to thousands. Without a cap one runaway
/// generation could use up the whole fast-lane ceiling.
pub(crate) const PLANNER_MAX_TOKENS: u32 = 16_384;

/// The shortest timeout a planner call gets, even past the deadline: the
/// forced-synthesis turn runs *after* the ceiling, and must still have time
/// to answer.
pub(crate) const CALL_TIMEOUT_FLOOR: Duration = Duration::from_secs(120);

/// The timeout for one planner call starting `now`. Pure.
///
/// No deadline: the configured (`KASTELLAN_LLM_TIMEOUT_MS`) timeout. With a
/// deadline: the time left, but never below [`CALL_TIMEOUT_FLOOR`] and never
/// above the configured timeout.
pub(crate) fn call_timeout(configured: Duration, deadline: Option<Instant>, now: Instant) -> Duration {
    match deadline {
        None => configured,
        Some(d) => configured.min(d.saturating_duration_since(now).max(CALL_TIMEOUT_FLOOR)),
    }
}

/// Which limit a task has reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exhausted {
    /// `plan_count >= max_plans`.
    Plans,
    /// The wall-clock deadline has passed.
    Time,
}

/// Has this task used up its budget? Pure: the caller passes `now`.
///
/// The plan count wins when both are spent, so a cap-hit row keeps the
/// wording operators already count.
pub(crate) fn exhausted(
    plan_count: u32,
    max_plans: u32,
    deadline: Option<Instant>,
    now: Instant,
) -> Option<Exhausted> {
    if plan_count >= max_plans {
        return Some(Exhausted::Plans);
    }
    match deadline {
        Some(d) if now >= d => Some(Exhausted::Time),
        _ => None,
    }
}

/// Whole seconds left before `deadline`, for the planner's `plan_budget`.
/// Zero once it has passed; `None` when the task has no ceiling.
pub(crate) fn seconds_left(deadline: Option<Instant>, now: Instant) -> Option<u64> {
    deadline.map(|d| d.saturating_duration_since(now).as_secs())
}

/// The deadline for a run that starts `now`, given the lane's ceiling.
pub(crate) fn deadline_from(now: Instant, ceiling: Option<Duration>) -> Option<Instant> {
    ceiling.and_then(|c| now.checked_add(c))
}

/// The `Failed` detail for a task that reached a limit with nothing gathered,
/// or whose synthesis turn still did not answer (`after_synthesis`).
///
/// The plan-cap wording is unchanged from before #922 (`5>=5`, never the
/// off-by-one `6>=5` a synthesis turn's extra formulation would print).
pub(crate) fn exhausted_detail(which: Exhausted, max_plans: u32, after_synthesis: bool) -> String {
    let head = match which {
        Exhausted::Plans => format!("plan_iteration_cap_exceeded ({max_plans}>={max_plans})"),
        Exhausted::Time => "plan_time_ceiling_exceeded".to_string(),
    };
    if after_synthesis {
        format!("{head}; forced synthesis did not produce a final answer")
    } else {
        head
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_under_both_limits_is_not_exhausted() {
        let now = Instant::now();
        assert_eq!(exhausted(2, 5, Some(now + Duration::from_secs(1)), now), None);
        assert_eq!(exhausted(2, 5, None, now), None);
    }

    #[test]
    fn the_plan_count_is_reached_at_equality() {
        let now = Instant::now();
        assert_eq!(exhausted(5, 5, None, now), Some(Exhausted::Plans));
        assert_eq!(exhausted(4, 5, None, now), None);
    }

    #[test]
    fn the_deadline_is_reached_at_equality_and_after() {
        let now = Instant::now();
        assert_eq!(exhausted(0, 5, Some(now), now), Some(Exhausted::Time));
        assert_eq!(exhausted(0, 5, Some(now), now + Duration::from_secs(1)), Some(Exhausted::Time));
    }

    #[test]
    fn the_plan_count_wins_when_both_are_spent() {
        let now = Instant::now();
        assert_eq!(exhausted(5, 5, Some(now), now), Some(Exhausted::Plans));
    }

    #[test]
    fn seconds_left_counts_down_to_zero_and_never_below() {
        let now = Instant::now();
        let d = now + Duration::from_secs(900);
        assert_eq!(seconds_left(Some(d), now), Some(900));
        assert_eq!(seconds_left(Some(d), now + Duration::from_millis(1500)), Some(898));
        assert_eq!(seconds_left(Some(d), d + Duration::from_secs(5)), Some(0));
        assert_eq!(seconds_left(None, now), None);
    }

    #[test]
    fn a_lane_without_a_ceiling_has_no_deadline() {
        let now = Instant::now();
        assert_eq!(deadline_from(now, None), None);
        assert_eq!(deadline_from(now, Some(Duration::from_secs(60))), Some(now + Duration::from_secs(60)));
    }

    #[test]
    fn the_details_name_their_limit() {
        assert_eq!(exhausted_detail(Exhausted::Plans, 5, false), "plan_iteration_cap_exceeded (5>=5)");
        assert_eq!(
            exhausted_detail(Exhausted::Plans, 5, true),
            "plan_iteration_cap_exceeded (5>=5); forced synthesis did not produce a final answer"
        );
        assert_eq!(exhausted_detail(Exhausted::Time, 5, false), "plan_time_ceiling_exceeded");
        assert!(exhausted_detail(Exhausted::Time, 5, true).ends_with("did not produce a final answer"));
    }

    #[test]
    fn no_deadline_keeps_the_configured_timeout() {
        assert_eq!(call_timeout(Duration::from_secs(600), None, Instant::now()), Duration::from_secs(600));
    }

    #[test]
    fn the_time_left_clamps_the_call() {
        let now = Instant::now();
        let t = call_timeout(Duration::from_secs(600), Some(now + Duration::from_secs(300)), now);
        assert_eq!(t, Duration::from_secs(300));
    }

    #[test]
    fn the_floor_holds_past_the_deadline() {
        let now = Instant::now();
        let past = now.checked_sub(Duration::from_secs(10)).unwrap_or(now);
        assert_eq!(call_timeout(Duration::from_secs(600), Some(past), now), CALL_TIMEOUT_FLOOR);
        let soon = now + Duration::from_secs(30);
        assert_eq!(call_timeout(Duration::from_secs(600), Some(soon), now), CALL_TIMEOUT_FLOOR);
    }

    #[test]
    fn a_short_configured_timeout_wins() {
        let now = Instant::now();
        let t = call_timeout(Duration::from_secs(60), Some(now + Duration::from_secs(300)), now);
        assert_eq!(t, Duration::from_secs(60));
    }
}
