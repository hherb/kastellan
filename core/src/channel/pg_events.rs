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
//! (`worker_stderr/report/audit_lost.rs`). The daemon's own sinks for the
//! same action (`channel.reply_undelivered`, from a polled driver) already
//! kept that promise; this writer is the bus's half.
//!
//! What it does not catch: an insert still awaited when the bus is stopped.
//! `ChannelBus::shutdown` aborts its pumps, the awaited future is dropped, and
//! no `Err` arm runs — so that row, written or not, gets no line (#813).

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
    fn with_reporter(pool: sqlx::PgPool, report: Reporter) -> Self {
        Self { pool, report }
    }
}

#[async_trait::async_trait]
impl ChannelEvents for PgChannelEvents {
    async fn enqueue(&self, lane: Lane, payload: Value) -> anyhow::Result<i64> {
        Ok(tasks::insert_pending(&self.pool, lane, payload).await?)
    }

    /// Best-effort, as the trait says — never fatal — and a row whose insert
    /// fails is reported on `[audit-lost]` (#808). One still awaited when the
    /// bus stops is not: see the module doc.
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
/// `writer`'s, if the insert fails.
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
pub(crate) async fn audit_or_report(
    pool: &sqlx::PgPool,
    writer: AuditLostWriter,
    action: &str,
    payload: Value,
    report: Reporter,
) {
    let about = describe_row(&payload);
    if let Err(e) = kastellan_db::audit::insert(pool, ACTOR, action, payload).await {
        report(writer, &format_row_lost(action, &about, &e));
    }
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

/// Test fixtures shared with the boot supervisor's sink, which reports through
/// [`audit_or_report`] too.
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
}

#[cfg(test)]
mod tests {
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
}
