//! `pg_events`' tests, split out to keep the module under the 500-LOC soft cap.

use super::*;
use crate::channel::undelivered::{UndeliveredReason, UndeliveredReply};
use crate::channel::{ChannelId, ConversationId, OutgoingMessage, PeerId};
use std::sync::Mutex;

#[test]
fn the_line_names_what_the_row_is_about_and_nothing_else() {
    let payload = serde_json::json!({
        "channel": "matrix",
        "peer": "@a:srv",
        "task_id": 7,
        "reason": "send_failed",
        "body": "NEVER-IN-A-LINE",
    });
    let line = format_row_lost("channel.reply_undelivered", &describe_row(&payload), &"boom");
    assert_eq!(
        line,
        r#"channel.reply_undelivered row for channel "matrix", peer "@a:srv", task_id 7, reason "send_failed" not written: boom"#
    );
    assert!(!line.contains("NEVER-IN-A-LINE"), "only the named fields are read: {line}");
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
    type Insert = std::pin::Pin<Box<dyn std::future::Future<Output = Result<i64, String>> + Send>>;
    async fn settle(insert: Insert) {
        settle_or_report(insert, AuditLostWriter::Bus, "channel.received", r#" for channel "matrix""#, record_settled)
            .await;
    }

    settle(Box::pin(async { Ok(1) })).await;
    assert_eq!(take_settled(), [], "a written row says nothing");

    settle(Box::pin(async { Err("boom".to_string()) })).await;
    assert_eq!(
        take_settled(),
        [(AuditLostWriter::Bus, r#"channel.received row for channel "matrix" not written: boom"#.to_string())]
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
}
