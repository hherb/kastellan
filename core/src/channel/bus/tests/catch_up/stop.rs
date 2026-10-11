//! #832: what a bus stop abandons, through a running bus — each case either
//! recoverable (a queued reply stays unclaimed) or said on `[audit-lost]`
//! (through `FakeEvents::lost` here). A child of the claim tests for their
//! `Backlog` fake.

use super::*;
use std::sync::atomic::AtomicBool;

/// A channel whose `send` records and then, when `park` is set, never
/// returns — a transport wedged mid-send, so the pump stays on the item it
/// took and everything behind it stays queued. `recv` parks on a sender the
/// test holds.
struct StopChannel {
    inbound_rx: mpsc::Receiver<IncomingMessage>,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    park: bool,
}
#[async_trait::async_trait]
impl Channel for StopChannel {
    fn id(&self) -> ChannelId {
        matrix()
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.inbound_rx.recv().await
    }
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg);
        if self.park {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

struct Stopped {
    bus: ChannelBus,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    inbound: mpsc::Sender<IncomingMessage>,
}

fn bus_with(
    events: Arc<dyn ChannelEvents>,
    completed: Arc<dyn CompletedTasks>,
    park: bool,
    asks: Option<Arc<AskWiring>>,
) -> Stopped {
    let (inbound, inbound_rx) = mpsc::channel(1);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let channel = StopChannel { inbound_rx, sent: sent.clone(), park };
    let bus = ChannelBus::spawn(
        vec![Box::new(channel)],
        Arc::new(StaticPairings::from_peers([PeerId("@me:srv".into())])),
        None,
        events,
        completed,
        asks,
    );
    Stopped { bus, sent, inbound }
}

/// Poll `cond` until true, or fail after 5 s (real time; multi-thread tests).
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// `ChannelEvents` whose `enqueue` never returns — a wedged Postgres under
/// the inbound path's non-audit await — after flagging that it was reached.
#[derive(Default)]
struct ParkingEnqueue {
    entered: AtomicBool,
    lost: Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl ChannelEvents for ParkingEnqueue {
    async fn enqueue(&self, _lane: Lane, _payload: Value) -> anyhow::Result<i64> {
        self.entered.store(true, Ordering::SeqCst);
        std::future::pending().await
    }
    async fn audit(&self, _action: &str, _payload: Value) {}
    fn report_lost(&self, line: &str) {
        self.lost.lock().unwrap().push(line.to_string());
    }
}

/// W1: the transport has handed the message over (Matrix's cursor is past
/// it), and the stop lands while it is being enqueued. No task, no row — so
/// the line is the only trace, and it must name the peer, never the body.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_mid_enqueue_reports_the_inbound_message() {
    let ev = Arc::new(ParkingEnqueue::default());
    let s = bus_with(ev.clone(), Arc::new(ParkingCompleted), false, None);

    s.inbound.send(msg("@me:srv", "SECRET-INBOUND-BODY: summarise my mail")).await.unwrap();
    eventually("the enqueue is under way", || ev.entered.load(Ordering::SeqCst)).await;
    s.bus.shutdown().await;

    let lost = ev.lost.lock().unwrap().clone();
    assert_eq!(
        lost,
        [r#"a message for channel "matrix", peer "@me:srv" was abandoned mid-handling: the bus stopped while it was in flight; it may not have been enqueued or recorded"#]
    );
}

/// W5: raised asks still queued when the pump ends are not recoverable (no
/// task reply to re-find; `ask.delivered` was their row) — each is said,
/// by channel and peer, never by its body, which carries the approval token.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_with_asks_queued_reports_each_ask() {
    let ev = Arc::new(FakeEvents::default());
    let outbox = Arc::new(ChannelOutbox::new());
    let resolver: Arc<dyn AskResolver> = Arc::new(RecordingResolver::default());
    let wiring = Arc::new(AskWiring { outbox: outbox.clone(), resolver });
    let s = bus_with(ev.clone(), Arc::new(ParkingCompleted), true, Some(wiring));

    for n in 0..3 {
        outbox
            .try_deliver(OutgoingMessage {
                channel: matrix(),
                peer: PeerId("@me:srv".into()),
                conversation: ConversationId("!room:srv".into()),
                body: format!("SECRET-ASK-BODY {n}: /approve tok{n}"),
            })
            .expect("queued");
    }
    // The pump takes the first and wedges in its send; two stay queued.
    eventually("the first ask is in the transport", || s.sent.lock().unwrap().len() == 1).await;
    s.bus.shutdown().await;

    let lost = ev.lost.lock().unwrap().clone();
    let line = r#"an ask message for channel "matrix", peer "@me:srv" was not sent: the bus stopped with it queued"#;
    assert_eq!(lost, [line, line], "one line per queued ask");
}

/// W2: replies still queued at a stop were never claimed, so they say
/// nothing and a fresh bus delivers them. Only the reply wedged in the
/// transport — claimed, `channel.replied`, mid-send — is said.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_with_replies_queued_leaves_them_unclaimed() {
    let rows = vec![(1, channel_row("one")), (2, channel_row("two")), (3, channel_row("three"))];
    let (backlog, _n) = Backlog::new(rows);
    let ev = Arc::new(FakeEvents::default());
    let s = bus_with(ev.clone(), Arc::new(backlog.clone()), true, None);

    eventually("the first reply is in the transport", || s.sent.lock().unwrap().len() == 1).await;
    s.bus.shutdown().await;

    assert_eq!(backlog.disposition(1), Some(ReplyDisposition::Routed), "claimed, mid-send");
    assert_eq!((backlog.disposition(2), backlog.disposition(3)), (None, None), "queued, unclaimed");
    assert_eq!(
        *ev.lost.lock().unwrap(),
        [r#"task 1's reply for channel "matrix", peer "@me:srv" may not have been sent: the bus stopped while it was in flight"#]
    );

    // The next bus's start sweep delivers what the stop left queued.
    let next = bus_with(Arc::new(FakeEvents::default()), Arc::new(backlog.clone()), false, None);
    eventually("the two queued replies", || next.sent.lock().unwrap().len() == 2).await;
    let bodies: Vec<String> = next.sent.lock().unwrap().iter().map(|m| m.body.clone()).collect();
    assert_eq!(bodies, ["two", "three"]);
    next.bus.shutdown().await;
}

/// The unroutable twin of W3: a stop on the claim that settles a reply
/// nobody can deliver may leave it settled with no row. Staged with a claim
/// that never returns.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_mid_unroutable_claim_reports_it() {
    struct ParkingClaim {
        inner: Arc<Backlog>,
        entered: AtomicBool,
    }
    #[async_trait::async_trait]
    impl CompletedTasks for ParkingClaim {
        async fn next_completed(&self) -> Option<i64> {
            std::future::pending().await
        }
        async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
            self.inner.load(id).await
        }
        async fn claim(&self, _id: i64, _d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
            self.entered.store(true, Ordering::SeqCst);
            std::future::pending().await
        }
        async fn unsettled(&self, a: i64, l: i64) -> anyhow::Result<Vec<i64>> {
            self.inner.unsettled(a, l).await
        }
    }
    let unroutable = (serde_json::json!({"kind":"channel","peer":"@me:srv"}), None);
    let (inner, _n) = Backlog::new(vec![(9, unroutable)]);
    let completed = Arc::new(ParkingClaim { inner, entered: AtomicBool::new(false) });
    let ev = Arc::new(FakeEvents::default());
    let s = bus_with(ev.clone(), completed.clone(), false, None);

    eventually("the unroutable claim is under way", || completed.entered.load(Ordering::SeqCst)).await;
    s.bus.shutdown().await;

    assert_eq!(
        *ev.lost.lock().unwrap(),
        ["task 9's unroutable reply may have been settled without its channel.reply_unroutable row: the bus stopped while it was in flight"]
    );
}

/// W6: `shutdown` used to `let _ = h.await`, so a pump that panicked left
/// nothing behind but the death signal. It is now counted (and logged at
/// ERROR). Debug and test builds only: release is `panic = "abort"`.
#[tokio::test(flavor = "multi_thread")]
async fn a_panicked_pump_is_counted_at_shutdown() {
    struct PanickingChannel;
    #[async_trait::async_trait]
    impl Channel for PanickingChannel {
        fn id(&self) -> ChannelId {
            matrix()
        }
        async fn recv(&mut self) -> Option<IncomingMessage> {
            panic!("test: the transport's recv panicked");
        }
        async fn send(&self, _msg: OutgoingMessage) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let bus = ChannelBus::spawn(
        vec![Box::new(PanickingChannel)],
        Arc::new(StaticPairings::new()),
        None,
        Arc::new(FakeEvents::default()),
        Arc::new(ParkingCompleted),
        None,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), bus.death_signal())
        .await
        .expect("the panicked pump rang the bell");

    assert_eq!(bus.shutdown().await, 1, "one pump panicked; the aborted one did not");
}
