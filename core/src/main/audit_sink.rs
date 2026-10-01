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
//!   and at most [`MAX_QUEUED`] are in flight in all, running or waiting; past
//!   that a row is **shed**, and the
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
//! The shutdown line names the first few pending rows (#797: a cancelled task
//! never runs its `on_failure`), counts a Matrix driver still stuck as a loss
//! (#796: its queued replies go unaudited) and thins out the per-row reports of
//! a flood of refused rows ([`should_report`], #798) — all in `audit_sink_report.rs`.
//!
//! What still goes unreported: a row lost to a crash (SIGKILL, OOM, a panic
//! under `panic = "abort"`), because nothing runs after one.
//!
//! One writer for both channels (Matrix's `channel.reply_undelivered`, email's
//! `channel.skipped_ack_only`). Before #789 each sink hand-rolled its own
//! insert, and the two had already drifted: one spawned, one blocked.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::Semaphore;

/// How many of these inserts may use the pool at once: a quarter of
/// [`kastellan_db::pool::DEFAULT_MAX_CONNECTIONS`], so a flood of them leaves
/// the rest to every other writer.
const MAX_CONNECTIONS: usize = 4;

/// How many may be in flight in all, running or waiting for one of those
/// connections (the permit is held for a row's whole life). Far above any
/// honest burst (a Matrix queue's overflow, one email poll's skipped ids), and
/// a bound on what a flood or a wedged Postgres can pile up.
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

/// The two bounds on these inserts ([`MAX_CONNECTIONS`], [`MAX_QUEUED`]).
/// Named fields, because both are a bare `usize` (#800).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    /// Rows in flight in all, running or waiting.
    pub(crate) queued: usize,
    /// Rows using the pool at once.
    pub(crate) connections: usize,
}

/// How many rows a ledger names individually at shutdown; the rest are counted.
const NAMED_AT_SHUTDOWN: usize = 5;

/// How many shed / refused rows [`should_report`] reports one by one before
/// it thins them out.
const REPORT_EACH_UP_TO: usize = 16;

/// Pure: whether the `n`th (0-based) row refused on the caller's thread gets
/// its own report (#798).
///
/// A shed or refused row is reported by its sink **on the driver thread**
/// (one tracing ERROR and a stderr write, either of which can block), and a
/// flood — email's `skipped` list has no length cap — would have the driver do
/// that for every id. So the first [`REPORT_EACH_UP_TO`] are reported, then
/// only the 2^k-th: the report stays logarithmic in the flood. The rows left
/// out are counted ([`Drained::unreported`]) and the shutdown line says how many.
fn should_report(n: usize) -> bool {
    n < REPORT_EACH_UP_TO || n.is_power_of_two()
}

/// The bounds on these inserts, and what is in flight under them.
///
/// The invariant: a row spawned while [`drain`] closes the ledger must either
/// see `closed` or be counted by drain's final snapshot — never neither, which
/// would be a row lost with no line. `spawn` counts (`pending`) before it reads
/// `closed`, and `drain` stores `closed` before it counts, all `SeqCst`. That is
/// why `pending` is its own counter (a shed row needs counting too) and not the
/// queue semaphore's free permits.
pub(crate) struct Ledger {
    queued: Semaphore,
    connections: Semaphore,
    /// Rows spawned and not yet finished (written, failed, or cancelled).
    pending: AtomicUsize,
    /// [`SinkWriter`]s alive: each is owned by one driver's audit hook, so
    /// this is the number of drivers that may still write a row.
    live_sinks: AtomicUsize,
    /// How many of `live_sinks` are [`SinkKind::Replies`].
    live_replies: AtomicUsize,
    /// Set by [`drain`]: no row starts after it.
    closed: AtomicBool,
    /// Rows refused on the caller's thread so far (shed, or after the close),
    /// for [`should_report`].
    refused: AtomicUsize,
    /// Who the pending rows are, for the shutdown line (#797). Only for
    /// naming: the count that gates `drain` is `pending`.
    labels: Mutex<Vec<(u64, String)>>,
    next_row: AtomicU64,
}

impl Ledger {
    pub(crate) const fn new(bounds: Bounds) -> Self {
        Self {
            queued: Semaphore::const_new(bounds.queued),
            connections: Semaphore::const_new(bounds.connections),
            pending: AtomicUsize::new(0),
            live_sinks: AtomicUsize::new(0),
            live_replies: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            refused: AtomicUsize::new(0),
            labels: Mutex::new(Vec::new()),
            next_row: AtomicU64::new(0),
        }
    }

    /// What is still in flight.
    ///
    /// `live_sinks` is read BEFORE `pending`, and the order matters: a driver
    /// spawns its last rows (`pending += 1`) and only then drops its writer
    /// (`live_sinks -= 1`). Reading `pending` first could see 0 before those
    /// rows and `live_sinks` 0 after the drop, calling a driver that just
    /// queued rows "settled". Reading the writers first means a drop seen here
    /// puts every row that driver spawned before this `pending` load.
    pub(crate) fn snapshot(&self) -> Drained {
        let sinks_live = self.live_sinks.load(Ordering::SeqCst);
        let replies_live = self.live_replies.load(Ordering::SeqCst);
        let rows_pending = self.pending.load(Ordering::SeqCst);
        Drained { rows_pending, sinks_live, replies_live, ..Drained::default() }
    }

    /// [`Self::snapshot`], plus who the pending rows are and how many refused
    /// rows went unreported. Taken once, at the end of [`drain`].
    fn final_snapshot(&self) -> Drained {
        let counts = self.snapshot();
        let named = {
            let labels = self.labels.lock().unwrap_or_else(|p| p.into_inner());
            labels.iter().take(NAMED_AT_SHUTDOWN).map(|(_, l)| l.clone()).collect()
        };
        let refused = self.refused.load(Ordering::SeqCst);
        let reported = (0..refused).filter(|&n| should_report(n)).count();
        Drained { named, unreported: refused - reported, ..counts }
    }
}

/// The daemon's. Every sink shares it, so the bound is on the load these
/// inserts put on the pool, not on each channel's share of it.
static LEDGER: Ledger =
    Ledger::new(Bounds { queued: MAX_QUEUED, connections: MAX_CONNECTIONS });

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

/// What a sink's driver does with rows at the end of its life — which decides
/// whether a driver [`drain`] finds still running is a loss (#796).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SinkKind {
    /// Matrix's: the driver audits every reply still queued as it exits
    /// (`channel.reply_undelivered`, reason `driver_exit`). Stuck past the
    /// drain bound — in a worker call, or flushing a long queue — it never
    /// gets there, so those replies are **unaudited**: a loss, and reported
    /// as one.
    Replies,
    /// Email's: nothing is audited once the driver's bus is gone (the skipped
    /// ids' acks stop there), so a driver still in its 15 s long-poll costs
    /// nothing. INFO.
    SkippedIds,
}

/// One sink's way to write rows, and its **lease** on the ledger: while a
/// `SinkWriter` is alive, [`drain`] counts its driver as one that may still
/// write a row.
///
/// A sink's hook closure owns its writer, and the driver owns the hook, so the
/// lease ends exactly when the driver drops its hooks — as its thread returns,
/// after any `driver_exit` rows are spawned. No driver has to remember to
/// report its exit.
///
/// ⚠️ A writer made for a bring-up attempt that is then abandoned (Matrix's
/// login timeout leaves its `spawn_blocking` running) keeps its lease until
/// that task ends, so a shutdown can wait the full bound for a driver that
/// never started. The line then says a driver "had not exited": true of the
/// lease, and harmless, because that task writes no rows either.
pub(crate) struct SinkWriter {
    ledger: &'static Ledger,
    handle: tokio::runtime::Handle,
    pool: PgPool,
    kind: SinkKind,
}

impl SinkWriter {
    /// A writer on the daemon's ledger, spawning onto `handle`.
    pub(crate) fn new(pool: PgPool, handle: tokio::runtime::Handle, kind: SinkKind) -> Self {
        Self::with_ledger(&LEDGER, pool, handle, kind)
    }

    /// A writer on `ledger`: tests use their own, so their counts are theirs.
    pub(crate) fn with_ledger(
        ledger: &'static Ledger,
        pool: PgPool,
        handle: tokio::runtime::Handle,
        kind: SinkKind,
    ) -> Self {
        ledger.live_sinks.fetch_add(1, Ordering::SeqCst);
        if kind == SinkKind::Replies {
            ledger.live_replies.fetch_add(1, Ordering::SeqCst);
        }
        Self { ledger, handle, pool, kind }
    }

    /// Insert one audit row on the runtime, without waiting for it.
    ///
    /// Returns at once — call it from a thread that must not block (the polled
    /// driver's). `on_failure` is told of a row that was not written, with
    /// why: on the runtime if the insert fails, or right here, on the caller's
    /// thread, if the row is shed or the daemon is shutting down (see the
    /// module docs). It should report the row on the `[audit-lost]` marker
    /// with enough to match it to the caller's own line for the event (a
    /// conversation, a message id), and must not block either. On the caller's
    /// thread it is called for the first [`REPORT_EACH_UP_TO`] refused rows and
    /// then only for the 2^k-th ([`should_report`]); the shutdown line counts
    /// the rest.
    ///
    /// `label` names the row for that shutdown line (#797), which cannot ask
    /// `on_failure` — a task cancelled with the runtime never runs it. Keep it
    /// short and free of the row's content: a channel and an id.
    ///
    /// The returned handle is for tests, which await it to see the outcome;
    /// `None` when the row was never tried. In production it is dropped, which
    /// detaches the task: it still runs.
    pub(crate) fn spawn(
        &self,
        actor: &'static str,
        action: &'static str,
        payload: serde_json::Value,
        label: String,
        on_failure: impl FnOnce(Unwritten<'_>) + Send + 'static,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let ledger = self.ledger;
        // Counted BEFORE `closed` is read: see `Ledger`'s doc.
        ledger.pending.fetch_add(1, Ordering::SeqCst);
        let row = PendingRow::new(ledger, label);
        if ledger.closed.load(Ordering::SeqCst) {
            drop(row);
            refuse(ledger, Unwritten::AfterShutdown, on_failure);
            return None;
        }
        let Ok(queued) = ledger.queued.try_acquire() else {
            drop(row);
            refuse(ledger, Unwritten::Shed, on_failure);
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

/// Tell `on_failure` of a row refused on the caller's thread — unless the
/// flood has passed [`should_report`]'s thinning, in which case it is only
/// counted.
fn refuse(ledger: &Ledger, why: Unwritten<'_>, on_failure: impl FnOnce(Unwritten<'_>)) {
    if should_report(ledger.refused.fetch_add(1, Ordering::SeqCst)) {
        on_failure(why);
    }
}

impl Drop for SinkWriter {
    fn drop(&mut self) {
        if self.kind == SinkKind::Replies {
            self.ledger.live_replies.fetch_sub(1, Ordering::SeqCst);
        }
        self.ledger.live_sinks.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One row counted in [`Ledger::pending`], and named in its label list, until
/// dropped.
struct PendingRow {
    ledger: &'static Ledger,
    id: u64,
}

impl PendingRow {
    /// The caller has already counted the row in `pending`.
    fn new(ledger: &'static Ledger, label: String) -> Self {
        let id = ledger.next_row.fetch_add(1, Ordering::SeqCst);
        ledger.labels.lock().unwrap_or_else(|p| p.into_inner()).push((id, label));
        Self { ledger, id }
    }
}

impl Drop for PendingRow {
    fn drop(&mut self) {
        let mut labels = self.ledger.labels.lock().unwrap_or_else(|p| p.into_inner());
        labels.retain(|(id, _)| *id != self.id);
        drop(labels);
        self.ledger.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What [`drain`] left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Drained {
    /// Rows spawned and still unwritten.
    pub(crate) rows_pending: usize,
    /// Drivers still holding a sink.
    pub(crate) sinks_live: usize,
    /// Of those, the [`SinkKind::Replies`] ones: their queued replies go
    /// unaudited while they stay stuck.
    pub(crate) replies_live: usize,
    /// Labels of the first few pending rows ([`NAMED_AT_SHUTDOWN`]); only the
    /// final snapshot of [`drain`] fills it.
    pub(crate) named: Vec<String>,
    /// Refused rows ([`should_report`]) that never got a report of their own;
    /// also only in the final snapshot.
    pub(crate) unreported: usize,
}

impl Drained {
    fn settled(&self) -> bool {
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
    ledger.final_snapshot()
}

#[path = "audit_sink_report.rs"]
mod report;
pub(crate) use report::*;

/// Test builds only: a Postgres that never answers, for proving a sink does
/// not wait for its insert — and that it did try one. Shared by this module's
/// tests and each sink's own.
#[cfg(test)]
#[path = "audit_sink_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "audit_sink_tests.rs"]
mod tests;
