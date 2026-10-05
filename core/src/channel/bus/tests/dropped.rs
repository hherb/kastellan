//! #815: a message or reply the bus drops must leave an audit row.
//!
//! Before #815 both drops were a `warn!` only — invisible under
//! `RUST_LOG=error`, and absent from `audit_log`, so the channel rows no
//! longer accounted for every message in and every reply out. Each drop now
//! writes a row; if that row's own insert fails, the bus's writer already says
//! so on `[audit-lost]` (#808), so neither drop can vanish silently.

use super::*;

/// A body the tests look for in every row, to prove it is never persisted.
const SECRET_BODY: &str = "SECRET-BODY-714";

/// What a failing enqueue says; it is transport/DB text, never a fixed label,
/// so it must stay in the daemon log and out of the row.
const DB_TEXT: &str = "SECRET-DB-TEXT: connection refused";

/// The inbound half: a recognised, clean message whose enqueue fails is a
/// message the agent never saw. It writes `channel.enqueue_failed` — channel,
/// peer and conversation, never the body or the error — and no
/// `channel.received`, which would claim a task id it does not have.
#[tokio::test]
async fn a_failed_enqueue_audits_enqueue_failed_and_not_received() {
    let ev = FakeEvents { enqueue_fails_with: Some(DB_TEXT), ..FakeEvents::default() };
    let auth = StaticPairings::from_peers([PeerId("@me:srv".into())]);

    let ack = handle_inbound(&auth, None, None, &ev, &msg("@me:srv", SECRET_BODY)).await;

    assert!(ack.is_none(), "an outage sends the peer nothing it could probe");
    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    let (action, payload) = &audited[0];
    assert_eq!(action, actions::ENQUEUE_FAILED);
    assert_eq!(
        payload,
        &serde_json::json!({"channel": "matrix", "peer": "@me:srv", "conversation": "!room:srv"})
    );
    let rendered = payload.to_string();
    assert!(!rendered.contains(SECRET_BODY), "never the body: {rendered}");
    assert!(!rendered.contains("SECRET-DB-TEXT"), "never the error: {rendered}");
}

/// The outbound half: the per-channel pump has gone (its receiver dropped), so
/// the reply cannot even be queued. `handle_completed` writes
/// `channel.reply_undelivered` with reason `queue_closed` — and no
/// `channel.replied`, because it was never routed.
#[tokio::test]
async fn a_reply_to_a_closed_send_queue_audits_queue_closed() {
    let ev = FakeEvents::default();
    let mut rows = HashMap::new();
    rows.insert(
        7i64,
        (
            serde_json::json!({"kind":"channel","channel":"matrix","peer":"@me:srv","conversation":"!room:srv"}),
            Some(serde_json::json!({"kind":"completed","message": SECRET_BODY})),
        ),
    );
    let completed = FakeCompleted { ids: Mutex::new(vec![7]), rows };
    let (tx, rx) = mpsc::channel::<OutgoingMessage>(4);
    drop(rx); // the pump that drained this queue is gone
    let mut senders = HashMap::new();
    senders.insert(ChannelId("matrix".into()), tx);

    assert!(handle_completed(&completed, &ev, &senders, 7).await.is_none(), "nothing was sent");

    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    let (action, payload) = &audited[0];
    assert_eq!(action, actions::REPLY_UNDELIVERED);
    assert_eq!(payload["channel"], "matrix");
    assert_eq!(payload["peer"], "@me:srv");
    assert_eq!(payload["reason"], "queue_closed");
    assert!(payload["observed_at"].is_string(), "every writer stamps the event: {payload}");
    assert!(!payload.to_string().contains(SECRET_BODY), "never the body: {payload}");
}
