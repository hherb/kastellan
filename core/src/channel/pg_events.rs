//! The channel bus's real DB seam, [`PgChannelEvents`]: enqueue a channel
//! task, write a `channel.*` audit row — and **say so on the `[audit-lost]`
//! marker when that row is not written** (#808).
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

use serde_json::Value;

use kastellan_db::tasks::{self, Lane};

use super::audit_text::quoted_id;
use super::bus::ChannelEvents;
use crate::worker_stderr::{emit_audit_lost_report, AuditLostWriter};

/// The `actor` every bus audit row is written under.
const ACTOR: &str = "channel";

/// Real DB-backed `ChannelEvents` over the runtime pool.
pub struct PgChannelEvents {
    pool: sqlx::PgPool,
}

impl PgChannelEvents {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ChannelEvents for PgChannelEvents {
    async fn enqueue(&self, lane: Lane, payload: Value) -> anyhow::Result<i64> {
        Ok(tasks::insert_pending(&self.pool, lane, payload).await?)
    }

    /// Best-effort, as the trait says — never fatal — but never silent: a
    /// row that was not written is reported on `[audit-lost]` (#808).
    async fn audit(&self, action: &str, payload: Value) {
        audit_or_report(&self.pool, action, payload, emit_report).await;
    }
}

/// How a lost row is said: [`emit_report`] in production, a recorder in a
/// test. A `fn` pointer, for the reason the daemon's sinks give (their
/// `audit_sink::Reporter`): the emitter's own record is not readable from a
/// test without a scoped subscriber, which flakes.
type Reporter = fn(AuditLostWriter, &str);

/// The production [`Reporter`]. Whether the stderr fallback was written is
/// dropped: nothing more can be done for a row the report itself could not
/// reach anyone about (the daemon's sinks drop it for the same reason).
fn emit_report(writer: AuditLostWriter, line: &str) {
    emit_audit_lost_report(writer, line);
}

/// Insert one bus audit row; report it through `report` if the insert fails.
///
/// Who the row is about is read **before** the insert, which consumes the
/// payload. Only the `channel` and `peer` fields are read, so nothing else in
/// the payload can reach the line.
async fn audit_or_report(pool: &sqlx::PgPool, action: &str, payload: Value, report: Reporter) {
    let parties = describe_parties(&payload);
    if let Err(e) = kastellan_db::audit::insert(pool, ACTOR, action, payload).await {
        report(AuditLostWriter::Bus, &format_bus_row_lost(action, &parties, &e));
    }
}

/// Pure: who a bus row is about, for its lost-row line — ` for channel "x",
/// peer "y"`, either part left out when the payload has no such string field,
/// and empty when it has neither. Each value [`quoted_id`]'d: a peer comes
/// from outside the core.
fn describe_parties(payload: &Value) -> String {
    let field = |key: &str| payload.get(key).and_then(Value::as_str).map(quoted_id);
    let parts: Vec<String> = [("channel", field("channel")), ("peer", field("peer"))]
        .into_iter()
        .filter_map(|(name, value)| value.map(|v| format!("{name} {v}")))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" for {}", parts.join(", "))
    }
}

/// Pure: the `[audit-lost]` report for a bus row that was not written —
/// the action, who it was about ([`describe_parties`]), and why.
fn format_bus_row_lost(action: &str, parties: &str, why: &dyn std::fmt::Display) -> String {
    format!("{action} row{parties} not written: {why}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    #[test]
    fn the_line_names_the_action_channel_and_peer_and_nothing_else() {
        let payload = serde_json::json!({
            "channel": "matrix",
            "peer": "@a:srv",
            "body": "NEVER-IN-A-LINE",
        });
        let line = format_bus_row_lost("channel.reply_undelivered", &describe_parties(&payload), &"boom");
        assert_eq!(
            line,
            r#"channel.reply_undelivered row for channel "matrix", peer "@a:srv" not written: boom"#
        );
        assert!(!line.contains("NEVER-IN-A-LINE"), "only channel and peer are read: {line}");
    }

    #[test]
    fn a_missing_or_non_string_party_is_left_out() {
        assert_eq!(describe_parties(&serde_json::json!({"channel": "email"})), r#" for channel "email""#);
        assert_eq!(describe_parties(&serde_json::json!({"peer": 7})), "");
        assert_eq!(describe_parties(&serde_json::json!("not an object")), "");
    }

    /// A peer is outside input: quoted, so a newline or a forged second entry
    /// stays inside its own quotes.
    #[test]
    fn a_hostile_peer_is_quoted() {
        let parties = describe_parties(&serde_json::json!({"peer": "x\", peer \"forged\n[SKIP]"}));
        assert_eq!(parties, r#" for peer "x\", peer \"forged\n[SKIP]""#);
        assert!(!parties.contains('\n'));
    }

    /// What the recorder below was told.
    static SAID: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

    fn record(writer: AuditLostWriter, line: &str) {
        SAID.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
    }

    /// #808: a failed insert reaches the reporter, as the bus's, naming the
    /// row. The pool points at a port nothing listens on; sqlx retries a
    /// refused connect until its acquire timeout, kept short here.
    #[tokio::test]
    async fn a_failed_insert_is_reported_on_the_marker() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        }; // dropped: the port now refuses
        let options = sqlx::postgres::PgConnectOptions::new()
            .host("127.0.0.1")
            .port(port)
            .username("nobody")
            .database("nothing");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(1))
            .connect_lazy_with(options);

        let payload = serde_json::json!({"channel": "matrix", "peer": "@lost:srv"});
        audit_or_report(&pool, "channel.reply_undelivered", payload, record).await;

        let said = SAID.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].0, AuditLostWriter::Bus);
        assert!(
            said[0].1.starts_with(r#"channel.reply_undelivered row for channel "matrix", peer "@lost:srv" not written: "#),
            "{said:?}"
        );
    }
}
