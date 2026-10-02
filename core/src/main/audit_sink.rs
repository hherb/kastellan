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
//!   sink's `on_failure` says so (thinned past a flood, `audit_sink_thinning.rs`),
//!   like any other row that was not written.
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
//!   is refused ([`Unwritten::AfterShutdown`]) and, thinned past a flood,
//!   reported by its sink — rather than spawned onto a runtime that is going
//!   away, where tokio drops it without a word.
//!
//! The shutdown line names the first few pending rows (#797: a cancelled task
//! never runs its `on_failure`), counts a stuck driver that audits on exit as
//! a possible loss (#796: its queued replies go unaudited) but not one that
//! never finished starting (#802, `audit_sink_lease.rs`), and counts the
//! refused rows whose reports were thinned out (#798, `audit_sink_thinning.rs`)
//! — the lines are in `audit_sink_report.rs`. Rows refused after that line,
//! past the thinning, are counted again by [`unreported_since`] for one last
//! line just before the daemon exits (#802).
//!
//! What still goes unreported: a row lost to a crash (SIGKILL, OOM, a panic
//! under `panic = "abort"`), because nothing runs after one; and a row refused
//! in the instant between that last line and the process's exit.
//!
//! One writer for both channels (Matrix's `channel.reply_undelivered`, email's
//! `channel.skipped_ack_only`). Before #789 each sink hand-rolled its own
//! insert, and the two had already drifted: one spawned, one blocked.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
/// stop there: [`SinkKind::SilentOnExit`]), and [`report_drained`] says so at
/// INFO, not as a loss.
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
    /// Leases that count ([`SinkWriter`]s whose driver is up): the drivers
    /// that may still write a row, and that [`drain`] waits for.
    live_sinks: AtomicUsize,
    /// How many of `live_sinks` are [`SinkKind::AuditsOnExit`].
    live_auditing: AtomicUsize,
    /// Leases whose driver has not finished starting (`audit_sink_lease.rs`):
    /// not waited for, and not a loss.
    starting: AtomicUsize,
    /// Set by [`drain`]: no row starts after it.
    closed: AtomicBool,
    /// Rows shed on the caller's thread. Counted apart from [`Self::late`]:
    /// a flood shed earlier must not silence the rows refused at shutdown.
    shed: Mutex<Thinning>,
    /// Rows refused after the close.
    late: Mutex<Thinning>,
    /// Who the pending rows are, oldest first, for the shutdown line (#797).
    /// Only for naming: the count that gates `drain` is `pending`.
    labels: Mutex<BTreeMap<u64, String>>,
    /// Pairs a label with its [`PendingRow`].
    next_row: AtomicU64,
}

impl Ledger {
    pub(crate) const fn new(bounds: Bounds) -> Self {
        Self {
            queued: Semaphore::const_new(bounds.queued),
            connections: Semaphore::const_new(bounds.connections),
            pending: AtomicUsize::new(0),
            live_sinks: AtomicUsize::new(0),
            live_auditing: AtomicUsize::new(0),
            starting: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            shed: Mutex::new(Thinning::new()),
            late: Mutex::new(Thinning::new()),
            labels: Mutex::new(BTreeMap::new()),
            next_row: AtomicU64::new(0),
        }
    }

    /// What is in flight now.
    ///
    /// `live_sinks` is read BEFORE `pending`, and the order matters: a driver
    /// spawns its last rows (`pending += 1`) and only then drops its writer
    /// (`live_sinks -= 1`). Reading `pending` first could see 0 before those
    /// rows and `live_sinks` 0 after the drop, calling a driver that just
    /// queued rows "settled". Reading the writers first means a drop seen here
    /// puts every row that driver spawned before this `pending` load.
    pub(crate) fn snapshot(&self) -> InFlight {
        self.snapshot_around(|| {})
    }

    /// [`Self::snapshot`], running `between` after the writers are counted and
    /// before `pending` is: the seam a test uses to put a driver's last row
    /// and its exit exactly inside the window the read order guards (#799).
    fn snapshot_around(&self, between: impl FnOnce()) -> InFlight {
        let sinks_live = self.live_sinks.load(Ordering::SeqCst);
        let auditing_live = self.live_auditing.load(Ordering::SeqCst);
        let starting = self.starting.load(Ordering::SeqCst);
        between();
        let rows_pending = self.pending.load(Ordering::SeqCst);
        InFlight { rows_pending, sinks_live, auditing_live, starting }
    }

    /// Refused rows that got no report of their own, shed and late together.
    fn unreported(&self) -> usize {
        let count = |t: &Mutex<Thinning>| t.lock().unwrap_or_else(|p| p.into_inner()).unreported();
        count(&self.shed) + count(&self.late)
    }

    /// [`Self::snapshot`], plus who the pending rows are and how many refused
    /// rows went unreported. Taken once, at the end of [`drain`].
    pub(crate) fn final_snapshot(&self) -> Drained {
        let in_flight = self.snapshot();
        let named = {
            let labels = self.labels.lock().unwrap_or_else(|p| p.into_inner());
            labels.values().take(NAMED_AT_SHUTDOWN).cloned().collect()
        };
        Drained { in_flight, named, unreported: self.unreported() }
    }
}

/// The daemon's. Every sink shares it, so the bound is on the load these
/// inserts put on the pool, not on each channel's share of it.
static LEDGER: Ledger =
    Ledger::new(Bounds { queued: MAX_QUEUED, connections: MAX_CONNECTIONS });

/// Why a row was not written, as `on_failure` is told.
#[derive(Clone, Copy, Debug)]
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

/// One sink's way to write rows, and its lease on the ledger — see
/// `audit_sink_lease.rs`, which also has its constructors and its `Drop`.
pub(crate) struct SinkWriter {
    ledger: &'static Ledger,
    handle: tokio::runtime::Handle,
    pool: PgPool,
    lease: Arc<lease::Lease>,
}

impl SinkWriter {
    /// Insert one audit row on the runtime, without waiting for it.
    ///
    /// Returns at once — call it from a thread that must not block (the polled
    /// driver's). `on_failure` is told of a row that was not written, with
    /// why: on the runtime if the insert fails, or right here, on the caller's
    /// thread, if the row is shed or the daemon is shutting down (see the
    /// module docs). It should report the row on the `[audit-lost]` marker
    /// with enough to match it to the caller's own line for the event (a
    /// conversation, a message id), and must not block either. On the caller's
    /// thread it is called only for the rows `audit_sink_thinning.rs` lets
    /// through; the shutdown lines count the rest.
    ///
    /// `label` names the row for that shutdown line (#797), which cannot ask
    /// `on_failure` — a task cancelled with the runtime never runs it. Keep it
    /// short and free of the row's content: a channel and a [`quoted_id`].
    ///
    /// `Ok` is the spawned insert's handle, for tests, which await it to see
    /// the outcome; in production it is dropped, which detaches the task: it
    /// still runs. `Err` is why the row was never tried (#797) — already told
    /// to `on_failure` unless thinned out, so a caller may ignore it.
    pub(crate) fn spawn(
        &self,
        actor: &'static str,
        action: &'static str,
        payload: serde_json::Value,
        label: String,
        on_failure: impl FnOnce(Unwritten<'_>) + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<()>, Unwritten<'static>> {
        let ledger = self.ledger;
        // Counts the row (see `Ledger`'s doc for why before `closed` is read).
        let row = PendingRow::new(ledger, label);
        if ledger.closed.load(Ordering::SeqCst) {
            drop(row);
            return Err(refuse(&ledger.late, Unwritten::AfterShutdown, on_failure));
        }
        let Ok(queued) = ledger.queued.try_acquire() else {
            drop(row);
            return Err(refuse(&ledger.shed, Unwritten::Shed, on_failure));
        };
        let pool = self.pool.clone();
        Ok(self.handle.spawn(async move {
            // Dropped when the task ends however it ends, cancelled included.
            let _row = row;
            let _queued = queued;
            // Only a closed semaphore errs, and this one is never closed. Were
            // it closed, no insert could start any more — which is what
            // `AfterShutdown` says — so the row is reported, not dropped (#802).
            let Ok(_connection) = ledger.connections.acquire().await else {
                on_failure(Unwritten::AfterShutdown);
                return;
            };
            if let Err(e) = kastellan_db::audit::insert(&pool, actor, action, payload).await {
                on_failure(Unwritten::Insert(&e));
            }
        }))
    }
}

/// Tell `on_failure` of a row refused on the caller's thread — unless the
/// burst has passed the thinning, in which case it is only counted — and hand
/// `why` back for the caller's `Err`.
fn refuse(
    thinning: &Mutex<Thinning>,
    why: Unwritten<'static>,
    on_failure: impl FnOnce(Unwritten<'_>),
) -> Unwritten<'static> {
    // The lock is released before `on_failure` runs: a report can block.
    let report = thinning.lock().unwrap_or_else(|p| p.into_inner()).refuse(Instant::now());
    if report {
        on_failure(why);
    }
    why
}

/// One row counted in [`Ledger::pending`], and named in its label list, until
/// dropped. Counted BEFORE `closed` is read: see [`Ledger`]'s doc.
struct PendingRow {
    ledger: &'static Ledger,
    id: u64,
}

impl PendingRow {
    fn new(ledger: &'static Ledger, label: String) -> Self {
        ledger.pending.fetch_add(1, Ordering::SeqCst);
        let id = ledger.next_row.fetch_add(1, Ordering::SeqCst);
        ledger.labels.lock().unwrap_or_else(|p| p.into_inner()).insert(id, label);
        Self { ledger, id }
    }
}

impl Drop for PendingRow {
    fn drop(&mut self) {
        self.ledger.labels.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.id);
        self.ledger.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What is in flight at one moment: what [`drain`] polls (#802 split this
/// from [`Drained`], whose extra fields only the final snapshot fills).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InFlight {
    /// Rows spawned and still unwritten.
    pub(crate) rows_pending: usize,
    /// Drivers up and still holding a sink.
    pub(crate) sinks_live: usize,
    /// Of those, the [`SinkKind::AuditsOnExit`] ones: their queued replies go
    /// unaudited while they stay stuck.
    pub(crate) auditing_live: usize,
    /// Drivers holding a sink that never finished starting: not counted in
    /// `sinks_live`, not waited for, not a loss.
    pub(crate) starting: usize,
}

impl InFlight {
    /// Nothing left to wait for. A starting lease is not: its driver never
    /// ran, so it has no row to write.
    fn settled(&self) -> bool {
        self.rows_pending == 0 && self.sinks_live == 0
    }
}

/// What [`drain`] left behind: the last [`InFlight`], who the pending rows
/// are, and how many refused rows went unreported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Drained {
    pub(crate) in_flight: InFlight,
    /// Labels of the oldest pending rows, at most [`NAMED_AT_SHUTDOWN`].
    pub(crate) named: Vec<String>,
    /// Refused rows that never got a report of their own, so far.
    pub(crate) unreported: usize,
}

/// Shutdown: wait up to [`DRAIN_BOUND`] for the daemon's channel drivers to
/// exit and their rows to land, then close the ledger, so a row a late driver
/// tries after this is refused and reported ([`Unwritten::AfterShutdown`]).
/// Call it after the channels are stopped and before the pool closes; hand the
/// result to [`report_drained`], and later to [`unreported_since`].
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

/// The refused rows thinned out since `drained` was reported — late rows a
/// still-running driver tried after the shutdown line (#802). For the last
/// line before the daemon exits ([`report_unreported_since`]).
pub(crate) fn unreported_since(drained: &Drained) -> usize {
    unreported_on_since(&LEDGER, drained)
}

/// [`unreported_since`] on `ledger`.
fn unreported_on_since(ledger: &'static Ledger, drained: &Drained) -> usize {
    ledger.unreported().saturating_sub(drained.unreported)
}

#[path = "audit_sink_lease.rs"]
mod lease;
pub(crate) use lease::SinkKind;

#[path = "audit_sink_thinning.rs"]
mod thinning;
use thinning::Thinning;

#[path = "audit_sink_report.rs"]
mod report;
pub(crate) use report::{emit_report, quoted_id, report_drained, report_unreported_since, Reporter};

/// Test builds only: a Postgres that never answers, for proving a sink does
/// not wait for its insert — and that it did try one. Shared by this module's
/// tests and each sink's own.
#[cfg(test)]
#[path = "audit_sink_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "audit_sink_tests.rs"]
mod tests;
