//! Pure pieces of the reply catch-up (#825): how late a reply is, the note
//! that says so, and the sweep's cadence. No I/O — the sweep itself lives
//! beside `handle_completed` in `bus_outbound`.
//!
//! A reply is *late* when it goes out more than [`LATE_AFTER`] after its task
//! finished — which only happens when the live NOTIFY was missed and the
//! sweep found it. Late replies are delivered anyway (the operator chose no
//! age cap) but say so, measured from when the peer *asked*, in relative
//! terms so the peer's time zone never matters.

use time::{Duration, OffsetDateTime};

/// Which path routed a reply — recorded as `via` on `channel.replied`, so an
/// operator can count how often catch-up actually saved one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// The live `tasks_completed` NOTIFY.
    Notify,
    /// The backlog sweep.
    CatchUp,
}

impl Via {
    /// The row's spelling — a committed operator-facing value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::CatchUp => "catch_up",
        }
    }
}

/// A reply this long after its task finished carries a delay note.
pub const LATE_AFTER: Duration = Duration::minutes(5);

/// How often the outbound pump re-sweeps the backlog while it runs. The
/// start-of-pump sweep covers everything missed while the bus was down; this
/// one covers a load or claim that failed while it was up.
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Backlog ids read per page.
pub const SWEEP_PAGE: i64 = 100;

/// How late a reply sent `now` is, if it is late enough to say so. Measured
/// from `finished_at` (when the reply was due), falling back to `created_at`.
/// `None` for an on-time reply — and for a `finished_at` in the future, which
/// is clock skew between Postgres and the daemon, not a late reply.
pub fn lateness(
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<Duration> {
    let late_by = now - finished_at.unwrap_or(created_at);
    (late_by > LATE_AFTER).then_some(late_by)
}

/// The line prepended to a late reply, or `None` when it is on time. Built
/// from timestamps only — never from task content.
pub fn delay_note(
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<String> {
    lateness(created_at, finished_at, now)
        .map(|_| format!("(Delayed reply — you sent this {} ago.)", humanize(now - created_at)))
}

/// A duration in at most two units: `47 min`, `3 h 12 min`, `2 days 4 h`.
/// Never `0 min` (the floor is one minute), never negative.
pub fn humanize(d: Duration) -> String {
    let mins = d.whole_minutes().max(1);
    let (days, hours, m) = (mins / (24 * 60), (mins % (24 * 60)) / 60, mins % 60);
    let day_word = |n: i64| if n == 1 { "day" } else { "days" };
    match (days, hours, m) {
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} {}", day_word(d)),
        (d, h, _) => format!("{d} {} {h} h", day_word(d)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;
    use time::Duration;

    const NOW: OffsetDateTime = datetime!(2026-10-08 12:00:00 UTC);

    #[test]
    fn a_reply_exactly_late_after_is_not_late() {
        let f = NOW - LATE_AFTER;
        assert_eq!(lateness(f - Duration::minutes(1), Some(f), NOW), None);
        assert_eq!(delay_note(f - Duration::minutes(1), Some(f), NOW), None);
    }

    #[test]
    fn a_reply_one_second_past_late_after_is_late() {
        let f = NOW - LATE_AFTER - Duration::seconds(1);
        assert_eq!(lateness(f, Some(f), NOW), Some(LATE_AFTER + Duration::seconds(1)));
        assert!(delay_note(f, Some(f), NOW).is_some());
    }

    #[test]
    fn the_age_is_counted_from_when_the_peer_asked() {
        // Asked 3 h 12 min ago, finished 3 h ago: the peer waited 3 h 12 min.
        let created = NOW - Duration::minutes(192);
        let finished = NOW - Duration::hours(3);
        assert_eq!(
            delay_note(created, Some(finished), NOW).as_deref(),
            Some("(Delayed reply — you sent this 3 h 12 min ago.)")
        );
    }

    #[test]
    fn a_missing_finished_at_falls_back_to_created_at() {
        let created = NOW - Duration::hours(1);
        assert_eq!(lateness(created, None, NOW), Some(Duration::hours(1)));
    }

    /// (RF 1) Postgres's clock ahead of the daemon's: never a note, never a
    /// negative lateness.
    #[test]
    fn a_finish_in_the_future_is_not_late() {
        let f = NOW + Duration::minutes(10);
        assert_eq!(lateness(NOW - Duration::hours(2), Some(f), NOW), None);
        assert_eq!(delay_note(NOW - Duration::hours(2), Some(f), NOW), None);
        assert_eq!(humanize(Duration::minutes(-5)), "1 min", "a negative age never renders");
    }

    #[test]
    fn humanize_has_two_units_at_most_and_never_zero_minutes() {
        assert_eq!(humanize(Duration::seconds(59)), "1 min");
        assert_eq!(humanize(Duration::minutes(47)), "47 min");
        assert_eq!(humanize(Duration::minutes(60)), "1 h");
        assert_eq!(humanize(Duration::minutes(192)), "3 h 12 min");
        assert_eq!(humanize(Duration::hours(24)), "1 day");
        assert_eq!(humanize(Duration::hours(25) + Duration::minutes(30)), "1 day 1 h");
        assert_eq!(
            humanize(Duration::days(2) + Duration::hours(4) + Duration::minutes(30)),
            "2 days 4 h"
        );
        assert_eq!(humanize(Duration::days(3)), "3 days");
    }

    #[test]
    fn via_spells_the_row_values() {
        assert_eq!(Via::Notify.as_str(), "notify");
        assert_eq!(Via::CatchUp.as_str(), "catch_up");
    }
}
