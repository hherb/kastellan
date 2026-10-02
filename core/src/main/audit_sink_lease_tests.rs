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
    drain_ledger, report_drained, Bounds, Drained, InFlight, PendingRow,
};
use kastellan_core::worker_stderr::AuditLostWriter;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// See `audit_sink_tests.rs`'s `NEVER`: a bound no test is meant to reach.
const NEVER: Duration = Duration::from_secs(60);

fn ledger(queued: usize, connections: usize) -> Ledger {
    Ledger::new(Bounds { queued, connections })
}

/// A runtime and a stalled pool, the pool made inside the runtime (it spawns
/// its maintenance task as it is made). The listener outlives the test's calls.
fn runtime_and_pool() -> (tokio::runtime::Runtime, PgPool, std::net::TcpListener) {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, listener) = rt.block_on(async { stalled_pool() });
    (rt, pool, listener)
}

#[test]
fn the_ledger_counts_live_auditing_sinks_apart_from_the_rest() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let mk = |kind| SinkWriter::with_ledger(&LEDGER, pool.clone(), rt.handle().clone(), kind);
    let (m, e) = (mk(SinkKind::AuditsOnExit), mk(SinkKind::SilentOnExit));
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
/// counted apart, not waited for, and not reported as a loss. Before, every
/// shutdown waited the full bound for it and then called it a stuck driver
/// whose queued replies might go unaudited.
#[test]
fn a_lease_that_never_started_is_not_waited_for_or_called_a_loss() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkKind::AuditsOnExit);
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
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkKind::AuditsOnExit);
    starting.started();
    assert_eq!(
        LEDGER.snapshot(),
        InFlight { sinks_live: 1, auditing_live: 1, ..InFlight::default() }
    );
    drop(writer);
    assert_eq!(LEDGER.snapshot(), InFlight::default());
}

/// #802: `started()` after the writer is gone counts nothing — the driver
/// failed and dropped its hooks before the caller could say it was up.
#[test]
fn started_after_the_writer_is_gone_counts_nothing() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    let (writer, starting) =
        SinkWriter::starting_with_ledger(&LEDGER, pool, rt.handle().clone(), SinkKind::AuditsOnExit);
    drop(writer);
    starting.started();
    assert_eq!(LEDGER.snapshot(), InFlight::default());
}

/// #802: `started()` racing the writer's drop, from two threads, never leaves
/// a driver counted and never counts one twice — whichever wins.
#[test]
fn started_racing_the_writer_s_drop_leaves_no_count() {
    static LEDGER: Ledger = ledger_const();
    let (rt, pool, _listener) = runtime_and_pool();
    for _ in 0..500 {
        let (writer, starting) = SinkWriter::starting_with_ledger(
            &LEDGER,
            pool.clone(),
            rt.handle().clone(),
            SinkKind::AuditsOnExit,
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

/// #792 + #802: a row counted before the close is in the final snapshot,
/// named — the half of the close-versus-spawn handshake a stress test can only
/// hit by chance, made directly: count, then close, then look.
#[test]
fn a_row_counted_before_the_close_is_in_the_final_snapshot() {
    static LEDGER: Ledger = ledger_const();
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let row = PendingRow::new(&LEDGER, "COUNTED-BEFORE-CLOSE".into());
    let d = rt.block_on(drain_ledger(&LEDGER, Duration::ZERO));
    assert_eq!(d.in_flight.rows_pending, 1);
    assert_eq!(d.named, ["COUNTED-BEFORE-CLOSE"]);
    drop(row);
    assert_eq!(LEDGER.final_snapshot(), Drained::default(), "a dropped row leaves no name");
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
    let writer = SinkWriter::with_ledger(&LEDGER, pool, rt.handle().clone(), SinkKind::SilentOnExit);
    let s = LEDGER.snapshot_around(|| {
        let spawned = writer.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {});
        assert!(spawned.is_ok(), "the queue has room");
        drop(writer);
    });
    assert!(!s.settled(), "a driver that queued a row on its way out is not settled: {s:?}");
    assert_eq!((s.sinks_live, s.rows_pending), (1, 1));
    assert_eq!(LEDGER.snapshot().sinks_live, 0, "POSITIVE CONTROL: the writer is gone now");
}

/// #799: the same read order under real contention — a smoke test beside the
/// deterministic one above, for an interleaving the seam does not model. Each
/// round, four drivers each spawn one row (which never finishes: no pool
/// connection is ever free) and then drop their writer, while this thread
/// snapshots. Each writer seen gone had spawned its row first, so a snapshot
/// must count at least one row per writer it no longer sees. Reading
/// `pending` before `live_sinks` breaks that: it can see no rows, then no
/// writers.
#[test]
fn a_writer_seen_gone_has_its_rows_counted() {
    const DRIVERS: usize = 4;
    let (rt, pool, _listener) = runtime_and_pool();
    for round in 0..200 {
        let ledger: &'static Ledger = Box::leak(Box::new(ledger(1024, 0)));
        let writers: Vec<_> = (0..DRIVERS)
            .map(|_| {
                SinkWriter::with_ledger(ledger, pool.clone(), rt.handle().clone(), SinkKind::SilentOnExit)
            })
            .collect();
        let done = Arc::new(AtomicBool::new(false));
        let go = Arc::new(std::sync::Barrier::new(DRIVERS + 1));
        // Snapshotting before the drivers are released, so it overlaps them.
        let watcher = {
            let done = done.clone();
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    let s = ledger.snapshot();
                    assert!(
                        s.rows_pending >= DRIVERS - s.sinks_live,
                        "round {round}: {s:?} — a writer seen gone without its row"
                    );
                }
            })
        };
        let drivers: Vec<_> = writers
            .into_iter()
            .map(|w| {
                let go = go.clone();
                std::thread::spawn(move || {
                    go.wait();
                    let spawned = w.spawn("a", "b", serde_json::json!({}), "t".into(), |_| {});
                    assert!(spawned.is_ok(), "the queue has room");
                    drop(w);
                })
            })
            .collect();
        go.wait();
        for d in drivers {
            d.join().unwrap();
        }
        done.store(true, Ordering::SeqCst);
        watcher.join().expect("the snapshot invariant held");
        let s = ledger.snapshot();
        assert_eq!((s.rows_pending, s.sinks_live), (DRIVERS, 0), "POSITIVE CONTROL: all ran");
    }
    // The rows' tasks wait for a connection that never comes; they end with
    // the runtime.
}
