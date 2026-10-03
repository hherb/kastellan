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
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `Bounds` in one line, for the statics below.
pub(super) const fn bounds(queued: usize, connections: usize) -> Bounds {
    Bounds { queued, connections }
}


type OnFailure = Box<dyn FnOnce(Unwritten<'_>) + Send>;

/// A drain bound no test is meant to reach. A test that expects `drain` to
/// return early asserts it took under half of this: a wrong wait takes all of
/// it, and half a minute is room for any loaded host (#799 — the old
/// thresholds were 1 s and 5 s).
const NEVER: Duration = Duration::from_secs(60);

/// Records what `on_failure` was told.
fn recorder() -> (Arc<Mutex<Option<String>>>, impl FnOnce(Unwritten<'_>) + Send + 'static) {
    let told: Arc<Mutex<Option<String>>> = Arc::default();
    let seen = told.clone();
    (told, move |u: Unwritten<'_>| *seen.lock().unwrap() = Some(u.to_string()))
}

/// A runtime and a writer on `ledger` against the stalled pool; the listener
/// is returned so it outlives the test's calls.
pub(super) fn stalled_writer(
    ledger: &'static Ledger,
) -> (tokio::runtime::Runtime, SinkWriter, std::net::TcpListener) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    // Made inside the runtime, but the test thread stays outside it — like the
    // driver thread that calls the real sinks.
    let (pool, listener) = rt.block_on(async { stalled_pool() });
    let writer = SinkWriter::with_ledger(ledger, pool, rt.handle().clone(), SinkChannel::Email);
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
        task = Some(writer.spawn("a", "b", serde_json::json!({}), "t".into(), on_failure));
    });
    let task = task.expect("spawn was called").expect("not shed");
    rt.block_on(task).expect("the insert task ran to completion");
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
        assert!(matches!(spawn(Box::new(on_failure)), Err(Unwritten::Shed(_))), "the second row must be shed");
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
        let writer = SinkWriter::with_ledger(ledger, pool.clone(), rt.handle().clone(), SinkChannel::Email);
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
    assert_eq!(LEDGER.snapshot(), InFlight { sinks_live: 1, ..InFlight::default() });

    let task = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "a spawned row is pending");
    rt.block_on(task).expect("the insert task ran to completion");
    assert_eq!(LEDGER.snapshot().rows_pending, 0, "a finished row is not");

    drop(writer);
    assert_eq!(LEDGER.snapshot(), InFlight::default());
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
    let d = rt.block_on(drain_ledger(&LEDGER, NEVER));
    assert_eq!(d, Drained::default());
    assert!(t.elapsed() < NEVER / 2, "{:?}", t.elapsed());
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
    let d = rt.block_on(drain_ledger(&LEDGER, NEVER));
    let took = t.elapsed();
    driver.join().unwrap();
    assert_eq!(d, Drained::default());
    assert!(took >= Duration::from_millis(300), "it must wait for the writer: {took:?}");
    assert!(took < NEVER / 2, "and return once it is gone: {took:?}");
}

/// #792: past its bound, `drain` returns what is still in flight, and closes
/// the ledger: a row tried after that is refused at once and said so, rather
/// than spawned onto a runtime that is going away.
#[test]
fn past_its_bound_drain_counts_what_is_left_and_refuses_later_rows() {
    // No connection is ever free, so the stalled row stays pending however
    // slow the host is (it would otherwise fail after the pool's timeout).
    static LEDGER: Ledger = Ledger::new(bounds(8, 0));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let stuck = writer
        .spawn("a", "b", serde_json::json!({}), "STUCK-ROW".into(), |_| {})
        .expect("not shed");
    let _matrix = SinkWriter::with_ledger(&LEDGER, writer.pool.clone(), rt.handle().clone(), SinkChannel::Matrix);

    let bound = ACQUIRE_TIMEOUT / 3;
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, bound));
    assert!(t.elapsed() >= bound, "it waited its bound: {:?}", t.elapsed());
    assert_eq!(
        (d.in_flight.rows_pending, d.in_flight.sinks_live, d.in_flight.auditing_live),
        (1, 2, 1),
        "the stalled row, its writer, and a Matrix one"
    );
    // `drain` returns the FINAL snapshot: the names and the thinned count
    // reach `report_drained` (a `snapshot()` there would leave them empty).
    assert_eq!(d.named, vec!["STUCK-ROW".to_string()]);
    assert_eq!(d.unreported, Unreported::default());

    let (told, on_failure) = recorder();
    assert!(matches!(
        writer.spawn("a", "b", serde_json::json!({}), "t".into(), on_failure),
        Err(Unwritten::AfterShutdown(_))
    ));
    let told = told.lock().unwrap().clone();
    assert!(
        told.as_deref().is_some_and(|t| t.starts_with("not tried: the daemon was shutting down")),
        "a row after the drain must be refused and said so: {told:?}"
    );
    assert_eq!(LEDGER.snapshot().rows_pending, 1, "a refused row is not left counted");
    drop(stuck); // it waits for a connection forever; it ends with the runtime
}

/// #798: a flood of rows refused through a sink reports its first few, then
/// thins out — the ledger wired to `audit_sink_thinning.rs`, whose own tests
/// pin the predicate.
#[test]
fn a_flood_of_shed_rows_is_reported_one_by_one_then_thinned() {
    static LEDGER: Ledger = Ledger::new(bounds(0, 1));
    let (_rt, writer, _listener) = stalled_writer(&LEDGER);
    let told = Arc::new(Mutex::new(0usize));
    for _ in 0..20 {
        let told = told.clone();
        let _ = writer.spawn("a", "b", serde_json::json!({}), "t".into(), move |_| *told.lock().unwrap() += 1);
    }
    let reported = *told.lock().unwrap();
    assert_eq!(reported, 17, "n = 0..15 each, n = 16 (a power of two); n = 17..19 thinned out");
    assert_eq!(LEDGER.final_snapshot().unreported.email, UnsaidCounts { shed: 3, late: 0 });
    assert_eq!(LEDGER.final_snapshot().unreported.matrix, UnsaidCounts::default(), "email's flood is email's");
    assert!(LEDGER.final_snapshot().named.is_empty(), "a refused row leaves no label behind");
}

/// #807: the ledger thins each channel on its own. A Matrix row shed in the
/// middle of an email flood still gets a line of its own, naming its place —
/// before #807 it was row 101 of one shared burst, and said nothing.
#[test]
fn an_email_flood_does_not_thin_a_matrix_rows_line() {
    static LEDGER: Ledger = Ledger::new(bounds(0, 1));
    let (rt, email, _listener) = stalled_writer(&LEDGER);
    let matrix = SinkWriter::with_ledger(&LEDGER, email.pool.clone(), rt.handle().clone(), SinkChannel::Matrix);
    let flood = Arc::new(AtomicUsize::new(0));
    for _ in 0..100 {
        let flood = flood.clone();
        let _ = email.spawn("a", "b", serde_json::json!({}), "t".into(), move |_| {
            flood.fetch_add(1, Ordering::SeqCst);
        });
    }
    assert!(flood.load(Ordering::SeqCst) < 100, "POSITIVE CONTROL: the email flood is thinned");
    let (told, on_failure) = recorder();
    let _ = matrix.spawn("a", "b", serde_json::json!({}), "t".into(), on_failure);
    let told = told.lock().unwrap().clone();
    assert!(
        told.as_deref().is_some_and(|t| t.ends_with("; refused row 1 of this burst")),
        "the Matrix row is its own burst's first, and reported: {told:?}"
    );
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
                .spawn("a", "b", serde_json::json!({}), format!("row-{i}").as_str().into(), |_| {})
                .expect("not shed")
        })
        .collect();
    let d = LEDGER.final_snapshot();
    assert_eq!(d.in_flight.rows_pending, 7);
    assert_eq!(d.named.len(), NAMED_AT_SHUTDOWN, "only a few are named");
    assert_eq!(d.named, ["row-0", "row-1", "row-2", "row-3", "row-4"], "the oldest, in order");
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
    // No connection is ever free: the row stays pending (see above).
    static LEDGER: Ledger = Ledger::new(bounds(8, 0));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let stuck = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}).expect("not shed");
    drop(writer);
    assert_eq!(LEDGER.snapshot().sinks_live, 0, "POSITIVE CONTROL: no sink is live");

    let bound = ACQUIRE_TIMEOUT / 3;
    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, bound));
    assert!(t.elapsed() >= bound, "it must wait on the row alone: {:?}", t.elapsed());
    assert_eq!(d.in_flight.rows_pending, 1);
    drop(stuck);
}

/// #799: the close-versus-spawn handshake under real contention. Every spawn
/// that was NOT refused must be counted by the final snapshot — a row counted
/// by neither would be lost with no line. No connection is ever free, so no
/// row finishes (and none can leave the count) during the test.
#[test]
fn a_row_spawned_while_the_drain_closes_is_refused_or_counted() {
    // No connection is ever free, so no accepted row can finish and leave the
    // count before the snapshot, however long the threads take.
    static LEDGER: Ledger = Ledger::new(bounds(100_000, 0));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let writer = Arc::new(writer);
    let accepted = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (writer, accepted) = (writer.clone(), accepted.clone());
            std::thread::spawn(move || loop {
                match writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}) {
                    Ok(_) => accepted.fetch_add(1, Ordering::SeqCst),
                    Err(_) => return,
                };
            })
        })
        .collect();
    // Close only once rows are being spawned — waited for, not slept for, so
    // a slow host cannot void the positive control below (#802).
    let deadline = Instant::now() + NEVER;
    while accepted.load(Ordering::SeqCst) < 100 && Instant::now() < deadline {
        std::thread::yield_now();
    }
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
    let pending = d.in_flight.rows_pending;
    assert!(pending >= accepted, "every accepted row is counted: {d:?} vs {accepted}");
    assert!(pending <= accepted + 4, "only rows caught mid-refusal are extra: {d:?} vs {accepted}");
}

/// #802: rows refused after the drain, past the thinning, were counted
/// nowhere — the shutdown line was already written. They are counted since
/// it, for the daemon's last line.
#[test]
fn rows_thinned_after_the_drain_are_counted_for_the_last_line() {
    // No queue room: every row before the drain is shed.
    static LEDGER: Ledger = Ledger::new(bounds(0, 1));
    let (rt, writer, _listener) = stalled_writer(&LEDGER);
    let refuse_20 = || {
        let told = Arc::new(AtomicUsize::new(0));
        for _ in 0..20 {
            let told = told.clone();
            let _ = writer.spawn("a", "b", serde_json::json!({}), "t".into(), move |_| {
                told.fetch_add(1, Ordering::SeqCst);
            });
        }
        told.load(Ordering::SeqCst)
    };
    assert_eq!(refuse_20(), 17, "POSITIVE CONTROL: 20 shed, 0..16 and 16 reported");
    let d = rt.block_on(drain_ledger(&LEDGER, Duration::ZERO));
    assert_eq!(d.unreported.total(), 3, "the shed rows are the drain line's");
    assert_eq!(unreported_on_since(&LEDGER, &d), 0, "so none is counted twice");
    assert_eq!(refuse_20(), 17, "20 refused after the drain, a burst of their own");
    assert_eq!(unreported_on_since(&LEDGER, &d), 3, "only the late ones");
}

/// What the last lines said, for the test below.
static LAST_SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_last(_: kastellan_core::worker_stderr::AuditLostWriter, line: &str) {
    LAST_SAID.lock().unwrap().push(line.to_string());
}

/// #806 review: the last word is said BEFORE the close as well as after it.
/// A close that never returns — sqlx's `Pool::close` waiting on an insert
/// stuck in a wedged Postgres — cannot swallow the rows thinned since the
/// drain; rows thinned while the close runs get a line of their own, and none
/// is counted in both.
#[test]
fn the_last_line_is_said_before_a_close_that_may_never_return() {
    static NEVER_CLOSES: Ledger = Ledger::new(bounds(0, 1));
    static CLOSES: Ledger = Ledger::new(bounds(0, 1));
    let refuse_20 = |writer: &SinkWriter| {
        for _ in 0..20 {
            let _ = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {});
        }
    };

    let (rt, writer, _listener) = stalled_writer(&NEVER_CLOSES);
    let d = rt.block_on(drain_ledger(&NEVER_CLOSES, Duration::ZERO));
    refuse_20(&writer);
    let close = report::close_on_then_report(&NEVER_CLOSES, &d, std::future::pending(), record_last);
    let hung = rt.block_on(async { tokio::time::timeout(Duration::from_millis(50), close).await });
    assert!(hung.is_err(), "POSITIVE CONTROL: the close never returned");
    let said = |needle: &str| LAST_SAID.lock().unwrap().iter().filter(|l| l.contains(needle)).count();
    assert_eq!(said("3 more channel audit rows were refused"), 1, "{:?}", LAST_SAID.lock().unwrap());

    let (rt, writer, _listener) = stalled_writer(&CLOSES);
    let d = rt.block_on(drain_ledger(&CLOSES, Duration::ZERO));
    for _ in 0..5 {
        refuse_20(&writer); // 100 late rows, 0..16 and 16, 32, 64 reported: 81 thinned
    }
    let close = async { refuse_20(&writer) }; // 20 more, 0 reported: 101 thinned
    rt.block_on(report::close_on_then_report(&CLOSES, &d, close, record_last));
    assert_eq!(said("81 more channel audit rows were refused"), 1, "{:?}", LAST_SAID.lock().unwrap());
    assert_eq!(said("20 more channel audit rows were refused"), 1, "{:?}", LAST_SAID.lock().unwrap());
}
