//! `pg_events`' test fixtures (their doc is on the `mod` line there). Split
//! out of `pg_events.rs` to keep it under the 500-LOC soft cap.

use std::time::Duration;

use super::LostRows;

/// A lost-row count of the test's own, so no other test's lost rows can
/// thin its lines or add to its counts. Leaked: the writers hold it as
/// `'static`, as production holds [`super::LOST_ROWS`].
pub(crate) fn own_lost_rows() -> &'static LostRows {
    Box::leak(Box::new(LostRows::new()))
}

/// A lazy pool aimed at a port nothing listens on, so every insert fails.
/// sqlx retries a refused connect until its acquire timeout, kept short
/// here (1 s).
pub(crate) fn refused_pool() -> sqlx::PgPool {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }; // dropped: the port now refuses
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .username("nobody")
        .database("nothing");
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(1))
        .connect_lazy_with(options)
}

/// A lazy pool whose every connection is accepted and **closed at once**,
/// during the startup handshake, with no Postgres. The insert fails fast
/// with a broken connection — not a refused one, which sqlx retries until
/// its acquire timeout ([`refused_pool`]). No statement is ever sent, but
/// `sqlx::Error` does not say which phase an `Io` error came from, so #858
/// calls it *unknown* — the over-hedge `outcome_of`'s doc describes, and
/// the same error a connection lost mid-statement gives. The accepting
/// thread lives as long as the test process.
///
/// Build it inside a runtime, as [`stalled_pool`].
pub(crate) fn closing_pool() -> sqlx::PgPool {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            // Read the client's first message before closing, so the close
            // is a FIN and the insert's error always EOF. Closed with it
            // unread, the close is a RST instead, and whether the client then
            // reads a reset or an EOF is a race: two causes (#859), and a
            // flood of them got a line for each, as two real faults would.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = std::io::Read::read(&mut stream, &mut [0u8; 1024]);
        }
    });
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .username("nobody")
        .database("nothing");
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy_with(options)
}

/// A lazy pool whose every insert **hangs** until its 30 s acquire
/// timeout, long after any test's stop: a listener that takes the TCP
/// handshake and never answers Postgres's startup message — a wedged
/// Postgres, with no Postgres. The insert is still awaited when the test
/// stops its task, which is the case #813 is about.
///
/// The listener is returned so it outlives the test's calls, and so
/// [`connected`] can tell the test the insert has started. (The daemon
/// binary's `audit_sink::test_support` has a variant of this fixture —
/// a shorter timeout, a synchronous wait — since a binary's test items
/// are not visible to the library's.)
///
/// Build it inside a runtime: a lazy pool spawns its maintenance task as
/// it is made.
pub(crate) fn stalled_pool() -> (sqlx::PgPool, std::net::TcpListener) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("non-blocking listener");
    let port = listener.local_addr().expect("addr").port();
    let options = sqlx::postgres::PgConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .username("nobody")
        .database("nothing");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(30))
        .connect_lazy_with(options);
    (pool, listener)
}

/// Wait (up to 5 s) until something connects to `listener`: the insert
/// against [`stalled_pool`] is under way. Async, and sleeping between
/// polls, so the task making the connection can run on the same runtime.
///
/// Returns the accepted stream, which the caller must **hold** until the
/// stop it is testing: dropped, it would close the connection, and the
/// insert would see an EOF — an error that could return before the stop
/// — rather than a Postgres that never answers.
#[must_use = "dropping the stream closes the connection the insert is waiting on"]
pub(crate) async fn connected(listener: &std::net::TcpListener) -> std::net::TcpStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("accept on the stalled pool's listener: {e}"),
        }
        assert!(std::time::Instant::now() < deadline, "the insert never connected");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
