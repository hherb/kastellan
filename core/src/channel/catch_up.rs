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

/// How often the completed-task pump re-sweeps the backlog while it runs. The
/// start-of-pump sweep covers everything missed while the bus was down; this
/// one covers what goes missing while it is up: a load or claim that failed,
/// a NOTIFY dropped when the tick won the pump's `select!`, and the NOTIFYs
/// sent while `PgListener` silently reconnected after an I/O drop (`recv`
/// reconnects without ending the pump, and Postgres never replays them).
pub const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Backlog ids read per page.
pub const SWEEP_PAGE: i64 = 100;

/// After this many failed sweeps in a row (10 minutes from the first at
/// [`SWEEP_EVERY`]) the completed-task pump says so at ERROR: replies are
/// piling up in the backlog while the bus otherwise looks healthy.
pub const SWEEP_FAILURES_LOUD: u32 = 3;

/// A channel's pump says its reply claims keep failing ([`ClaimStreak`]) once
/// at least this many have failed in a row…
pub const CLAIM_FAILURES_LOUD: u32 = 3;

/// …and the run is at least this old. Claims come one per reply, so a queue
/// of 30 drained through a one-second database blip is 30 failures in a row
/// that the next sweep repairs; the count alone would call that an outage. A
/// lone reply re-queued by each sweep reaches it at the third, 10 minutes in —
/// the old per-sweep pace.
pub const CLAIM_FAILURES_LOUD_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// What one sweep came to — its per-sweep summary, and what tells a failed
/// sweep from a quiet one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// The backlog read failed, or did not move past its cursor.
    pub read_failed: bool,
    /// Replies queued, unclaimed, for their channel's pump (which claims
    /// each at its send since #832).
    pub queued: usize,
    /// Replies for a channel this bus does not serve, left unclaimed.
    pub unserved: usize,
    /// Replies whose load failed, left for the next sweep.
    pub load_failed: usize,
    /// Unroutable replies whose settling claim failed — left for the next
    /// sweep, or recorded (`claim_uncertain`) when a re-read found the task
    /// settled anyway. Since #832 these are the sweep's only claims; a
    /// routable reply's claim failures are counted by its channel's pump.
    pub claim_failed: usize,
}

impl SweepReport {
    /// A sweep failed when it could not read the backlog, or when it queued
    /// nothing and at least one (unroutable) claim failed. A database that
    /// answers `SELECT` but refuses a routable reply's claim `UPDATE` is no
    /// longer seen here since #832: the channel's pump claims, and its own
    /// streak says so (`reply claims keep failing`). A failed load
    /// is not a sweep failure: one task whose row will not load (a poison
    /// row) would otherwise make every sweep fail for ever, and the loud line
    /// would then be false for every other reply. Loads are in the INFO
    /// summary instead.
    pub fn is_failure(&self) -> bool {
        self.read_failed || (self.claim_failed > 0 && self.queued == 0)
    }
}

/// The completed-task pump's run of failed sweeps. A single failure is a WARN
/// where it happens and does not ring the death bell (one bad read is not a
/// dead pump); this is what makes a failure that persists louder than one
/// that passed. Pure: the caller says what failed.
#[derive(Debug, Default)]
pub struct Streak(u32);

impl Streak {
    /// Count one outcome. `Some(n)` when `n` consecutive failures have reached
    /// [`SWEEP_FAILURES_LOUD`] — every sweep from then on, which at
    /// [`SWEEP_EVERY`] is one line per five minutes; any success resets.
    pub fn record(&mut self, failed: bool) -> Option<u32> {
        if !failed {
            self.0 = 0;
            return None;
        }
        self.0 = self.0.saturating_add(1);
        (self.0 >= SWEEP_FAILURES_LOUD).then_some(self.0)
    }
}

/// A channel pump's run of failed reply claims (#832: the claim happens at
/// the send, so this is where a database refusing it shows). Unlike
/// [`Streak`], one outcome per *reply*, not per five minutes — so it is loud
/// only once the run is both [`CLAIM_FAILURES_LOUD`] long and
/// [`CLAIM_FAILURES_LOUD_AFTER`] old, and then says so again only each time
/// the count has doubled: a refusing database with a backlog of 500 is a
/// handful of ERRORs, not 500. Pure: the caller passes the clock.
#[derive(Debug, Default)]
pub struct ClaimStreak {
    failed: u32,
    since: Option<std::time::Instant>,
    /// The count last said, if this run has been said at all.
    said: Option<u32>,
}

impl ClaimStreak {
    /// Count one claim. `Some(n)` when the ERROR is due, `n` the failures in a
    /// row; a claim that worked (won or lost) resets the run.
    pub fn record(&mut self, failed: bool, now: std::time::Instant) -> Option<u32> {
        if !failed {
            *self = Self::default();
            return None;
        }
        self.failed = self.failed.saturating_add(1);
        let since = *self.since.get_or_insert(now);
        if self.failed < CLAIM_FAILURES_LOUD || now.duration_since(since) < CLAIM_FAILURES_LOUD_AFTER {
            return None;
        }
        match self.said {
            Some(said) if self.failed < said.saturating_mul(2) => None,
            _ => {
                self.said = Some(self.failed);
                Some(self.failed)
            }
        }
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
        assert!(!report(false, 1, 2).is_failure(), "one was queued: the sweep is doing its job");
        assert!(!report(false, 0, 0).is_failure(), "an empty backlog is a quiet sweep");
        let unserved = SweepReport { unserved: 5, ..SweepReport::default() };
        assert!(!unserved.is_failure(), "another bus's replies are not a failure");
        let poison = SweepReport { load_failed: 1, ..SweepReport::default() };
        assert!(!poison.is_failure(), "one row that will not load must not make every sweep fail");
    }

    #[test]
    fn a_streak_gets_loud_after_enough_failures_and_resets_on_a_success() {
        let mut streak = Streak::default();
        for _ in 1..SWEEP_FAILURES_LOUD {
            assert_eq!(streak.record(true), None, "below the streak: WARN only");
        }
        assert_eq!(streak.record(true), Some(SWEEP_FAILURES_LOUD));
        assert_eq!(streak.record(true), Some(SWEEP_FAILURES_LOUD + 1), "stays loud");
        assert_eq!(streak.record(false), None);
        assert_eq!(streak.record(true), None, "a success reset the count");
    }

    /// A queue drained through a short blip: many failures, all inside the
    /// age floor, so nothing is said — and the next good claim resets it.
    #[test]
    fn a_claim_streak_inside_its_age_floor_stays_quiet() {
        let t0 = std::time::Instant::now();
        let mut s = ClaimStreak::default();
        for i in 0..30 {
            assert_eq!(s.record(true, t0 + std::time::Duration::from_millis(i * 30)), None, "failure {i}");
        }
        assert_eq!(s.record(false, t0 + std::time::Duration::from_secs(1)), None);
        let later = t0 + CLAIM_FAILURES_LOUD_AFTER * 2;
        assert_eq!(s.record(true, later), None, "a success reset the run and its start");
    }

    /// Loud once the run is long enough AND old enough, then only at each
    /// doubling: a refusing database with a large backlog is a few ERRORs.
    #[test]
    fn a_claim_streak_is_said_once_old_enough_then_at_each_doubling() {
        let t0 = std::time::Instant::now();
        let old = t0 + CLAIM_FAILURES_LOUD_AFTER;
        let mut s = ClaimStreak::default();
        assert_eq!(s.record(true, t0), None);
        assert_eq!(s.record(true, old), None, "old enough, not long enough");
        assert_eq!(s.record(true, old), Some(CLAIM_FAILURES_LOUD), "both: said");
        let said: Vec<u32> = (0..50).filter_map(|_| s.record(true, old)).collect();
        assert_eq!(said, [6, 12, 24, 48], "then only at each doubling");
    }

    /// A lone reply re-queued by each sweep fails once per `SWEEP_EVERY`: it
    /// is said at the third, as the per-sweep streak was.
    #[test]
    fn a_claim_streak_paced_by_sweeps_is_said_at_the_third() {
        let t0 = std::time::Instant::now();
        let mut s = ClaimStreak::default();
        assert_eq!(s.record(true, t0), None);
        assert_eq!(s.record(true, t0 + SWEEP_EVERY), None);
        assert_eq!(s.record(true, t0 + SWEEP_EVERY * 2), Some(3));
    }
}
