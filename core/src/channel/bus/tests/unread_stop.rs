//! #846 at the bus: a stop drops a polled channel's inbound buffer with
//! messages in it the bus never read. The channel's [`InboundReceiver`]
//! reports them as it is dropped — inside `ChannelBus::shutdown`, which
//! aborts the pump that owns the channel and joins it. So the report is made
//! before `shutdown` returns, which is what puts its row ahead of the
//! daemon's audit-sink drain.
//!
//! [`InboundReceiver`]: crate::channel::polled_driver::InboundReceiver

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::channel::matrix::MatrixChannel;
use crate::channel::polled_driver::{
    DriverAudit, InboundDropStage, InboundDropped, PolledBatch, PolledEvent, PolledWorkerDriver,
    PolledWorkerSpec, WorkerCalls, REFUSAL_BACKOFF, REPLY_GIVE_UP,
};

/// A worker whose first poll answers three messages from one peer; every
/// later poll is empty. It counts its polls: a second poll means the driver
/// has handed all three to the bus's buffer (it polls again only after the
/// batch's last send returned).
struct ThreeThenNothing {
    polls: Arc<AtomicUsize>,
}

impl WorkerCalls for ThreeThenNothing {
    fn call(&self, method: &str, _params: Value) -> anyhow::Result<Value> {
        if method.ends_with(".poll") && self.polls.fetch_add(1, Ordering::SeqCst) == 0 {
            let ev = |i: usize| serde_json::json!({"peer": "@me:srv", "body": format!("m{i}")});
            return Ok(serde_json::json!({"events": [ev(0), ev(1), ev(2)]}));
        }
        Ok(serde_json::json!({"events": []}))
    }
}

fn parse(v: Value) -> anyhow::Result<PolledBatch> {
    let events = v["events"].as_array().cloned().unwrap_or_default();
    Ok(PolledBatch::whole(events
        .into_iter()
        .map(|e| PolledEvent {
            peer: e["peer"].as_str().unwrap_or_default().into(),
            conversation: "!room:srv".into(),
            body: e["body"].as_str().unwrap_or_default().into(),
            evidence: None,
            ack_token: None,
        })
        .collect()))
}

fn encode(_m: &OutgoingMessage) -> Value {
    serde_json::json!({})
}

/// An authorizer that never answers: the pump that asks it holds the first
/// message forever, so the rest stay in the buffer, unread.
struct NeverAnswers {
    asked: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PeerAuthorizer for NeverAnswers {
    async fn authorize(&self, _c: &ChannelId, _p: &PeerId, _e: Option<&PeerEvidence>) -> AuthDecision {
        self.asked.store(true, Ordering::SeqCst);
        std::future::pending().await
    }
}

/// Wait (bounded) until `cond` holds.
async fn until(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cond() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bus_stop_reports_the_messages_its_pump_never_read_before_shutdown_returns() {
    let spec = PolledWorkerSpec {
        label: "bus-unread",
        init_method: "x.init",
        poll_method: "x.poll",
        send_method: "x.send",
        ack_method: None,
        poll_timeout_ms: 5,
        refusal_backoff: REFUSAL_BACKOFF,
        reply_give_up: REPLY_GIVE_UP,
        drain: None,
    };
    let polls = Arc::new(AtomicUsize::new(0));
    let dropped: Arc<Mutex<Vec<(InboundDropStage, usize)>>> = Arc::default();
    let sink = Arc::clone(&dropped);
    let audit = DriverAudit {
        inbound_dropped: Some(Arc::new(move |d: InboundDropped<'_>| {
            sink.lock().unwrap().push((d.stage, d.dropped.get()))
        })),
        ..DriverAudit::none()
    };
    let id = ChannelId("matrix".into());
    let calls = Box::new(ThreeThenNothing { polls: Arc::clone(&polls) });
    let (driver, _identity) =
        PolledWorkerDriver::spawn(spec, calls, parse, encode, None, None, audit, id.clone())
            .expect("driver spawn");
    let asked = Arc::new(AtomicBool::new(false));
    let bus = ChannelBus::spawn(
        vec![Box::new(MatrixChannel::from_driver(id, driver))],
        Arc::new(NeverAnswers { asked: Arc::clone(&asked) }),
        None,
        Arc::new(FakeEvents::default()),
        Arc::new(ParkingCompleted),
        None,
    );

    // The pump has read message 0 and is stuck on it; the driver has handed
    // over all three, so 1 and 2 sit in the buffer.
    until("the pump to read the first message", || asked.load(Ordering::SeqCst)).await;
    until("the driver to finish the batch", || polls.load(Ordering::SeqCst) >= 2).await;
    assert!(dropped.lock().unwrap().is_empty(), "nothing reported while the bus runs");

    bus.shutdown().await;

    // Read at once: the report was made inside `shutdown`, not after it.
    assert_eq!(*dropped.lock().unwrap(), [(InboundDropStage::Buffered, 2)]);
}
