//! The production [`BootAuditSink`]: one `audit_log` row per bring-up event.
//!
//! Split out of the supervisor so the retry loop itself stays database-free,
//! and therefore unit-testable without a cluster.
//!
//! These rows are the durable half of #514's answer to "was the bot deaf, and
//! for how long?". The daemon log has the same information, but it is a
//! plaintext file that rotates and that nobody reads until they notice
//! silence; `audit_log` is queryable after the fact and is where the rest of
//! the channel's history already lives.
//!
//! A failure to *write* the row is non-fatal: an unavailable Postgres must not
//! stop the supervisor from retrying the channel — that would trade the bug
//! for a worse one. It is not silent either: since #814 it is said on the
//! `[audit-lost]` marker at ERROR (`AuditLostWriter::ChannelSupervisor`),
//! naming the action and the channel — never the `cause` — through the same
//! `channel::pg_events::audit_or_report` the bus's own rows use. Before #814
//! it was a WARN only, invisible under `RUST_LOG=error`.
//!
//! **Which means these rows are missing in exactly one important case, and it
//! is worth being precise rather than reassuring about it (#517):** the
//! reachable cause of a *pump death* is a sustained Postgres outage — sqlx
//! reconnects transparently, so the listener only gives up when the reconnect
//! itself keeps failing. The sink needs the same pool. So a channel that dies
//! because Postgres went away writes **no** [`BootAudit::Died`] row and no
//! `boot_failed` rows for the retries that follow; they appear only once
//! Postgres is back, from that point on. The durable record of *that* outage is
//! the daemon log (`~/.local/state/kastellan/*.out`), not `audit_log` — where,
//! since #814, each missing row has its own `[audit-lost]` line.
//!
//! The rows remain the durable record for every death Postgres survives — a
//! panicking pump, an inbound transport that closed — and for the bring-up
//! failures #514 was about, which is why they are still worth writing.

use futures::future::BoxFuture;
use sqlx::PgPool;

use super::{BootAudit, BootAuditSink};
use crate::channel::actions;
use crate::channel::pg_events::{audit_or_report, emit_report, Reporter};
use crate::worker_stderr::AuditLostWriter;

/// Build the sink for one channel. `channel` is captured, so every row carries
/// it without the supervisor having to thread it through.
///
/// Payloads carry the channel name, counters and the (already capped) cause —
/// never message content, and never anything a peer supplied.
pub fn pg_boot_audit_sink(pool: PgPool, channel: &str) -> BootAuditSink {
    sink_reporting_to(pool, channel, emit_report)
}

/// [`pg_boot_audit_sink`], with a lost row said through `report` — the
/// production emitter there, a recorder in a test.
fn sink_reporting_to(pool: PgPool, channel: &str, report: Reporter) -> BootAuditSink {
    let channel = channel.to_string();
    Box::new(move |event: BootAudit| {
        let pool = pool.clone();
        let channel = channel.clone();
        Box::pin(async move {
            let (action, payload) = row_for(&channel, event);
            // Non-fatal, as before #814 — but a row that is not written is
            // now said on `[audit-lost]` at ERROR, not a WARN that
            // `RUST_LOG=error` hides.
            audit_or_report(&pool, AuditLostWriter::ChannelSupervisor, action, payload, report)
                .await;
        }) as BoxFuture<'static, ()>
    })
}

/// Pure: the action and payload of `event`'s row for `channel`.
fn row_for(channel: &str, event: BootAudit) -> (&'static str, serde_json::Value) {
    match event {
        BootAudit::Started { attempts } => (
            actions::BOOT_STARTED,
            serde_json::json!({ "channel": channel, "attempts": attempts }),
        ),
        BootAudit::Failed { attempt, retry_in_ms, fatal, cause } => (
            actions::BOOT_FAILED,
            serde_json::json!({
                "channel": channel,
                "attempt": attempt,
                "retry_in_ms": retry_in_ms,
                "fatal": fatal,
                "cause": cause,
            }),
        ),
        BootAudit::Died { ran_ms, retry_in_ms } => (
            actions::CHANNEL_DIED,
            serde_json::json!({
                "channel": channel,
                "ran_ms": ran_ms,
                "retry_in_ms": retry_in_ms,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::pg_events::test_support::refused_pool;
    use crate::worker_stderr::AuditLostWriter;
    use std::sync::Mutex;

    /// Each event's row, pinned whole: the action and the payload are the
    /// operator-facing interface, and the channel is the captured one.
    #[test]
    fn each_event_has_its_row() {
        assert_eq!(
            row_for("matrix", BootAudit::Started { attempts: 2 }),
            (actions::BOOT_STARTED, serde_json::json!({"channel": "matrix", "attempts": 2}))
        );
        assert_eq!(
            row_for(
                "email",
                BootAudit::Failed { attempt: 3, retry_in_ms: Some(500), fatal: false, cause: "c".into() }
            ),
            (
                actions::BOOT_FAILED,
                serde_json::json!({
                    "channel": "email", "attempt": 3, "retry_in_ms": 500, "fatal": false, "cause": "c",
                })
            )
        );
        assert_eq!(
            row_for("matrix", BootAudit::Died { ran_ms: 60_000, retry_in_ms: 1_000 }),
            (
                actions::CHANNEL_DIED,
                serde_json::json!({"channel": "matrix", "ran_ms": 60_000, "retry_in_ms": 1_000})
            )
        );
    }

    /// What the recorder below was told.
    static SAID: Mutex<Vec<(AuditLostWriter, String)>> = Mutex::new(Vec::new());

    fn record(writer: AuditLostWriter, line: &str) {
        SAID.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
    }

    /// #814: a bring-up row whose insert fails is said on `[audit-lost]`, as
    /// the channel supervisor's, naming the action and the channel — and
    /// never the `cause`, which is capped error text, not a fixed label. Both
    /// rows go through one sink, as production's do, so a revert to the old
    /// `warn!`-only arm fails here.
    #[tokio::test]
    async fn a_failed_insert_is_reported_on_the_marker_without_its_cause() {
        let sink = sink_reporting_to(refused_pool(), "matrix", record);
        sink(BootAudit::Died { ran_ms: 5, retry_in_ms: 10 }).await;
        sink(BootAudit::Failed {
            attempt: 1,
            retry_in_ms: Some(10),
            fatal: false,
            cause: "SECRET-CAUSE-TEXT".into(),
        })
        .await;

        let said = SAID.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said.iter().all(|(w, _)| *w == AuditLostWriter::ChannelSupervisor), "{said:?}");
        assert!(
            said[0].1.starts_with(r#"channel.died row for channel "matrix" not written: "#),
            "{said:?}"
        );
        assert!(
            said[1].1.starts_with(r#"channel.boot_failed row for channel "matrix" not written: "#),
            "{said:?}"
        );
        assert!(!said[1].1.contains("SECRET-CAUSE-TEXT"), "never the cause: {said:?}");
    }
}
