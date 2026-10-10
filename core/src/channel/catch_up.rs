//! Pure pieces of the reply catch-up (#825): how late a reply is, the note
//! that says so, the sweep's cadence, and what a sweep reports. No I/O — the
//! sweep itself lives beside `handle_completed` in `bus_outbound`.
//!
//! A reply is *late* when it goes out more than [`LATE_AFTER`] after its task
//! finished — normally because the live NOTIFY was missed and the sweep found
//! it, though a slow load or a long wait for queue space behind a start
//! sweep's backlog can do it too. Late replies are delivered anyway (the
//! operator chose no age cap) but say so, measured from when the peer
//! *asked*, in relative terms so the peer's time zone never matters.
use kastellan_db::tasks::reply_claim::ClaimedReply;
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
/// one covers what goes missing while it is up: a load or claim that failed,
/// a NOTIFY dropped when the tick won the pump's `select!`, and the NOTIFYs
/// sent while `PgListener` silently reconnected after an I/O drop (`recv`
/// reconnects without ending the pump, and Postgres never replays them).
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Backlog ids read per page.
pub const SWEEP_PAGE: i64 = 100;

/// After this many failed sweeps in a row (10 minutes from the first at
/// [`SWEEP_EVERY`]) the pump says so at ERROR: replies are piling up in the
/// backlog while the bus otherwise looks healthy.
pub const SWEEP_FAILURES_LOUD: u32 = 3;

/// What one sweep came to — its per-sweep summary, and what tells a failed
/// sweep from a quiet one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// The backlog read failed, or did not move past its cursor.
    pub read_failed: bool,
    /// Replies claimed and queued.
    pub queued: usize,
    /// Replies for a channel this bus does not serve, left unclaimed.
    pub unserved: usize,
    /// Replies whose load failed, left for the next sweep.
    pub load_failed: usize,
    /// Replies whose claim failed — left for the next sweep, or recorded
    /// (`claim_unknown`) when a re-read found the task settled anyway.
    pub claim_failed: usize,
}

impl SweepReport {
    /// A sweep failed when it could not read the backlog, or when it claimed
    /// nothing and every claim it tried failed — the shape of a database that
    /// still answers `SELECT` but refuses the claim's `UPDATE`. A failed load
    /// is not a sweep failure: one task whose row will not load (a poison
    /// row) would otherwise make every sweep fail for ever, and the loud line
    /// would then be false for every other reply. Loads are in the INFO
    /// summary instead.
    pub fn is_failure(&self) -> bool {
        self.read_failed || (self.claim_failed > 0 && self.queued == 0)
    }
}

/// Consecutive failed sweeps. A sweep's own failure is a WARN and does not
/// ring the death bell (one bad read is not a dead pump); this is what makes
/// a failure that persists louder than one that passed.
#[derive(Debug, Default)]
pub struct FailedSweeps(u32);

impl FailedSweeps {
    /// Count one sweep. `Some(n)` when `n` consecutive failures have reached
    /// [`SWEEP_FAILURES_LOUD`]; any sweep that did not fail resets the count.
    pub fn record(&mut self, report: &SweepReport) -> Option<u32> {
        if !report.is_failure() {
            self.0 = 0;
            return None;
        }
        self.0 = self.0.saturating_add(1);
        (self.0 >= SWEEP_FAILURES_LOUD).then_some(self.0)
    }
}

/// How late a reply sent `now` is, if it is late enough to say so. Measured
/// from `finished_at` (when the reply was due), falling back to `created_at`.
/// `None` for an on-time reply — and for a `finished_at` in the future, which
/// is clock skew between Postgres and the daemon, not a late reply. (The
/// daemon's clock *ahead* by more than [`LATE_AFTER`] would mark every reply
/// late; measuring with Postgres's own `now()` is #841.)
pub fn lateness(
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<Duration> {
    let late_by = now - finished_at.unwrap_or(created_at);
    (late_by > LATE_AFTER).then_some(late_by)
}

/// A claimed reply's lateness and the line prepended to it, or `None` when it
/// is on time. One value, so the row's `delayed_secs` and the body's note
/// cannot disagree. Built from timestamps only — never from task content.
pub fn late_reply(claim: &ClaimedReply, now: OffsetDateTime) -> Option<(Duration, String)> {
    let late_by = lateness(claim.created_at, claim.finished_at, now)?;
    let note = format!("(Delayed reply — you sent this {} ago.)", humanize(now - claim.created_at));
    Some((late_by, note))
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

    fn claim(created_at: OffsetDateTime, finished_at: OffsetDateTime) -> ClaimedReply {
        ClaimedReply { created_at, finished_at: Some(finished_at) }
    }

    fn note(c: &ClaimedReply) -> Option<String> {
        late_reply(c, NOW).map(|(_, note)| note)
    }

    #[test]
    fn a_reply_exactly_late_after_is_not_late() {
        let f = NOW - LATE_AFTER;
        assert_eq!(lateness(f - Duration::minutes(1), Some(f), NOW), None);
        assert_eq!(late_reply(&claim(f - Duration::minutes(1), f), NOW), None);
    }

    #[test]
    fn a_reply_one_second_past_late_after_is_late() {
        let f = NOW - LATE_AFTER - Duration::seconds(1);
        assert_eq!(lateness(f, Some(f), NOW), Some(LATE_AFTER + Duration::seconds(1)));
        let (late_by, _) = late_reply(&claim(f, f), NOW).expect("late");
        assert_eq!(late_by, LATE_AFTER + Duration::seconds(1), "the row's value is the lateness");
    }

    #[test]
    fn the_age_is_counted_from_when_the_peer_asked() {
        // Asked 3 h 12 min ago, finished 3 h ago: the peer waited 3 h 12 min.
        let created = NOW - Duration::minutes(192);
        let finished = NOW - Duration::hours(3);
        assert_eq!(
            note(&claim(created, finished)).as_deref(),
            Some("(Delayed reply — you sent this 3 h 12 min ago.)")
        );
    }

    #[test]
    fn a_missing_finished_at_falls_back_to_created_at() {
        let created = NOW - Duration::hours(1);
        assert_eq!(lateness(created, None, NOW), Some(Duration::hours(1)));
    }

    /// Postgres's clock ahead of the daemon's: never a note, never a
    /// negative lateness.
    #[test]
    fn a_finish_in_the_future_is_not_late() {
        let f = NOW + Duration::minutes(10);
        assert_eq!(lateness(NOW - Duration::hours(2), Some(f), NOW), None);
        assert_eq!(note(&claim(NOW - Duration::hours(2), f)), None);
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

    fn report(read_failed: bool, queued: usize, claim_failed: usize) -> SweepReport {
        SweepReport { read_failed, queued, claim_failed, ..SweepReport::default() }
    }

    #[test]
    fn a_sweep_fails_when_it_cannot_read_or_every_claim_fails() {
        assert!(report(true, 0, 0).is_failure(), "the backlog read failed");
        assert!(report(false, 0, 2).is_failure(), "every claim it tried failed");
        assert!(!report(false, 1, 2).is_failure(), "one got through: the claim path works");
        assert!(!report(false, 0, 0).is_failure(), "an empty backlog is a quiet sweep");
        let unserved = SweepReport { unserved: 5, ..SweepReport::default() };
        assert!(!unserved.is_failure(), "another bus's replies are not a failure");
        let poison = SweepReport { load_failed: 1, ..SweepReport::default() };
        assert!(!poison.is_failure(), "one row that will not load must not make every sweep fail");
    }

    #[test]
    fn failed_sweeps_get_loud_after_a_streak_and_reset_on_a_good_one() {
        let mut streak = FailedSweeps::default();
        let bad = report(true, 0, 0);
        for _ in 1..SWEEP_FAILURES_LOUD {
            assert_eq!(streak.record(&bad), None, "below the streak: WARN only");
        }
        assert_eq!(streak.record(&bad), Some(SWEEP_FAILURES_LOUD));
        assert_eq!(streak.record(&bad), Some(SWEEP_FAILURES_LOUD + 1), "stays loud");
        assert_eq!(streak.record(&report(false, 0, 0)), None);
        assert_eq!(streak.record(&bad), None, "a good sweep reset the count");
    }
}
