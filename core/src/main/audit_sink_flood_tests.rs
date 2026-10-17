//! `audit_sink`'s tests for floods of lost rows — shed (#798, #807) and
//! failed inserts (#858, #861) — and for a failed insert's place in the
//! shutdown lines. Split from `audit_sink_tests.rs` to keep both under the
//! 500-LOC soft cap; `#[path]`-included in `audit_sink.rs`, so `super::` is
//! `audit_sink`.
//!
//! As there, every test uses its own `static` [`Ledger`].

use super::test_support::{closing_pool, stalled_pool};
use super::tests::{bounds, stalled_writer};
use super::*;
use kastellan_core::audit_thinning::unreported_of;
use kastellan_core::worker_stderr::AuditLostWriter;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// #798: a flood of rows refused through a sink reports its first few, then
/// thins out — the ledger wired to `audit_sink_thinning.rs`, whose predicate
/// `kastellan_core::audit_thinning`'s tests pin. #807: each line it lets
/// through says where the row stands in its burst, so the flood's size is in
/// the lines themselves.
#[test]
fn a_flood_of_shed_rows_is_reported_one_by_one_then_thinned() {
    static LEDGER: Ledger = Ledger::new(bounds(0, 1));
    let (_rt, writer, _listener) = stalled_writer(&LEDGER);
    let told: Arc<Mutex<Vec<String>>> = Arc::default();
    for _ in 0..33 {
        let told = told.clone();
        let _ = writer.spawn("a", "b", serde_json::json!({}), "t".into(), move |why| {
            told.lock().unwrap().push(why.to_string());
        });
    }
    let told = told.lock().unwrap().clone();
    assert_eq!(told.len(), 18, "n = 0..15 each, then n = 16 and 32 (powers of two); the rest thinned out");
    assert!(told[16].ends_with("; row 17 of this burst"), "{:?}", told[16]);
    assert!(
        told[17].ends_with("; row 33 of this burst (15 before it had no line of their own)"),
        "{:?}",
        told[17]
    );
    assert_eq!(LEDGER.final_snapshot().unreported.email, UnsaidCounts { shed: 15, late: 0, failed_not_written: 0, failed_unknown: 0 });
    assert_eq!(LEDGER.final_snapshot().unreported.matrix, UnsaidCounts::default(), "email's flood is email's");
    assert!(LEDGER.final_snapshot().named.is_empty(), "a refused row leaves no label behind");
}


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
/// shutdown line. #858: each line says the row *may* not have been written:
/// the fixture's connection closes before any statement is sent, but the
/// `Io` error it gives is the one a connection lost mid-statement gives too.
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
                told.lock().unwrap().push(why.said());
            })
            .expect("never shed: one row at a time");
        rt.block_on(task).expect("the insert task ran to completion");
    }
    let told = told.lock().unwrap().clone();
    assert_eq!(told.len(), 18, "n = 0..15 each, then n = 16 and 32 (powers of two); the rest thinned out");
    assert!(told.iter().all(|l| l.starts_with("may not have been written: ")), "{told:#?}");
    assert!(told[16].ends_with("; row 17 of this burst"), "{:?}", told[16]);
    assert!(
        told[17].ends_with("; row 33 of this burst (15 before it had no line of their own)"),
        "{:?}",
        told[17]
    );
    let unreported = LEDGER.final_snapshot().unreported;
    assert_eq!(unreported.email, UnsaidCounts { failed_unknown: 15, ..UnsaidCounts::default() });
    assert_eq!(unreported.matrix, UnsaidCounts::default(), "email's flood is email's");
    let line = report::format_unreported_at_shutdown(unreported.email).expect("a shutdown line");
    assert!(line.contains("15 failed to insert over the daemon's life, may have been written"), "{line}");
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
    let (stalled, _listener) = rt.block_on(async { stalled_pool() });
    let holder = SinkWriter::with_ledger(&LEDGER, stalled, rt.handle().clone(), SinkChannel::Email);
    let _held = holder.spawn("a", "b", serde_json::json!({}), "held".into(), |_| {}).expect("the one slot");
    let told: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = told.clone();
    let shed = writer.spawn("a", "b", serde_json::json!({}), "t".into(), move |why| {
        *seen.lock().unwrap() = Some(why.to_string());
    });
    assert!(matches!(shed, Err(Unwritten::Shed(_))), "POSITIVE CONTROL: the slot is taken: {shed:?}");
    let told = told.lock().unwrap().clone().expect("the shed row has a line of its own");
    assert!(told.ends_with("; row 1 of this burst"), "{told}");
}

/// #837, #865: the writer counts a row under its own action. A flood of one
/// action's failed inserts leaves another action's first failure on the same
/// channel a line of its own — the row a dropped action would have thinned.
#[test]
fn a_flood_of_one_actions_failed_inserts_does_not_thin_anothers_line() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 1));
    let (rt, writer) = closing_writer(&LEDGER);
    for _ in 0..40 {
        let task = writer.spawn("a", "channel.inbound_dropped", serde_json::json!({}), "t".into(), |_| {});
        rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
    }
    let told: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = told.clone();
    let task = writer.spawn("a", "channel.reply_undelivered", serde_json::json!({}), "t".into(), move |why| {
        *seen.lock().unwrap() = Some(why.to_string());
    });
    rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
    let told = told.lock().unwrap().clone().expect("the other action's row has a line of its own");
    assert!(told.ends_with("; row 1 of this burst"), "{told}");
    assert_eq!(LEDGER.unreported().email.failed_unknown, unreported_of(40), "the channel sums its actions");
}

/// What [`record_last`] was told, with whose line it was.
static LAST_SAID: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_last(writer: AuditLostWriter, line: &str) {
    LAST_SAID.lock().unwrap().push((writer, line.to_string()));
}

/// #861: a row still pending at the drain's snapshot whose insert then fails
/// past the thinning is counted for the daemon's last line, under its own
/// channel's writer — not lost between the shutdown report (which named it
/// pending) and the process's exit.
///
/// How the row is held pending: the ledger has one connection, and a Matrix
/// row against the stalled pool holds it for the pool's acquire timeout. The
/// email row behind it waits for that connection through the drain, then
/// fails at once against the closing pool — its burst's 18th, thinned (rows
/// 1–17 got lines). The Matrix row's own failure is its burst's first, so it
/// gets a line and adds nothing to the count.
#[test]
fn a_failed_insert_pending_at_the_drain_is_counted_in_the_last_line() {
    static LEDGER: Ledger = Ledger::new(bounds(64, 1));
    let (rt, email) = closing_writer(&LEDGER);
    for _ in 0..17 {
        let task = email.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");
        rt.block_on(task).expect("the insert task ran to completion");
    }
    assert_eq!(LEDGER.unreported().email.failed_unknown, 0, "POSITIVE CONTROL: rows 1-17 all got lines");

    let (stalled, _listener) = rt.block_on(async { stalled_pool() });
    let holder = SinkWriter::with_ledger(&LEDGER, stalled, rt.handle().clone(), SinkChannel::Matrix);
    let held = holder.spawn("a", "b", serde_json::json!({}), "held".into(), |_| {}).expect("not shed");
    let behind = email.spawn("a", "b", serde_json::json!({}), "behind".into(), |_| {}).expect("not shed");

    let d = rt.block_on(drain_ledger(&LEDGER, Duration::ZERO));
    assert_eq!(d.in_flight.rows_pending, 2, "POSITIVE CONTROL: both rows are pending at the snapshot");
    let close = async {
        behind.await.expect("the email row's task ran");
        held.await.expect("the matrix row's task ran");
    };
    rt.block_on(report::close_on_then_report(&LEDGER, &d, close, record_last));

    let said = LAST_SAID.lock().unwrap().clone();
    assert_eq!(
        said,
        [(
            AuditLostWriter::Email,
            "1 more channel audit row had no line of its own after the shutdown report (a driver \
             still running past the drain, or an insert still pending at it): 1 failed to \
             insert, may have been written (the connection was lost mid-insert: look for them \
             in audit_log)"
                .to_string()
        )],
        "said once, after the close, as email's — and nothing for Matrix's reported row"
    );
}
