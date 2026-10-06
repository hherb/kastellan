//! #815: a message or reply the bus drops must leave an audit row.
//!
//! Before #815 both drops were a `warn!` only — invisible under
//! `RUST_LOG=error`, and absent from `audit_log`, so the channel rows no
//! longer accounted for every message in and every reply out. Each drop now
//! writes a row; if that row's own insert fails, the bus's writer already says
//! so on `[audit-lost]` (#808) — and, since #813, so does an insert still
//! awaited when the bus is stopped, shown here against the real writer over a
//! stalled pool, since the in-process fakes have no insert to abandon.

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

/// Yields one completed task once `release` fires, then parks: the
/// completion arrives exactly when the test says, not before the per-channel
/// pump has ended.
struct GatedCompleted {
    release: Option<tokio::sync::oneshot::Receiver<()>>,
    row: (Value, Option<Value>),
}
#[async_trait::async_trait]
impl CompletedTasks for GatedCompleted {
    async fn next_completed(&mut self) -> Option<i64> {
        match self.release.take() {
            Some(release) => {
                let _ = release.await;
                Some(7)
            }
            None => std::future::pending().await,
        }
    }
    async fn load(&self, _id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        Ok(Some(self.row.clone()))
    }
}

/// What [`record_lost`] was told: the real bus writer's `[audit-lost]` lines.
static LOST: Mutex<Vec<(crate::worker_stderr::AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_lost(writer: crate::worker_stderr::AuditLostWriter, line: &str) {
    LOST.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #813, through the sequence its #824 comment names: a per-channel pump
/// ends and rings the death bell; a completion for that channel then finds
/// its queue closed, so the outbound pump awaits a `queue_closed` row; and
/// the channel supervisor, reacting to the bell, stops the bus — aborting
/// the pump that is awaiting that very insert. Against a wedged Postgres the
/// abort wins, and before #813 the row left no trace at all.
///
/// The real writer (`PgChannelEvents`) over a stalled pool, not a fake: the
/// guard under test lives in its insert.
#[tokio::test(flavor = "multi_thread")]
async fn a_queue_closed_row_abandoned_by_the_stop_is_reported() {
    let (pool, listener) = crate::channel::pg_events::test_support::stalled_pool();
    let events = Arc::new(crate::channel::pg_events::PgChannelEvents::with_reporter(pool, record_lost));
    let (release, gate) = tokio::sync::oneshot::channel();
    let completed = GatedCompleted {
        release: Some(gate),
        row: (
            serde_json::json!({"kind":"channel","channel":"matrix","peer":"@me:srv","conversation":"!room:srv"}),
            Some(serde_json::json!({"kind":"completed","message": SECRET_BODY})),
        ),
    };
    let (inbound_tx, inbound_rx) = mpsc::channel::<IncomingMessage>(1);
    let channel = RefusingChannel { id: ChannelId("matrix".into()), inbound_rx };
    let bus = ChannelBus::spawn(
        vec![Box::new(channel)],
        Arc::new(StaticPairings::new()),
        None,
        events,
        Box::new(completed),
        None,
    );

    // The per-channel pump ends: its inbound closes, and the bell rings.
    drop(inbound_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), bus.death_signal())
        .await
        .expect("a pump whose inbound closed rings the bell");

    // A completion for that channel now: its queue is closed, so the outbound
    // pump writes `queue_closed` — and the stalled pool holds the insert.
    release.send(()).unwrap();
    let _held = crate::channel::pg_events::test_support::connected(&listener).await;

    // The supervisor's reaction to the bell.
    bus.shutdown().await;

    let lost = LOST.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(lost[0].0, crate::worker_stderr::AuditLostWriter::Bus);
    assert!(
        lost[0].1.starts_with(
            r#"channel.reply_undelivered row for channel "matrix", peer "@me:srv", reason "queue_closed" may not have been written"#
        ),
        "{lost:?}"
    );
    assert!(!lost[0].1.contains(SECRET_BODY), "never the body: {lost:?}");
}
