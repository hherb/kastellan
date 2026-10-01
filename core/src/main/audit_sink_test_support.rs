//! Test builds only: the stalled-Postgres fixture for `audit_sink` and each
//! sink's own tests. Split out of `audit_sink.rs` to keep it under the 500-LOC
//! soft cap; `#[path]`-included there as `audit_sink::test_support`.

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
