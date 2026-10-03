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
//!   it waits, bounded by [`DRAIN_BOUND`], for every driver holding a
//!   started sink to exit (a Matrix driver audits its still-queued replies as
//!   it goes; one whose bring-up never finished is not waited for, see
//!   `audit_sink_lease.rs`) and for every spawned row to finish, then
//!   **closes** the ledger. What is still pending then is reported on the
//!   `[audit-lost]` marker ([`report_drained`]), and a row a late driver
//!   tries to write after that is refused ([`Unwritten::AfterShutdown`]) and,
//!   thinned past a flood, reported by its sink — rather than spawned onto a
//!   runtime that is going away, where tokio drops it without a word.
//!
//! The shutdown line names the first few pending rows (#797: a cancelled task
//! never runs its `on_failure`), counts a stuck driver that audits on exit as
//! a possible loss (#796: its queued replies go unaudited) but not one that
//! never finished starting (#802, `audit_sink_lease.rs`), and counts the
//! refused rows whose reports were thinned out (#798, `audit_sink_thinning.rs`)
//! — the lines are in `audit_sink_report.rs`. Rows refused after the drain's
//! snapshot, past the thinning, are counted afresh for the daemon's last
//! lines ([`close_then_report_unreported`], #802): said once before the pool
//! closes, because that close has no bound, and once more after it.
//!
//! What still goes unreported: a row lost to a crash (SIGKILL, OOM, a panic
//! under `panic = "abort"`), because nothing runs after one; a row refused,
//! past the thinning, during a pool close the service manager's SIGKILL ends;
//! and one refused between the last line and the process's exit. That last
//! gap is usually an instant, but not always: dropping the runtime waits for
//! its blocking tasks, so a Matrix login abandoned at its timeout can hold the
//! process open until the SDK's own timeouts end it, or the SIGKILL (#805).
//!
//! What may be said twice: a row caught by the final snapshot in the middle
//! of being refused is counted as pending there and then refused — reported,
//! or counted in the last lines — over-counting, which is the safe direction.
//!
//! One writer for both channels (Matrix's `channel.reply_undelivered`, email's
//! `channel.skipped_ack_only`). Before #789 each sink hand-rolled its own
//! insert, and the two had already drifted: one spawned, one blocked.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

const _: () = assert!(
    MAX_CONNECTIONS > 0 && MAX_QUEUED >= MAX_CONNECTIONS,
    "no connection would wedge every row until the drain called it lost"
);

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
    /// Rows refused on the caller's thread — shed, or tried after the close —
    /// thinned and counted per channel and per kind (#798, #807).
    refusals: Refusals,
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
            refusals: Refusals::new(),
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
    ///
    /// `starting` is read before the live counts for the same reason against
    /// a lease being promoted (#802), which counts itself live before it
    /// stops counting as starting: a promotion this read misses is one the
    /// live reads after it can only see, so a driver is counted once or twice
    /// (the safe direction), never not at all.
    pub(crate) fn snapshot(&self) -> InFlight {
        self.snapshot_around(|| {}, || {})
    }

    /// [`Self::snapshot`], running `after_starting` once `starting` is read
    /// and `after_live` once the writers are: the seams a test uses to promote
    /// a lease (#802), or put a driver's last row and its exit (#799), exactly
    /// inside the window each read order guards.
    fn snapshot_around(&self, after_starting: impl FnOnce(), after_live: impl FnOnce()) -> InFlight {
        let starting = self.starting.load(Ordering::SeqCst);
        after_starting();
        let sinks_live = self.live_sinks.load(Ordering::SeqCst);
        let auditing_live = self.live_auditing.load(Ordering::SeqCst);
        after_live();
        let rows_pending = self.pending.load(Ordering::SeqCst);
        InFlight { rows_pending, sinks_live, auditing_live, starting }
    }

    /// Refused rows that got no report of their own, per channel and kind.
    fn unreported(&self) -> Unreported {
        self.refusals.unreported()
    }

    /// [`Self::snapshot`], plus who the pending rows are and how many refused
    /// rows went unreported. Taken once, at the end of [`drain`].
    fn final_snapshot(&self) -> Drained {
        let in_flight = self.snapshot();
        let named = {
            let labels = self.labels.lock().unwrap_or_else(|p| p.into_inner());
            labels.values().take(NAMED_AT_SHUTDOWN).cloned().collect()
        };
        Drained { in_flight, named, unreported: self.unreported() }
    }

    /// Test builds only: the labels [`drain`] would name now, for a sink's
    /// tests outside this module.
    #[cfg(test)]
    pub(crate) fn named_pending(&self) -> Vec<String> {
        self.final_snapshot().named
    }
}

/// The daemon's. Every sink shares it, so the bound is on the load these
/// inserts put on the pool, not on each channel's share of it.
static LEDGER: Ledger =
    Ledger::new(Bounds { queued: MAX_QUEUED, connections: MAX_CONNECTIONS });

/// The daemon's ledger, for a sink's constructor (`SinkWriter::with_ledger`).
pub(crate) fn daemon_ledger() -> &'static Ledger {
    &LEDGER
}

/// One sink's way to write rows, and its lease on the ledger — see
/// `audit_sink_lease.rs`, which also has its constructors and its `Drop`.
/// Never `Clone`: a clone's drop would end a lease the other still holds.
pub(crate) struct SinkWriter {
    ledger: &'static Ledger,
    handle: tokio::runtime::Handle,
    pool: PgPool,
    /// Whose refused rows these are (#807).
    channel: SinkChannel,
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
    /// `on_failure` — a task cancelled with the runtime never runs it. A
    /// [`RowLabel`], so a worker-supplied id in it is always quoted (#802).
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
        label: RowLabel,
        on_failure: impl FnOnce(Unwritten<'_>) + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<()>, Unwritten<'static>> {
        self.spawn_around(actor, action, payload, label, on_failure, || {})
    }

    /// [`Self::spawn`], running `between` after the row is counted and before
    /// `closed` is read: the seam a test uses to close the ledger exactly
    /// inside the window the handshake guards (#802).
    fn spawn_around(
        &self,
        actor: &'static str,
        action: &'static str,
        payload: serde_json::Value,
        label: RowLabel,
        on_failure: impl FnOnce(Unwritten<'_>) + Send + 'static,
        between: impl FnOnce(),
    ) -> Result<tokio::task::JoinHandle<()>, Unwritten<'static>> {
        let (ledger, channel) = (self.ledger, self.channel);
        // Counts the row (see `Ledger`'s doc for why before `closed` is read).
        let row = PendingRow::new(ledger, label);
        between();
        // A refused row is counted as refused BEFORE it stops counting as
        // pending, so a snapshot between the two sees it twice, never neither.
        if ledger.closed.load(Ordering::SeqCst) {
            let why = refuse(ledger.refusals.late(channel), Unwritten::AfterShutdown, on_failure);
            drop(row);
            return Err(why);
        }
        let Ok(queued) = ledger.queued.try_acquire() else {
            let why = refuse(ledger.refusals.shed(channel), Unwritten::Shed, on_failure);
            drop(row);
            return Err(why);
        };
        let pool = self.pool.clone();
        Ok(self.handle.spawn(async move {
            // Dropped when the task ends however it ends, cancelled included.
            let _row = row;
            let _queued = queued;
            // Only a closed semaphore errs, and this one is never closed. Were
            // it closed, no insert could start any more — which is what
            // `AfterShutdown` says — so the row is refused like a late one:
            // counted, and reported unless thinned out (#802, #807).
            let Ok(_connection) = ledger.connections.acquire().await else {
                refuse(ledger.refusals.late(channel), Unwritten::AfterShutdown, on_failure);
                return;
            };
            if let Err(e) = kastellan_db::audit::insert(&pool, actor, action, payload).await {
                on_failure(Unwritten::Insert(&e));
            }
        }))
    }
}

/// One row counted in [`Ledger::pending`], and named in its label list, until
/// dropped. Counted BEFORE `closed` is read: see [`Ledger`]'s doc.
struct PendingRow {
    ledger: &'static Ledger,
    id: u64,
}

impl PendingRow {
    fn new(ledger: &'static Ledger, label: RowLabel) -> Self {
        ledger.pending.fetch_add(1, Ordering::SeqCst);
        let id = ledger.next_row.fetch_add(1, Ordering::SeqCst);
        ledger.labels.lock().unwrap_or_else(|p| p.into_inner()).insert(id, label.into_string());
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
    /// came up — no bus was built, so no reply was queued and it has no row
    /// to write.
    fn settled(&self) -> bool {
        self.rows_pending == 0 && self.sinks_live == 0
    }
}

/// What [`drain`] left behind: the last [`InFlight`], who the pending rows
/// are, and how many refused rows went unreported. Its fields are this
/// module's, and outside tests only [`drain`] makes one, so the baseline
/// [`close_then_report_unreported`] counts from is always the drain's own.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone, Default))]
pub(crate) struct Drained {
    in_flight: InFlight,
    /// Labels of the oldest pending rows, at most [`NAMED_AT_SHUTDOWN`].
    named: Vec<String>,
    /// Refused rows that never got a report of their own, so far, per
    /// channel and kind (#807).
    unreported: Unreported,
}

/// Shutdown: wait up to [`DRAIN_BOUND`] for the daemon's channel drivers to
/// exit and their rows to land, then close the ledger, so a row a late driver
/// tries after this is refused and reported ([`Unwritten::AfterShutdown`]).
/// Call it after the channels are stopped and before the pool closes; hand the
/// result to [`report_drained`], and then to [`close_then_report_unreported`].
pub(crate) async fn drain() -> Drained {
    drain_ledger(&LEDGER, DRAIN_BOUND).await
}

/// [`drain`] on `ledger`, bounded by `bound`.
async fn drain_ledger(ledger: &'static Ledger, bound: Duration) -> Drained {
    let deadline = tokio::time::Instant::now() + bound;
    while !ledger.snapshot().settled() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(DRAIN_POLL).await;
    }
    close_and_count(ledger, || {})
}

/// The end of [`drain_ledger`]: close first, then count (see `Ledger`'s doc),
/// running `between` in the window between the two — the seam a test uses to
/// spawn or count a row exactly there (#802).
fn close_and_count(ledger: &'static Ledger, between: impl FnOnce()) -> Drained {
    ledger.closed.store(true, Ordering::SeqCst);
    between();
    ledger.final_snapshot()
}

/// The refused rows thinned out since `drained` was taken — late rows a
/// still-running driver tried after the drain's snapshot (#802).
fn unreported_on_since(ledger: &'static Ledger, drained: &Drained) -> usize {
    ledger.unreported().total().saturating_sub(drained.unreported.total())
}

#[path = "audit_sink_lease.rs"]
mod lease;
pub(crate) use lease::{SinkChannel, Starting};

#[path = "audit_sink_thinning.rs"]
mod thinning;
use thinning::{refuse, Refusals};
pub(crate) use thinning::{BurstTally, UnsaidCounts, Unreported};

#[path = "audit_sink_report.rs"]
mod report;
pub(crate) use report::{
    close_then_report_unreported, emit_report, quoted_id, report_drained, Reporter, RowLabel,
    Unwritten,
};

/// Test builds only: a Postgres that never answers, for proving a sink does
/// not wait for its insert — and that it did try one. Shared by this module's
/// tests and each sink's own.
#[cfg(test)]
#[path = "audit_sink_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "audit_sink_tests.rs"]
mod tests;
