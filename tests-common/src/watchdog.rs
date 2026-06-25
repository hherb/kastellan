//! Wall-clock watchdogs for async test steps.
//!
//! A test that `.await`s a future which never resolves hangs *forever*
//! at 0 % CPU — and because `cargo test` runs the threads of one binary
//! in parallel, a single stuck future can wedge the whole binary (and,
//! via the process-global serial lock, sibling tests too). The
//! `memory_layers_e2e` suite hit exactly this: its only unbounded await
//! is `pool.close()`, so a stuck close (or a starved single-worker
//! runtime under heavy multi-cluster load) presents as a silent hang
//! with no failing assertion to point at.
//!
//! [`await_within`] bounds any future with a timeout and turns a hang
//! into a loud, labelled panic. [`close_pool`] applies it to the common
//! `pool.close()` teardown so e2e tests fail fast and self-describe
//! instead of wedging.

use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;

/// Default cap applied by [`close_pool`].
///
/// A healthy `pool.close()` returns in milliseconds; 30 s is generous
/// headroom that still fails a genuinely-stuck close well inside a
/// CI step rather than letting it hang the run.
pub const DEFAULT_POOL_CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Await `fut`, panicking if it does not resolve within `timeout`.
///
/// `label` names the awaited step so the panic points straight at the
/// stuck operation (e.g. `"pool.close()"`). Returns the future's output
/// unchanged on the happy path, so it is a drop-in wrapper around any
/// `value = some_future.await`.
pub async fn await_within<F: Future>(label: &str, timeout: Duration, fut: F) -> F::Output {
    match tokio::time::timeout(timeout, fut).await {
        Ok(value) => value,
        Err(_) => panic!(
            "watchdog: `{label}` did not complete within {timeout:?} — the test was about to \
             hang. Suspect a connection held across pool.close() (an undropped PgListener), a \
             starved single-worker runtime under load, or a wedged external command."
        ),
    }
}

/// Close `pool` under an explicit watchdog `timeout`.
///
/// Drop-in for `pool.close().await` in tests: identical effect on the
/// happy path, but a stuck close panics with a clear message instead of
/// wedging the suite.
pub async fn close_pool_bounded(pool: &PgPool, timeout: Duration) {
    await_within("pool.close()", timeout, pool.close()).await;
}

/// Close `pool` under [`DEFAULT_POOL_CLOSE_TIMEOUT`].
pub async fn close_pool(pool: &PgPool) {
    close_pool_bounded(pool, DEFAULT_POOL_CLOSE_TIMEOUT).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The happy path is transparent: a ready future's value passes
    /// through unchanged.
    #[tokio::test]
    async fn await_within_returns_value_for_ready_future() {
        let got = await_within("ready", Duration::from_secs(5), async { 42 }).await;
        assert_eq!(got, 42, "a ready future's value must pass through");
    }

    /// A future that never resolves must trip the watchdog and panic
    /// (rather than hang the test), with a message naming the watchdog.
    #[tokio::test]
    #[should_panic(expected = "watchdog")]
    async fn await_within_panics_on_timeout() {
        // `std::future::pending` never resolves; the 50 ms cap must fire.
        await_within(
            "stuck",
            Duration::from_millis(50),
            std::future::pending::<()>(),
        )
        .await;
    }
}
