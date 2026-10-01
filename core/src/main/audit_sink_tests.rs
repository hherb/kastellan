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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    let writer = SinkWriter::with_ledger(ledger, pool, rt.handle().clone());
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
    static LEDGER: Ledger = Ledger::new(8, 8);
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
        task = writer.spawn("a", "b", serde_json::json!({}), on_failure);
    });
    rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
    assert!(told.lock().unwrap().is_some(), "a failed insert reaches `on_failure`");
}

/// The positive control for [`assert_insert_attempted`]: the stalled pool
/// opens no connection of its own accord, so a connection means an insert
/// was tried.
#[test]
fn a_stalled_pool_connects_only_for_an_insert() {
    static LEDGER: Ledger = Ledger::new(8, 8);
    let (rt, writer, listener) = stalled_writer(&LEDGER);
    assert!(
        !connected_within(&listener, Duration::from_millis(300)),
        "POSITIVE CONTROL: the lazy pool must not connect before it is used"
    );
    let task = writer.spawn("a", "b", serde_json::json!({}), |_| {});
    assert_insert_attempted("SinkWriter::spawn", &listener);
    rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
}

/// Past the queue bound a row is shed — at once, on the caller's thread,
/// and said so through `on_failure` — and the slot comes back once the
/// queued insert gives up.
#[test]
fn past_the_queue_bound_a_row_is_shed_and_said_so() {
    static ONE_SLOT: Ledger = Ledger::new(1, 1);
    let (rt, writer, _listener) = stalled_writer(&ONE_SLOT);
    let spawn =
        |on_failure: OnFailure| writer.spawn("a", "b", serde_json::json!({}), on_failure);

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
    static ONE_CONNECTION: Ledger = Ledger::new(2, 1);
    static TWO_CONNECTIONS: Ledger = Ledger::new(2, 2);
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { stalled_pool() });
    let both = |ledger: &'static Ledger| {
        let writer = SinkWriter::with_ledger(ledger, pool.clone(), rt.handle().clone());
        let t = Instant::now();
        let tasks: Vec<_> = (0..2)
            .map(|_| writer.spawn("a", "b", serde_json::json!({}), |_| {}).expect("not shed"))
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
    static LEDGER: Ledger = Ledger::new(8, 8);
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    assert_eq!(LEDGER.snapshot(), Drained { rows_pending: 0, sinks_live: 1 });

    let task = writer.spawn("a", "b", serde_json::json!({}), |_| {}).expect("not shed");
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "a spawned row is pending");
    rt.block_on(task).expect("the insert task ran to completion");
    assert_eq!(LEDGER.snapshot().rows_pending, 0, "a finished row is not");

    drop(writer);
    assert_eq!(LEDGER.snapshot(), Drained { rows_pending: 0, sinks_live: 0 });
}

/// #792: a row whose task is cancelled — the runtime dropped under it, as at
/// the end of `main` — stops counting too, so a later count is not inflated
/// by rows nobody can finish.
#[test]
fn a_cancelled_row_stops_counting() {
    static LEDGER: Ledger = Ledger::new(8, 8);
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let _task = writer.spawn("a", "b", serde_json::json!({}), |_| {}).expect("not shed");
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "POSITIVE CONTROL: the row is pending");
    drop(rt);
    assert_eq!(LEDGER.snapshot().rows_pending, 0);
}

/// #792: with nothing in flight, `drain` returns at once and reports nothing.
#[test]
fn a_settled_ledger_drains_at_once() {
    static LEDGER: Ledger = Ledger::new(8, 8);
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, Duration::from_secs(10)));
    assert_eq!(d, Drained { rows_pending: 0, sinks_live: 0 });
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
}

/// #792: `drain` waits for the last driver to let go of its sink — a Matrix
/// driver audits its queued replies on the way out — and no longer.
#[test]
fn drain_waits_for_the_last_writer_and_no_longer() {
    static LEDGER: Ledger = Ledger::new(8, 8);
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let driver = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(writer);
    });
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, Duration::from_secs(10)));
    let took = t.elapsed();
    driver.join().unwrap();
    assert_eq!(d, Drained { rows_pending: 0, sinks_live: 0 });
    assert!(took >= Duration::from_millis(300), "it must wait for the writer: {took:?}");
    assert!(took < Duration::from_secs(5), "and return once it is gone: {took:?}");
}

/// #792: past its bound, `drain` returns what is still in flight, and closes
/// the ledger: a row tried after that is refused at once and said so, rather
/// than spawned onto a runtime that is going away.
#[test]
fn past_its_bound_drain_counts_what_is_left_and_refuses_later_rows() {
    static LEDGER: Ledger = Ledger::new(8, 8);
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let stuck = writer.spawn("a", "b", serde_json::json!({}), |_| {}).expect("not shed");

    let bound = ACQUIRE_TIMEOUT / 3;
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, bound));
    assert!(t.elapsed() >= bound, "it waited its bound: {:?}", t.elapsed());
    assert_eq!(d, Drained { rows_pending: 1, sinks_live: 1 }, "the stalled row and its writer");

    let (told, on_failure) = recorder();
    assert!(writer.spawn("a", "b", serde_json::json!({}), on_failure).is_none());
    let told = told.lock().unwrap().clone();
    assert!(
        told.as_deref().is_some_and(|t| t.starts_with("not tried: the daemon was shutting down")),
        "a row after the drain must be refused and said so: {told:?}"
    );
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "a refused row is not left counted");
    rt.block_on(stuck).expect("the stalled insert ran to completion");
}

#[test]
fn the_shutdown_lines_count_rows_and_drivers_and_are_silent_at_zero() {
    let bound = Duration::from_secs(3);
    let none = Drained { rows_pending: 0, sinks_live: 0 };
    assert_eq!(format_pending_at_shutdown(none, bound), None);
    assert_eq!(format_live_at_shutdown(none, bound), None);

    let one = Drained { rows_pending: 1, sinks_live: 1 };
    assert_eq!(
        format_pending_at_shutdown(one, bound).unwrap(),
        "1 channel audit row was still unwritten after waiting 3 s at shutdown; the database \
         pool closes next, so it is lost unless already mid-write"
    );
    assert_eq!(
        format_live_at_shutdown(one, bound).unwrap(),
        "1 channel driver had not exited after 3 s at shutdown; an audit row one writes from \
         now on is refused and reported on the [audit-lost] marker"
    );
    let many = Drained { rows_pending: 2, sinks_live: 2 };
    assert!(format_pending_at_shutdown(many, bound).unwrap().starts_with("2 channel audit rows were"));
    assert!(format_pending_at_shutdown(many, bound).unwrap().contains("so they are lost"));
    assert!(format_live_at_shutdown(many, bound).unwrap().starts_with("2 channel drivers had"));
}
