//! The lease's own tests (#792, #796, #799, #802): what a writer counts, when
//! a starting one starts to count, and the read order [`Ledger::snapshot`]
//! depends on. `#[path]`-included by `audit_sink_lease.rs`, so `super::` is
//! the lease module and `crate::audit_sink` the ledger's.
//!
//! Every test uses its own `static` [`Ledger`] (or a leaked one), never the
//! daemon's: tests run in parallel in one process.

use super::*;
use crate::audit_sink::test_support::stalled_pool;
use crate::audit_sink::{
    close_and_count, drain_ledger, report_drained, Bounds, InFlight, PendingRow, Unwritten,
};
use kastellan_core::worker_stderr::AuditLostWriter;
use std::time::{Duration, Instant};

/// See `audit_sink_tests.rs`'s `NEVER`: a bound no test is meant to reach.
const NEVER: Duration = Duration::from_secs(60);

/// A runtime and a stalled pool, the pool made inside the runtime (it spawns
/// its maintenance task as it is made). The listener outlives the test's calls.
fn runtime_and_pool() -> (tokio::runtime::Runtime, PgPool, std::net::TcpListener) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, listener) = rt.block_on(async { stalled_pool() });
    (rt, pool, listener)
}

/// `SinkChannel::ALL` is hand-kept, and the shutdown lines say only the
/// channels in it: a variant left out would be a channel whose thinned rows
/// are never said. The `match` has no wildcard, so a new variant fails to
/// compile here until it has an arm — the prompt to list it in this loop,
/// where the assert checks `ALL` has it too. A tripwire, not a proof: Rust
/// cannot enumerate an enum's variants without a derive crate.
#[test]
fn every_channel_is_in_all() {
    for channel in [SinkChannel::Matrix, SinkChannel::Email] {
        match channel {
            SinkChannel::Matrix | SinkChannel::Email => {
                assert!(SinkChannel::ALL.contains(&channel), "{channel:?} missing from ALL")
            }
        }
    }
    assert_eq!(SinkChannel::ALL.len(), 2, "and nothing listed twice");
}

#[test]
fn the_ledger_counts_live_auditing_sinks_apart_from_the_rest() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let mk = |kind| SinkWriter::with_ledger(&LEDGER, pool.clone(), rt.handle().clone(), kind);
    let (m, e) = (mk(SinkChannel::Matrix), mk(SinkChannel::Email));
    assert_eq!((LEDGER.snapshot().sinks_live, LEDGER.snapshot().auditing_live), (2, 1));
    drop(m);
    assert_eq!((LEDGER.snapshot().sinks_live, LEDGER.snapshot().auditing_live), (1, 0));
    drop(e);
    assert_eq!(LEDGER.snapshot(), InFlight::default());
}

/// `ledger(8, 8)` for a `static`.
const fn ledger_const() -> Ledger {
    Ledger::new(Bounds { queued: 8, connections: 8 })
}

static CLEAN_SAID: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_clean(_: AuditLostWriter, line: &str) {
    CLEAN_SAID.lock().unwrap().push(line.to_string());
}

/// #802: a lease whose driver never finished starting — a Matrix login
/// abandoned at its timeout, the blocking task still holding the writer — is
/// counted apart, not waited for, and not reported as a loss. Before, a
/// shutdown after such a login waited the full bound for it and then called it
/// a stuck driver whose queued replies might go unaudited.
#[test]
fn a_lease_that_never_started_is_not_waited_for_or_called_a_loss() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Matrix);
    drop(starting); // the bring-up was abandoned: `started()` is never called
    assert_eq!(LEDGER.snapshot(), InFlight { starting: 1, ..InFlight::default() });

    let t = Instant::now();
    let d = rt.block_on(drain_ledger(&LEDGER, NEVER));
    assert!(t.elapsed() < NEVER / 2, "a starting lease must not be waited for: {:?}", t.elapsed());
    assert_eq!(d.in_flight, InFlight { starting: 1, ..InFlight::default() });
    report_drained(&d, record_clean);
    assert_eq!(*CLEAN_SAID.lock().unwrap(), Vec::<String>::new(), "and is no loss");
    assert!(
        crate::audit_sink::report::format_starting_at_shutdown(&d).is_some(),
        "it is said, at INFO"
    );
    drop(writer);
    assert_eq!(LEDGER.snapshot(), InFlight::default(), "the writer's drop ends it");
}

/// #802: once started, the same lease counts as a live driver — waited for,
/// and a possible loss if stuck — until the writer goes.
#[test]
fn a_started_lease_counts_until_its_writer_goes() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Matrix);
    starting.started();
    assert_eq!(
        LEDGER.snapshot(),
        InFlight { sinks_live: 1, auditing_live: 1, ..InFlight::default() }
    );
    drop(writer);
    assert_eq!(LEDGER.snapshot(), InFlight::default());
}

/// #802: a promotion keeps the lease's kind. A started lease of a driver
/// silent on exit (email's) is live but not auditing, so stuck it is INFO, not
/// a loss — every other starting lease here audits on exit, which a promotion
/// that ignored the kind would pass.
#[test]
fn a_started_silent_lease_is_live_but_not_auditing() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Email);
    starting.started();
    assert_eq!(LEDGER.snapshot(), InFlight { sinks_live: 1, ..InFlight::default() });
    drop(writer);
    assert_eq!(LEDGER.snapshot(), InFlight::default());
}

/// #802: a promotion counts the lease live BEFORE it stops counting it as
/// starting, so a snapshot taken between the two sees it — twice, the safe
/// direction. The other order shows a just-started driver counted nowhere,
/// which the drain calls settled.
#[test]
fn a_promotion_counts_live_before_it_stops_counting_as_starting() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Matrix);
    let mut mid = None;
    starting.0.promote_around(|| mid = Some(LEDGER.snapshot()));
    let mid = mid.expect("the seam ran");
    assert_eq!(mid, InFlight { starting: 1, sinks_live: 1, auditing_live: 1, ..InFlight::default() });
    assert!(!mid.settled());
    drop(writer);
}

/// #802: `snapshot` reads `starting` before the live counts. A lease promoted
/// exactly between them is seen — as starting by the first read, live by the
/// second. Reading the live counts first sees it in neither: a just-started
/// driver, called settled.
#[test]
fn a_lease_promoted_mid_snapshot_is_counted() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Matrix);
    let s = LEDGER.snapshot_around(|| starting.started(), || {});
    assert_eq!((s.starting, s.sinks_live), (1, 1), "seen twice, never not at all: {s:?}");
    assert!(!s.settled());
    assert_eq!(LEDGER.snapshot().starting, 0, "POSITIVE CONTROL: it was promoted");
    drop(writer);
}

/// #802: `started()` after the writer is gone counts nothing — the driver
/// failed and dropped its hooks before the caller could say it was up.
#[test]
fn started_after_the_writer_is_gone_counts_nothing() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Matrix);
    drop(writer);
    starting.started();
    assert_eq!(LEDGER.snapshot(), InFlight::default());
}

/// #802: `started()` racing the writer's drop, from two threads, never leaves
/// a driver counted and never counts one twice — whichever wins. This states
/// the contract; it would rarely catch the lease's lock going (the window is
/// a few instructions), which the lock's one-line design guards instead.
#[test]
fn started_racing_the_writer_s_drop_leaves_no_count() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    for _ in 0..500 {
        let (writer, starting) = SinkWriter::starting_with_ledger(
            &LEDGER,
            pool.clone(),
            rt.handle().clone(),
            SinkChannel::Matrix,
        );
        let go = Arc::new(std::sync::Barrier::new(2));
        let go2 = go.clone();
        let t = std::thread::spawn(move || {
            go2.wait();
            starting.started();
        });
        go.wait();
        drop(writer);
        t.join().unwrap();
        assert_eq!(LEDGER.snapshot(), InFlight::default());
    }
}

/// #802: the drain's half of the close-versus-spawn handshake, at the exact
/// point it guards. A row counted after the close (but before the count) must
/// be in the final snapshot, and a row spawned there must be refused.
/// Counting before closing breaks the first; closing after the window breaks
/// the second.
#[test]
fn the_drain_closes_before_it_counts() {
    static LEDGER: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let (rt, pool, _listener) = runtime_and_pool();
    let writer = SinkWriter::with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Email);
    let mut row = None;
    let mut refused = None;
    let d = close_and_count(&LEDGER, || {
        row = Some(PendingRow::new(&LEDGER, "COUNTED-IN-THE-WINDOW".into()));
        refused = Some(writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {}));
    });
    assert_eq!(d.named, ["COUNTED-IN-THE-WINDOW"], "a row counted after the close is counted");
    assert!(
        matches!(refused, Some(Err(Unwritten::AfterShutdown(_)))),
        "a row spawned after the close is refused: {refused:?}"
    );
    drop(row);
}

/// #802: the spawn's half. The ledger closes after the row is counted and
/// before `closed` is read: the row must be refused, and a count taken in that
/// window must already include it. Reading `closed` before counting lets the
/// row through uncounted.
#[test]
fn a_spawn_counts_its_row_before_it_reads_closed() {
    static LEDGER: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let (rt, pool, _listener) = runtime_and_pool();
    let writer = SinkWriter::with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Email);
    let mut counted = None;
    let spawned = writer.spawn_around(
        "a",
        "b",
        serde_json::json!({}),
        "SPAWNED-AS-THE-DRAIN-CLOSED".into(),
        |_| {},
        || counted = Some(close_and_count(&LEDGER, || {})),
    );
    let counted = counted.expect("the seam ran");
    assert_eq!(counted.named, ["SPAWNED-AS-THE-DRAIN-CLOSED"], "{counted:?}");
    assert!(matches!(spawned, Err(Unwritten::AfterShutdown(_))), "{spawned:?}");
    assert_eq!(LEDGER.snapshot().rows_pending, 0, "the refused row stops counting");
}

/// #799: the read order `Ledger::snapshot` depends on, made deterministic: a
/// driver spawns its last row and drops its writer exactly between the two
/// reads. The writer was counted, so the row must be too. Reading `pending`
/// before the writers sees neither — a driver that just queued a row, called
/// settled.
#[test]
fn a_driver_exiting_mid_snapshot_is_seen_with_its_row() {
    static LEDGER: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let (rt, pool, _listener) = runtime_and_pool();
    let writer = SinkWriter::with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Email);
    let s = LEDGER.snapshot_around(|| {}, || {
        let spawned = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {});
        assert!(spawned.is_ok(), "the queue has room");
        drop(writer);
    });
    assert!(!s.settled(), "a driver that queued a row on its way out is not settled: {s:?}");
    assert_eq!((s.sinks_live, s.rows_pending), (1, 1));
    assert_eq!(LEDGER.snapshot().sinks_live, 0, "POSITIVE CONTROL: the writer is gone now");
}
