//! PG-gated e2e for the reply catch-up (#825): replies that finished while no
//! bus listened are delivered when the buses start — once, across two buses
//! (#497) — the late one with its note, and the live NOTIFY path still works
//! after. And (#832) replies still queued when a bus stops are not lost: they
//! were never claimed, so the next bus delivers them. Skip-as-pass without a
//! supervisor/PG.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use kastellan_core::channel::auth::StaticPairings;
use kastellan_core::channel::bus::{ChannelBus, PgChannelEvents, PgCompletedTasks};
use kastellan_core::channel::{actions, Channel, ChannelId, IncomingMessage, OutgoingMessage};
use kastellan_db::tasks::{self, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Records every send and parks on `recv` (the test keeps the sender), so the
/// per-channel pump stays alive the way a real one does between messages.
/// With `park_sends`, a send also never returns after recording: a wedged
/// transport, holding the pump on its first item.
struct RecordingChannel {
    inbound_rx: mpsc::Receiver<IncomingMessage>,
    sent: Arc<Mutex<Vec<OutgoingMessage>>>,
    park_sends: bool,
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
        if self.park_sends {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

fn payload() -> serde_json::Value {
    serde_json::json!({"kind": "channel", "instruction": "q", "channel": "matrix",
                       "peer": "@me:srv", "conversation": "!room:srv"})
}

/// A channel task that finished `finished_ago_mins` ago, asked one minute
/// before that, answering `body` — written with raw SQL because the
/// production writers stamp `now()`.
async fn finished_task(pool: &sqlx::PgPool, body: &str, finished_ago_mins: i32) -> i64 {
    let id = tasks::insert_pending(pool, Lane::Fast, payload()).await.expect("insert");
    sqlx::query(
        "UPDATE tasks SET state = 'completed', \
                finished_at = now() - make_interval(mins => $2), \
                created_at  = now() - make_interval(mins => $2 + 1), \
                result = $3 WHERE id = $1",
    )
    .bind(id)
    .bind(finished_ago_mins)
    .bind(serde_json::json!({"kind": "text", "body": body}))
    .execute(pool)
    .await
    .expect("finish");
    id
}

async fn spawn_bus(
    pool: &sqlx::PgPool,
    sent: &Arc<Mutex<Vec<OutgoingMessage>>>,
    park_sends: bool,
) -> (ChannelBus, mpsc::Sender<IncomingMessage>) {
    let (tx, inbound_rx) = mpsc::channel(1);
    let completed = PgCompletedTasks::connect(pool.clone()).await.expect("LISTEN");
    let bus = ChannelBus::spawn(
        vec![Box::new(RecordingChannel { inbound_rx, sent: sent.clone(), park_sends })],
        Arc::new(StaticPairings::new()),
        None,
        Arc::new(PgChannelEvents::new(pool.clone())),
        Arc::new(completed),
        None,
    );
    (bus, tx)
}

async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missed_replies_are_delivered_once_when_the_buses_start() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rcu-d",
        "rcu-l",
        &format!("kastellan-supervisor-test-pg-rcu-{suffix}"),
    );
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "reply-catch-up-e2e"}),
    )
    .await
    .expect("probe");
    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    // Finished while no bus listened: one three hours ago, one just now.
    let late = finished_task(&pool, "late answer", 180).await;
    let fresh = finished_task(&pool, "fresh answer", 0).await;

    // Two buses serving the same channel: the claim must pick one router.
    let sent = Arc::new(Mutex::new(Vec::new()));
    let (bus_a, _ia) = spawn_bus(&pool, &sent, false).await;
    let (bus_b, _ib) = spawn_bus(&pool, &sent, false).await;

    eventually("two catch-up sends", || sent.lock().unwrap().len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // room for a wrong third
    let bodies: Vec<String> = sent.lock().unwrap().iter().map(|m| m.body.clone()).collect();
    assert_eq!(bodies.len(), 2, "once each, across two buses: {bodies:?}");
    assert!(
        bodies.contains(&"(Delayed reply — you sent this 3 h 1 min ago.)\n\nlate answer".to_string()),
        "{bodies:?}"
    );
    assert!(bodies.contains(&"fresh answer".to_string()), "{bodies:?}");

    for id in [late, fresh] {
        let d: Option<String> =
            sqlx::query_scalar("SELECT reply_disposition FROM tasks WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(d.as_deref(), Some("routed"));
    }

    // The live path still works after the sweep, and still once.
    let live = tasks::insert_pending(&pool, Lane::Fast, payload()).await.expect("insert live");
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().expect("claim");
    assert_eq!(claimed.id, live);
    tasks::finalize(
        &pool,
        live,
        "completed",
        Some(serde_json::json!({"kind": "text", "body": "live answer"})),
        None,
    )
    .await
    .expect("finalize");
    eventually("the live send", || sent.lock().unwrap().len() >= 3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sent.lock().unwrap().len(), 3, "the NOTIFY reached both buses; one sent");

    bus_a.shutdown().await;
    bus_b.shutdown().await;

    let rows = kastellan_db::audit::fetch_since(&pool, 0, 500).await.expect("audit");
    let replied: Vec<_> = rows.iter().filter(|r| r.action == actions::REPLIED).collect();
    assert_eq!(replied.len(), 3, "one channel.replied per task");
    let row_for = |id: i64| {
        replied
            .iter()
            .find(|r| r.payload["task_id"] == id)
            .expect("a channel.replied row for the task")
            .payload
            .clone()
    };
    assert_eq!(row_for(late)["via"], "catch_up");
    assert!(row_for(late)["delayed_secs"].as_i64().unwrap() >= 3 * 3600);
    assert_eq!(row_for(fresh)["via"], "catch_up");
    assert!(row_for(fresh).get("delayed_secs").is_none());
    assert_eq!(row_for(live)["via"], "notify");
    pool.close().await;
}

/// The task ids whose reply is settled as `disposition` (`None`: unsettled).
async fn with_disposition(pool: &sqlx::PgPool, ids: &[i64], disposition: Option<&str>) -> Vec<i64> {
    let mut hits = Vec::new();
    for &id in ids {
        let d: Option<String> = sqlx::query_scalar("SELECT reply_disposition FROM tasks WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("read the claim");
        if d.as_deref() == disposition {
            hits.push(id);
        }
    }
    hits
}

/// #832: a bus stopped with replies still in its channel's queue. Before
/// #832 they were claimed (`routed`) before being queued, so the stop lost
/// them for good. Now they are queued unclaimed: the stop leaves them in the
/// backlog, and the next bus's start sweep delivers them — once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replies_queued_at_a_stop_are_delivered_by_the_next_bus() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rcs-d",
        "rcs-l",
        &format!("kastellan-supervisor-test-pg-rcs-{suffix}"),
    );
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "reply-stop-e2e"}),
    )
    .await
    .expect("probe");
    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let ids = [
        finished_task(&pool, "one", 0).await,
        finished_task(&pool, "two", 0).await,
        finished_task(&pool, "three", 0).await,
    ];

    // Bus A: its start sweep queues all three; the pump claims the first and
    // wedges in the transport, so the other two stay queued. Then it stops.
    let sent_a = Arc::new(Mutex::new(Vec::new()));
    let (bus_a, _ia) = spawn_bus(&pool, &sent_a, true).await;
    eventually("the first reply in the transport", || sent_a.lock().unwrap().len() == 1).await;
    bus_a.shutdown().await;

    let in_flight = with_disposition(&pool, &ids, Some("routed")).await;
    assert_eq!(in_flight.len(), 1, "only the reply mid-send was claimed: {in_flight:?}");
    let queued = with_disposition(&pool, &ids, None).await;
    assert_eq!(queued.len(), 2, "the queued two are still in the backlog: {queued:?}");

    // Bus B delivers exactly those two.
    let sent_b = Arc::new(Mutex::new(Vec::new()));
    let (bus_b, _ib) = spawn_bus(&pool, &sent_b, false).await;
    eventually("the two queued replies", || sent_b.lock().unwrap().len() >= 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // room for a wrong third
    bus_b.shutdown().await;

    let first = sent_a.lock().unwrap()[0].body.clone();
    let mut later: Vec<String> = sent_b.lock().unwrap().iter().map(|m| m.body.clone()).collect();
    later.sort();
    let mut expected: Vec<String> =
        ["one", "two", "three"].iter().map(|b| b.to_string()).filter(|b| *b != first).collect();
    expected.sort();
    assert_eq!(later, expected, "the next bus sends the two the stop left, and only those");
    assert_eq!(with_disposition(&pool, &ids, Some("routed")).await.len(), 3, "all settled now");
    pool.close().await;
}
