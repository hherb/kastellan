//! Writing an audit row from a polled channel driver's thread **without
//! stalling the driver** (#789), without letting a flood of them starve every
//! other writer's audit rows, and **saying so when one is lost** (#792).
//!
//! The polled channel driver (`kastellan_core::channel::polled_driver`) is
//! DB-free by design. It calls its audit hooks synchronously, on its own std
//! thread — the thread every conversation, every poll and every ack waits on.
//! A hook that `block_on`s its insert therefore stalls the whole channel
//! whenever Postgres is slow or unreachable: up to the pool's acquire timeout
//! (`kastellan_db::pool`'s `ACQUIRE_TIMEOUT`) per audited event, multiplied by
//! every event in a batch.
//!
//! So the daemon's hooks **spawn** the insert onto the runtime and return at
//! once, through a [`SinkWriter`]. What that costs, stated where it is paid:
//!
//! - **Bounded, not free.** `block_on` ran one insert at a time; spawned
//!   inserts run side by side, on the pool the whole daemon writes its audit
//!   rows through. A compromised worker is in scope and email's `skipped` list
//!   has no length cap, so unbounded, one poll could start thousands of
//!   inserts and time out tool-dispatch and scheduler rows waiting behind
//!   them. At most [`MAX_CONNECTIONS`] of these inserts use the pool at once
//!   and at most [`MAX_QUEUED`] wait; past that a row is **shed**, and the
//!   sink's `on_failure` says so, like any other row that was not written.
//! - **After the fact.** The row is written after the hook returns, so an
//!   email id is acked before its row exists: a crash in between loses the
//!   row, and the id is not redelivered. Rows can also land out of order
//!   relative to the events; each row's payload carries its event's own time
//!   (`observed_at`), because `audit_log.ts` is the insert's.
//! - **At shutdown (#792).** Before the pool closes, `main` calls [`drain`]:
//!   it waits, bounded by [`DRAIN_BOUND`], for every driver holding a sink to
//!   exit (a Matrix driver audits its still-queued replies as it goes) and for
//!   every spawned row to finish, then **closes** the ledger. What is still
//!   pending then is reported on the `[audit-lost]` marker
//!   ([`report_drained`]), and a row a late driver tries to write after that
//!   is refused and reported by its sink ([`Unwritten::AfterShutdown`]),
//!   rather than spawned onto a runtime that is going away — where tokio
//!   drops it without a word.
//!
//! What still goes unreported: a row lost to a crash (SIGKILL, OOM, a panic
//! under `panic = "abort"`), because nothing runs after one.
//!
//! One writer for both channels (Matrix's `channel.reply_undelivered`, email's
//! `channel.skipped_ack_only`). Before #789 each sink hand-rolled its own
//! insert, and the two had already drifted: one spawned, one blocked.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::Semaphore;

/// How many of these inserts may use the pool at once: a quarter of
/// [`kastellan_db::pool::DEFAULT_MAX_CONNECTIONS`], so a flood of them leaves
/// the rest to every other writer.
const MAX_CONNECTIONS: usize = 4;

/// How many may wait for one of those. Far above any honest burst (a Matrix
/// queue's overflow, one email poll's skipped ids), and a bound on what a
/// flood or a wedged Postgres can pile up.
const MAX_QUEUED: usize = 1024;

/// How long [`drain`] waits at shutdown for the channel drivers to exit and
/// their rows to land.
///
/// Long enough for a Matrix driver to notice its channel is gone (it long-polls
/// for [`kastellan_core::channel::matrix::POLL_MS`] at a time, asserted below)
/// and write its `driver_exit` rows. Short enough to leave most of the service
/// manager's stop budget — 10 s from SIGTERM to SIGKILL on both hosts
/// (`TimeoutStopSec` / `ExitTimeOut`, `kastellan-supervisor`) — to the
/// scheduler shutdown it runs beside. An email driver long-polls for 15 s, so
/// it is often still running when this expires; that costs nothing, because
/// it audits nothing once its bus is gone (`polled_driver`'s skipped-id acks
/// stop there), and [`report_drained`] says so at INFO, not as a loss.
pub(crate) const DRAIN_BOUND: Duration = Duration::from_secs(3);

const _: () = assert!(
    DRAIN_BOUND.as_millis() > kastellan_core::channel::matrix::POLL_MS as u128,
    "the drain must outlast one Matrix long-poll, or every shutdown reports its driver"
);

/// How often [`drain`] looks again.
const DRAIN_POLL: Duration = Duration::from_millis(20);

/// The bounds on these inserts, and what is in flight under them.
///
/// `pending` is its own counter, not the queue semaphore's free permits,
/// because closing needs `SeqCst` on both sides: a row spawned while
/// [`drain`] closes the ledger must either see `closed` or be counted by it —
/// never neither, which would be a row lost with no line.
pub(crate) struct Ledger {
    queued: Semaphore,
    connections: Semaphore,
    /// Rows spawned and not yet finished (written, failed, or cancelled).
    pending: AtomicUsize,
    /// [`SinkWriter`]s alive: each is owned by one driver's audit hook, so
    /// this is the number of drivers that may still write a row.
    live_sinks: AtomicUsize,
    /// Set by [`drain`]: no row starts after it.
    closed: AtomicBool,
}

impl Ledger {
    pub(crate) const fn new(queued: usize, connections: usize) -> Self {
        Self {
            queued: Semaphore::const_new(queued),
            connections: Semaphore::const_new(connections),
            pending: AtomicUsize::new(0),
            live_sinks: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        }
    }

    /// What is still in flight.
    pub(crate) fn snapshot(&self) -> Drained {
        Drained {
            rows_pending: self.pending.load(Ordering::SeqCst),
            sinks_live: self.live_sinks.load(Ordering::SeqCst),
        }
    }
}

/// The daemon's. Every sink shares it, so the bound is on the load these
/// inserts put on the pool, not on each channel's share of it.
static LEDGER: Ledger = Ledger::new(MAX_QUEUED, MAX_CONNECTIONS);

/// Why a row was not written, as `on_failure` is told.
#[derive(Debug)]
pub(crate) enum Unwritten<'a> {
    /// The insert ran and failed.
    Insert(&'a kastellan_db::DbError),
    /// Never tried: the queue was full.
    Shed,
    /// Never tried: the daemon had already drained its audit writes and was
    /// shutting down (#792).
    AfterShutdown,
}

impl std::fmt::Display for Unwritten<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Insert(e) => e.fmt(f),
            Self::Shed => f.write_str(
                "shed: too many audit inserts already waiting for the pool (a flood of \
                 audited events, or a wedged Postgres)",
            ),
            Self::AfterShutdown => f.write_str(
                "not tried: the daemon was shutting down and had already drained its audit \
                 writes",
            ),
        }
    }
}

/// One sink's way to write rows, and its **lease** on the ledger: while a
/// `SinkWriter` is alive, [`drain`] counts its driver as one that may still
/// write a row.
///
/// A sink's hook closure owns its writer, and the driver owns the hook, so the
/// lease ends exactly when the driver drops its hooks — as its thread returns,
/// after any `driver_exit` rows are spawned. No driver has to remember to
/// report its exit.
pub(crate) struct SinkWriter {
    ledger: &'static Ledger,
    handle: tokio::runtime::Handle,
    pool: PgPool,
}

impl SinkWriter {
    /// A writer on the daemon's ledger, spawning onto `handle`.
    pub(crate) fn new(pool: PgPool, handle: tokio::runtime::Handle) -> Self {
        Self::with_ledger(&LEDGER, pool, handle)
    }

    /// A writer on `ledger`: tests use their own, so their counts are theirs.
    pub(crate) fn with_ledger(
        ledger: &'static Ledger,
        pool: PgPool,
        handle: tokio::runtime::Handle,
    ) -> Self {
        ledger.live_sinks.fetch_add(1, Ordering::SeqCst);
        Self { ledger, handle, pool }
    }

    /// Insert one audit row on the runtime, without waiting for it.
    ///
    /// Returns at once — call it from a thread that must not block (the polled
    /// driver's). `on_failure` is told of a row that was not written, with
    /// why: on the runtime if the insert fails, or right here, on the caller's
    /// thread, if the row is shed or the daemon is shutting down (see the
    /// module docs). It should report the row on the `[audit-lost]` marker
    /// with enough to match it to the caller's own line for the event (a
    /// conversation, a message id), and must not block either.
    ///
    /// The returned handle is for tests, which await it to see the outcome;
    /// `None` when the row was never tried. In production it is dropped, which
    /// detaches the task: it still runs.
    pub(crate) fn spawn(
        &self,
        actor: &'static str,
        action: &'static str,
        payload: serde_json::Value,
        on_failure: impl FnOnce(Unwritten<'_>) + Send + 'static,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let ledger = self.ledger;
        // Counted BEFORE `closed` is read: see `Ledger`'s doc.
        ledger.pending.fetch_add(1, Ordering::SeqCst);
        let row = PendingRow(ledger);
        if ledger.closed.load(Ordering::SeqCst) {
            drop(row);
            on_failure(Unwritten::AfterShutdown);
            return None;
        }
        let Ok(queued) = ledger.queued.try_acquire() else {
            drop(row);
            on_failure(Unwritten::Shed);
            return None;
        };
        let pool = self.pool.clone();
        Some(self.handle.spawn(async move {
            // Dropped when the task ends however it ends, cancelled included.
            let _row = row;
            let _queued = queued;
            // `Err` only for a closed semaphore, and these are never closed.
            let _connection = ledger.connections.acquire().await.ok();
            if let Err(e) = kastellan_db::audit::insert(&pool, actor, action, payload).await {
                on_failure(Unwritten::Insert(&e));
            }
        }))
    }
}

impl Drop for SinkWriter {
    fn drop(&mut self) {
        self.ledger.live_sinks.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One row counted in [`Ledger::pending`] until dropped.
struct PendingRow(&'static Ledger);

impl Drop for PendingRow {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What [`drain`] left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Drained {
    /// Rows spawned and still unwritten.
    pub(crate) rows_pending: usize,
    /// Drivers still holding a sink.
    pub(crate) sinks_live: usize,
}

impl Drained {
    fn settled(self) -> bool {
        self.rows_pending == 0 && self.sinks_live == 0
    }
}

/// Shutdown: wait up to [`DRAIN_BOUND`] for the daemon's channel drivers to
/// exit and their rows to land, then close the ledger, so a row a late driver
/// tries after this is refused and reported ([`Unwritten::AfterShutdown`]).
/// Call it after the channels are stopped and before the pool closes; hand the
/// result to [`report_drained`].
pub(crate) async fn drain() -> Drained {
    drain_ledger(&LEDGER, DRAIN_BOUND).await
}

/// [`drain`] on `ledger`, bounded by `bound`.
async fn drain_ledger(ledger: &'static Ledger, bound: Duration) -> Drained {
    let deadline = tokio::time::Instant::now() + bound;
    while !ledger.snapshot().settled() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(DRAIN_POLL).await;
    }
    // Close first, then count: see `Ledger`'s doc.
    ledger.closed.store(true, Ordering::SeqCst);
    ledger.snapshot()
}

/// Pure: the `[audit-lost]` line for rows still pending when [`drain`] gave
/// up, or `None` when there were none.
pub(crate) fn format_pending_at_shutdown(d: Drained, bound: Duration) -> Option<String> {
    (d.rows_pending > 0).then(|| {
        format!(
            "{} channel audit row{} still unwritten after waiting {} s at shutdown; the \
             database pool closes next, so {} lost unless already mid-write",
            d.rows_pending,
            if d.rows_pending == 1 { " was" } else { "s were" },
            bound.as_secs(),
            if d.rows_pending == 1 { "it is" } else { "they are" },
        )
    })
}

/// Pure: the INFO line for drivers still running when [`drain`] gave up, or
/// `None` when there were none. Not a loss by itself — see [`DRAIN_BOUND`] —
/// and a row one of them does try is reported as it happens.
pub(crate) fn format_live_at_shutdown(d: Drained, bound: Duration) -> Option<String> {
    (d.sinks_live > 0).then(|| {
        format!(
            "{} channel driver{} had not exited after {} s at shutdown; an audit row one \
             writes from now on is refused and reported on the [audit-lost] marker",
            d.sinks_live,
            if d.sinks_live == 1 { "" } else { "s" },
            bound.as_secs(),
        )
    })
}

/// Say what [`drain`] left behind: rows still pending on the `[audit-lost]`
/// marker, drivers still running at INFO.
pub(crate) fn report_drained(d: Drained) {
    if let Some(line) = format_pending_at_shutdown(d, DRAIN_BOUND) {
        kastellan_core::worker_stderr::emit_audit_lost_report("shutdown", &line);
    }
    if let Some(line) = format_live_at_shutdown(d, DRAIN_BOUND) {
        tracing::info!("{line}");
    }
}

/// Test builds only: a Postgres that never answers, for proving a sink does
/// not wait for its insert — and that it did try one. Shared by this module's
/// tests and each sink's own.
#[cfg(test)]
#[path = "audit_sink_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "audit_sink_tests.rs"]
mod tests;
