//! #815: a message or reply the bus drops must leave an audit row.
//!
//! Before #815 both drops were a `warn!` only — invisible under
//! `RUST_LOG=error`, and absent from `audit_log`, so the channel rows no
//! longer accounted for every message in and every reply out. Each drop now
//! writes a row; if that row's own insert fails, the bus's writer already says
//! so on `[audit-lost]` (#808). The one exception is an insert still awaited
//! when the bus is stopped (#813), which these in-process fakes cannot show.

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
    // The literal, not only the constant: operators query `audit_log` by this
    // string, so a respelling of the constant is a breaking change.
    assert_eq!(action, "channel.enqueue_failed");
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

/// A refused inbound ack leaves a row too (#824). The pump hands every ack
/// `handle_inbound` returns to the transport — "✓ Approved", "✗ not
/// answerable", the usage hint, the pairing confirmation — and until #824 a
/// refusal was a WARN only. `EmailChannel::send` refuses every one, so no
/// email operator's missing confirmation was ever in `audit_log`.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_ack_audits_reply_undelivered() {
    let ev = Arc::new(FakeEvents::default());
    let (inbound_tx, inbound_rx) = mpsc::channel::<IncomingMessage>(1);
    let channel = RefusingChannel { id: ChannelId("matrix".into()), inbound_rx };
    let bus = ChannelBus::spawn(
        vec![Box::new(channel)],
        Arc::new(StaticPairings::new()),
        Some(Arc::new(FakePairing { code: Some("123456") })),
        ev.clone(),
        Box::new(ParkingCompleted),
        None,
    );

    // An unpaired peer presenting the live code is paired and acknowledged.
    inbound_tx.send(msg("@new:srv", "123456")).await.unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let payload = loop {
        let seen = ev.audited.lock().unwrap().clone();
        if let Some((_, payload)) = seen.iter().find(|(a, _)| a == actions::REPLY_UNDELIVERED) {
            assert!(seen.iter().any(|(a, _)| a == actions::PAIRED), "the ack answers a pairing: {seen:?}");
            break payload.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a refused ack must audit {}; saw {seen:?}",
            actions::REPLY_UNDELIVERED,
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(payload["channel"], "matrix");
    assert_eq!(payload["peer"], "@new:srv");
    assert_eq!(payload["reason"], "send_failed");
    assert!(payload["observed_at"].is_string(), "every writer stamps the event: {payload}");
    let rendered = payload.to_string();
    assert!(!rendered.contains(PAIRED_ACK_BODY), "never the ack's body: {rendered}");
    bus.shutdown().await;
}
