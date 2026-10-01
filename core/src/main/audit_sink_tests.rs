//! `audit_sink`'s own tests. Split out of `audit_sink.rs` to keep it under the
//! 500-LOC soft cap; `#[path]`-included there, so `super::` is `audit_sink`.
//!
//! Every test uses its own `static` [`Ledger`], never the daemon's: tests run
//! in parallel in one process, and a shared ledger's counts would be everyone's.

use super::test_support::{
    assert_insert_attempted, assert_returns_at_once, connected_within, stalled_pool,
    ACQUIRE_TIMEOUT,
};
use super::*;
use kastellan_core::worker_stderr::AuditLostWriter;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `Bounds` in one line, for the statics below.
const fn bounds(queued: usize, connections: usize) -> Bounds {
    Bounds { queued, connections }
}

type OnFailure = Box<dyn FnOnce(Unwritten<'_>) + Send>;

/// Records what `on_failure` was told.
fn recorder() -> (Arc<Mutex<Option<String>>>, impl FnOnce(Unwritten<'_>) + Send + 'static) {
    let told: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = told.clone();
    (told, move |u: Unwritten<'_>| *seen.lock().unwrap() = Some(u.to_string()))
}

/// A runtime and a writer on `ledger` against the stalled pool; the listener
/// is returned so it outlives the test's calls.
fn stalled_writer(
    ledger: &'static Ledger,
) -> (tokio::runtime::Runtime, SinkWriter, std::net::TcpListener) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    // Made inside the runtime, but the test thread stays outside it — like the
    // driver thread that calls the real sinks.
    let (pool, listener) = rt.block_on(async { stalled_pool() });
    let writer = SinkWriter::with_ledger(ledger, pool, rt.handle().clone(), SinkKind::SkippedIds);
    (rt, writer, listener)
}

/// #789: the insert does not hold the caller. The positive control first:
/// the same insert, **waited for** from the same kind of thread (as the
/// email sink did with `block_on`), takes the whole acquire timeout — so
/// the fixture really stalls, and the fast return below is the helper's
/// doing. Then the helper's failure callback does run, once the pool
/// gives up.
#[test]
fn the_insert_does_not_hold_the_calling_thread() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);

    let t = Instant::now();
    let blocked =
        rt.block_on(kastellan_db::audit::insert(&writer.pool, "a", "b", serde_json::json!({})));
    assert!(blocked.is_err(), "POSITIVE CONTROL: the stalled pool must fail the insert");
    assert!(
        t.elapsed() >= ACQUIRE_TIMEOUT,
        "POSITIVE CONTROL: waiting for the insert must take the acquire timeout, or this \
         fixture does not stall and the assertion below proves nothing: {:?}",
        t.elapsed()
    );

    let (told, on_failure) = recorder();
    let mut task = None;
    assert_returns_at_once("SinkWriter::spawn", || {
        task = writer.spawn("a", "b", serde_json::json!({}), "t".into(), on_failure);
    });
    rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
    assert!(told.lock().unwrap().is_some(), "a failed insert reaches `on_failure`");
}

/// The positive control for [`assert_insert_attempted`]: the stalled pool
/// opens no connection of its own accord, so a connection means an insert
/// was tried.
#[test]
fn a_stalled_pool_connects_only_for_an_insert() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let (rt, writer, listener) = stalled_writer(&LEDGER);
    assert!(
        !connected_within(&listener, Duration::from_millis(300)),
        "POSITIVE CONTROL: the lazy pool must not connect before it is used"
    );
    let task = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {});
    assert_insert_attempted("SinkWriter::spawn", &listener);
    rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
}

/// Past the queue bound a row is shed — at once, on the caller's thread,
/// and said so through `on_failure` — and the slot comes back once the
/// queued insert gives up.
#[test]
fn past_the_queue_bound_a_row_is_shed_and_said_so() {
    static ONE_SLOT: Ledger = Ledger::new(bounds(1, 1));
    let (rt, writer, _listener) = stalled_writer(&ONE_SLOT);
    let spawn =
        |on_failure: OnFailure| writer.spawn("a", "b", serde_json::json!({}), "t".into(), on_failure);

    let queued = spawn(Box::new(|_: Unwritten<'_>| {}))
        .expect("POSITIVE CONTROL: the first row fits the queue");
    let (told, on_failure) = recorder();
    assert_returns_at_once("a shed row", || {
        assert!(spawn(Box::new(on_failure)).is_none(), "the second row must be shed");
    });
    let told = told.lock().unwrap().clone();
    assert!(
        told.as_deref().is_some_and(|t| t.starts_with("shed:")),
        "a shed row must reach `on_failure` at once, as shed: {told:?}"
    );
    assert_eq!(ONE_SLOT.snapshot().rows_pending, 1, "a shed row is not left counted as pending");

    rt.block_on(queued).expect("the queued insert ran to completion");
    let again = spawn(Box::new(|_: Unwritten<'_>| {})).expect("the slot comes back");
    rt.block_on(again).expect("the insert task ran to completion");
}

/// At most `connections` inserts use the pool at once, the rest wait: two
/// stalled inserts through one connection take two acquire timeouts, one
/// after the other. The control: through two, they take one.
#[test]
fn inserts_share_a_bounded_number_of_connections() {
    static ONE_CONNECTION: Ledger = Ledger::new(bounds(2, 1));
    static TWO_CONNECTIONS: Ledger = Ledger::new(bounds(2, 2));
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { stalled_pool() });
    let both = |ledger: &'static Ledger| {
        let writer = SinkWriter::with_ledger(ledger, pool.clone(), rt.handle().clone(), SinkKind::SkippedIds);
        let t = Instant::now();
        let tasks: Vec<_> = (0..2)
            .map(|_| writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed"))
            .collect();
        for task in tasks {
            rt.block_on(task).expect("the insert task ran to completion");
        }
        t.elapsed()
    };
    let parallel = both(&TWO_CONNECTIONS);
    assert!(
        parallel < ACQUIRE_TIMEOUT * 2,
        "POSITIVE CONTROL: two inserts through two connections wait side by side: {parallel:?}"
    );
    let serial = both(&ONE_CONNECTION);
    assert!(
        serial >= ACQUIRE_TIMEOUT * 2,
        "two inserts through one connection must wait one after the other: {serial:?}"
    );
}

/// #792: a row is counted from its spawn until its task ends, and a writer
/// is counted while it lives — the two things `drain` waits for.
#[test]
fn a_row_counts_until_its_task_ends_and_a_writer_while_it_lives() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    assert_eq!(LEDGER.snapshot(), Drained { rows_pending: 0, sinks_live: 1, ..Drained::default() });

    let task = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "a spawned row is pending");
    rt.block_on(task).expect("the insert task ran to completion");
    assert_eq!(LEDGER.snapshot().rows_pending, 0, "a finished row is not");

    drop(writer);
    assert_eq!(LEDGER.snapshot(), Drained::default());
}

/// #792: a row whose task is cancelled — the runtime dropped under it, as at
/// the end of `main` — stops counting too, so a later count is not inflated
/// by rows nobody can finish.
#[test]
fn a_cancelled_row_stops_counting() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let _task = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "POSITIVE CONTROL: the row is pending");
    drop(rt);
    assert_eq!(LEDGER.snapshot().rows_pending, 0);
}

/// #792: with nothing in flight, `drain` returns at once and reports nothing.
#[test]
fn a_settled_ledger_drains_at_once() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, Duration::from_secs(10)));
    assert_eq!(d, Drained::default());
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

/// #792: `drain` waits for the last driver to let go of its sink — a Matrix
/// driver audits its queued replies on the way out — and no longer.
#[test]
fn drain_waits_for_the_last_writer_and_no_longer() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let driver = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(writer);
    });
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, Duration::from_secs(10)));
    let took = t.elapsed();
    driver.join().unwrap();
    assert_eq!(d, Drained::default());
    assert!(took >= Duration::from_millis(300), "it must wait for the writer: {took:?}");
    assert!(took < Duration::from_secs(5), "and return once it is gone: {took:?}");
}

/// #792: past its bound, `drain` returns what is still in flight, and closes
/// the ledger: a row tried after that is refused at once and said so, rather
/// than spawned onto a runtime that is going away.
#[test]
fn past_its_bound_drain_counts_what_is_left_and_refuses_later_rows() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let stuck = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");

    let bound = ACQUIRE_TIMEOUT / 3;
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, bound));
    assert!(t.elapsed() >= bound, "it waited its bound: {:?}", t.elapsed());
    assert_eq!(
        (d.rows_pending, d.sinks_live),
        (1, 1),
        "the stalled row and its writer"
    );

    let (told, on_failure) = recorder();
    assert!(writer.spawn("a", "b", serde_json::json!({}), "t".into(), on_failure).is_none());
    let told = told.lock().unwrap().clone();
    assert!(
        told.as_deref().is_some_and(|t| t.starts_with("not tried: the daemon was shutting down")),
        "a row after the drain must be refused and said so: {told:?}"
    );
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "a refused row is not left counted");
    rt.block_on(stuck).expect("the stalled insert ran to completion");
}

// --- #792 / #796 / #797 / #798: what the shutdown says -------------------

/// A [`Reporter`] that records, for the tests that read what `report_drained`
/// says. One static, shared: each test asserts on lines only it can produce.
static SAID: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record(writer: AuditLostWriter, line: &str) {
    SAID.lock().unwrap().push((writer, line.to_string()));
}

fn said_containing(needle: &str) -> Vec<(AuditLostWriter, String)> {
    SAID.lock().unwrap().iter().filter(|(_, l)| l.contains(needle)).cloned().collect()
}

#[test]
fn the_counting_words_agree_with_their_number() {
    assert_eq!(counted(1, "row", "was", "were"), "1 row was");
    assert_eq!(counted(2, "row", "was", "were"), "2 rows were");
    assert_eq!(counted(0, "row", "was", "were"), "0 rows were");
}

#[test]
fn the_shutdown_lines_count_rows_and_drivers_and_are_silent_at_zero() {
    let bound = Duration::from_secs(3);
    let none = Drained::default();
    assert_eq!(format_pending_at_shutdown(&none, bound), None);
    assert_eq!(format_stuck_replies_at_shutdown(&none, bound), None);
    assert_eq!(format_live_at_shutdown(&none, bound), None);
    assert_eq!(format_unreported_at_shutdown(&none), None);

    let one = Drained { rows_pending: 1, sinks_live: 1, ..Drained::default() };
    assert_eq!(
        format_pending_at_shutdown(&one, bound).unwrap(),
        "1 channel audit row was still unwritten after waiting 3 s at shutdown; the database \
         pool closes next, so it is lost unless already mid-write"
    );
    assert_eq!(
        format_live_at_shutdown(&one, bound).unwrap(),
        "1 channel driver had not exited after 3 s at shutdown; an audit row one writes from \
         now on is refused and reported on the [audit-lost] marker"
    );
    let many = Drained { rows_pending: 2, sinks_live: 2, ..Drained::default() };
    assert!(format_pending_at_shutdown(&many, bound).unwrap().starts_with("2 channel audit rows were"));
    assert!(format_pending_at_shutdown(&many, bound).unwrap().contains("so they are lost"));
    assert!(format_live_at_shutdown(&many, bound).unwrap().starts_with("2 channel drivers had"));
}

/// #797: the line names the rows it can, and counts the ones it cannot.
#[test]
fn the_pending_line_names_the_rows_it_knows_and_counts_the_rest() {
    let bound = Duration::from_secs(3);
    let d = Drained {
        rows_pending: 7,
        named: vec!["email skipped message <a@h>".into(), "matrix reply to !r (gave_up)".into()],
        ..Drained::default()
    };
    let line = format_pending_at_shutdown(&d, bound).unwrap();
    assert!(
        line.contains("(email skipped message <a@h>; matrix reply to !r (gave_up); and 5 more)"),
        "{line}"
    );
}

/// #796: a Matrix driver still running is a loss, said as one; an email driver
/// still running is not, and stays at INFO.
#[test]
fn a_stuck_matrix_driver_is_a_loss_and_a_stuck_email_driver_is_not() {
    let bound = Duration::from_secs(3);
    let matrix = Drained { sinks_live: 1, replies_live: 1, ..Drained::default() };
    let line = format_stuck_replies_at_shutdown(&matrix, bound).unwrap();
    assert!(line.contains("not audited as `channel.reply_undelivered`"), "{line}");
    assert_eq!(format_live_at_shutdown(&matrix, bound), None, "not also an INFO line");

    let email = Drained { sinks_live: 1, replies_live: 0, ..Drained::default() };
    assert_eq!(format_stuck_replies_at_shutdown(&email, bound), None);
    assert!(format_live_at_shutdown(&email, bound).is_some());

    let both = Drained { sinks_live: 3, replies_live: 1, ..Drained::default() };
    assert!(format_live_at_shutdown(&both, bound).unwrap().starts_with("2 channel drivers had"));
}

#[test]
fn the_ledger_counts_live_replies_sinks_apart_from_the_rest() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { stalled_pool() });
    let mk = |kind| SinkWriter::with_ledger(&LEDGER, pool.clone(), rt.handle().clone(), kind);
    let (m, e) = (mk(SinkKind::Replies), mk(SinkKind::SkippedIds));
    assert_eq!((LEDGER.snapshot().sinks_live, LEDGER.snapshot().replies_live), (2, 1));
    drop(m);
    assert_eq!((LEDGER.snapshot().sinks_live, LEDGER.snapshot().replies_live), (1, 0));
    drop(e);
    assert_eq!(LEDGER.snapshot(), Drained::default());
}

/// `report_drained` sends each loss to the reporter under the shutdown writer,
/// and nothing for a clean drain.
#[test]
fn report_drained_reports_each_loss_through_the_reporter() {
    report_drained(&Drained::default(), record);
    let d = Drained {
        rows_pending: 1,
        sinks_live: 1,
        replies_live: 1,
        named: vec!["REPORT-DRAINED-ROW".into()],
        unreported: 4,
    };
    report_drained(&d, record);
    for needle in ["REPORT-DRAINED-ROW", "channel driver had not exited", "without a line of its own"] {
        let said = said_containing(needle);
        assert_eq!(said.len(), 1, "{needle}: {said:?}");
        assert_eq!(said[0].0, AuditLostWriter::Shutdown);
    }
}

/// #798: a flood of refused rows reports its first few, then thins out.
#[test]
fn refused_rows_are_reported_one_by_one_then_thinned() {
    assert!((0..REPORT_EACH_UP_TO).all(should_report), "the first rows are each reported");
    assert!(should_report(16) && should_report(32) && should_report(1024), "then powers of two");
    assert!(!should_report(17) && !should_report(33) && !should_report(1000));
    // The census's own count agrees with the predicate.
    static LEDGER: Ledger = Ledger::new(bounds(0, 1));
    let (_rt, writer, _listener) = stalled_writer(&LEDGER);
    let told = Arc::new(Mutex::new(0usize));
    for _ in 0..20 {
        let told = told.clone();
        writer.spawn("a", "b", serde_json::json!({}), "t".into(), move |_| *told.lock().unwrap() += 1);
    }
    let reported = *told.lock().unwrap();
    assert_eq!(reported, 17, "16 each, then n = 16 only (n = 17..19 thinned out)");
    assert_eq!(LEDGER.final_snapshot().unreported, 20 - reported);
}

/// #797: the final snapshot names pending rows — a cancelled task cannot —
/// and a finished row drops off the list.
#[test]
fn the_final_snapshot_names_pending_rows_up_to_a_few() {
    static LEDGER: Ledger = Ledger::new(bounds(32, 8));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let tasks: Vec<_> = (0..7)
        .map(|i| {
            writer
                .spawn("a", "b", serde_json::json!({}), format!("row-{i}"), |_| {})
                .expect("not shed")
        })
        .collect();
    let d = LEDGER.final_snapshot();
    assert_eq!(d.rows_pending, 7);
    assert_eq!(d.named.len(), NAMED_AT_SHUTDOWN, "only a few are named");
    assert!(d.named.iter().all(|n| n.starts_with("row-")), "{:?}", d.named);
    for t in tasks {
        rt.block_on(t).expect("the insert task ran to completion");
    }
    assert_eq!(LEDGER.final_snapshot().named, Vec::<String>::new(), "finished rows are not named");
}

/// #799: `drain` waits for a pending row even with no live sink — the bound
/// is what ends the wait. (`settled` ignoring `rows_pending` passed every
/// other test.)
#[test]
fn drain_waits_for_a_pending_row_with_no_live_sink() {
    static LEDGER: Ledger = Ledger::new(bounds(8, 8));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let stuck = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");
    drop(writer);
    assert_eq!(LEDGER.snapshot().sinks_live, 0, "POSITIVE CONTROL: no sink is live");

    let bound = ACQUIRE_TIMEOUT / 3;
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, bound));
    assert!(t.elapsed() >= bound, "it must wait on the row alone: {:?}", t.elapsed());
    assert_eq!(d.rows_pending, 1);
    rt.block_on(stuck).expect("the stalled insert ran to completion");
}

/// #799: the close-versus-spawn handshake under real contention. Every spawn
/// that was NOT refused must be counted by the final snapshot — a row counted
/// by neither would be lost with no line. The pool is stalled, so no row
/// finishes (and none can leave the count) during the test.
#[test]
fn a_row_spawned_while_the_drain_closes_is_refused_or_counted() {
    static LEDGER: Ledger = Ledger::new(bounds(100_000, 4));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let writer = Arc::new(writer);
    let accepted = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (writer, accepted) = (writer.clone(), accepted.clone());
            std::thread::spawn(move || loop {
                match writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}) {
                    Some(_) => accepted.fetch_add(1, Ordering::SeqCst),
                    None => return,
                };
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(20));
    let d = rt.block_on(drain_ledger(&LEDGER, Duration::ZERO));
    for t in threads {
        t.join().unwrap();
    }
    assert!(accepted.load(Ordering::SeqCst) > 0, "POSITIVE CONTROL: the threads spawned rows");
    // At LEAST every accepted row is counted. A thread that had counted a row
    // and was about to be refused (it reads `closed` after counting) can be
    // caught mid-refusal by the snapshot, so up to one extra per thread is
    // counted — over-counting is the safe direction; under-counting is the bug.
    let accepted = accepted.load(Ordering::SeqCst);
    assert!(d.rows_pending >= accepted, "every accepted row is counted: {d:?} vs {accepted}");
    assert!(d.rows_pending <= accepted + 4, "only rows caught mid-refusal are extra: {d:?} vs {accepted}");
}
