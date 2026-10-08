//! #825: the catch-up sweep, through a running bus. A child of the claim
//! tests so it shares their `Backlog` fake (and `channel_row`,
//! `matrix_sender`) without widening them.

use super::*;

/// A channel whose `send` records, and whose `recv` parks on a sender the
/// test holds (dropping it is how a test would end the inbound pump).
struct RecordingChannel {
    inbound_rx: mpsc::Receiver<IncomingMessage>,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    refuse: bool,
}
#[async_trait::async_trait]
impl Channel for RecordingChannel {
    fn id(&self) -> ChannelId {
        ChannelId("matrix".into())
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.inbound_rx.recv().await
    }
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg);
        if self.refuse {
            anyhow::bail!("test transport refuses (EmailChannel::send's shape)");
        }
        Ok(())
    }
}

struct Rig {
    bus: ChannelBus,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    ev: Arc<FakeEvents>,
    _inbound: mpsc::Sender<IncomingMessage>,
}

fn rig(backlog: &Arc<Backlog>, refuse: bool) -> Rig {
    let (inbound_tx, inbound_rx) = mpsc::channel(1);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let ev = Arc::new(FakeEvents::default());
    let channel = RecordingChannel { inbound_rx, sent: sent.clone(), refuse };
    let bus = ChannelBus::spawn(
        vec![Box::new(channel)],
        Arc::new(StaticPairings::new()),
        None,
        ev.clone(),
        Box::new(backlog.clone()),
        None,
    );
    Rig { bus, sent, ev, _inbound: inbound_tx }
}

/// Poll `cond` until true, or fail after 5 s (real time; multi-thread tests).
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn bodies(sent: &Mutex<Vec<OutgoingMessage>>) -> Vec<String> {
    sent.lock().unwrap().iter().map(|m| m.body.clone()).collect()
}

/// The backlog left while no bus listened goes out when one starts.
/// (RF 4) In task-id order.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_bus_delivers_the_backlog_in_task_order() {
    let (backlog, _n) =
        Backlog::new(vec![(3, channel_row("c")), (1, channel_row("a")), (2, channel_row("b"))]);
    let r = rig(&backlog, false);
    eventually("three sends", || r.sent.lock().unwrap().len() == 3).await;
    assert_eq!(bodies(&r.sent), ["a", "b", "c"]);
    let rows = r.ev.audited.lock().unwrap().clone();
    assert!(rows.iter().all(|(a, p)| a == actions::REPLIED && p["via"] == "catch_up"), "{rows:?}");
    r.bus.shutdown().await;
}

/// A NOTIFY for a task the start sweep already routed sends nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn a_notify_after_the_sweep_does_not_resend() {
    let (backlog, notify) = Backlog::new(vec![(1, channel_row("a"))]);
    let r = rig(&backlog, false);
    eventually("the sweep's send", || r.sent.lock().unwrap().len() == 1).await;
    notify.send(1).unwrap();
    // Let the pump take the NOTIFY: its claim call is the proof it ran.
    eventually("the NOTIFY's claim", || backlog.claim_calls.load(Ordering::SeqCst) == 2).await;
    assert_eq!(r.sent.lock().unwrap().len(), 1, "exactly one send");
    r.bus.shutdown().await;
}

/// (RF 5) A task for a channel no bus here serves is never claimed, and the
/// cursor moves past it: more than a page of them does not starve the one
/// behind.
#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_pages_past_tasks_it_does_not_serve() {
    let mut rows: Vec<(i64, (Value, Option<Value>))> = (1..=150)
        .map(|id| {
            let mut row = channel_row("not mine");
            row.0["channel"] = "email".into();
            (id, row)
        })
        .collect();
    rows.push((151, channel_row("mine")));
    let (backlog, _n) = Backlog::new(rows);
    let r = rig(&backlog, false);
    eventually("the served task", || r.sent.lock().unwrap().len() == 1).await;
    assert_eq!(bodies(&r.sent), ["mine"]);
    assert!((1..=150).all(|id| backlog.disposition(id).is_none()), "unserved stays unclaimed");
    r.bus.shutdown().await;
}

/// (RF 3) A backlog far bigger than the per-channel queue (32) all goes
/// out: the sweep waits on the queue while the per-channel pump drains it.
#[tokio::test(flavor = "multi_thread")]
async fn a_backlog_larger_than_the_queue_is_all_delivered() {
    let rows = (1..=80).map(|id| (id, channel_row(&format!("r{id}")))).collect();
    let (backlog, _n) = Backlog::new(rows);
    let r = rig(&backlog, false);
    eventually("eighty sends", || r.sent.lock().unwrap().len() == 80).await;
    r.bus.shutdown().await;
}

/// (RF 2) A transport that refuses every send — email's today — is tried
/// once per reply: claimed, refused, recorded, never swept again.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_catch_up_reply_is_not_resent_by_later_sweeps() {
    let (backlog, _n) = Backlog::new(vec![(1, channel_row("a"))]);
    let r = rig(&backlog, true);
    eventually("the refused send", || {
        r.ev.audited.lock().unwrap().iter().any(|(a, _)| a == actions::REPLY_UNDELIVERED)
    })
    .await;
    // A second sweep over the same backlog (as the periodic tick would run).
    let (senders, _rx) = matrix_sender();
    sweep(&backlog, &*r.ev, &senders).await;
    assert_eq!(r.sent.lock().unwrap().len(), 1, "one attempt");
    let undelivered = r
        .ev
        .audited
        .lock()
        .unwrap()
        .iter()
        .filter(|(a, _)| a == actions::REPLY_UNDELIVERED)
        .count();
    assert_eq!(undelivered, 1);
    r.bus.shutdown().await;
}

/// A sweep that cannot read the backlog says so and carries on: it is not a
/// pump death, so the bell does not ring.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sweep_does_not_end_the_pump() {
    let (backlog, notify) = Backlog::new(vec![(1, channel_row("a"))]);
    backlog.unsettled_fails.store(true, Ordering::SeqCst);
    let r = rig(&backlog, false);
    eventually("the start sweep ran", || backlog.unsettled_calls.load(Ordering::SeqCst) >= 1).await;
    let died = tokio::time::timeout(std::time::Duration::from_millis(200), r.bus.death_signal()).await;
    assert!(died.is_err(), "a failed sweep must not ring the death bell");
    // The live path still works.
    notify.send(1).unwrap();
    eventually("the NOTIFY's send", || r.sent.lock().unwrap().len() == 1).await;
    r.bus.shutdown().await;
}

/// The periodic tick re-sweeps: a reply whose load failed while the bus
/// stayed up goes out on the next tick, without a restart.
#[tokio::test(start_paused = true)]
async fn the_periodic_tick_resweeps_the_backlog() {
    let (backlog, _n) = Backlog::new(vec![(1, channel_row("a"))]);
    backlog.load_fails_once.lock().unwrap().insert(1); // the start sweep's load fails
    let r = rig(&backlog, false);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(backlog.unsettled_calls.load(Ordering::SeqCst), 1, "the start sweep only");
    assert!(r.sent.lock().unwrap().is_empty(), "its load failed");

    tokio::time::sleep(crate::channel::catch_up::SWEEP_EVERY + std::time::Duration::from_secs(1)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(backlog.unsettled_calls.load(Ordering::SeqCst) >= 2, "the tick swept again");
    assert_eq!(bodies(&r.sent), ["a"]);
    r.bus.shutdown().await;
}
