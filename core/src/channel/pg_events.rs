//! The channel bus's real DB seam, [`PgChannelEvents`]: enqueue a channel
//! task, write one of the bus's audit rows (`channel.*`, and `ask.resolved`)
//! — and **say so on the `[audit-lost]` marker when that row is not written**
//! (#808). Since #814 the channel boot supervisor's sink writes its
//! `channel.started`/`boot_failed`/`died` rows through the same
//! [`audit_or_report`], under its own `[audit-lost]` writer.
//!
//! Split out of `bus.rs` (over the 500-LOC soft cap) when its failure report
//! grew a seam of its own; `bus` re-exports it, so the path
//! `channel::bus::PgChannelEvents` is unchanged.
//!
//! Before #808 a failed insert here was a `tracing` WARN only: no stderr
//! fallback, no marker. Under `RUST_LOG=error` the loss was invisible — and
//! an alert keyed on `[audit-lost]` is promised every row the audit trail is
//! missing, whatever the operator's `RUST_LOG`
//! (`worker_stderr/report/audit_lost.rs`; at the default `RUST_LOG` only since
//! #828, before which the traced line carried no marker). The daemon's own
//! sinks for the same action (`channel.reply_undelivered`, from a polled
//! driver) already reported their failed inserts; this writer is the bus's
//! half.
//!
//! Since #813 that includes an insert still awaited when the bus is stopped:
//! `ChannelBus::shutdown` aborts its pumps, the awaited future is dropped and
//! no `Err` arm runs, so a drop guard says it instead — as a row that *may*
//! not have been written. What is still unsaid: a row lost to a crash, and a
//! stop that lands on any *other* await of a pump — an inbound message's
//! `enqueue` or pairing lookup, a reply still in a channel's queue (#832).
//! Separately, these lines are not thinned: one per lost row, where the
//! daemon's sinks thin their shed and after-shutdown refusals (#829).

use serde_json::Value;

use kastellan_db::tasks::{self, Lane};

use super::audit_text::quoted_id;
use super::bus::ChannelEvents;
use crate::worker_stderr::{emit_audit_lost_report, AuditLostWriter};

/// The `actor` every row [`audit_or_report`] writes is under: the bus's, and
/// since #814 the boot supervisor's.
const ACTOR: &str = "channel";

/// Real DB-backed `ChannelEvents` over the runtime pool.
pub struct PgChannelEvents {
    pool: sqlx::PgPool,
    /// How a lost row is said: [`emit_report`], but for a test's recorder.
    report: Reporter,
}

impl PgChannelEvents {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool, report: emit_report }
    }

    /// Test builds only: lost rows said to `report`, so a test can read them
    /// through the trait method production calls.
    #[cfg(test)]
    pub(crate) fn with_reporter(pool: sqlx::PgPool, report: Reporter) -> Self {
        Self { pool, report }
    }
}

#[async_trait::async_trait]
impl ChannelEvents for PgChannelEvents {
    async fn enqueue(&self, lane: Lane, payload: Value) -> anyhow::Result<i64> {
        Ok(tasks::insert_pending(&self.pool, lane, payload).await?)
    }

    /// Best-effort, as the trait says — never fatal — and a row whose insert
    /// fails is reported on `[audit-lost]` (#808), as is one still awaited
    /// when the bus stops (#813).
    async fn audit(&self, action: &str, payload: Value) {
        audit_or_report(&self.pool, AuditLostWriter::Bus, action, payload, self.report).await;
    }
}

/// How a lost row is said: [`emit_report`] in production, a recorder in a
/// test. A `fn` pointer, because the emitter's own record is a `tracing`
/// event, which a test can read only through a scoped subscriber — and those
/// flake on the interest cache.
pub(crate) type Reporter = fn(AuditLostWriter, &str);

/// The production [`Reporter`]. Whether the stderr fallback was written is
/// dropped: nothing more can be done for a row the report itself could not
/// reach anyone about (the daemon's sinks drop it for the same reason).
pub(crate) fn emit_report(writer: AuditLostWriter, line: &str) {
    emit_audit_lost_report(writer, line);
}

/// Insert one `channel`-actor audit row; report it through `report`, as
/// `writer`'s, if the insert fails or is abandoned mid-await.
///
/// Two writers share it: the bus ([`AuditLostWriter::Bus`]) and, since #814,
/// the channel boot supervisor's sink
/// ([`AuditLostWriter::ChannelSupervisor`]) — one shape of lost-row line for
/// every `channel.*` row.
///
/// What the row is about is read **before** the insert, which consumes the
/// payload. Only the fields [`describe_row`] names are read, so nothing else
/// in the payload — a message body, a token, a bring-up `cause` — can reach
/// the line.
///
/// **A row whose insert never returns is reported too** (#813). When the bus
/// stops, it aborts its pumps, and a pump parked on this `await` has its
/// future dropped: no `Err` ever arrives. Under a wedged Postgres an insert
/// takes up to the pool's acquire timeout (10 s), or longer on a connection
/// already acquired, since no statement timeout bounds it — so a pump with a
/// row to write is likely parked here when the bus stops. The
/// [`PendingInsert`] guard says so from its `Drop`, which runs on
/// cancellation (a dropped future is not an unwind, so `panic = "abort"` does
/// not void it).
pub(crate) async fn audit_or_report(
    pool: &sqlx::PgPool,
    writer: AuditLostWriter,
    action: &str,
    payload: Value,
    report: Reporter,
) {
    let about = describe_row(&payload);
    let insert = kastellan_db::audit::insert(pool, ACTOR, action, payload);
    settle_or_report(insert, writer, action, &about, report).await;
}

/// Await `insert` under a [`PendingInsert`] guard: the one place a row's
/// outcome is said. Generic over the insert so a test can drive every
/// outcome — `Ok`, `Err`, abandoned, panicked — without a Postgres; creating
/// the insert future above runs none of it, so nothing is awaited unguarded.
async fn settle_or_report<E: std::fmt::Display>(
    insert: impl std::future::Future<Output = Result<i64, E>>,
    writer: AuditLostWriter,
    action: &str,
    about: &str,
    report: Reporter,
) {
    // No `.await` may come before this line: a stop landing there would
    // abandon the row with nothing said.
    let pending = PendingInsert::new(writer, action, about, report);
    pending.settle(insert.await);
}

/// A drop guard held across one audit insert. Every way the insert can end
/// says exactly one thing, or nothing: [`PendingInsert::settle`] reports an
/// `Err` and stays quiet on `Ok`; dropped unsettled — the future awaiting
/// the insert was dropped (an aborted task, a runtime shutting down) — it
/// reports the row as abandoned (#813).
struct PendingInsert<'a> {
    writer: AuditLostWriter,
    action: &'a str,
    about: &'a str,
    report: Reporter,
    /// Set by [`PendingInsert::settle`]; a guard dropped with it unset says
    /// the row was abandoned.
    settled: bool,
}

impl<'a> PendingInsert<'a> {
    /// Arm the guard. Construct it directly before awaiting the insert.
    fn new(writer: AuditLostWriter, action: &'a str, about: &'a str, report: Reporter) -> Self {
        Self { writer, action, about, report, settled: false }
    }

    /// The insert returned: disarm, and report it if it failed.
    fn settle<E: std::fmt::Display>(mut self, result: Result<i64, E>) {
        self.settled = true;
        if let Err(e) = result {
            (self.report)(self.writer, &format_row_lost(self.action, self.about, &e));
        }
    }
}

impl Drop for PendingInsert<'_> {
    fn drop(&mut self) {
        if !self.settled {
            // Only an unwinding build gets here mid-panic (release is
            // `panic = "abort"`), but there a panic is not a stop.
            let cause = if std::thread::panicking() { Abandoned::Panicked } else { Abandoned::Stopped };
            (self.report)(self.writer, &format_row_abandoned(self.action, self.about, cause));
        }
    }
}

/// Why an insert never returned, for [`format_row_abandoned`].
#[derive(Clone, Copy)]
enum Abandoned {
    /// Its future was dropped: the task was aborted, or the runtime shut down.
    Stopped,
    /// It panicked (unwinding builds only).
    Panicked,
}

/// The string fields a lost-row line names, in order: who the row is about,
/// then why it was written — fixed labels, e.g. `reason: "send_failed"`, or
/// `field: "conversation"` on a `channel.rejected_malformed` row (#818).
const STRING_FIELDS: [&str; 4] = ["channel", "peer", "field", "reason"];

/// The [`STRING_FIELDS`] that say *why*, named after the ids.
const WHY_FIELDS: [&str; 2] = ["field", "reason"];

/// The integer fields it names: the core's own ids, to match the loss to the
/// `tasks` table or an ask (`ask.resolved` has no channel or peer).
const ID_FIELDS: [&str; 2] = ["task_id", "ask_id"];

/// Pure: what a `channel.*` (or `ask.resolved`) row is about, for its
/// lost-row line — ` for channel "x", peer "y", task_id 7, reason "z"`,
/// each part left out when the payload has no such field of that type, and
/// empty when it has none. Each string is [`quoted_id`]'d: a peer comes from
/// outside the core. The ids are integers, so they need no quoting.
fn describe_row(payload: &Value) -> String {
    let strings = STRING_FIELDS
        .iter()
        .filter_map(|&key| payload.get(key).and_then(Value::as_str).map(|v| (key, quoted_id(v))));
    let ids = ID_FIELDS
        .iter()
        .filter_map(|&key| payload.get(key).and_then(Value::as_i64).map(|v| (key, v.to_string())));
    // Who first, then the ids, then why.
    let (who, why): (Vec<_>, Vec<_>) = strings.partition(|(key, _)| !WHY_FIELDS.contains(key));
    let parts: Vec<String> =
        who.into_iter().chain(ids).chain(why).map(|(key, value)| format!("{key} {value}")).collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" for {}", parts.join(", "))
    }
}

/// Pure: the `[audit-lost]` report for a `channel.*` row that was not written
/// — the action, what it was about ([`describe_row`]), and why.
fn format_row_lost(action: &str, about: &str, why: &dyn std::fmt::Display) -> String {
    format!("{action} row{about} not written: {why}")
}

/// Pure: the `[audit-lost]` report for a row whose insert was abandoned
/// mid-await (#813). "*May* not have been written", unlike
/// [`format_row_lost`]: the statement may have reached Postgres before the
/// task was stopped, so the row may be there — only a query can say.
fn format_row_abandoned(action: &str, about: &str, cause: Abandoned) -> String {
    let cause = match cause {
        Abandoned::Stopped => "its task was stopped before the insert returned",
        Abandoned::Panicked => "the insert panicked",
    };
    format!("{action} row{about} may not have been written: {cause}")
}

/// Test fixtures for [`audit_or_report`]'s two writers: this module's tests,
/// the bus's (`bus/tests/dropped.rs`), and the boot supervisor's sink.
#[cfg(test)]
pub(crate) mod test_support {
    use std::time::Duration;

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
}

#[cfg(test)]
mod tests;
