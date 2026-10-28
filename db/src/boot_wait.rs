//! Waiting for Postgres to accept connections at daemon boot (issue [#909]).
//!
//! # Why boot waits at all
//!
//! The daemon's first database dial used to be its only one: a refusal
//! propagated straight to a non-zero exit. On a host reboot that loses a race
//! the daemon cannot win by itself. `kastellan-postgres` is a `Type=simple`
//! systemd unit, so core's `After=kastellan-postgres.service` orders only the
//! *exec* of `postgres`, not the moment it starts listening, and launchd has
//! no ordering at all. Measured on the DGX reboot of 2026-10-25: core's first
//! start failed with `Connection refused (os error 111)`, and
//! `Restart=on-failure` recovered it 5 s later. That works, but a reboot then
//! looks like a crash in the restart count and the logs.
//!
//! # What is waited for, and what is not
//!
//! Three answers mean "Postgres is not up **yet**" ([`not_up_yet`]):
//!
//! - **connection refused**: a socket file is there (typically a stale one
//!   left by an unclean stop) but nothing listens on it yet. This is the
//!   measured one.
//! - **no such file**: the socket file has not been created yet.
//! - **SQLSTATE `57P03`** (`cannot_connect_now`): the postmaster answered,
//!   but it is still starting up or replaying WAL after a crash.
//!
//! Each is retried for up to [`BOOT_WAIT_BUDGET`]. Every other error refuses
//! at once, exactly as before: a peer-auth rejection, a missing role or a
//! protocol error will not fix itself by waiting. When the budget runs out the
//! last error is returned, so the daemon's refusal is unchanged; the wait only
//! stops a start race from looking like an outage.
//!
//! Two failures that look like the ones above refuse at once instead:
//!
//! - a missing socket **directory**. Postgres never creates it
//!   (`kastellan-db-init` does, once), so its absence is a wrong data dir or
//!   an uninitialised cluster, which no amount of waiting fixes.
//! - a dial that gets no answer within [`BOOT_DIAL_TIMEOUT`]. A starting
//!   postmaster answers at once (with `57P03` if it is not ready), so a
//!   silent one is wedged, not starting.
//!
//! This is the same shape as the guard backend's `/props` wait (#609,
//! `core/src/cassandra/guard_model/tier/props_wait.rs`).
//!
//! [#909]: https://github.com/hherb/kastellan/issues/909

use std::future::Future;
use std::io;
use std::time::Duration;

use sqlx::{Connection, PgConnection};
// tokio's clock, not std's: a paused test runtime then moves it, so the
// budget arithmetic can be pinned to the millisecond (#915).
use tokio::time::Instant;

use crate::conn::ConnectSpec;
use crate::DbError;

/// How long boot keeps dialling a Postgres that is not up yet.
///
/// 60 s: a clean start takes well under a second, so this leaves room for
/// crash recovery replaying WAL (which answers `57P03` the whole time).
/// It is still short enough that a cluster that will never come up costs one
/// minute per restart instead of hiding the refusal.
pub const BOOT_WAIT_BUDGET: Duration = Duration::from_secs(60);

/// Pause between dials. A refused UDS dial costs microseconds, so the pause
/// is set by how many log lines a slow start writes, not by load.
pub const BOOT_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Longest a single dial may go unanswered. Without it a postmaster that
/// accepts the socket and never answers would hang boot past any budget.
pub const BOOT_DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// SQLSTATE `cannot_connect_now`: "the database system is starting up",
/// "… is not yet accepting connections", "… is shutting down".
const CANNOT_CONNECT_NOW: &str = "57P03";

/// Is this dial failure a Postgres that is not up **yet**? Pure.
///
/// Takes the error's parts, not the error, so that every arm can be unit
/// tested: a `sqlx` database error cannot be built by hand. [`not_up_yet`]
/// pulls the parts out of a real error.
pub fn not_up_yet_parts(io_kind: Option<io::ErrorKind>, sqlstate: Option<&str>) -> bool {
    matches!(io_kind, Some(io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound))
        || sqlstate == Some(CANNOT_CONNECT_NOW)
}

/// [`not_up_yet_parts`] for a real `sqlx` error.
pub fn not_up_yet(err: &sqlx::Error) -> bool {
    let io_kind = match err {
        sqlx::Error::Io(e) => Some(e.kind()),
        _ => None,
    };
    let sqlstate = err.as_database_error().and_then(|d| d.code());
    not_up_yet_parts(io_kind, sqlstate.as_deref())
}

/// Dial until one dial succeeds, while the failures are [`not_up_yet`] and
/// the next attempt still starts inside `budget`. Returns the last error
/// otherwise.
///
/// Generic over the dial so that the loop can be tested without a server.
/// The IO lives in the caller's `dial` closure; [`wait_for_postgres`] is the
/// production one. The budget limits when a new attempt may *start*, so the
/// last attempt can overrun it by however long that dial takes
/// ([`wait_for_postgres`] caps each dial at [`BOOT_DIAL_TIMEOUT`]).
pub async fn wait_until_accepting<F, Fut>(
    mut dial: F,
    budget: Duration,
    interval: Duration,
) -> Result<(), sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), sqlx::Error>>,
{
    let started = Instant::now();
    let mut attempt = 1u32;
    loop {
        let err = match dial().await {
            Ok(()) => {
                if attempt > 1 {
                    tracing::info!(
                        attempts = attempt,
                        waited_ms = started.elapsed().as_millis() as u64,
                        "postgres is accepting connections"
                    );
                }
                return Ok(());
            }
            Err(e) => e,
        };
        if !not_up_yet(&err) {
            return Err(err);
        }
        if started.elapsed() + interval >= budget {
            // The refusal that follows names only the last error. This line
            // is what records that it was waited for, and for how long.
            tracing::warn!(
                attempts = attempt,
                waited_ms = started.elapsed().as_millis() as u64,
                error = %err,
                "postgres never accepted a connection inside the boot wait budget"
            );
            return Err(err);
        }
        tracing::warn!(
            attempt,
            error = %err,
            retry_in_ms = interval.as_millis() as u64,
            budget_ms = budget.as_millis() as u64,
            "postgres not accepting connections yet; waiting"
        );
        tokio::time::sleep(interval).await;
        attempt += 1;
    }
}

/// Wait, at daemon boot, until the cluster in `spec` accepts a connection.
///
/// Dials the maintenance database (`postgres`), because that one exists
/// before the application database has been created. Call it before
/// [`crate::probe::run`]. Returns once a dial succeeds; otherwise returns
/// the error that ended the wait (see the module docs).
pub async fn wait_for_postgres(spec: &ConnectSpec) -> Result<(), DbError> {
    let dir = &spec.socket_dir;
    // `Ok(false)` only: a directory we may not stat is left for the dial to report.
    if matches!(dir.try_exists(), Ok(false)) {
        return Err(DbError::Connect(format!(
            "socket directory {} does not exist; check KASTELLAN_DATA_DIR and that the cluster was initialised",
            dir.display()
        )));
    }
    let opts = spec.for_maintenance_db().to_pg_connect_options();
    wait_until_accepting(
        || async {
            let conn = tokio::time::timeout(BOOT_DIAL_TIMEOUT, PgConnection::connect_with(&opts))
                .await
                .map_err(|_| {
                    sqlx::Error::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("no answer within {} s", BOOT_DIAL_TIMEOUT.as_secs()),
                    ))
                })??;
            // Closing politely is not part of the answer, so its error is ignored.
            let _ = conn.close().await;
            Ok(())
        },
        BOOT_WAIT_BUDGET,
        BOOT_RETRY_INTERVAL,
    )
    .await
    .map_err(|e| DbError::Connect(format!("{e} (socket directory {})", dir.display())))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::{Path, PathBuf};

    use kastellan_tests_common::{unique_temp_root, PathGuard};
    use sqlx::postgres::{PgConnectOptions, PgSslMode};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::*;

    #[test]
    fn only_a_starting_postgres_is_waited_for() {
        use io::ErrorKind::*;
        assert!(not_up_yet_parts(Some(ConnectionRefused), None), "the measured DGX race");
        assert!(not_up_yet_parts(Some(NotFound), None), "the socket file is not there yet");
        assert!(not_up_yet_parts(None, Some("57P03")), "starting up / recovering");
        for kind in [PermissionDenied, ConnectionReset, TimedOut, InvalidData, Other] {
            assert!(!not_up_yet_parts(Some(kind), None), "{kind:?}");
        }
        // 28000 = peer auth refused, 3D000 = no such database, 53300 = too many connections.
        for code in ["28000", "3D000", "53300"] {
            assert!(!not_up_yet_parts(None, Some(code)), "{code}");
        }
        assert!(!not_up_yet_parts(None, None));
    }

    fn refused(n: u32) -> sqlx::Error {
        sqlx::Error::Io(io::Error::new(io::ErrorKind::ConnectionRefused, format!("attempt {n}")))
    }

    const FAST: Duration = Duration::from_millis(5);

    #[tokio::test]
    async fn a_refused_dial_is_waited_out() {
        let calls = Cell::new(0);
        wait_until_accepting(
            || {
                calls.set(calls.get() + 1);
                let n = calls.get();
                async move { if n < 3 { Err(refused(n)) } else { Ok(()) } }
            },
            Duration::from_secs(5),
            FAST,
        )
        .await
        .expect("the third dial is accepted");
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn any_other_error_refuses_on_the_first_dial() {
        let calls = Cell::new(0);
        let err = wait_until_accepting(
            || {
                calls.set(calls.get() + 1);
                async { Err(sqlx::Error::Protocol("not postgres".into())) }
            },
            Duration::from_secs(5),
            FAST,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, sqlx::Error::Protocol(_)), "{err:?}");
        assert_eq!(calls.get(), 1, "waiting will not turn this into Postgres");
    }

    /// On a paused clock the arithmetic is exact. Dials start at 0, 100, …,
    /// 1100 ms; one at 1200 ms would not start *inside* a 1200 ms budget, so
    /// the wait ends after the 12th, at 1100 ms. The budget is a multiple of
    /// the interval on purpose: that is the one case where `>=` and `>` (or a
    /// check without `+ interval`) differ — both make 13 dials and end at 1200.
    ///
    /// Each attempt's error names its number, so "the last error" is proven
    /// rather than any refusal passing. The fake yields like I/O, and the whole
    /// wait sits under a timeout, so a mutant that drops the budget check is a
    /// red test rather than a hung binary.
    #[tokio::test(start_paused = true)]
    async fn a_postgres_that_never_comes_up_is_refused_once_the_budget_is_spent() {
        let calls = Cell::new(0);
        let started = Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(60),
            wait_until_accepting(
                || {
                    calls.set(calls.get() + 1);
                    let n = calls.get();
                    async move {
                        tokio::task::yield_now().await;
                        Err(refused(n))
                    }
                },
                Duration::from_millis(1200),
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("the budget ends the wait")
        .unwrap_err();
        let n = calls.get();
        assert_eq!(err.to_string(), refused(n).to_string(), "the last error, not the first");
        assert_eq!(n, 12, "one dial per interval, none past the budget");
        assert_eq!(started.elapsed(), Duration::from_millis(1100), "ends at the last dial");
    }

    /// The budget counts the dials' own time, not just the pauses: 250 ms
    /// dials paced 100 ms apart start at 0, 350 and 700 ms; one at 1050 ms
    /// would not start inside a 1000 ms budget. A loop that summed only its
    /// sleeps would make 10 dials. Under a timeout for the same reason as above.
    #[tokio::test(start_paused = true)]
    async fn slow_dials_spend_the_budget_too() {
        let calls = Cell::new(0);
        let started = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(60),
            wait_until_accepting(
                || {
                    calls.set(calls.get() + 1);
                    let n = calls.get();
                    async move {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        Err(refused(n))
                    }
                },
                Duration::from_millis(1000),
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("the budget ends the wait")
        .unwrap_err();
        assert_eq!(calls.get(), 3);
        assert_eq!(started.elapsed(), Duration::from_millis(950), "the third dial's end");
    }

    // The tests below drive the real `sqlx` client, because the hand-built
    // errors above cannot stand in for what it actually returns.

    fn tcp_opts(port: u16) -> PgConnectOptions {
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .username("kastellan")
            .database("postgres")
            .ssl_mode(PgSslMode::Disable)
    }

    /// A port with nothing listening on it gives the same refusal as the
    /// DGX race.
    #[tokio::test]
    async fn sqlx_reports_a_refused_dial_as_not_up_yet() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        }; // dropped, so nothing listens on it now
        let err = PgConnection::connect_with(&tcp_opts(port)).await.unwrap_err();
        assert!(not_up_yet(&err), "{err:?}");
    }

    /// A fresh, existing socket directory, removed when the guard drops.
    fn socket_dir() -> (PathBuf, PathGuard) {
        let dir = unique_temp_root("bw");
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), PathGuard { path: dir })
    }

    fn uds_opts(dir: &Path) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(&dir.to_string_lossy())
            .username("kastellan")
            .database("postgres")
    }

    fn spec_for(dir: &Path) -> ConnectSpec {
        ConnectSpec {
            socket_dir: dir.to_path_buf(),
            user: "kastellan".into(),
            database: crate::conn::DEFAULT_APPLICATION_DB.into(),
        }
    }

    /// `ConnectSpec` dials a socket directory. A directory with no
    /// `.s.PGSQL.5432` in it yet is a Postgres that has not started.
    #[tokio::test]
    async fn sqlx_reports_a_missing_socket_as_not_up_yet() {
        let (dir, _guard) = socket_dir();
        let err = PgConnection::connect_with(&uds_opts(&dir)).await.unwrap_err();
        assert!(not_up_yet(&err), "{err:?}");
    }

    /// The measured DGX race, on the transport it happened on: a socket file
    /// left behind with nothing listening on it.
    #[tokio::test]
    async fn sqlx_reports_a_stale_unix_socket_as_not_up_yet() {
        let (dir, _guard) = socket_dir();
        drop(std::os::unix::net::UnixListener::bind(dir.join(".s.PGSQL.5432")).unwrap());
        let err = PgConnection::connect_with(&uds_opts(&dir)).await.unwrap_err();
        assert!(not_up_yet(&err), "{err:?}");
    }

    /// Postgres never creates its socket directory, so a missing one is
    /// refused at once rather than waited on for the whole budget.
    #[tokio::test]
    async fn a_missing_socket_directory_is_refused_at_once() {
        let dir = unique_temp_root("bw-none");
        let started = Instant::now();
        let err = wait_for_postgres(&spec_for(&dir)).await.unwrap_err().to_string();
        assert!(started.elapsed() < BOOT_RETRY_INTERVAL, "no wait: {:?}", started.elapsed());
        assert!(err.contains(&dir.display().to_string()), "names the directory: {err}");
    }

    /// The postmaster's `ErrorResponse` for a FATAL error with `code`.
    fn fatal(code: &str, message: &str) -> Vec<u8> {
        let mut fields = Vec::new();
        for (tag, value) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', code), (b'M', message)] {
            fields.push(tag);
            fields.extend_from_slice(value.as_bytes());
            fields.push(0);
        }
        fields.push(0);
        let mut msg = vec![b'E'];
        msg.extend_from_slice(&(fields.len() as i32 + 4).to_be_bytes());
        msg.extend_from_slice(&fields);
        msg
    }

    /// Read the client's StartupMessage body, first declining TLS if it
    /// asks (sqlx's default `Prefer` does, even over a Unix socket).
    async fn read_startup<S: AsyncRead + AsyncWrite + Unpin>(sock: &mut S) -> Vec<u8> {
        const SSL_REQUEST: [u8; 4] = 80877103i32.to_be_bytes();
        loop {
            let len = sock.read_i32().await.unwrap() as usize;
            let mut body = vec![0u8; len - 4];
            sock.read_exact(&mut body).await.unwrap();
            if body != SSL_REQUEST {
                return body;
            }
            sock.write_all(b"N").await.unwrap();
        }
    }

    /// A server that answers the startup message the way a starting
    /// postmaster does: `FATAL 57P03 the database system is starting up`.
    async fn starting_up_server(listener: tokio::net::TcpListener) {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_startup(&mut sock).await;
        let _ = sock.write_all(&fatal(CANNOT_CONNECT_NOW, "the database system is starting up")).await;
    }

    /// The production dial goes to the maintenance database (the application
    /// one may not exist yet), and its refusal names where it dialled.
    #[tokio::test]
    async fn wait_for_postgres_dials_the_maintenance_db_and_names_the_socket_dir() {
        let (dir, _guard) = socket_dir();
        let listener = tokio::net::UnixListener::bind(dir.join(".s.PGSQL.5432")).unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let startup = read_startup(&mut sock).await;
            let _ = sock.write_all(&fatal("28000", "peer authentication failed")).await;
            startup
        });
        let err = wait_for_postgres(&spec_for(&dir)).await.unwrap_err().to_string();
        let startup = server.await.unwrap();
        let wanted = format!("database\0{}\0", crate::conn::MAINTENANCE_DB);
        assert!(
            startup.windows(wanted.len()).any(|w| w == wanted.as_bytes()),
            "startup message: {:?}",
            String::from_utf8_lossy(&startup)
        );
        assert!(err.contains("28000") || err.contains("peer authentication"), "{err}");
        assert!(err.contains(&dir.display().to_string()), "names the directory: {err}");
    }

    /// A postmaster that accepts the socket and never answers is wedged, not
    /// starting: the dial times out and the wait ends instead of hanging boot.
    #[tokio::test]
    async fn a_postmaster_that_never_answers_ends_the_wait() {
        let (dir, _guard) = socket_dir();
        let listener = tokio::net::UnixListener::bind(dir.join(".s.PGSQL.5432")).unwrap();
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let err = tokio::time::timeout(BOOT_DIAL_TIMEOUT * 2, wait_for_postgres(&spec_for(&dir)))
            .await
            .expect("the dial timeout ends the wait")
            .unwrap_err()
            .to_string();
        server.abort();
        assert!(err.contains("no answer within"), "{err}");
    }

    #[tokio::test]
    async fn sqlx_reports_starting_up_as_not_up_yet() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(starting_up_server(listener));
        let err = PgConnection::connect_with(&tcp_opts(port)).await.unwrap_err();
        server.await.unwrap();
        assert!(not_up_yet(&err), "{err:?}");
    }
}
