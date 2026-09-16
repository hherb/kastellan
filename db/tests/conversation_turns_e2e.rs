//! PG-gated e2e for the conversation lookup and `tasks.turn_record`
//! (migration 0026, issue #701). Skip-as-pass without a supervisor/PG
//! (Mac without a cluster, root CI container); live on the DGX, and live on a
//! Mac that exports `KASTELLAN_PG_BIN_DIR`.

use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Seed a finished channel task and return its id.
///
/// Sets the terminal state and a controlled `finished_at` with raw SQL on
/// purpose: the production writer always stamps `now()`, and these cases are
/// about the window boundary.
async fn seed_finished(
    pool: &sqlx::PgPool,
    channel: &str,
    peer: &str,
    conversation: &str,
    instruction: &str,
    state: &str,
    finished_minutes_ago: i64,
) -> i64 {
    let id = kastellan_db::tasks::insert_pending(
        pool,
        kastellan_db::tasks::Lane::Fast,
        serde_json::json!({
            "kind": "channel",
            "instruction": instruction,
            "channel": channel,
            "peer": peer,
            "conversation": conversation,
        }),
    )
    .await
    .expect("insert pending");

    sqlx::query(
        "UPDATE tasks SET state = $2, \
                finished_at = now() - make_interval(mins => $3::int), \
                result = $4 \
          WHERE id = $1",
    )
    .bind(id)
    .bind(state)
    .bind(i32::try_from(finished_minutes_ago).expect("minutes fit in i32"))
    .bind(serde_json::json!({"kind": "text", "body": instruction}))
    .execute(pool)
    .await
    .expect("seed finished task");

    id
}

#[test]
fn finalize_persists_a_turn_record() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "conv-d",
        "conv-l",
        &format!("kastellan-supervisor-test-pg-conv-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "conversation-e2e"}),
        )
        .await
        .expect("probe run");

        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");

        let id = kastellan_db::tasks::insert_pending(
            &pool,
            kastellan_db::tasks::Lane::Fast,
            serde_json::json!({"kind": "channel", "instruction": "hello"}),
        )
        .await
        .expect("insert pending");

        // `finalize` only matches state='running', so claim it first.
        kastellan_db::tasks::claim_one(&pool, kastellan_db::tasks::Lane::Fast, 60)
            .await
            .expect("claim")
            .expect("a pending task");

        let record = serde_json::json!({
            "calls": [{"tool": "mail", "method": "mail.get_message",
                       "parameters": {"message_id": 38036},
                       "returns": "The email for booking FHZ4XR."}],
            "data_class": "Personal",
        });

        kastellan_db::tasks::finalize(
            &pool,
            id,
            "completed",
            Some(serde_json::json!({"kind": "text", "body": "done"})),
            Some(record.clone()),
        )
        .await
        .expect("finalize");

        let stored: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT turn_record FROM tasks WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .expect("select turn_record");

        assert_eq!(stored, Some(record), "finalize must persist the turn record");
    });
}

#[test]
fn the_lookup_takes_the_newest_three_inside_the_window_for_this_peer_only() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "convq-d",
        "convq-l",
        &format!("kastellan-supervisor-test-pg-convq-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "conversation-query"}),
        )
        .await
        .expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");

        let room = "!room:example.org";
        let peer = "@horst:example.org";

        // Four in-window turns for this peer: only the newest three are taken.
        let oldest = seed_finished(&pool, "matrix", peer, room, "turn 1", "completed", 240).await;
        let t2 = seed_finished(&pool, "matrix", peer, room, "turn 2", "completed", 180).await;
        let t3 = seed_finished(&pool, "matrix", peer, room, "turn 3", "failed", 120).await;
        let t4 = seed_finished(&pool, "matrix", peer, room, "turn 4", "refused", 60).await;
        // Outside the 5 h window.
        let stale = seed_finished(&pool, "matrix", peer, room, "stale", "completed", 301).await;
        // Same room, a different peer.
        let other_peer =
            seed_finished(&pool, "matrix", "@eve:example.org", room, "eve", "completed", 30).await;
        // Same peer, a different room.
        let other_room = seed_finished(
            &pool, "matrix", peer, "!other:example.org", "elsewhere", "completed", 30,
        )
        .await;
        // Still pending: not a turn, and it is the one doing the asking.
        let asking = kastellan_db::tasks::insert_pending(
            &pool,
            kastellan_db::tasks::Lane::Fast,
            serde_json::json!({"kind": "channel", "instruction": "in flight",
                               "channel": "matrix", "peer": peer, "conversation": room}),
        )
        .await
        .expect("insert asking task");

        let rows = kastellan_db::tasks::turns::conversation_turns(
            &pool,
            "matrix",
            peer,
            room,
            time::OffsetDateTime::now_utc(),
            asking,
            5,
            3,
        )
        .await
        .expect("conversation_turns");

        let ids: Vec<i64> = rows.iter().map(|r| r.task_id).collect();
        assert_eq!(ids, vec![t4, t3, t2], "newest first, limit 3");
        for absent in [oldest, stale, other_peer, other_room, asking] {
            assert!(!ids.contains(&absent), "task {absent} must not be a turn here");
        }
        assert_eq!(
            rows[0].payload.get("instruction").and_then(|v| v.as_str()),
            Some("turn 4"),
        );
    });
}

#[test]
fn a_crashed_turn_is_returned_and_carries_no_result() {
    // `notify_task_completed` (migration 0005, widened by 0012) fires for
    // 'crashed' too, so the peer did get a reply for it: the lookup's state
    // list must match that trigger's list. A crashed task never reached
    // `finalize`, so its result is NULL and its record is absent.
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "convc-d",
        "convc-l",
        &format!("kastellan-supervisor-test-pg-convc-{suffix}"),
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "conversation-crashed"}),
        )
        .await
        .expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");

        let room = "!c:example.org";
        let peer = "@horst:example.org";
        let crashed = seed_finished(&pool, "matrix", peer, room, "boom", "crashed", 10).await;
        sqlx::query("UPDATE tasks SET result = NULL WHERE id = $1")
            .bind(crashed)
            .execute(&pool)
            .await
            .expect("null the result");

        let rows = kastellan_db::tasks::turns::conversation_turns(
            &pool,
            "matrix",
            peer,
            room,
            time::OffsetDateTime::now_utc(),
            -1,
            5,
            3,
        )
        .await
        .expect("conversation_turns");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_id, crashed);
        assert!(rows[0].result.is_none());
        assert!(rows[0].turn_record.is_none());
    });
}
