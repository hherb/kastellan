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
pub(crate) mod test_support {
    use sqlx::PgPool;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// How long the stalled pool waits for a connection before giving up.
    /// Long enough that a blocking call is unmistakable, short enough to keep
    /// the test quick.
    pub(crate) const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1500);

    /// A pool whose every connection attempt **hangs**: a TCP listener that
    /// completes the handshake (the kernel does that, from the backlog) and
    /// then never answers Postgres's startup message. That is a slow or wedged
    /// Postgres, with no Postgres. The listener is returned so it outlives the
    /// test's calls, and so [`assert_insert_attempted`] can ask it.
    ///
    /// Build it inside `rt` (`rt.block_on(async { stalled_pool() })`): a lazy
    /// pool spawns its maintenance task as it is made, and panics with no
    /// runtime to spawn it on.
    pub(crate) fn stalled_pool() -> (PgPool, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        listener.set_nonblocking(true).expect("a non-blocking listener");
        let port = listener.local_addr().expect("local addr").port();
        let options = sqlx::postgres::PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .username("nobody")
            .database("nothing");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_lazy_with(options);
        (pool, listener)
    }

    /// Assert that `call` returns well inside [`ACQUIRE_TIMEOUT`] — that it
    /// did not wait for an insert against the stalled pool.
    ///
    /// The bound is two thirds of the timeout, not a few milliseconds: a call
    /// that waits takes the WHOLE timeout, so two thirds separates the two with
    /// room to spare on a loaded host.
    pub(crate) fn assert_returns_at_once(what: &str, call: impl FnOnce()) {
        let t = Instant::now();
        call();
        let took = t.elapsed();
        assert!(
            took < ACQUIRE_TIMEOUT * 2 / 3,
            "{what} must return at once, not wait for its insert (#789): took {took:?}"
        );
    }

    /// Whether anything connected to `listener` within `within`.
    pub(crate) fn connected_within(listener: &TcpListener, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            match listener.accept() {
                Ok(_) => return true,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("accept on the stalled pool's listener: {e}"),
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Assert that an insert was actually attempted against the stalled pool:
    /// a sink that returns at once by writing nothing passes
    /// [`assert_returns_at_once`] too. Its positive control — the pool opens
    /// no connection unasked — is this module's
    /// `a_stalled_pool_connects_only_for_an_insert`.
    pub(crate) fn assert_insert_attempted(what: &str, listener: &TcpListener) {
        assert!(
            connected_within(listener, ACQUIRE_TIMEOUT),
            "{what} must actually try its insert: nothing connected to the pool"
        );
    }
}

#[cfg(test)]
mod tests {
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

    /// #789: the insert does not hold the caller. The positive control first:
    /// the same insert, **waited for** from the same kind of thread (as the
    /// email sink did with `block_on`), takes the whole acquire timeout — so
    /// the fixture really stalls, and the fast return below is the helper's
    /// doing. Then the helper's failure callback does run, once the pool
    /// gives up.
    #[test]
    fn the_insert_does_not_hold_the_calling_thread() {
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        // Made inside the runtime, but this thread stays outside it — like the
        // driver thread that calls the real sinks.
        let (pool, _listener) = rt.block_on(async { stalled_pool() });

        let t = Instant::now();
        let blocked = rt.block_on(kastellan_db::audit::insert(&pool, "a", "b", serde_json::json!({})));
        assert!(blocked.is_err(), "POSITIVE CONTROL: the stalled pool must fail the insert");
        assert!(
            t.elapsed() >= ACQUIRE_TIMEOUT,
            "POSITIVE CONTROL: waiting for the insert must take the acquire timeout, or this \
             fixture does not stall and the assertion below proves nothing: {:?}",
            t.elapsed()
        );

        let (told, on_failure) = recorder();
        let mut task = None;
        assert_returns_at_once("spawn_audit_insert", || {
            task = spawn_audit_insert(rt.handle(), &pool, "a", "b", serde_json::json!({}), on_failure);
        });
        rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
        assert!(told.lock().unwrap().is_some(), "a failed insert reaches `on_failure`");
    }

    /// The positive control for [`assert_insert_attempted`]: the stalled pool
    /// opens no connection of its own accord, so a connection means an insert
    /// was tried.
    #[test]
    fn a_stalled_pool_connects_only_for_an_insert() {
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let (pool, listener) = rt.block_on(async { stalled_pool() });
        assert!(
            !connected_within(&listener, Duration::from_millis(300)),
            "POSITIVE CONTROL: the lazy pool must not connect before it is used"
        );
        let task = spawn_audit_insert(rt.handle(), &pool, "a", "b", serde_json::json!({}), |_| {});
        assert_insert_attempted("spawn_audit_insert", &listener);
        rt.block_on(task.expect("not shed")).expect("the insert task ran to completion");
    }

    /// Past the queue bound a row is shed — at once, on the caller's thread,
    /// and said so through `on_failure` — and the slot comes back once the
    /// queued insert gives up.
    #[test]
    fn past_the_queue_bound_a_row_is_shed_and_said_so() {
        static ONE_SLOT: Limits = Limits::new(1, 1);
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let (pool, _listener) = rt.block_on(async { stalled_pool() });
        let spawn = |on_failure: OnFailure| {
            spawn_limited(&ONE_SLOT, rt.handle(), &pool, "a", "b", serde_json::json!({}), on_failure)
        };

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

        rt.block_on(queued).expect("the queued insert ran to completion");
        let again = spawn(Box::new(|_: Unwritten<'_>| {})).expect("the slot comes back");
        rt.block_on(again).expect("the insert task ran to completion");
    }

    /// At most `connections` inserts use the pool at once, the rest wait: two
    /// stalled inserts through one connection take two acquire timeouts, one
    /// after the other. The control: through two, they take one.
    #[test]
    fn inserts_share_a_bounded_number_of_connections() {
        static ONE_CONNECTION: Limits = Limits::new(2, 1);
        static TWO_CONNECTIONS: Limits = Limits::new(2, 2);
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let (pool, _listener) = rt.block_on(async { stalled_pool() });
        let both = |limits: &'static Limits| {
            let t = Instant::now();
            let tasks: Vec<_> = (0..2)
                .map(|_| {
                    spawn_limited(limits, rt.handle(), &pool, "a", "b", serde_json::json!({}), |_| {})
                        .expect("not shed")
                })
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
}
