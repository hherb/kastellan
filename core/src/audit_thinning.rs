//! Which refused audit rows get a report of their own (#798), counted per
//! burst (#802). [`Thinning`] and the predicates are pure — the clock is
//! passed in. Two kinds of writer hold them: the daemon's own audit sinks
//! (`main/audit_sink_thinning.rs`, one per channel, action and kind — #807,
//! #837) and, since #829, the channel bus's lost-row writer
//! (`channel/pg_events/lost_rows.rs`, one per writer, channel, action and
//! outcome — #864, #865). Moved here from the daemon binary in #829 so
//! both can share it: a binary's items are not visible to the library.
//!
//! A lost row is reported by the thread or task that tried to write it — a
//! daemon sink's shed or late row on the channel's driver thread, its failed
//! insert on the spawned insert task (#861), the bus's on its pump — as a
//! tracing ERROR, or the marked stderr line when nothing
//! records it; either can block. A flood (email's `skipped` list has no
//! length cap; a Postgres that answers every insert at once with an error)
//! would have that writer do so for every row. So within one burst the
//! first [`REPORT_EACH_UP_TO`] are
//! reported, then only the 2^k-th: the reports stay logarithmic in the flood.
//! The rows left out are counted, and the shutdown says how many.
//!
//! A burst ends after [`BURST_QUIET_GAP`] with no refused row. Before #802 the
//! count ran for the daemon's whole life, so a flood on Monday left Friday's
//! single shed row (index 1 000 001, not a power of two) without a line.
//!
//! **Each reported line carries its burst's count** (#807, [`BurstTally`]):
//! which row of the burst it is, and how many before it went unsaid. Before
//! #807 the 2^k-th lines were identical, and a flood's size was said only by
//! a graceful shutdown — an OOM, a SIGKILL or a crash lost it, and an alert
//! saw 26 lines for 10 000 rows, alike but for their ids, none saying how big
//! the flood was. Now the last line said is within a factor of two of the
//! burst's size, and the first row of the next burst says how the one before
//! it ended.
//!
//! What that leaves unsaid until then: the rows after a burst's last 2^k-th
//! line. They are said by the next burst's first line on the same count, or
//! by a graceful shutdown — and by neither if the daemon crashes first, or no
//! row on that count is ever refused again (#817).
//!
//! **A new cause is news** (#859). Past the first rows, a row that failed
//! for a different reason than the flood around it — the fault changing
//! from read-only to a REVOKE, or one payload Postgres cannot store — would
//! be thinned with the flood, and its reason never printed. So the first row
//! of each cause in a burst gets a line of its own ([`Thinning::refuse`]'s
//! `cause`; for failed inserts `kastellan_db::audit::InsertCause`, a class
//! that is the same for the same fault on any row). The *first* of each
//! cause, not each row whose cause differs from the last line's: two faults
//! interleaved would then give every row a line, which is the flood again.
//! At most [`MAX_CAUSES_PER_BURST`] causes are told apart per burst.
//!
//! The tally names no verdict (#864: it used to say "refused row 17" of a
//! row that may have been committed). Each count holds one kind of loss —
//! shed, late, or a failed insert of one outcome — so the line's own words
//! say whether the row is known not to be in the table, and "row 17 of this
//! burst" counts rows lost the same way.

use std::time::{Duration, Instant};

/// How many refused rows of one burst are reported one by one before
/// `should_report` thins them out.
pub const REPORT_EACH_UP_TO: usize = 16;

// `unreported_of`'s closed form counts REPORT_EACH_UP_TO as the first power of
// two it reports; any other value miscounts (or, at 0, panics) — and a
// `#[cfg(test)]` check is no guard in a release build.
const _: () = assert!(REPORT_EACH_UP_TO.is_power_of_two(), "unreported_of assumes a power of two");

/// How many distinct causes one burst tells apart (#859). Past it, a row of
/// a cause not yet seen is thinned like any other: causes are a small fixed
/// set (`kastellan_db::audit::InsertCause`), so a burst that has seen this
/// many is a burst of everything at once, not one with news in it.
pub const MAX_CAUSES_PER_BURST: usize = 8;

/// How long with no refused row ends a burst, so the next one is reported
/// afresh. A sustained flood never pauses this long, so it stays thinned; a
/// row refused after a quiet minute is news again.
pub const BURST_QUIET_GAP: Duration = Duration::from_secs(60);

/// Pure: whether the `n`th (0-based) refused row of a burst gets its own
/// report.
fn should_report(n: usize) -> bool {
    n < REPORT_EACH_UP_TO || n.is_power_of_two()
}

/// Pure: how many of a burst's first `refused` rows `should_report` leaves
/// out, when no row's cause is new — closed form. [`Thinning`] counts the
/// rows it reports instead (a new cause adds one); this is the reference its
/// tests check that count against.
pub fn unreported_of(refused: usize) -> usize {
    // Reported: indices 0..REPORT_EACH_UP_TO, then each power of two from
    // REPORT_EACH_UP_TO (itself one) up to the last index, `refused - 1`.
    let each = refused.min(REPORT_EACH_UP_TO);
    let powers = if refused > REPORT_EACH_UP_TO {
        (refused - 1).ilog2() as usize - REPORT_EACH_UP_TO.ilog2() as usize + 1
    } else {
        0
    };
    refused - each - powers
}

/// Where one refused row stands in its burst, for its own line (#807).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BurstTally {
    /// This row is the `nth` refused in its burst, counting from 1.
    pub nth: usize,
    /// It has a line of its own only because its cause is new in the burst
    /// (#859): past the first rows and not a 2^k-th one.
    pub first_of_cause: bool,
    /// How many of the burst's rows before this one got no line of their own.
    pub unsaid_before: usize,
    /// The burst this row's quiet gap ended, if that burst left rows unsaid:
    /// said once, here — the first row of a burst is always reported.
    pub ended: Option<EndedBurst>,
}

/// A burst that ended with rows left unsaid (#807).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndedBurst {
    /// Rows refused in it.
    pub rows: usize,
    /// Of those, the ones with no line of their own.
    pub unsaid: usize,
}

impl std::fmt::Display for BurstTally {
    /// `row 1025 of this burst (1002 before it had no line of their own)`,
    /// with `, the first with its cause` when that is why it has a line, then,
    /// on a new burst's first row, how the last one ended.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "row {} of this burst", self.nth)?;
        if self.first_of_cause {
            write!(f, ", the first with its cause")?;
        }
        let n = self.unsaid_before;
        if n > 0 {
            write!(f, " ({n} before it had no line of {})", if n == 1 { "its own" } else { "their own" })?;
        }
        if let Some(ended) = self.ended {
            write!(
                f,
                "; the burst before it ended after {} rows, {} of them with no line of their \
                 own",
                ended.rows, ended.unsaid
            )?;
        }
        Ok(())
    }
}

/// No row refused yet — [`Thinning::new`], which is also `const` so a
/// `static` can hold one.
impl<C> Default for Thinning<C> {
    fn default() -> Self {
        Self::new()
    }
}

/// One refused row's verdict from [`Thinning::refuse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Whether this row gets a line of its own.
    pub report: bool,
    /// Where it stands in its burst.
    pub tally: BurstTally,
}

/// The refused rows of one count — a daemon sink's channel, action and kind
/// (shed, refused after shutdown, or a failed insert of one outcome), or a
/// bus writer's channel, action and outcome: where the current burst stands,
/// and what earlier bursts left unreported.
///
/// `C` is what a row's cause is told apart by (#859): `()` for a kind with
/// one cause (a shed row is always "the queue was full"), a failed insert's
/// `kastellan_db::audit::InsertCause` otherwise.
#[derive(Debug)]
pub struct Thinning<C = ()> {
    /// Rows refused in the current burst.
    in_burst: usize,
    /// Of those, the ones that got a line of their own.
    reported_in_burst: usize,
    /// The causes seen in the current burst, at most
    /// [`MAX_CAUSES_PER_BURST`].
    seen: Vec<C>,
    /// When the last one was; `None` before the first.
    last: Option<Instant>,
    /// Rows left unreported by bursts that have ended.
    unreported_before: usize,
}

impl<C> Thinning<C> {
    /// No row refused yet.
    pub const fn new() -> Self {
        Self { in_burst: 0, reported_in_burst: 0, seen: Vec::new(), last: None, unreported_before: 0 }
    }

    /// How many refused rows got no report of their own, every burst so far.
    pub fn unreported(&self) -> usize {
        self.unreported_before + (self.in_burst - self.reported_in_burst)
    }
}

impl<C: PartialEq> Thinning<C> {
    /// Count one refused row at `now`, failed for `cause`: whether it gets a
    /// report of its own — one of the burst's first [`REPORT_EACH_UP_TO`], a
    /// 2^k-th, or the first of its cause in the burst (#859) — and its
    /// [`BurstTally`].
    ///
    /// A clock that steps backwards reads as no gap, never as a new burst,
    /// and does not move the burst's last row back either, so the gap after
    /// it is not stretched. (Production passes `Instant::now()`, read under
    /// the lock, so this is defence, not a case it meets.)
    pub fn refuse(&mut self, now: Instant, cause: C) -> Refusal {
        let quiet = self.last.is_some_and(|last| now.saturating_duration_since(last) >= BURST_QUIET_GAP);
        let mut ended = None;
        if quiet {
            let unsaid = self.in_burst - self.reported_in_burst;
            if unsaid > 0 {
                ended = Some(EndedBurst { rows: self.in_burst, unsaid });
            }
            self.unreported_before += unsaid;
            self.in_burst = 0;
            self.reported_in_burst = 0;
            self.seen.clear();
        }
        self.last = Some(self.last.map_or(now, |last| last.max(now)));
        let new_cause = !self.seen.contains(&cause) && self.seen.len() < MAX_CAUSES_PER_BURST;
        if new_cause {
            self.seen.push(cause);
        }
        let n = self.in_burst;
        let report = should_report(n) || new_cause;
        let tally = BurstTally {
            nth: n + 1,
            first_of_cause: report && !should_report(n),
            unsaid_before: n - self.reported_in_burst,
            ended,
        };
        self.in_burst += 1;
        self.reported_in_burst += usize::from(report);
        Refusal { report, tally }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_rows_are_reported_one_by_one_then_thinned() {
        assert!((0..REPORT_EACH_UP_TO).all(should_report), "the first rows are each reported");
        assert!(should_report(16) && should_report(32) && should_report(1024), "then powers of two");
        assert!(!should_report(17) && !should_report(33) && !should_report(1000));
    }

    /// The closed form agrees with the predicate it summarises.
    #[test]
    fn the_unreported_count_matches_the_predicate() {
        for n in (0..200).chain([1023, 1024, 1025, 4096, 1 << 20]) {
            let reported = (0..n).filter(|&i| should_report(i)).count();
            assert_eq!(unreported_of(n), n - reported, "n = {n}");
        }
    }

    /// Refuse `n` rows of one cause at `at`, returning how many were reported.
    fn refuse_n(t: &mut Thinning, n: usize, at: Instant) -> usize {
        (0..n).filter(|_| t.refuse(at, ()).report).count()
    }

    /// #802: after a quiet gap the next row is reported again, and the earlier
    /// burst's unreported rows are still counted.
    #[test]
    fn a_quiet_gap_starts_a_new_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        assert_eq!(refuse_n(&mut t, 20, t0), 17, "POSITIVE CONTROL: the flood is thinned");
        assert!(!t.refuse(t0, ()).report, "the 21st row of the same burst is not reported");
        assert!(t.refuse(t0 + BURST_QUIET_GAP, ()).report, "a row after a quiet gap is");
        assert_eq!(t.unreported(), 4, "the first burst's 4 still count; the new row was reported");
    }

    /// A sustained flood never pauses for the gap, so it stays thinned however
    /// long it lasts.
    #[test]
    fn a_sustained_flood_stays_one_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        let step = BURST_QUIET_GAP / 2;
        let reported = (0..40u32).filter(|&i| t.refuse(t0 + step * i, ()).report).count();
        assert_eq!(reported, 16 + 2, "0..16, then 16 and 32: one burst over 20 minutes");
        assert_eq!(t.unreported(), 40 - 18);
    }

    /// Just short of the gap is still the same burst.
    #[test]
    fn a_gap_just_short_of_the_bound_is_the_same_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        refuse_n(&mut t, 17, t0);
        assert!(!t.refuse(t0 + BURST_QUIET_GAP - Duration::from_millis(1), ()).report, "row 17 of one burst");
    }

    /// A clock that steps backwards does not open a new burst.
    #[test]
    fn an_earlier_now_is_no_gap() {
        let t0 = Instant::now() + BURST_QUIET_GAP;
        let mut t = Thinning::new();
        refuse_n(&mut t, 17, t0);
        assert!(!t.refuse(t0 - BURST_QUIET_GAP, ()).report, "row 17 of one burst");
        let just_short = t0 + BURST_QUIET_GAP - Duration::from_millis(1);
        assert!(!t.refuse(just_short, ()).report, "the step back did not stretch the gap after t0");
    }

    /// #807: each row knows where it stands in its burst, so a 2^k-th line
    /// says how big the flood is so far — not only a graceful shutdown.
    #[test]
    fn each_refusal_carries_its_place_in_the_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        let tallies: Vec<Refusal> = (0..1025).map(|_| t.refuse(t0, ())).collect();
        assert_eq!(tallies[0].tally, BurstTally { nth: 1, first_of_cause: false, unsaid_before: 0, ended: None });
        assert_eq!(tallies[16].tally, BurstTally { nth: 17, first_of_cause: false, unsaid_before: 0, ended: None });
        let row_1025 = tallies[1024];
        assert!(row_1025.report, "POSITIVE CONTROL: index 1024 is a power of two");
        assert_eq!(row_1025.tally.nth, 1025);
        assert_eq!(row_1025.tally.unsaid_before, unreported_of(1024));
        assert_eq!(
            row_1025.tally.to_string(),
            format!("row 1025 of this burst ({} before it had no line of their own)", unreported_of(1024))
        );
    }

    /// #807: the first row after a quiet gap says how the burst before it
    /// ended, once — and only when that burst left rows unsaid.
    #[test]
    fn a_new_burst_says_how_the_last_one_ended() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        refuse_n(&mut t, 20, t0);
        let first = t.refuse(t0 + BURST_QUIET_GAP, ());
        assert!(first.report);
        assert_eq!(first.tally.ended, Some(EndedBurst { rows: 20, unsaid: 3 }));
        assert_eq!(
            first.tally.to_string(),
            "row 1 of this burst; the burst before it ended after 20 rows, 3 of them with no line \
             of their own"
        );
        assert_eq!(t.refuse(t0 + BURST_QUIET_GAP, ()).tally.ended, None, "said once");

        // A burst that had a line for every row ends with nothing to say.
        let mut quiet = Thinning::new();
        refuse_n(&mut quiet, 3, t0);
        assert_eq!(quiet.refuse(t0 + BURST_QUIET_GAP, ()).tally.ended, None);
    }
    /// #859: a row whose cause is new in the burst gets a line of its own
    /// mid-flood, saying why it has one — and the next row of that cause is
    /// thinned again, with the flood.
    #[test]
    fn the_first_row_of_a_new_cause_gets_its_own_line() {
        let t0 = Instant::now();
        let mut t: Thinning<char> = Thinning::new();
        let flood = (0..100).filter(|_| t.refuse(t0, 'a').report).count();
        assert_eq!(flood, REPORT_EACH_UP_TO + 3, "POSITIVE CONTROL: 0..16, then 16, 32 and 64");
        let b = t.refuse(t0, 'b');
        assert!(b.report, "index 100 is no power of two: only its cause gives it a line");
        assert_eq!(
            b.tally,
            BurstTally { nth: 101, first_of_cause: true, unsaid_before: unreported_of(100), ended: None }
        );
        assert_eq!(
            b.tally.to_string(),
            format!("row 101 of this burst, the first with its cause ({} before it had no line of their own)", unreported_of(100))
        );
        let again = t.refuse(t0, 'b');
        assert!(!again.report, "the cause is no longer new");
        assert_eq!(again.tally.unsaid_before, unreported_of(100), "row 101 had its line");
        assert_eq!(t.unreported(), unreported_of(102) - 1, "the extra line is not counted unsaid");
    }

    /// Why the FIRST of each cause, not each row whose cause differs from the
    /// last line's: two faults interleaved would give every row a line.
    #[test]
    fn interleaved_causes_stay_thinned() {
        let t0 = Instant::now();
        let mut t: Thinning<bool> = Thinning::new();
        let said = (0..1000).filter(|&i| t.refuse(t0, i % 2 == 0).report).count();
        assert_eq!(said, 1000 - unreported_of(1000), "both causes were first seen among the first rows");
    }

    /// A cause seen among the first rows is not news later in the burst.
    #[test]
    fn a_cause_seen_early_is_not_new_later() {
        let t0 = Instant::now();
        let mut t: Thinning<char> = Thinning::new();
        t.refuse(t0, 'b');
        refuse_cause(&mut t, 'a', 50, t0);
        assert!(!t.refuse(t0, 'b').report, "row 52: 'b' was row 1");
    }

    /// The causes told apart are bounded; past the bound a new one is
    /// thinned like the rest.
    #[test]
    fn the_causes_told_apart_are_bounded() {
        let t0 = Instant::now();
        let mut t: Thinning<usize> = Thinning::new();
        refuse_cause(&mut t, 0, 20, t0);
        let fresh = (1..MAX_CAUSES_PER_BURST).filter(|&c| t.refuse(t0, c).report).count();
        assert_eq!(fresh, MAX_CAUSES_PER_BURST - 1, "POSITIVE CONTROL: each new cause gets a line");
        assert!(!t.refuse(t0, MAX_CAUSES_PER_BURST).report, "one cause past the bound");
    }

    /// A quiet gap forgets the causes: a cause is news again in a new burst.
    #[test]
    fn a_new_burst_forgets_the_causes_seen() {
        let t0 = Instant::now();
        let mut t: Thinning<char> = Thinning::new();
        refuse_cause(&mut t, 'a', 20, t0);
        assert!(t.refuse(t0, 'b').report, "row 21: 'b' is new");
        let later = t0 + BURST_QUIET_GAP;
        refuse_cause(&mut t, 'a', 20, later);
        let b = t.refuse(later, 'b');
        assert!(b.report && b.tally.first_of_cause, "'b' is new in this burst too: {b:?}");
        // Each burst: 21 rows, of which rows 1-17 and 'b' had lines — 3 unsaid.
        assert_eq!(t.unreported(), 3 + 3, "each burst's 'b' line is not counted unsaid");
    }

    /// Refuse `n` rows of `cause` at `at`.
    fn refuse_cause<C: PartialEq + Copy>(t: &mut Thinning<C>, cause: C, n: usize, at: Instant) {
        for _ in 0..n {
            t.refuse(at, cause);
        }
    }

    /// The tally's `ended` subtracts the cause lines too.
    #[test]
    fn an_ended_burst_counts_its_cause_lines_as_said() {
        let t0 = Instant::now();
        let mut t: Thinning<char> = Thinning::new();
        refuse_cause(&mut t, 'a', 20, t0);
        t.refuse(t0, 'b');
        let next = t.refuse(t0 + BURST_QUIET_GAP, 'a');
        assert_eq!(next.tally.ended, Some(EndedBurst { rows: 21, unsaid: 3 }), "{next:?}");
    }
}
