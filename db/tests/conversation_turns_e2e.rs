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
            kastellan_db::tasks::turns::ConversationQuery {
                channel: "matrix",
                peer,
                conversation: room,
                window_anchor: time::OffsetDateTime::now_utc(),
                exclude_task_id: asking,
                window_hours: 5,
                limit: 3,
            },
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
            kastellan_db::tasks::turns::ConversationQuery {
                channel: "matrix",
                peer,
                conversation: room,
                window_anchor: time::OffsetDateTime::now_utc(),
                exclude_task_id: -1,
                window_hours: 5,
                limit: 3,
            },
        )
        .await
        .expect("conversation_turns");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_id, crashed);
        assert!(rows[0].result.is_none());
        assert!(rows[0].turn_record.is_none());
    });
}

/// Bring up a throwaway cluster and hand its admin pool to `body`.
///
/// Every test below needs the same three steps (bin dir, cluster, probe), and
/// repeating them was how the first round of these tests ended up sharing one
/// over-loaded case that reached none of its own predicates.
fn with_pg<F>(tag: &str, body: F)
where
    F: for<'a> FnOnce(
        &'a sqlx::PgPool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
{
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
        &format!("{tag}-d"),
        &format!("{tag}-l"),
        &format!("kastellan-supervisor-test-pg-{tag}-{suffix}"),
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
            serde_json::json!({"version": "test", "purpose": "conversation"}),
        )
        .await
        .expect("probe run");
        let pool = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec)
            .await
            .expect("admin pool");
        body(&pool).await;
    });
}

/// The query under test, with the constants every case below shares.
async fn turns_for(
    pool: &sqlx::PgPool,
    peer: &str,
    room: &str,
    exclude_task_id: i64,
) -> Vec<kastellan_db::tasks::turns::ConversationTurnRow> {
    kastellan_db::tasks::turns::conversation_turns(
        pool,
        kastellan_db::tasks::turns::ConversationQuery {
            channel: "matrix",
            peer,
            conversation: room,
            window_anchor: time::OffsetDateTime::now_utc(),
            exclude_task_id,
            window_hours: 5,
            // Deliberately above MAX_TURNS: these cases are about the
            // predicates, and a limit of 3 masks them — the first round of
            // these tests let four mutants survive that way.
            limit: 10,
        },
    )
    .await
    .expect("conversation_turns")
}

#[test]
fn the_window_reaches_back_exactly_five_hours_from_the_anchor() {
    // Nothing else in this file reaches the window predicate: with a limit of
    // 3 and four newer rows, an out-of-window row is excluded by the limit
    // whether or not the window works. Both mutants that delete or widen the
    // lower bound survived until this test existed.
    with_pg("convw", |pool| {
        Box::pin(async move {
            let room = "!w:example.org";
            let peer = "@horst:example.org";
            let inside = seed_finished(pool, "matrix", peer, room, "inside", "completed", 299).await;
            let outside =
                seed_finished(pool, "matrix", peer, room, "outside", "completed", 301).await;

            let ids: Vec<i64> = turns_for(pool, peer, room, -1).await.iter().map(|r| r.task_id).collect();
            assert!(ids.contains(&inside), "a turn 299 minutes old is inside a 5 h window");
            assert!(!ids.contains(&outside), "a turn 301 minutes old is outside it");
        })
    });
}

#[test]
fn a_turn_that_finished_after_the_asking_task_arrived_is_still_a_turn() {
    // The impatient follow-up: a user who types again while the bot is still
    // working produces a task whose `created_at` precedes the previous turn's
    // `finished_at`. An upper bound at the anchor would show that follow-up an
    // empty conversation — #701 reproducing under its own fix.
    with_pg("convi", |pool| {
        Box::pin(async move {
            let room = "!i:example.org";
            let peer = "@horst:example.org";
            // Finished one minute ago; the anchor is an hour before that.
            let just_finished =
                seed_finished(pool, "matrix", peer, room, "the turn being followed up", "completed", 1)
                    .await;

            let rows = kastellan_db::tasks::turns::conversation_turns(
                pool,
                kastellan_db::tasks::turns::ConversationQuery {
                    channel: "matrix",
                    peer,
                    conversation: room,
                    window_anchor: time::OffsetDateTime::now_utc() - time::Duration::hours(1),
                    exclude_task_id: -1,
                    window_hours: 5,
                    limit: 10,
                },
            )
            .await
            .expect("conversation_turns");

            let ids: Vec<i64> = rows.iter().map(|r| r.task_id).collect();
            assert!(
                ids.contains(&just_finished),
                "a turn that finished after the follow-up arrived is exactly the turn \
                 it is following up on",
            );
        })
    });
}

#[test]
fn the_asking_task_never_reads_itself() {
    // The previous round passed a *pending* task's id, which the state list
    // and `finished_at IS NOT NULL` already excluded — so deleting the
    // exclusion clause entirely left every test green.
    with_pg("convx", |pool| {
        Box::pin(async move {
            let room = "!x:example.org";
            let peer = "@horst:example.org";
            let sibling = seed_finished(pool, "matrix", peer, room, "sibling", "completed", 30).await;
            let itself = seed_finished(pool, "matrix", peer, room, "itself", "completed", 20).await;

            let ids: Vec<i64> = turns_for(pool, peer, room, itself).await.iter().map(|r| r.task_id).collect();
            assert!(!ids.contains(&itself), "a terminal task must not read itself");
            assert!(ids.contains(&sibling), "and excluding it must not exclude its neighbours");
        })
    });
}

#[test]
fn every_replied_to_state_is_a_turn_and_an_unfinished_one_is_not() {
    // The module doc claims this list mirrors `notify_task_completed`. Until
    // this test, four of the seven states were unpinned and deleting the whole
    // state clause left the suite green.
    with_pg("convs", |pool| {
        Box::pin(async move {
            let room = "!s:example.org";
            let peer = "@horst:example.org";
            let mut expected = Vec::new();
            for (i, state) in [
                "completed", "failed", "cancelled", "blocked", "timed_out", "crashed", "refused",
            ]
            .iter()
            .enumerate()
            {
                expected.push(
                    seed_finished(pool, "matrix", peer, room, state, state, 10 + i as i64).await,
                );
            }
            // A task still awaiting an operator, forced to carry a finished_at
            // so that ONLY the state clause can exclude it.
            let suspended =
                seed_finished(pool, "matrix", peer, room, "suspended", "awaiting_operator", 5).await;

            let ids: Vec<i64> = turns_for(pool, peer, room, -1).await.iter().map(|r| r.task_id).collect();
            for id in &expected {
                assert!(ids.contains(id), "every replied-to state is a turn; {id} was missing");
            }
            assert!(
                !ids.contains(&suspended),
                "a task still awaiting an operator was never replied to, so it is not a turn",
            );
        })
    });
}

#[test]
fn a_turn_on_another_channel_is_not_this_conversation() {
    // Same peer, same conversation id, different transport. Nothing pinned
    // this, so deleting the channel filter left the suite green — and email's
    // conversation ids become real the day threading ships.
    with_pg("convch", |pool| {
        Box::pin(async move {
            let room = "!ch:example.org";
            let peer = "@horst:example.org";
            let matrix_turn = seed_finished(pool, "matrix", peer, room, "matrix", "completed", 30).await;
            let email_turn = seed_finished(pool, "email", peer, room, "email", "completed", 20).await;

            let ids: Vec<i64> = turns_for(pool, peer, room, -1).await.iter().map(|r| r.task_id).collect();
            assert!(ids.contains(&matrix_turn));
            assert!(!ids.contains(&email_turn), "another transport is another conversation");
        })
    });
}
