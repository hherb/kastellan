//! `audit_thinning`'s tests, split out to keep the module under the 500-LOC
//! soft cap.

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
    (0..n).filter(|_| t.refuse(at, ()).report()).count()
}

/// #802: after a quiet gap the next row is reported again, and the earlier
/// burst's unreported rows are still counted.
#[test]
fn a_quiet_gap_starts_a_new_burst() {
    let t0 = Instant::now();
    let mut t = Thinning::new();
    assert_eq!(refuse_n(&mut t, 20, t0), 17, "POSITIVE CONTROL: the flood is thinned");
    assert!(!t.refuse(t0, ()).report(), "the 21st row of the same burst is not reported");
    assert!(t.refuse(t0 + BURST_QUIET_GAP, ()).report(), "a row after a quiet gap is");
    assert_eq!(t.unreported(), 4, "the first burst's 4 still count; the new row was reported");
}

/// A sustained flood never pauses for the gap, so it stays thinned however
/// long it lasts.
#[test]
fn a_sustained_flood_stays_one_burst() {
    let t0 = Instant::now();
    let mut t = Thinning::new();
    let step = BURST_QUIET_GAP / 2;
    let reported = (0..40u32).filter(|&i| t.refuse(t0 + step * i, ()).report()).count();
    assert_eq!(reported, 16 + 2, "0..16, then 16 and 32: one burst over 20 minutes");
    assert_eq!(t.unreported(), 40 - 18);
}

/// Just short of the gap is still the same burst.
#[test]
fn a_gap_just_short_of_the_bound_is_the_same_burst() {
    let t0 = Instant::now();
    let mut t = Thinning::new();
    refuse_n(&mut t, 17, t0);
    assert!(!t.refuse(t0 + BURST_QUIET_GAP - Duration::from_millis(1), ()).report(), "row 17 of one burst");
}

/// A clock that steps backwards does not open a new burst.
#[test]
fn an_earlier_now_is_no_gap() {
    let t0 = Instant::now() + BURST_QUIET_GAP;
    let mut t = Thinning::new();
    refuse_n(&mut t, 17, t0);
    assert!(!t.refuse(t0 - BURST_QUIET_GAP, ()).report(), "row 17 of one burst");
    let just_short = t0 + BURST_QUIET_GAP - Duration::from_millis(1);
    assert!(!t.refuse(just_short, ()).report(), "the step back did not stretch the gap after t0");
}

/// #807: each row knows where it stands in its burst, so a 2^k-th line
/// says how big the flood is so far — not only a graceful shutdown.
#[test]
fn each_refusal_carries_its_place_in_the_burst() {
    let t0 = Instant::now();
    let mut t = Thinning::new();
    let tallies: Vec<Refusal> = (0..1025).map(|_| t.refuse(t0, ())).collect();
    assert_eq!(tallies[0].tally, BurstTally { nth: 1, new_cause_since: None, unsaid_before: 0, ended: None });
    assert_eq!(tallies[16].tally, BurstTally { nth: 17, new_cause_since: None, unsaid_before: 0, ended: None });
    let row_1025 = tallies[1024];
    assert!(row_1025.report(), "POSITIVE CONTROL: index 1024 is a power of two");
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
    assert!(first.report());
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
    let flood = (0..100).filter(|_| t.refuse(t0, 'a').report()).count();
    assert_eq!(flood, REPORT_EACH_UP_TO + 3, "POSITIVE CONTROL: 0..16, then 16, 32 and 64");
    let b = t.refuse(t0, 'b');
    assert!(b.report(), "index 100 is no power of two: only its cause gives it a line");
    assert_eq!(
        b.tally,
        BurstTally { nth: 101, new_cause_since: Some(65), unsaid_before: unreported_of(100), ended: None }
    );
    assert_eq!(
        b.tally.to_string(),
        format!(
            "row 101 of this burst, the first with its cause since row 65 ({} before it had no line of their own)",
            unreported_of(100)
        )
    );
    let again = t.refuse(t0, 'b');
    assert!(!again.report(), "the cause is no longer new");
    assert_eq!(again.tally.unsaid_before, unreported_of(100), "row 101 had its line");
    assert_eq!(t.unreported(), unreported_of(102) - 1, "the extra line is not counted unsaid");
}

/// Why the FIRST of each cause, not each row whose cause differs from the
/// last line's: two faults interleaved would give every row a line. They
/// get one line more per stretch — the cause the 2^k-th row did not have
/// — so still logarithmic.
#[test]
fn interleaved_causes_stay_thinned() {
    let t0 = Instant::now();
    let mut t: Thinning<bool> = Thinning::new();
    let said = (0..1000).filter(|&i| t.refuse(t0, i % 2 == 0).report()).count();
    // Stretches start at 16, 32, 64, 128, 256 and 512, each on an even
    // row: the next odd row's cause is new in each.
    assert_eq!(said, 1000 - unreported_of(1000) + 6);
}

/// The 2^k-th row's own cause is the first its stretch has seen: the
/// next row of that cause is thinned, and a row of the cause the stretch
/// has not seen is not — even one seen before the line.
#[test]
fn a_stretch_starts_with_its_lines_cause() {
    let t0 = Instant::now();
    let mut t: Thinning<char> = Thinning::new();
    refuse_cause(&mut t, 'a', REPORT_EACH_UP_TO, t0);
    let line = t.refuse(t0, 'b');
    assert!(line.report(), "row 17 is a 2^k-th line");
    assert_eq!(line.tally.new_cause_since, None, "a 2^k-th line is not there for its cause");
    assert!(!t.refuse(t0, 'b').report(), "row 18: 'b' was row 17's cause");
    let a = t.refuse(t0, 'a');
    assert_eq!(a.tally.new_cause_since, Some(17), "row 19: 'a' is new since row 17: {a:?}");
    assert!(!t.refuse(t0, 'a').report(), "and then thinned");
}

/// The causes told apart are bounded per stretch, in memory as in lines;
/// past the bound a new one is thinned like the rest — until the next
/// 2^k-th line, when it is news again. A sustained flood never pauses,
/// so without that the first stretch's transients would hold the bound
/// for the whole burst, and a late PANIC would get no line.
#[test]
fn the_causes_told_apart_are_bounded_per_stretch() {
    let t0 = Instant::now();
    let mut t: Thinning<usize> = Thinning::new();
    refuse_cause(&mut t, 0, 20, t0);
    let fresh = (1..MAX_CAUSES_PER_STRETCH).filter(|&c| t.refuse(t0, c).report()).count();
    assert_eq!(fresh, MAX_CAUSES_PER_STRETCH - 1, "POSITIVE CONTROL: each new cause gets a line");
    let past = MAX_CAUSES_PER_STRETCH;
    assert!(!t.refuse(t0, past).report(), "one cause past the bound");
    assert!(!t.refuse(t0, past + 1).report(), "and another");
    assert_eq!(t.seen.len(), MAX_CAUSES_PER_STRETCH, "the causes held are bounded too");
    // Rows 30..32 of the flood's cause, then row 33 is the next line.
    let to_32 = 32 - t.in_burst;
    refuse_cause(&mut t, 0, to_32, t0);
    assert!(t.refuse(t0, 0).report(), "POSITIVE CONTROL: row 33 is a 2^k-th line");
    let late = t.refuse(t0, past);
    assert_eq!(late.tally.new_cause_since, Some(33), "news in the next stretch: {late:?}");
}

/// A quiet gap forgets the causes: a cause is news again in a new burst.
#[test]
fn a_new_burst_forgets_the_causes_seen() {
    let t0 = Instant::now();
    let mut t: Thinning<char> = Thinning::new();
    refuse_cause(&mut t, 'a', 20, t0);
    assert!(t.refuse(t0, 'b').report(), "row 21: 'b' is new");
    let later = t0 + BURST_QUIET_GAP;
    refuse_cause(&mut t, 'a', 20, later);
    let b = t.refuse(later, 'b');
    assert!(b.report() && b.tally.new_cause_since.is_some(), "'b' is new in this burst too: {b:?}");
    // Each burst: 21 rows, of which rows 1-17 and 'b' had lines — 3 unsaid.
    assert_eq!(t.unreported(), 3 + 3, "each burst's 'b' line is not counted unsaid");
}

/// Refuse `n` rows of `cause` at `at`.
fn refuse_cause<C: PartialEq + Copy>(t: &mut Thinning<C>, cause: C, n: usize, at: Instant) {
    for _ in 0..n {
        t.refuse(at, cause);
    }
}

/// A hand-built tally's line follows from it alone (the stored `report`
/// field this replaced could say otherwise).
#[test]
fn whether_a_row_has_a_line_follows_from_its_tally() {
    let tally = |nth, new_cause_since| BurstTally { nth, new_cause_since, unsaid_before: 0, ended: None };
    assert!(tally(16, None).has_own_line() && tally(17, None).has_own_line(), "the first rows, and row 17");
    assert!(!tally(18, None).has_own_line());
    assert!(tally(18, Some(17)).has_own_line(), "a new cause");
    assert!(tally(1025, None).has_own_line(), "index 1024");
    assert!(tally(0, None).has_own_line(), "no row 0, but no panic either");
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
