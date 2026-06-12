//! Hermetic full-loop test of the channel bus: a FakeChannel feeds an inbound
//! message; the bus screens + "enqueues" via a fake ChannelEvents; a fake
//! CompletedTasks then yields a matching completed task; the routed reply must
//! arrive back on the FakeChannel's outbox. No Postgres, no network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use kastellan_core::channel::auth::StaticPairings;
use kastellan_core::channel::bus::{ChannelBus, ChannelEvents, CompletedTasks};
use kastellan_core::channel::{
    Channel, ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerId,
};
use kastellan_db::tasks::Lane;

// ── A FakeChannel: feeds one inbound message, records outbound sends. ──
struct FakeChannel {
    id: ChannelId,
    inbound: Mutex<Vec<IncomingMessage>>,
    outbox: Arc<Mutex<Vec<OutgoingMessage>>>,
}
#[async_trait::async_trait]
impl Channel for FakeChannel {
    fn id(&self) -> ChannelId {
        self.id.clone()
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        let next = self.inbound.lock().unwrap().pop();
        if next.is_none() {
            // Park forever after draining so the select! stays alive for outbound.
            std::future::pending::<()>().await;
        }
        next
    }
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()> {
        self.outbox.lock().unwrap().push(msg);
        Ok(())
    }
}

// ── Fake DB seams (shared `enqueued` lets the completion echo the routing). ──
#[derive(Clone, Default)]
struct FakeEvents {
    enqueued: Arc<Mutex<Vec<Value>>>,
}
#[async_trait::async_trait]
impl ChannelEvents for FakeEvents {
    async fn enqueue(&self, _lane: Lane, payload: Value) -> anyhow::Result<i64> {
        self.enqueued.lock().unwrap().push(payload);
        Ok(1)
    }
    async fn audit(&self, _action: &str, _payload: Value) {}
}

struct FakeCompleted {
    enqueued: Arc<Mutex<Vec<Value>>>,
    yielded: bool,
}
#[async_trait::async_trait]
impl CompletedTasks for FakeCompleted {
    async fn next_completed(&mut self) -> Option<i64> {
        if self.yielded {
            // One completion total, then park so the pump stays alive.
            std::future::pending::<()>().await;
        }
        // Wait until the inbound message has been enqueued, then complete it.
        loop {
            if !self.enqueued.lock().unwrap().is_empty() {
                self.yielded = true;
                return Some(1);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn load(&self, _id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        let payload = self.enqueued.lock().unwrap().first().cloned();
        Ok(payload.map(|p| (p, Some(json!({"kind": "completed", "message": "You have 2 meetings."})))))
    }
}

#[tokio::test]
async fn inbound_message_round_trips_to_a_reply() {
    let outbox = Arc::new(Mutex::new(Vec::<OutgoingMessage>::new()));
    let ch = FakeChannel {
        id: ChannelId("matrix".into()),
        inbound: Mutex::new(vec![IncomingMessage {
            channel: ChannelId("matrix".into()),
            peer: PeerId("@me:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "what's on my calendar?".into(),
        }]),
        outbox: outbox.clone(),
    };

    let events = FakeEvents::default();
    let completed = FakeCompleted { enqueued: events.enqueued.clone(), yielded: false };

    let bus = ChannelBus::spawn(
        vec![Box::new(ch)],
        Arc::new(StaticPairings::from_peers([PeerId("@me:srv".into())])),
        Arc::new(events),
        Box::new(completed),
    );

    // Poll the outbox until the reply lands (bounded), then shutdown.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(m) = outbox.lock().unwrap().first().cloned() {
            assert_eq!(m.body, "You have 2 meetings.");
            assert_eq!(m.conversation, ConversationId("!room:srv".into()));
            assert_eq!(m.peer, PeerId("@me:srv".into()));
            assert_eq!(m.channel, ChannelId("matrix".into()));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "reply never arrived");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    bus.shutdown().await;
}
