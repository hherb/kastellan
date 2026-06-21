//! PG-gated e2e for the channel bus: pins the real DB seams
//! (`PgChannelEvents` enqueue + audit, `PgCompletedTasks` over the
//! `tasks_completed` NOTIFY) against a live cluster. Skip-as-pass when no
//! `KASTELLAN_PG_BIN_DIR` is configured (mirrors `injection_guard_e2e`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;

use tokio::sync::mpsc;

use kastellan_core::channel::auth::StaticPairings;
use kastellan_core::channel::bus::{
    handle_completed, handle_inbound, CompletedTasks, PgChannelEvents, PgCompletedTasks,
};
use kastellan_core::channel::{actions, ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerId};
use kastellan_db::tasks::{self, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "channel-bus-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_inbound_enqueues_and_completion_routes_a_reply() {
    // Skip-as-pass without a `systemd --user`/launchd supervisor (e.g. a root
    // CI container) — `bring_up_pg_cluster` needs one to run the PG service, and
    // `initdb` itself refuses to run as root. Mirrors `postgres_e2e` /
    // `injection_guard_e2e`. Live path runs on the DGX (real PG) + Mac.
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return; // skip-as-pass
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ch-d",
        "ch-l",
        &format!("kastellan-supervisor-test-pg-ch-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // ── Inbound: a paired, clean message must enqueue a `channel` task + audit. ──
    let events = PgChannelEvents::new(pool.clone());
    let authorizer = StaticPairings::from_peers([PeerId("@me:srv".into())]);
    let msg = IncomingMessage {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: "what's on my calendar?".into(),
    };
    handle_inbound(&authorizer, None, &events, &msg).await;

    let pending = tasks::list(&pool, Some(Lane::Fast), Some("pending"), 10)
        .await
        .expect("list pending");
    assert_eq!(pending.len(), 1, "exactly one channel task enqueued");
    let task = &pending[0];
    assert_eq!(task.payload["kind"], "channel");
    assert_eq!(task.payload["instruction"], "what's on my calendar?");
    assert_eq!(task.payload["channel"], "matrix");
    assert_eq!(task.payload["peer"], "@me:srv");
    assert_eq!(task.payload["conversation"], "!room:srv");

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 200).await.expect("audit fetch");
    assert!(
        audits.iter().any(|r| r.actor == "channel" && r.action == actions::RECEIVED),
        "expected a channel.received audit row"
    );

    // ── Outbound: listen, finalize the task, route the completion to a reply. ──
    // LISTEN before finalize so the NOTIFY is not missed.
    let mut completed = PgCompletedTasks::connect(pool.clone())
        .await
        .expect("connect completed-tasks listener");

    // Claim (pending → running) then finalize (running → completed, fires NOTIFY).
    let claimed = tasks::claim_one(&pool, Lane::Fast, 60)
        .await
        .expect("claim")
        .expect("a pending task to claim");
    assert_eq!(claimed.id, task.id);
    tasks::finalize(
        &pool,
        claimed.id,
        "completed",
        Some(serde_json::json!({"kind": "completed", "message": "You have 2 meetings."})),
    )
    .await
    .expect("finalize");

    let id = completed.next_completed().await.expect("a completed-task id");
    assert_eq!(id, task.id);

    let (tx, mut rx) = mpsc::channel::<OutgoingMessage>(4);
    let mut senders = HashMap::new();
    senders.insert(ChannelId("matrix".into()), tx);
    let out = handle_completed(&completed, &events, &senders, id)
        .await
        .expect("routed reply");
    assert_eq!(out.body, "You have 2 meetings.");
    assert_eq!(out.conversation, ConversationId("!room:srv".into()));
    let delivered = rx.recv().await.expect("reply delivered to channel sender");
    assert_eq!(delivered.peer, PeerId("@me:srv".into()));

    let audits = kastellan_db::audit::fetch_since(&pool, 0, 200).await.expect("audit fetch 2");
    assert!(
        audits.iter().any(|r| r.actor == "channel" && r.action == actions::REPLIED),
        "expected a channel.replied audit row"
    );

    // Drop the listener before pool.close() — `PgCompletedTasks` holds a
    // checked-out PoolConnection (sqlx 0.9 `PgListener` only releases it from
    // inside `recv()`), and `pool.close()` blocks until every connection is
    // returned, so a listener still in scope at close-time deadlocks the test.
    drop(completed);
    pool.close().await;
}
