//! #825: the reply claim, and the catch-up sweep that relies on it.
//!
//! `Backlog` is a `CompletedTasks` with real claim semantics in memory: a
//! task is claimable once, and only when it has a row. NOTIFY ids come in on
//! an mpsc the test holds, so a test decides when "Postgres" announces one.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::channel::catch_up::Via;

/// A channel task routed to `@me:srv` on `matrix`, answering `body`.
fn channel_row(body: &str) -> (Value, Option<Value>) {
    (
        serde_json::json!({"kind":"channel","channel":"matrix","peer":"@me:srv","conversation":"!room:srv"}),
        Some(serde_json::json!({"kind":"completed","message": body})),
    )
}

struct Backlog {
    rows: Mutex<HashMap<i64, (Value, Option<Value>)>>,
    /// When each task finished, relative to the claim's `now`.
    finished_ago: time::Duration,
    settled: Mutex<HashMap<i64, ReplyDisposition>>,
    /// Ids whose next `load` fails (each once).
    load_fails_once: Mutex<HashSet<i64>>,
    /// When set, every `unsettled` call fails.
    unsettled_fails: AtomicBool,
    unsettled_calls: AtomicUsize,
    claim_calls: AtomicUsize,
    notify_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<i64>>,
}

impl Backlog {
    /// A backlog of `rows` whose tasks all finished one second ago (on time),
    /// plus the sender a test announces NOTIFYs on. Keep the sender alive:
    /// dropping it parks `next_completed` for good.
    fn new(rows: Vec<(i64, (Value, Option<Value>))>) -> (Arc<Self>, mpsc::UnboundedSender<i64>) {
        Self::finished(rows, time::Duration::seconds(1))
    }

    /// As [`Backlog::new`], with every task finished `ago` before the claim.
    fn finished(
        rows: Vec<(i64, (Value, Option<Value>))>,
        ago: time::Duration,
    ) -> (Arc<Self>, mpsc::UnboundedSender<i64>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let backlog = Arc::new(Self {
            rows: Mutex::new(rows.into_iter().collect()),
            finished_ago: ago,
            settled: Mutex::new(HashMap::new()),
            load_fails_once: Mutex::new(HashSet::new()),
            unsettled_fails: AtomicBool::new(false),
            unsettled_calls: AtomicUsize::new(0),
            claim_calls: AtomicUsize::new(0),
            notify_rx: tokio::sync::Mutex::new(rx),
        });
        (backlog, tx)
    }

    fn disposition(&self, id: i64) -> Option<ReplyDisposition> {
        self.settled.lock().unwrap().get(&id).copied()
    }
}

#[async_trait::async_trait]
impl CompletedTasks for Arc<Backlog> {
    async fn next_completed(&mut self) -> Option<i64> {
        match self.notify_rx.lock().await.recv().await {
            Some(id) => Some(id),
            None => std::future::pending().await, // the test dropped its sender: park
        }
    }
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        if self.load_fails_once.lock().unwrap().remove(&id) {
            anyhow::bail!("SECRET-DB-TEXT: load refused");
        }
        Ok(self.rows.lock().unwrap().get(&id).cloned())
    }
    async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        self.claim_calls.fetch_add(1, Ordering::SeqCst);
        let mut settled = self.settled.lock().unwrap();
        if settled.contains_key(&id) || !self.rows.lock().unwrap().contains_key(&id) {
            return Ok(None);
        }
        settled.insert(id, d);
        let finished = time::OffsetDateTime::now_utc() - self.finished_ago;
        Ok(Some(ClaimedReply {
            created_at: finished - time::Duration::minutes(1),
            finished_at: Some(finished),
        }))
    }
    async fn unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>> {
        self.unsettled_calls.fetch_add(1, Ordering::SeqCst);
        if self.unsettled_fails.load(Ordering::SeqCst) {
            anyhow::bail!("SECRET-DB-TEXT: backlog read refused");
        }
        let settled = self.settled.lock().unwrap();
        let mut ids: Vec<i64> = self
            .rows
            .lock()
            .unwrap()
            .keys()
            .copied()
            .filter(|id| *id > after_id && !settled.contains_key(id))
            .collect();
        ids.sort_unstable();
        ids.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(ids)
    }
}

/// One open queue for `matrix`, plus its receiver.
fn matrix_sender() -> (HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>, mpsc::Receiver<OutgoingMessage>) {
    let (tx, rx) = mpsc::channel::<OutgoingMessage>(64);
    (HashMap::from([(ChannelId("matrix".into()), tx)]), rx)
}

fn actions_of(ev: &FakeEvents) -> Vec<String> {
    ev.audited.lock().unwrap().iter().map(|(a, _)| a.clone()).collect()
}

#[tokio::test]
async fn a_routed_reply_is_claimed_and_its_row_says_via() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let (senders, mut rx) = matrix_sender();

    let out = handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.expect("routed");

    assert_eq!(out.body, "done", "on time: no note");
    assert_eq!(rx.recv().await.unwrap().body, "done");
    assert_eq!(backlog.disposition(7), Some(ReplyDisposition::Routed));
    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, actions::REPLIED);
    assert_eq!(audited[0].1["via"], "notify");
    assert!(audited[0].1.get("delayed_secs").is_none(), "on time: {}", audited[0].1);
}

#[tokio::test]
async fn a_second_route_of_a_settled_task_sends_nothing_and_writes_nothing() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let (senders, mut rx) = matrix_sender();

    handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.expect("first wins");
    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.is_none());

    rx.recv().await.unwrap();
    assert!(rx.try_recv().is_err(), "exactly one send");
    assert_eq!(actions_of(&ev), vec![actions::REPLIED.to_string()]);
}

#[tokio::test]
async fn a_late_reply_carries_the_note_and_delayed_secs() {
    let (backlog, _n) = Backlog::finished(vec![(7, channel_row("done"))], time::Duration::hours(3));
    let ev = FakeEvents::default();
    let (senders, _rx) = matrix_sender();

    let out = handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.expect("routed");

    assert_eq!(out.body, "(Delayed reply — you sent this 3 h 1 min ago.)\n\ndone");
    let row = ev.audited.lock().unwrap()[0].1.clone();
    assert_eq!(row["via"], "catch_up");
    let secs = row["delayed_secs"].as_i64().expect("an integer");
    assert!((3 * 3600..3 * 3600 + 60).contains(&secs), "{row}");
}

/// The common closed-queue case is no longer a drop (#815's row): the next
/// bus's start sweep delivers it, so nothing is claimed and nothing written.
#[tokio::test]
async fn a_closed_queue_leaves_the_reply_for_catch_up() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let (senders, rx) = matrix_sender();
    drop(rx);

    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.is_none());

    assert_eq!(backlog.disposition(7), None, "unclaimed: still in the backlog");
    assert_eq!(backlog.claim_calls.load(Ordering::SeqCst), 0);
    assert!(actions_of(&ev).is_empty(), "{:?}", actions_of(&ev));
}

#[tokio::test]
async fn a_failed_load_leaves_the_reply_for_catch_up() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    backlog.load_fails_once.lock().unwrap().insert(7);
    let ev = FakeEvents::default();
    let (senders, mut rx) = matrix_sender();

    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.is_none());
    assert_eq!(backlog.disposition(7), None);

    // The next route (the sweep's) delivers it.
    handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.expect("delivered on retry");
    assert_eq!(rx.recv().await.unwrap().body, "done");
}

/// A channel task with no routing metadata has no one to reply to. The claim
/// makes its row exactly-once even with two buses seeing every NOTIFY.
#[tokio::test]
async fn an_unroutable_task_is_settled_with_one_row() {
    let row = (serde_json::json!({"kind":"channel","peer":"@me:srv"}), None); // no channel
    let (backlog, _n) = Backlog::new(vec![(9, row)]);
    let ev = FakeEvents::default();
    let (senders, _rx) = matrix_sender();

    for via in [Via::Notify, Via::CatchUp, Via::CatchUp] {
        assert!(handle_completed(&backlog, &ev, &senders, 9, via).await.is_none());
    }

    assert_eq!(backlog.disposition(9), Some(ReplyDisposition::Unroutable));
    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, "channel.reply_unroutable");
    assert_eq!(audited[0].1["task_id"], 9);
    assert!(audited[0].1["observed_at"].is_string(), "{}", audited[0].1);
}

#[tokio::test]
async fn a_reply_for_a_channel_this_bus_does_not_serve_is_not_claimed() {
    let mut row = channel_row("done");
    row.0["channel"] = "email".into();
    let (backlog, _n) = Backlog::new(vec![(7, row)]);
    let ev = FakeEvents::default();
    let (senders, _rx) = matrix_sender();

    assert!(handle_completed(&backlog, &ev, &senders, 7, Via::CatchUp).await.is_none());
    assert_eq!(backlog.disposition(7), None, "the email bus will route it");
    assert!(actions_of(&ev).is_empty());
}

/// The narrow race #815's `queue_closed` row still covers: the queue closes
/// between the closed-queue check and the send. Staged by a claim that drops
/// the queue's receiver before it returns.
#[tokio::test]
async fn a_queue_closing_after_the_claim_still_writes_queue_closed() {
    struct ClosingClaim {
        inner: Arc<Backlog>,
        rx: Mutex<Option<mpsc::Receiver<OutgoingMessage>>>,
    }
    #[async_trait::async_trait]
    impl CompletedTasks for ClosingClaim {
        async fn next_completed(&mut self) -> Option<i64> {
            std::future::pending().await
        }
        async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
            self.inner.load(id).await
        }
        async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
            drop(self.rx.lock().unwrap().take()); // the pump ends right now
            self.inner.claim(id, d).await
        }
        async fn unsettled(&self, a: i64, l: i64) -> anyhow::Result<Vec<i64>> {
            self.inner.unsettled(a, l).await
        }
    }
    let (inner, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let (senders, rx) = matrix_sender();
    let completed = ClosingClaim { inner, rx: Mutex::new(Some(rx)) };
    let ev = FakeEvents::default();

    assert!(handle_completed(&completed, &ev, &senders, 7, Via::Notify).await.is_none());

    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, actions::REPLY_UNDELIVERED);
    assert_eq!(audited[0].1["reason"], "queue_closed");
}
