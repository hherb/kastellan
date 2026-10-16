//! `audit_sink`'s tests for inserts that ran and failed (#858, #861): split
//! from `audit_sink_tests.rs`, which is over the 500-LOC soft cap;
//! `#[path]`-included in `audit_sink.rs`, so `super::` is `audit_sink`.
//!
//! As there, every test uses its own `static` [`Ledger`].

use super::test_support::closing_pool;
use super::tests::bounds;
use super::*;
use std::sync::{Arc, Mutex};

/// A runtime and an email writer on `ledger` against [`closing_pool`]:
/// every insert fails at once, its row's outcome unknown.
fn closing_writer(ledger: &'static Ledger) -> (tokio::runtime::Runtime, SinkWriter) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let pool = rt.block_on(async { closing_pool() });
    let writer = SinkWriter::with_ledger(ledger, pool, rt.handle().clone(), SinkChannel::Email);
    (rt, writer)
}

/// #861: a Postgres that fails every insert at once gets a line for the
/// first few rows, then one per power of two — as a flood of shed rows does —
/// each saying its place in the burst, and the rest are counted for the
/// shutdown line. #858: each line says the row *may* not have been written,
/// because a closed connection cannot say whether the statement committed.
///
/// The rows are written one at a time (each task awaited) so their order is
/// the burst's.
#[test]
fn a_flood_of_failed_inserts_is_reported_one_by_one_then_thinned() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 1));
    let (rt, writer) = closing_writer(&LEDGER);
    let told: Arc<Mutex<Vec<String>>> = Arc::default();
    for _ in 0..33 {
        let told = told.clone();
        let task = writer
            .spawn("a", "b", serde_json::json!({}), "t".into(), move |why| {
                told.lock().unwrap().push(format!("{}: {why}", why.verdict()));
            })
            .expect("never shed: one row at a time");
        rt.block_on(task).expect("the insert task ran to completion");
    }
    let told = told.lock().unwrap().clone();
    assert_eq!(told.len(), 18, "n = 0..15 each, then n = 16 and 32 (powers of two); the rest thinned out");
    assert!(told.iter().all(|l| l.starts_with("may not have been written: ")), "{told:#?}");
    assert!(told[16].ends_with("; refused row 17 of this burst"), "{:?}", told[16]);
    assert!(
        told[17].ends_with("; refused row 33 of this burst (15 before it had no line of their own)"),
        "{:?}",
        told[17]
    );
    let unreported = LEDGER.final_snapshot().unreported;
    assert_eq!(unreported.email, UnsaidCounts { shed: 0, late: 0, failed: 15 });
    assert_eq!(unreported.matrix, UnsaidCounts::default(), "email's flood is email's");
    let line = report::format_unreported_at_shutdown(unreported.email).expect("a shutdown line");
    assert!(line.contains("15 failed to insert"), "{line}");
}

/// #861: failed inserts thin in a kind of their own. A flood of them leaves
/// the channel's first shed row with a line of its own, as its burst's first.
#[test]
fn a_flood_of_failed_inserts_does_not_thin_a_shed_rows_line() {
    // One slot: a row in flight holds it, so the next is shed.
    static LEDGER: Ledger = Ledger::new(bounds(1, 1));
    let (rt, writer) = closing_writer(&LEDGER);
    for _ in 0..40 {
        let task = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");
        rt.block_on(task).expect("the insert task ran to completion");
    }
    let (stalled, _listener) = rt.block_on(async { test_support::stalled_pool() });
    let holder = SinkWriter::with_ledger(&LEDGER, stalled, rt.handle().clone(), SinkChannel::Email);
    let _held = holder.spawn("a", "b", serde_json::json!({}), "held".into(), |_| {}).expect("the one slot");
    let told: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = told.clone();
    let shed = writer.spawn("a", "b", serde_json::json!({}), "t".into(), move |why| {
        *seen.lock().unwrap() = Some(why.to_string());
    });
    assert!(matches!(shed, Err(Unwritten::Shed(_))), "POSITIVE CONTROL: the slot is taken: {shed:?}");
    let told = told.lock().unwrap().clone().expect("the shed row has a line of its own");
    assert!(told.ends_with("; refused row 1 of this burst"), "{told}");
}
