//! Which refused audit rows get a report of their own (#798), counted per
//! burst (#802). Pure: the clock is passed in. Split out of `audit_sink.rs` to
//! keep it under the 500-LOC soft cap; `#[path]`-included there.
//!
//! A shed or refused row is reported by its sink **on the driver thread** (a
//! tracing ERROR, or the marked stderr line when nothing records it — either
//! can block), and a flood —
//! email's `skipped` list has no length cap — would have the driver do that
//! for every id. So within one burst the first [`REPORT_EACH_UP_TO`] are
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
//! a graceful shutdown — an OOM, a SIGKILL or a crash lost it, and a weeks-long
//! daemon's alert saw ~18 anonymous lines for 10 000 rows. Now the last line
//! said is within a factor of two of the burst's size, and the first row of
//! the next burst says how the one before it ended.
//!
//! **Each channel thins on its own** ([`Refusals`], #807). One shared count
//! let a compromised email worker — its `skipped` list has no cap — hold a
//! shed burst open with one refusal every 59 s, so a Matrix
//! `channel.reply_undelivered` row shed meanwhile got no line naming its
//! conversation, only an anonymous share of the shutdown count.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::lease::SinkChannel;
use super::Unwritten;

/// How many refused rows of one burst are reported one by one before
/// [`should_report`] thins them out.
pub(super) const REPORT_EACH_UP_TO: usize = 16;

// `unreported_of`'s closed form counts REPORT_EACH_UP_TO as the first power of
// two it reports; any other value miscounts (or, at 0, panics) — and a
// `#[cfg(test)]` check is no guard in a release build.
const _: () = assert!(REPORT_EACH_UP_TO.is_power_of_two(), "unreported_of assumes a power of two");

/// How long with no refused row ends a burst, so the next one is reported
/// afresh. A sustained flood never pauses this long, so it stays thinned; a
/// row refused after a quiet minute is news again.
pub(super) const BURST_QUIET_GAP: Duration = Duration::from_secs(60);

/// Pure: whether the `n`th (0-based) refused row of a burst gets its own
/// report.
pub(super) fn should_report(n: usize) -> bool {
    n < REPORT_EACH_UP_TO || n.is_power_of_two()
}

/// Pure: how many of a burst's first `refused` rows [`should_report`] leaves
/// out — closed form, so a shutdown after a huge flood does not loop over it.
pub(super) fn unreported_of(refused: usize) -> usize {
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
pub(crate) struct BurstTally {
    /// This row is the `nth` refused in its burst, counting from 1.
    pub(crate) nth: usize,
    /// How many of the burst's rows before this one got no line of their own.
    pub(crate) unsaid_before: usize,
    /// The burst this row's quiet gap ended, if that burst left rows unsaid:
    /// said once, here — the first row of a burst is always reported.
    pub(crate) ended: Option<EndedBurst>,
}

/// A burst that ended with rows left unsaid (#807).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EndedBurst {
    /// Rows refused in it.
    pub(crate) rows: usize,
    /// Of those, the ones with no line of their own.
    pub(crate) unsaid: usize,
}

impl std::fmt::Display for BurstTally {
    /// `refused row 1024 of this burst (1007 before it had no line of their
    /// own)`, then, on a new burst's first row, how the last one ended.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused row {} of this burst", self.nth)?;
        let n = self.unsaid_before;
        if n > 0 {
            write!(f, " ({n} before it had no line of {})", if n == 1 { "its own" } else { "their own" })?;
        }
        if let Some(ended) = self.ended {
            write!(
                f,
                "; the burst before it ended after {} refused rows, {} of them with no line of \
                 their own",
                ended.rows, ended.unsaid
            )?;
        }
        Ok(())
    }
}

/// One refused row's verdict from [`Thinning::refuse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Refusal {
    /// Whether this row gets a line of its own.
    pub(super) report: bool,
    /// Where it stands in its burst.
    pub(super) tally: BurstTally,
}

/// The refused rows of one kind (shed, or refused after shutdown) on one
/// channel: where the current burst stands, and what earlier bursts left
/// unreported.
#[derive(Debug)]
pub(super) struct Thinning {
    /// Rows refused in the current burst.
    in_burst: usize,
    /// When the last one was; `None` before the first.
    last: Option<Instant>,
    /// Rows left unreported by bursts that have ended.
    unreported_before: usize,
}

impl Thinning {
    /// No row refused yet.
    pub(super) const fn new() -> Self {
        Self { in_burst: 0, last: None, unreported_before: 0 }
    }

    /// Count one refused row at `now`: whether it gets a report of its own,
    /// and its [`BurstTally`]. A clock that steps backwards reads as no gap,
    /// never as a new burst, and does not move the burst's last row back
    /// either, so the gap after it is not stretched. (Production passes
    /// `Instant::now()`, read under the lock, so this is defence, not a case
    /// it meets.)
    pub(super) fn refuse(&mut self, now: Instant) -> Refusal {
        let quiet = self.last.is_some_and(|last| now.saturating_duration_since(last) >= BURST_QUIET_GAP);
        let mut ended = None;
        if quiet {
            let unsaid = unreported_of(self.in_burst);
            if unsaid > 0 {
                ended = Some(EndedBurst { rows: self.in_burst, unsaid });
            }
            self.unreported_before += unsaid;
            self.in_burst = 0;
        }
        self.last = Some(self.last.map_or(now, |last| last.max(now)));
        let n = self.in_burst;
        self.in_burst += 1;
        let tally = BurstTally { nth: n + 1, unsaid_before: unreported_of(n), ended };
        Refusal { report: should_report(n), tally }
    }

    /// How many refused rows got no report of their own, every burst so far.
    pub(super) fn unreported(&self) -> usize {
        self.unreported_before + unreported_of(self.in_burst)
    }
}

/// One channel's refused rows: shed apart from late, so a flood shed earlier
/// cannot silence the rows refused at shutdown (#798).
#[derive(Debug)]
struct ChannelRefusals {
    shed: Mutex<Thinning>,
    late: Mutex<Thinning>,
}

impl ChannelRefusals {
    const fn new() -> Self {
        Self { shed: Mutex::new(Thinning::new()), late: Mutex::new(Thinning::new()) }
    }

    fn unreported(&self) -> UnsaidCounts {
        let count = |t: &Mutex<Thinning>| t.lock().unwrap_or_else(|p| p.into_inner()).unreported();
        UnsaidCounts { shed: count(&self.shed), late: count(&self.late) }
    }
}

/// Every channel's refused rows, each thinned on its own (#807).
#[derive(Debug)]
pub(super) struct Refusals {
    matrix: ChannelRefusals,
    email: ChannelRefusals,
}

impl Refusals {
    pub(super) const fn new() -> Self {
        Self { matrix: ChannelRefusals::new(), email: ChannelRefusals::new() }
    }

    fn of(&self, channel: SinkChannel) -> &ChannelRefusals {
        match channel {
            SinkChannel::Matrix => &self.matrix,
            SinkChannel::Email => &self.email,
        }
    }

    /// `channel`'s rows shed on the caller's thread.
    pub(super) fn shed(&self, channel: SinkChannel) -> &Mutex<Thinning> {
        &self.of(channel).shed
    }

    /// `channel`'s rows refused after the ledger closed.
    pub(super) fn late(&self, channel: SinkChannel) -> &Mutex<Thinning> {
        &self.of(channel).late
    }

    /// The refused rows that got no line of their own, so far, per channel.
    pub(super) fn unreported(&self) -> Unreported {
        Unreported { matrix: self.matrix.unreported(), email: self.email.unreported() }
    }
}

/// One channel's refused rows with no line of their own, by kind (#807: said
/// apart at shutdown, so last week's flood reads apart from this shutdown).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UnsaidCounts {
    /// Shed: too many inserts already waiting for the pool.
    pub(crate) shed: usize,
    /// Tried after the shutdown drain closed the ledger.
    pub(crate) late: usize,
}

impl UnsaidCounts {
    pub(crate) fn total(self) -> usize {
        self.shed + self.late
    }
}

/// Every channel's [`UnsaidCounts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Unreported {
    pub(crate) matrix: UnsaidCounts,
    pub(crate) email: UnsaidCounts,
}

impl Unreported {
    pub(crate) fn of(self, channel: SinkChannel) -> UnsaidCounts {
        match channel {
            SinkChannel::Matrix => self.matrix,
            SinkChannel::Email => self.email,
        }
    }

    /// Every channel's, both kinds.
    pub(crate) fn total(self) -> usize {
        self.matrix.total() + self.email.total()
    }
}

/// Tell `on_failure` of a row refused on the caller's thread — unless the
/// burst has passed the thinning, in which case it is only counted — and hand
/// the [`Unwritten`] (`why`, given the row's tally) back for the caller's `Err`.
pub(super) fn refuse(
    thinning: &Mutex<Thinning>,
    why: fn(BurstTally) -> Unwritten<'static>,
    on_failure: impl FnOnce(Unwritten<'_>),
) -> Unwritten<'static> {
    // The lock is released before `on_failure` runs: a report can block.
    let refusal = thinning.lock().unwrap_or_else(|p| p.into_inner()).refuse(Instant::now());
    let why = why(refusal.tally);
    if refusal.report {
        on_failure(why);
    }
    why
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

    /// Refuse `n` rows at `at`, returning how many were reported.
    fn refuse_n(t: &mut Thinning, n: usize, at: Instant) -> usize {
        (0..n).filter(|_| t.refuse(at).report).count()
    }

    /// #802: after a quiet gap the next row is reported again, and the earlier
    /// burst's unreported rows are still counted.
    #[test]
    fn a_quiet_gap_starts_a_new_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        assert_eq!(refuse_n(&mut t, 20, t0), 17, "POSITIVE CONTROL: the flood is thinned");
        assert!(!t.refuse(t0).report, "the 21st row of the same burst is not reported");
        assert!(t.refuse(t0 + BURST_QUIET_GAP).report, "a row after a quiet gap is");
        assert_eq!(t.unreported(), 4, "the first burst's 4 still count; the new row was reported");
    }

    /// A sustained flood never pauses for the gap, so it stays thinned however
    /// long it lasts.
    #[test]
    fn a_sustained_flood_stays_one_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        let step = BURST_QUIET_GAP / 2;
        let reported = (0..40u32).filter(|&i| t.refuse(t0 + step * i).report).count();
        assert_eq!(reported, 16 + 2, "0..16, then 16 and 32: one burst over 20 minutes");
        assert_eq!(t.unreported(), 40 - 18);
    }

    /// Just short of the gap is still the same burst.
    #[test]
    fn a_gap_just_short_of_the_bound_is_the_same_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        refuse_n(&mut t, 17, t0);
        assert!(!t.refuse(t0 + BURST_QUIET_GAP - Duration::from_millis(1)).report, "row 17 of one burst");
    }

    /// A clock that steps backwards does not open a new burst.
    #[test]
    fn an_earlier_now_is_no_gap() {
        let t0 = Instant::now() + BURST_QUIET_GAP;
        let mut t = Thinning::new();
        refuse_n(&mut t, 17, t0);
        assert!(!t.refuse(t0 - BURST_QUIET_GAP).report, "row 17 of one burst");
        let just_short = t0 + BURST_QUIET_GAP - Duration::from_millis(1);
        assert!(!t.refuse(just_short).report, "the step back did not stretch the gap after t0");
    }

    /// #807: each row knows where it stands in its burst, so a 2^k-th line
    /// says how big the flood is so far — not only a graceful shutdown.
    #[test]
    fn each_refusal_carries_its_place_in_the_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        let tallies: Vec<Refusal> = (0..1025).map(|_| t.refuse(t0)).collect();
        assert_eq!(tallies[0].tally, BurstTally { nth: 1, unsaid_before: 0, ended: None });
        assert_eq!(tallies[16].tally, BurstTally { nth: 17, unsaid_before: 0, ended: None });
        let row_1025 = tallies[1024];
        assert!(row_1025.report, "POSITIVE CONTROL: index 1024 is a power of two");
        assert_eq!(row_1025.tally.nth, 1025);
        assert_eq!(row_1025.tally.unsaid_before, unreported_of(1024));
        assert_eq!(
            row_1025.tally.to_string(),
            format!("refused row 1025 of this burst ({} before it had no line of their own)", unreported_of(1024))
        );
    }

    /// #807: the first row after a quiet gap says how the burst before it
    /// ended, once — and only when that burst left rows unsaid.
    #[test]
    fn a_new_burst_says_how_the_last_one_ended() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        refuse_n(&mut t, 20, t0);
        let first = t.refuse(t0 + BURST_QUIET_GAP);
        assert!(first.report);
        assert_eq!(first.tally.ended, Some(EndedBurst { rows: 20, unsaid: 3 }));
        assert_eq!(
            first.tally.to_string(),
            "refused row 1 of this burst; the burst before it ended after 20 refused rows, 3 of \
             them with no line of their own"
        );
        assert_eq!(t.refuse(t0 + BURST_QUIET_GAP).tally.ended, None, "said once");

        // A burst that had a line for every row ends with nothing to say.
        let mut quiet = Thinning::new();
        refuse_n(&mut quiet, 3, t0);
        assert_eq!(quiet.refuse(t0 + BURST_QUIET_GAP).tally.ended, None);
    }

    /// #807: one channel's flood leaves the other's rows reported, and the
    /// counts stay apart by channel and by kind.
    #[test]
    fn one_channels_flood_does_not_thin_the_others_lines() {
        let refusals = Refusals::new();
        let t0 = Instant::now();
        for _ in 0..100 {
            refusals.shed(SinkChannel::Email).lock().unwrap().refuse(t0);
        }
        let matrix = refusals.shed(SinkChannel::Matrix).lock().unwrap().refuse(t0);
        assert!(matrix.report, "Matrix's first shed row is its own burst's first");
        assert_eq!(matrix.tally.nth, 1);
        refusals.late(SinkChannel::Email).lock().unwrap().refuse(t0);
        let unreported = refusals.unreported();
        assert_eq!(unreported.email, UnsaidCounts { shed: unreported_of(100), late: 0 });
        assert_eq!(unreported.matrix, UnsaidCounts::default());
        assert_eq!(unreported.total(), unreported_of(100));
    }
}
