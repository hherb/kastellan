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

/// The outbound half: the per-channel pump has gone (its receiver dropped).
/// Until #825 `handle_completed` wrote `channel.reply_undelivered`
/// (`queue_closed`) here and the reply was lost. Now it leaves the reply
/// unclaimed — the next bus's catch-up sweep delivers it — so there is no
/// drop to record, and no row. The claim-then-close race that still writes
/// `queue_closed` is
/// `catch_up::a_queue_closing_after_the_claim_still_writes_queue_closed`.
#[tokio::test]
async fn a_reply_to_a_closed_send_queue_writes_no_row() {
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

    assert!(handle_completed(&completed, &ev, &senders, 7, Via::Notify).await.is_none(), "nothing was sent");

    let audited = ev.audited.lock().unwrap().clone();
    assert!(audited.is_empty(), "left for catch-up, not dropped: {audited:?}");
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
/// completion arrives exactly when the test says.
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
    /// Claims always succeed: this fake predates the reply claim (#825),
    /// whose own semantics are tested against `bus/tests/catch_up.rs`'s
    /// `Backlog` and real Postgres.
    async fn claim(&self, _id: i64, _d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        let now = time::OffsetDateTime::now_utc();
        Ok(Some(ClaimedReply { created_at: now, finished_at: Some(now) }))
    }
    async fn unsettled(&self, _after_id: i64, _limit: i64) -> anyhow::Result<Vec<i64>> {
        Ok(Vec::new())
    }
}

/// Accepts every send and parks on `recv`, so the only row the bus awaits is
/// the outbound pump's own `channel.replied` — a refusing transport would add
/// a second insert (`reply_undelivered`, `send_failed`) on the same stalled
/// pool.
struct AcceptingChannel {
    inbound_rx: mpsc::Receiver<IncomingMessage>,
}
#[async_trait::async_trait]
impl Channel for AcceptingChannel {
    fn id(&self) -> ChannelId {
        ChannelId("matrix".into())
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.inbound_rx.recv().await
    }
    async fn send(&self, _msg: OutgoingMessage) -> anyhow::Result<()> {
        Ok(())
    }
}

/// What [`record_lost`] was told: the real bus writer's lost-row reports, as
/// they reach the `[audit-lost]` emitter (before it folds in the writer and
/// the marker).
static LOST: Mutex<Vec<(crate::worker_stderr::AuditLostWriter, String)>> = Mutex::new(Vec::new());

fn record_lost(writer: crate::worker_stderr::AuditLostWriter, line: &str) {
    LOST.lock().unwrap_or_else(|p| p.into_inner()).push((writer, line.to_string()));
}

/// #813 through the outbound pump: a completion is claimed and routed, and
/// the pump then awaits its `channel.replied` insert — which a wedged
/// Postgres holds. The channel supervisor stops the bus (as it does on any
/// death), aborting the pump mid-insert, and #813's guard reports the row
/// that may not have been written.
///
/// Until #825 this was staged through a `queue_closed` row (a completion for
/// a channel whose pump had ended). A queue found closed now leaves the
/// reply for catch-up and writes no row, so the abandoned insert is the
/// `channel.replied` one — the same guard, the outbound pump's commonest row.
///
/// The real writer (`PgChannelEvents`) over a stalled pool, not a fake: the
/// guard under test lives in its insert.
#[tokio::test(flavor = "multi_thread")]
async fn a_replied_row_abandoned_by_the_stop_is_reported() {
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
    let channel = AcceptingChannel { inbound_rx };
    let bus = ChannelBus::spawn(
        vec![Box::new(channel)],
        Arc::new(StaticPairings::new()),
        None,
        events,
        Box::new(completed),
        None,
    );

    // The completion: claimed, queued, and its `channel.replied` insert
    // reaches the stalled pool, which holds it.
    release.send(()).unwrap();
    let _held = crate::channel::pg_events::test_support::connected(&listener).await;

    // The supervisor stops the bus.
    bus.shutdown().await;
    drop(inbound_tx);

    let lost = LOST.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(lost[0].0, crate::worker_stderr::AuditLostWriter::Bus);
    assert!(
        lost[0].1.starts_with(
            r#"channel.replied row for channel "matrix", peer "@me:srv", task_id 7 may not have been written"#
        ),
        "{lost:?}"
    );
    assert!(!lost[0].1.contains(SECRET_BODY), "never the body: {lost:?}");
}
