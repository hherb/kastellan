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
//! of each cause gets a line of its own ([`Thinning::refuse`]'s `cause`; for
//! failed inserts `kastellan_db::audit::InsertCause`, a class that is the
//! same for the same fault on any row). The *first* of each cause, not each
//! row whose cause differs from the last line's: two faults interleaved
//! would then give every row a line, which is the flood again.
//!
//! "First" is counted from the burst's last 2^k-th line, not from the
//! burst's start: each 2^k-th line starts the causes afresh, and at most
//! [`MAX_CAUSES_PER_STRETCH`] are told apart until the next. Counted from the
//! start, a flood that never pauses would fill the budget with its first
//! transients — a restart's EOF, reset and broken pipe — and keep it full
//! for hours, so a `PANIC` that came then, a row that may be committed, got
//! no line and nothing said why. Per stretch, a cause gets at most one line
//! between two 2^k-th lines, so the lines stay logarithmic in the flood.
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

// The first 2^k-th line is then REPORT_EACH_UP_TO itself: the row the first
// stretch of causes starts at (`Thinning::refuse`), and the first power of two
// the tests' closed form `unreported_of` counts — which miscounts otherwise
// (or, at 0, panics). A const, so changing the knob fails the build, not only
// the tests.
const _: () = assert!(REPORT_EACH_UP_TO.is_power_of_two(), "unreported_of assumes a power of two");

/// How many distinct causes one stretch of a burst — from one 2^k-th line to
/// the next — tells apart (#859). Past it, a row of a cause not yet seen is
/// thinned until the next 2^k-th line starts the causes afresh. Causes are
/// fixed classes, never row values, but there are hundreds of them (every
/// SQLSTATE); a stretch that has seen this many distinct faults is one of
/// everything at once, not one with news in it.
pub const MAX_CAUSES_PER_STRETCH: usize = 8;

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
    /// `Some(m)`: it has a line of its own only because its cause is new
    /// since row `m` — the burst's last 2^k-th line, where the causes told
    /// apart start afresh (#859). `None` for a row that is one of the
    /// first, a 2^k-th, or thinned.
    pub new_cause_since: Option<usize>,
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
    /// with `, the first with its cause since row 513` when that is why it
    /// has a line, then, on a new burst's first row, how the last one ended.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "row {} of this burst", self.nth)?;
        if let Some(since) = self.new_cause_since {
            write!(f, ", the first with its cause since row {since}")?;
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

impl BurstTally {
    /// Whether this row gets a line of its own: one of the burst's first
    /// [`REPORT_EACH_UP_TO`], a 2^k-th, or the first of its cause since the
    /// last 2^k-th line. Derived, never stored, so it cannot disagree with
    /// the tally that says why.
    pub fn has_own_line(&self) -> bool {
        // `nth` counts from 1; a hand-built 0 reads as one of the first.
        self.nth <= REPORT_EACH_UP_TO || should_report(self.nth - 1) || self.new_cause_since.is_some()
    }
}

/// One refused row's verdict from [`Thinning::refuse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Where it stands in its burst.
    pub tally: BurstTally,
}

impl Refusal {
    /// Whether this row gets a line of its own ([`BurstTally::has_own_line`]).
    pub fn report(&self) -> bool {
        self.tally.has_own_line()
    }
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
    /// The causes seen since the burst's last 2^k-th line, at most
    /// [`MAX_CAUSES_PER_STRETCH`].
    seen: Vec<C>,
    /// The row (`nth`, from 1) of that line: what a cause is new since.
    causes_since: usize,
    /// When the last one was; `None` before the first.
    last: Option<Instant>,
    /// Rows left unreported by bursts that have ended.
    unreported_before: usize,
}

impl<C> Thinning<C> {
    /// No row refused yet.
    pub const fn new() -> Self {
        Self { in_burst: 0, reported_in_burst: 0, seen: Vec::new(), causes_since: 1, last: None, unreported_before: 0 }
    }

    /// How many refused rows got no report of their own, every burst so far.
    pub fn unreported(&self) -> usize {
        self.unreported_before + (self.in_burst - self.reported_in_burst)
    }
}

impl<C: PartialEq> Thinning<C> {
    /// Count one refused row at `now`, failed for `cause`: whether it gets a
    /// report of its own — one of the burst's first [`REPORT_EACH_UP_TO`], a
    /// 2^k-th, or the first of its cause since the last 2^k-th line (#859) —
    /// and its [`BurstTally`].
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
            self.causes_since = 1;
        }
        self.last = Some(self.last.map_or(now, |last| last.max(now)));
        let n = self.in_burst;
        let line = should_report(n);
        if line && n >= REPORT_EACH_UP_TO {
            // A 2^k-th line: the causes start afresh, its own the first seen.
            self.seen.clear();
            self.causes_since = n + 1;
        }
        let new_cause = !self.seen.contains(&cause) && self.seen.len() < MAX_CAUSES_PER_STRETCH;
        if new_cause {
            self.seen.push(cause);
        }
        let tally = BurstTally {
            nth: n + 1,
            new_cause_since: (new_cause && !line).then_some(self.causes_since),
            unsaid_before: n - self.reported_in_burst,
            ended,
        };
        let refusal = Refusal { tally };
        self.in_burst += 1;
        self.reported_in_burst += usize::from(refusal.report());
        refusal
    }
}

#[cfg(test)]
mod tests;
