//! Which refused audit rows get a report of their own (#798), counted per
//! burst (#802). Pure: the clock is passed in. Split out of `audit_sink.rs` to
//! keep it under the 500-LOC soft cap; `#[path]`-included there.
//!
//! A shed or refused row is reported by its sink **on the driver thread** (one
//! tracing ERROR and a stderr write, either of which can block), and a flood —
//! email's `skipped` list has no length cap — would have the driver do that
//! for every id. So within one burst the first [`REPORT_EACH_UP_TO`] are
//! reported, then only the 2^k-th: the reports stay logarithmic in the flood.
//! The rows left out are counted, and the shutdown says how many.
//!
//! A burst ends after [`BURST_QUIET_GAP`] with no refused row. Before #802 the
//! count ran for the daemon's whole life, so a flood on Monday left Friday's
//! single shed row (index 1 000 001, not a power of two) without a line.

use std::time::{Duration, Instant};

/// How many refused rows of one burst are reported one by one before
/// [`should_report`] thins them out.
pub(super) const REPORT_EACH_UP_TO: usize = 16;

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

/// The refused rows of one kind (shed, or refused after shutdown): where the
/// current burst stands, and what earlier bursts left unreported.
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

    /// Count one refused row at `now`, and say whether it gets a report of
    /// its own. A clock that steps backwards reads as no gap, never as a new
    /// burst.
    pub(super) fn refuse(&mut self, now: Instant) -> bool {
        let quiet = self.last.is_some_and(|last| now.saturating_duration_since(last) >= BURST_QUIET_GAP);
        if quiet {
            self.unreported_before += unreported_of(self.in_burst);
            self.in_burst = 0;
        }
        self.last = Some(now);
        let n = self.in_burst;
        self.in_burst += 1;
        should_report(n)
    }

    /// How many refused rows got no report of their own, every burst so far.
    pub(super) fn unreported(&self) -> usize {
        self.unreported_before + unreported_of(self.in_burst)
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

    /// Refuse `n` rows at `at`, returning how many were reported.
    fn refuse_n(t: &mut Thinning, n: usize, at: Instant) -> usize {
        (0..n).filter(|_| t.refuse(at)).count()
    }

    /// #802: after a quiet gap the next row is reported again, and the earlier
    /// burst's unreported rows are still counted.
    #[test]
    fn a_quiet_gap_starts_a_new_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        assert_eq!(refuse_n(&mut t, 20, t0), 17, "POSITIVE CONTROL: the flood is thinned");
        assert!(!t.refuse(t0), "the 21st row of the same burst is not reported");
        assert!(t.refuse(t0 + BURST_QUIET_GAP), "a row after a quiet gap is");
        assert_eq!(t.unreported(), 4, "the first burst's 4 still count; the new row was reported");
    }

    /// A sustained flood never pauses for the gap, so it stays thinned however
    /// long it lasts.
    #[test]
    fn a_sustained_flood_stays_one_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        let step = BURST_QUIET_GAP / 2;
        let reported = (0..40u32).filter(|&i| t.refuse(t0 + step * i)).count();
        assert_eq!(reported, 16 + 2, "0..16, then 16 and 32: one burst over 20 minutes");
        assert_eq!(t.unreported(), 40 - 18);
    }

    /// Just short of the gap is still the same burst.
    #[test]
    fn a_gap_just_short_of_the_bound_is_the_same_burst() {
        let t0 = Instant::now();
        let mut t = Thinning::new();
        refuse_n(&mut t, 17, t0);
        assert!(!t.refuse(t0 + BURST_QUIET_GAP - Duration::from_millis(1)), "row 17 of one burst");
    }

    /// A clock that steps backwards does not open a new burst.
    #[test]
    fn an_earlier_now_is_no_gap() {
        let t0 = Instant::now() + BURST_QUIET_GAP;
        let mut t = Thinning::new();
        refuse_n(&mut t, 17, t0);
        assert!(!t.refuse(t0 - BURST_QUIET_GAP), "row 17 of one burst");
    }
}
