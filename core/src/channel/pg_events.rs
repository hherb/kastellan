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
//! not have been written. Since #832 a stop on any other pump await that can
//! lose work is said through the same writer too (`ChannelEvents::report_lost`,
//! from `bus_guards`): an inbound message mid-handling, a reply or an ask
//! mid-send, a raised ask still queued, an unroutable claim in flight — and a
//! reply claim whose outcome cannot be known (`bus_deliver`). Inbound messages
//! the bus had not yet read are not this writer's: a polled channel's own
//! receiver counts them into a `channel.inbound_dropped` row as the stop drops
//! it (#846, `polled_driver/inbound_rx.rs`). What is still unsaid: a crash —
//! and with it, since #829, the count of a flood's rows thinned out after its
//! last 2^k-th line, which only the next burst's first line on the same
//! count, or the shutdown's line, says (#817).
//!
//! Since #829 a failed insert's lines are **thinned** past a flood, per
//! writer, channel, action and outcome ([`lost_rows`]; the last two since
//! #864/#865) — but for the first row of each new cause in a burst (#859) —
//! each carrying its place in the burst,
//! and the shutdown says how many got no line of their own
//! ([`report_unsaid_lost_rows`]). And the insert no longer decodes an id it
//! discards (`kastellan_db::audit::insert_without_id`), so "not written" is
//! no longer said of a row that was written but whose `RETURNING id` failed
//! to decode. Since #858 nor is it said of a row whose insert lost its
//! connection, which may have been committed: that line says the row "may
//! not have been written" (`InsertFailure`).

use serde_json::Value;

use kastellan_db::audit::{InsertCause, InsertOutcome};
use kastellan_db::tasks::{self, Lane};

use super::audit_text::{insert_verdict, quoted_id};
use super::bus::ChannelEvents;
use crate::audit_thinning::BurstTally;
use crate::worker_stderr::{emit_audit_lost_report, AuditLostWriter};

mod lost_rows;
use lost_rows::{format_unsaid, RowKey};
pub(crate) use lost_rows::{LostRows, LOST_ROWS};

/// The `actor` every row [`audit_or_report`] writes is under: the bus's, and
/// since #814 the boot supervisor's.
const ACTOR: &str = "channel";

/// Real DB-backed `ChannelEvents` over the runtime pool.
pub struct PgChannelEvents {
    pool: sqlx::PgPool,
    /// How a lost row is said: [`emit_report`], but for a test's recorder.
    report: Reporter,
    /// Which lost rows get a line of their own (#829): the process-wide
    /// [`LOST_ROWS`], but for a test's own.
    lost: &'static LostRows,
}

impl PgChannelEvents {
    pub fn new(pool: sqlx::PgPool) -> Self {
        let (report, lost) = production_writer();
        Self { pool, report, lost }
    }

    /// Test builds only: lost rows said to `report` and counted apart from
    /// every other test's, so a test can read them through the trait method
    /// production calls.
    #[cfg(test)]
    pub(crate) fn with_reporter(pool: sqlx::PgPool, report: Reporter) -> Self {
        Self { pool, report, lost: test_support::own_lost_rows() }
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
    async fn audit(&self, action: &'static str, payload: Value) {
        audit_or_report(&self.pool, AuditLostWriter::Bus, action, payload, self.report, self.lost).await;
    }

    /// Work a bus stop abandoned (#832): the same writer and reporter as a
    /// lost row, so both reach `[audit-lost]` — and a test's recorder.
    fn report_lost(&self, line: &str) {
        (self.report)(AuditLostWriter::Bus, line);
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
fn emit_report(writer: AuditLostWriter, line: &str) {
    emit_audit_lost_report(writer, line);
}

/// The production reporter and count, for every writer built on
/// [`audit_or_report`] — the bus's, the boot supervisor's — and for the
/// shutdown that reads the count: one place names [`LOST_ROWS`], so no writer
/// can count where the shutdown does not read (#829).
pub(crate) fn production_writer() -> (Reporter, &'static LostRows) {
    (emit_report, &LOST_ROWS)
}

/// Say, once, how many rows every bus writer lost (failed to insert) and gave no line
/// of their own (#829) — one `[audit-lost]` line per writer, channel, action
/// and outcome that has any (#864). The daemon calls it after the channels stop, when nothing writes
/// through these writers any more; called twice, it would say the same rows
/// again. The count is every burst's, so it includes rows a later burst's
/// first line already summed up ("the burst before it ended after …").
pub fn report_unsaid_lost_rows() {
    let (report, lost) = production_writer();
    report_unsaid(lost, report);
}

/// [`report_unsaid_lost_rows`] over `lost`, through `report`.
fn report_unsaid(lost: &LostRows, report: Reporter) {
    for unsaid in lost.unsaid() {
        report(unsaid.writer, &format_unsaid(&unsaid));
    }
}

/// Insert one `channel`-actor audit row; report it through `report`, as
/// `writer`'s, if the insert fails or is abandoned mid-await. A failed
/// insert counts in `lost` under the row's `channel`, action and outcome,
/// which thins its line past a flood (#829, #864) — unless its cause is new
/// in the burst (#859).
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
    action: &'static str,
    payload: Value,
    report: Reporter,
    lost: &LostRows,
) {
    let about = describe_row(&payload);
    let channel = row_channel_name(&payload).map(str::to_owned);
    // No `RETURNING id` (#829): the id is discarded here, and decoding it was
    // the one step of ours that could fail after a row known to be written.
    let insert = kastellan_db::audit::insert_without_id(pool, ACTOR, action, payload);
    let row = RowReport { writer, action, about: &about, channel: channel.as_deref(), report, lost };
    settle_or_report(insert, row).await;
}

/// Who says a row's loss, and what they name — everything
/// [`settle_or_report`] needs besides the insert.
#[derive(Clone, Copy)]
struct RowReport<'a> {
    writer: AuditLostWriter,
    action: &'static str,
    /// [`describe_row`]'s words.
    about: &'a str,
    /// The row's own `channel` field, which its loss is counted under.
    channel: Option<&'a str>,
    report: Reporter,
    lost: &'a LostRows,
}

/// Await `insert` under a [`PendingInsert`] guard: the one place a row's
/// outcome is said. Generic over the insert so a test can drive every
/// outcome — `Ok`, `Err`, abandoned, panicked — without a Postgres; creating
/// the insert future above runs none of it, so nothing is awaited unguarded.
async fn settle_or_report<E: InsertFailure>(
    insert: impl std::future::Future<Output = Result<(), E>>,
    row: RowReport<'_>,
) {
    // No `.await` may come before this line: a stop landing there would
    // abandon the row with nothing said.
    let pending = PendingInsert::new(row);
    pending.settle(insert.await);
}

/// A drop guard held across one audit insert. Every way the insert can end
/// says at most one thing: [`PendingInsert::settle`] reports an `Err` —
/// unless its burst has passed the thinning, when it is only counted (#829)
/// — and stays quiet on `Ok`; dropped unsettled — the future awaiting the
/// insert was dropped (an aborted task, a runtime shutting down) — it
/// reports the row as abandoned (#813), unthinned: that happens once per
/// pump a stop finds parked here, never once per inbound message.
struct PendingInsert<'a> {
    row: RowReport<'a>,
    /// Set by [`PendingInsert::settle`]; a guard dropped with it unset says
    /// the row was abandoned.
    settled: bool,
}

impl<'a> PendingInsert<'a> {
    /// Arm the guard. Construct it directly before awaiting the insert.
    fn new(row: RowReport<'a>) -> Self {
        Self { row, settled: false }
    }

    /// The insert returned: disarm, and report it if it failed.
    fn settle<E: InsertFailure>(mut self, result: Result<(), E>) {
        self.settled = true;
        if let Err(e) = result {
            let row = self.row;
            // `lose` releases its lock before the report, which can block.
            let key = RowKey { writer: row.writer, channel: row.channel, action: row.action, outcome: e.outcome() };
            let lost = row.lost.lose(key, e.cause());
            if lost.report {
                let line = format_row_lost(row.action, row.about, &e, lost.tally);
                (row.report)(row.writer, &line);
            }
        }
    }
}

impl Drop for PendingInsert<'_> {
    fn drop(&mut self) {
        if !self.settled {
            // Only an unwinding build gets here mid-panic (release is
            // `panic = "abort"`), but there a panic is not a stop.
            let cause = if std::thread::panicking() { Abandoned::Panicked } else { Abandoned::Stopped };
            let row = self.row;
            (row.report)(row.writer, &format_row_abandoned(row.action, row.about, cause));
        }
    }
}

/// A failed insert, as [`settle_or_report`] reports it: what to print,
/// whether the row may still be in the table (#858), and what kind of
/// failure it was (#859). The real one is
/// [`kastellan_db::audit::InsertError`]; a trait so the tests can drive
/// every outcome with no Postgres — a plain string is one known not written,
/// and `tests.rs`'s `Unknown` one that may have been.
trait InsertFailure: std::fmt::Display {
    fn outcome(&self) -> InsertOutcome;
    fn cause(&self) -> InsertCause;
}

impl InsertFailure for kastellan_db::audit::InsertError {
    fn outcome(&self) -> InsertOutcome {
        kastellan_db::audit::InsertError::outcome(self)
    }

    fn cause(&self) -> InsertCause {
        kastellan_db::audit::InsertError::cause(self)
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

/// Pure: the channel a row's loss is counted under (#829) — its own
/// `channel` field, a string spelled by the core's own code (the bus's
/// registered channel ids, the boot supervisor's channel name), never by a
/// peer. `None` for a row with no such field
/// (`ask.resolved`, `channel.reply_unroutable`).
fn row_channel_name(payload: &Value) -> Option<&str> {
    payload.get("channel").and_then(Value::as_str)
}

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

/// Pure: the `[audit-lost]` report for a `channel.*` row whose insert failed
/// — the action, what it was about ([`describe_row`]), whether it is known
/// not to be in the table ([`insert_verdict`] of `why`'s own outcome, #858),
/// why, and where it stands in its count's burst of lost rows — its writer,
/// channel, action and outcome (#829, #864, #865) — so a thinned flood's
/// lines still say how big it is.
fn format_row_lost(action: &str, about: &str, why: &impl InsertFailure, tally: BurstTally) -> String {
    format!("{action} row{about} {}: {why}; {tally}", insert_verdict(why.outcome()))
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
    format!("{action} row{about} {}: {cause}", insert_verdict(InsertOutcome::Unknown))
}

/// Test fixtures for [`audit_or_report`]'s two writers: this module's tests,
/// the bus's (`bus/tests/dropped.rs`), and the boot supervisor's sink.
#[cfg(test)]
pub(crate) mod test_support {
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
            for stream in listener.incoming() {
                drop(stream);
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
}

#[cfg(test)]
mod tests;
