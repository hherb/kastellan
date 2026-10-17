//! `pg_events`' tests, split out to keep the module under the 500-LOC soft cap.

use super::*;
use crate::audit_thinning::REPORT_EACH_UP_TO;
use crate::channel::undelivered::{UndeliveredReason, UndeliveredReply};
use crate::channel::{ChannelId, ConversationId, OutgoingMessage, PeerId};
use std::sync::Mutex;
use test_support::own_lost_rows;

/// Postgres's 25006, read_only_sql_transaction: a plain string's cause.
const READ_ONLY: InsertCause = InsertCause::Answered(kastellan_db::audit::SqlState::parse("25006").unwrap());

/// A test's failed insert as a plain string: known not written, the shape
/// every test before #858 was written against — all of one cause (#859),
/// whatever the text says, as a real fault's rows are.
impl InsertFailure for &str {
    fn outcome(&self) -> InsertOutcome {
        InsertOutcome::NotWritten
    }

    fn cause(&self) -> InsertCause {
        READ_ONLY
    }
}

impl InsertFailure for String {
    fn outcome(&self) -> InsertOutcome {
        InsertOutcome::NotWritten
    }

    fn cause(&self) -> InsertCause {
        READ_ONLY
    }
}

/// A test's failed insert whose row may have been committed (#858).
struct Unknown(&'static str);

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl InsertFailure for Unknown {
    fn outcome(&self) -> InsertOutcome {
        InsertOutcome::Unknown
    }

    fn cause(&self) -> InsertCause {
        InsertCause::Io { kind: std::io::ErrorKind::ConnectionReset, os: None }
    }
}

/// A bus `channel.received` row for channel `matrix`, said through `report`
/// and counted in `lost`.
fn received_row(report: Reporter, lost: &'static LostRows) -> RowReport<'static> {
    RowReport {
        writer: AuditLostWriter::Bus,
        action: "channel.received",
        about: r#" for channel "matrix""#,
        channel: Some("matrix"),
        report,
        lost,
    }
}

#[test]
fn the_line_names_what_the_row_is_about_and_nothing_else() {
    let payload = serde_json::json!({
        "channel": "matrix",
        "peer": "@a:srv",
        "task_id": 7,
        "reason": "send_failed",
        "body": "NEVER-IN-A-LINE",
    });
    let first = BurstTally { nth: 1, new_cause_since: None, unsaid_before: 0, ended: None };
    let about = describe_row(&payload);
    let line = format_row_lost("channel.reply_undelivered", &about, &"boom", first);
    assert_eq!(
        line,
        r#"channel.reply_undelivered row for channel "matrix", peer "@a:srv", task_id 7, reason "send_failed" not written: boom; row 1 of this burst"#
    );
    assert!(!line.contains("NEVER-IN-A-LINE"), "only the named fields are read: {line}");
    // #858: an insert whose connection broke may have been committed.
    let unknown = format_row_lost("channel.received", "", &Unknown("reset"), first);
    assert_eq!(unknown, "channel.received row may not have been written: reset; row 1 of this burst");
}

#[test]
fn a_missing_or_mistyped_field_is_left_out() {
    assert_eq!(describe_row(&serde_json::json!({"channel": "email"})), r#" for channel "email""#);
    assert_eq!(describe_row(&serde_json::json!({"peer": 7, "task_id": "7"})), "");
    assert_eq!(describe_row(&serde_json::json!("not an object")), "");
}

/// A malformed-id refusal (#818) names which id was malformed, after the
/// parties — the field is a fixed label, so it is a "why", not a "who".
#[test]
fn a_malformed_row_names_its_field() {
    let payload = serde_json::json!({
        "channel": "matrix", "peer": "@a\u{0}:srv", "field": "peer", "reason": "nul",
    });
    assert_eq!(
        describe_row(&payload),
        r#" for channel "matrix", peer "@a\0:srv", field "peer", reason "nul""#
    );
}

/// #815: a `channel.enqueue_failed` row is usually lost to the same outage
/// that failed the enqueue, so its line is the drop's only trace — it
/// must name who sent the message.
#[test]
fn an_enqueue_failed_row_names_its_channel_and_peer() {
    let payload = serde_json::json!({
        "channel": "matrix", "peer": "@a:srv", "conversation": "!r:srv",
    });
    assert_eq!(describe_row(&payload), r#" for channel "matrix", peer "@a:srv""#);
}

/// `ask.resolved` has no channel or peer: its line still names the ask.
#[test]
fn a_row_with_no_parties_is_named_by_its_ids() {
    let payload = serde_json::json!({
        "ask_id": 3, "task_id": 9, "choice": "approve", "resolved_by": "@op:srv", "via": "channel",
    });
    assert_eq!(describe_row(&payload), " for task_id 9, ask_id 3");
}

/// The real producer's payload, not a hand-built one: a renamed key in
/// [`UndeliveredReply::payload`] would leave the line naming nobody.
#[test]
fn the_line_names_a_real_undelivered_reply() {
    let out = OutgoingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@a:srv".into()),
        conversation: ConversationId("!r:srv".into()),
        body: "NEVER-IN-A-LINE".into(),
    };
    let reply = UndeliveredReply::of(&out, UndeliveredReason::SendFailed, time::OffsetDateTime::UNIX_EPOCH);
    assert_eq!(
        describe_row(&reply.payload()),
        r#" for channel "matrix", peer "@a:srv", reason "send_failed""#
    );
}

/// A peer is outside input: quoted, so a newline or a forged second entry
/// stays inside its own quotes.
#[test]
fn a_hostile_peer_is_quoted() {
    let about = describe_row(&serde_json::json!({"peer": "x\", peer \"forged\n[SKIP]"}));
    assert_eq!(about, r#" for peer "x\", peer \"forged\n[SKIP]""#);
    assert!(!about.contains('\n'));
}

/// What the recorder below was told.
static SAID: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record(writer: AuditLostWriter, line: &str) {
    SAID.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #808: a failed insert reaches the reporter, as the bus's, naming the
/// row — through the `ChannelEvents::audit` the bus calls, so a revert of
/// that method to swallowing the error fails here. The pool points at a
/// port nothing listens on ([`test_support::refused_pool`]).
#[tokio::test]
async fn a_failed_insert_is_reported_on_the_marker() {
    let events = PgChannelEvents::with_reporter(test_support::refused_pool(), record);
    let payload = serde_json::json!({"channel": "matrix", "peer": "@lost:srv"});
    ChannelEvents::audit(&events, "channel.reply_undelivered", payload).await;

    let said = SAID.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, AuditLostWriter::Bus);
    assert!(
        said[0].1.starts_with(r#"channel.reply_undelivered row for channel "matrix", peer "@lost:srv" not written: "#),
        "{said:?}"
    );
}

/// What [`record_unknown`] was told — its own static, as the others.
static UNKNOWN: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_unknown(writer: AuditLostWriter, line: &str) {
    UNKNOWN.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #858: an insert that lost its connection says its row *may* not have
/// been written — through the `ChannelEvents::audit` the bus calls, against
/// a pool whose every connection is closed at once
/// ([`test_support::closing_pool`]). The refused pool above is the positive
/// control: the same path still says "not written" of a row Postgres never
/// got.
#[tokio::test]
async fn an_insert_that_lost_its_connection_may_not_have_been_written() {
    let events = PgChannelEvents::with_reporter(test_support::closing_pool(), record_unknown);
    let payload = serde_json::json!({"channel": "matrix", "peer": "@maybe:srv"});
    ChannelEvents::audit(&events, "channel.received", payload).await;

    let said = UNKNOWN.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].1.starts_with(
            r#"channel.received row for channel "matrix", peer "@maybe:srv" may not have been written: "#
        ),
        "{said:?}"
    );
}

/// What [`record_abandoned`] was told — its own static, so the test above
/// running alongside cannot add to it.
static ABANDONED: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_abandoned(writer: AuditLostWriter, line: &str) {
    ABANDONED.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #813: an insert whose future is dropped while it is still awaited —
/// the bus aborting a pump parked in `audit(…).await` — never reaches the
/// `Err` arm. It is reported anyway, as a row that *may* not have been
/// written: the statement may have reached Postgres before the abort.
#[tokio::test]
async fn an_insert_abandoned_mid_await_is_reported() {
    let (pool, listener) = test_support::stalled_pool();
    let events = std::sync::Arc::new(PgChannelEvents::with_reporter(pool, record_abandoned));
    let task = tokio::spawn({
        let events = events.clone();
        async move {
            let payload = serde_json::json!({"channel": "matrix", "peer": "@gone:srv", "reason": "queue_closed"});
            ChannelEvents::audit(&*events, "channel.reply_undelivered", payload).await;
        }
    });
    let _held = test_support::connected(&listener).await;
    assert!(ABANDONED.lock().unwrap().is_empty(), "nothing is said while the insert is awaited");

    task.abort();
    let ended = task.await.expect_err("aborted mid-insert");
    assert!(ended.is_cancelled(), "a stop, not a panic, is what this line must be about: {ended}");

    let said = ABANDONED.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, AuditLostWriter::Bus);
    assert_eq!(
        said[0].1,
        r#"channel.reply_undelivered row for channel "matrix", peer "@gone:srv", reason "queue_closed" may not have been written: its task was stopped before the insert returned"#
    );
}

/// What [`record_settled`] was told — one static, one test, phases run
/// in sequence.
static SETTLED: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_settled(writer: AuditLostWriter, line: &str) {
    SETTLED.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

fn take_settled() -> Vec<(AuditLostWriter, String)> {
    std::mem::take(&mut *SETTLED.lock().unwrap_or_else(|p| p.into_inner()))
}

/// Every way an insert can end says exactly one thing, or nothing — the
/// `Ok` case above all, which no Postgres-free test of the real insert
/// can reach: a guard left armed on success would say "may not have
/// been written" for every row the bus ever wrote, at ERROR.
#[tokio::test]
async fn each_outcome_of_an_insert_says_one_thing_or_nothing() {
    type Insert = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;
    static LOST: LostRows = LostRows::new();
    async fn settle(insert: Insert) {
        settle_or_report(insert, received_row(record_settled, &LOST)).await;
    }

    settle(Box::pin(async { Ok(()) })).await;
    assert_eq!(take_settled(), [], "a written row says nothing");

    settle(Box::pin(async { Err("boom".to_string()) })).await;
    assert_eq!(
        take_settled(),
        [(
            AuditLostWriter::Bus,
            r#"channel.received row for channel "matrix" not written: boom; row 1 of this burst"#.to_string()
        )]
    );

    let stopped = tokio::spawn(settle(Box::pin(std::future::pending())));
    tokio::task::yield_now().await;
    stopped.abort();
    assert!(stopped.await.expect_err("aborted").is_cancelled());
    assert_eq!(
        take_settled(),
        [(
            AuditLostWriter::Bus,
            r#"channel.received row for channel "matrix" may not have been written: its task was stopped before the insert returned"#
                .to_string()
        )]
    );

    let panicked = tokio::spawn(settle(Box::pin(async { panic!("insert panicked on purpose") })));
    assert!(panicked.await.expect_err("panicked").is_panic());
    assert_eq!(
        take_settled(),
        [(
            AuditLostWriter::Bus,
            r#"channel.received row for channel "matrix" may not have been written: the insert panicked"#.to_string()
        )]
    );

    // An abandoned row is said, not counted (#829 thins failed inserts only):
    // the next failure is the burst's second, after the first `Err` above.
    settle(Box::pin(async { Err("again".to_string()) })).await;
    assert_eq!(
        take_settled(),
        [(
            AuditLostWriter::Bus,
            r#"channel.received row for channel "matrix" not written: again; row 2 of this burst"#.to_string()
        )]
    );
}

/// What [`record_in_flight`] was told — its own static, as above.
static IN_FLIGHT: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_in_flight(writer: AuditLostWriter, line: &str) {
    IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #832: a guard's lost-work line goes through the same reporter as a lost
/// row, as the bus's writer — so a test reading the real writer sees both,
/// and production puts both on `[audit-lost]`.
#[tokio::test]
async fn work_lost_in_flight_is_reported_through_the_writer() {
    let events = PgChannelEvents::with_reporter(test_support::refused_pool(), record_in_flight);
    ChannelEvents::report_lost(&events, "task 7's reply may not have been sent");

    let said = IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(said, [(AuditLostWriter::Bus, "task 7's reply may not have been sent".to_string())]);
}

mod thinning;
