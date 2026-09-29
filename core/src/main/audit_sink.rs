//! Writing an audit row from a polled channel driver's thread **without
//! stalling the driver** (#789).
//!
//! The polled channel driver (`kastellan_core::channel::polled_driver`) is
//! DB-free by design. It calls its audit hooks synchronously, on its own std
//! thread — the thread every conversation, every poll and every ack waits on.
//! A hook that `block_on`s its insert therefore stalls the whole channel
//! whenever Postgres is slow or unreachable: up to a pool-acquire timeout
//! (sqlx's default is 30 s) per audited event, multiplied by every event in a
//! batch.
//!
//! So the daemon's hooks **spawn** the insert onto the runtime and return at
//! once. The cost is stated where it is paid: a row still being written as the
//! daemon shuts down can be lost with the runtime. The driver's own log line
//! for the event stands either way, and a failed insert is logged by the
//! `on_failure` callback with whatever lets a reader match it to that line.
//!
//! One helper for both channels (Matrix's `channel.reply_undelivered`, email's
//! `channel.skipped_ack_only`). Before #789 each sink hand-rolled its own
//! insert, and the two had already drifted: one spawned, one blocked.

use sqlx::PgPool;

/// Insert one audit row on `handle`'s runtime, without waiting for it.
///
/// Returns at once — call it from a thread that must not block (the polled
/// driver's). `on_failure` runs on the runtime if the insert fails; it should
/// log the failure with enough to match it to the caller's own line for the
/// event (a conversation, a message id).
///
/// The returned handle is for tests, which await it to see the outcome. In
/// production it is dropped, which detaches the task: it still runs.
pub(crate) fn spawn_audit_insert(
    handle: &tokio::runtime::Handle,
    pool: &PgPool,
    actor: &'static str,
    action: &'static str,
    payload: serde_json::Value,
    on_failure: impl FnOnce(&kastellan_db::DbError) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let pool = pool.clone();
    handle.spawn(async move {
        if let Err(e) = kastellan_db::audit::insert(&pool, actor, action, payload).await {
            on_failure(&e);
        }
    })
}

/// Test builds only: a Postgres that never answers, for proving a sink does
/// not wait for its insert. Shared by this module's test and each sink's own.
#[cfg(test)]
pub(crate) mod test_support {
    use sqlx::PgPool;
    use std::time::{Duration, Instant};

    /// How long the stalled pool waits for a connection before giving up.
    /// Long enough that a blocking call is unmistakable, short enough to keep
    /// the test quick.
    pub(crate) const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1500);

    /// A pool whose every connection attempt **hangs**: a TCP listener that
    /// completes the handshake (the kernel does that, from the backlog) and
    /// then never answers Postgres's startup message. That is a slow or wedged
    /// Postgres, with no Postgres. The listener is returned so it outlives the
    /// test's calls.
    ///
    /// Build it inside `rt` (`rt.block_on(async { stalled_pool() })`): a lazy
    /// pool spawns its maintenance task as it is made, and panics with no
    /// runtime to spawn it on.
    pub(crate) fn stalled_pool() -> (PgPool, std::net::TcpListener) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port");
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
    pub(crate) fn assert_returns_at_once(what: &str, call: impl FnOnce()) {
        let t = Instant::now();
        call();
        let took = t.elapsed();
        assert!(
            took < ACQUIRE_TIMEOUT / 3,
            "{what} must return at once, not wait for its insert (#789): took {took:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{assert_returns_at_once, stalled_pool, ACQUIRE_TIMEOUT};
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

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

        let failed: Arc<Mutex<Option<String>>> = Arc::default();
        let seen = failed.clone();
        let mut task = None;
        assert_returns_at_once("spawn_audit_insert", || {
            task = Some(spawn_audit_insert(rt.handle(), &pool, "a", "b", serde_json::json!({}), move |e| {
                *seen.lock().unwrap() = Some(e.to_string());
            }));
        });
        rt.block_on(task.expect("spawned")).expect("the insert task ran to completion");
        assert!(failed.lock().unwrap().is_some(), "a failed insert reaches `on_failure`");
    }
}
