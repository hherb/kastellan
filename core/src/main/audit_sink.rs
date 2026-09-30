//! Writing an audit row from a polled channel driver's thread **without
//! stalling the driver** (#789), and without letting a flood of them starve
//! every other writer's audit rows.
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
//! once, through [`spawn_audit_insert`]. What that costs, stated where it is
//! paid:
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
//!   relative to the events, and `audit_log.ts` is the insert's time.
//! - **At shutdown.** A row still waiting or being written as the daemon
//!   shuts down can be lost with the runtime, and a cancelled insert runs no
//!   `on_failure`. The driver's own line for the event stands either way;
//!   #792 is for reporting these losses rather than only stating them.
//!
//! One helper for both channels (Matrix's `channel.reply_undelivered`, email's
//! `channel.skipped_ack_only`). Before #789 each sink hand-rolled its own
//! insert, and the two had already drifted: one spawned, one blocked.

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

/// The two bounds, as one value so a test can use its own.
pub(crate) struct Limits {
    queued: Semaphore,
    connections: Semaphore,
}

impl Limits {
    pub(crate) const fn new(queued: usize, connections: usize) -> Self {
        Self { queued: Semaphore::const_new(queued), connections: Semaphore::const_new(connections) }
    }
}

/// The daemon's. Every sink shares it, so the bound is on the load these
/// inserts put on the pool, not on each channel's share of it.
static LIMITS: Limits = Limits::new(MAX_QUEUED, MAX_CONNECTIONS);

/// Why a row was not written, as `on_failure` is told.
#[derive(Debug)]
pub(crate) enum Unwritten<'a> {
    /// The insert ran and failed.
    Insert(&'a kastellan_db::DbError),
    /// Never tried: the queue was full.
    Shed,
}

impl std::fmt::Display for Unwritten<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Insert(e) => e.fmt(f),
            Self::Shed => f.write_str(
                "shed: too many audit inserts already waiting for the pool (a flood of \
                 audited events, or a wedged Postgres)",
            ),
        }
    }
}

/// Insert one audit row on `handle`'s runtime, without waiting for it.
///
/// Returns at once — call it from a thread that must not block (the polled
/// driver's). `on_failure` is told of a row that was not written, with why: on
/// the runtime if the insert fails, or right here, on the caller's thread, if
/// the row is shed (see the module docs). It should log with enough to match
/// the row to the caller's own line for the event (a conversation, a message
/// id), and must not block either.
///
/// The returned handle is for tests, which await it to see the outcome; `None`
/// when the row was shed. In production it is dropped, which detaches the
/// task: it still runs.
pub(crate) fn spawn_audit_insert(
    handle: &tokio::runtime::Handle,
    pool: &PgPool,
    actor: &'static str,
    action: &'static str,
    payload: serde_json::Value,
    on_failure: impl FnOnce(Unwritten<'_>) + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_limited(&LIMITS, handle, pool, actor, action, payload, on_failure)
}

/// [`spawn_audit_insert`] against `limits`.
fn spawn_limited(
    limits: &'static Limits,
    handle: &tokio::runtime::Handle,
    pool: &PgPool,
    actor: &'static str,
    action: &'static str,
    payload: serde_json::Value,
    on_failure: impl FnOnce(Unwritten<'_>) + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    let Ok(queued) = limits.queued.try_acquire() else {
        on_failure(Unwritten::Shed);
        return None;
    };
    let pool = pool.clone();
    Some(handle.spawn(async move {
        let _queued = queued;
        // `Err` only for a closed semaphore, and these are never closed.
        let _connection = limits.connections.acquire().await.ok();
        if let Err(e) = kastellan_db::audit::insert(&pool, actor, action, payload).await {
            on_failure(Unwritten::Insert(&e));
        }
    }))
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
