//! PG-gated e2e for the reply claim (#825, migration 0027). Skip-as-pass
//! without a supervisor/PG; live on the DGX, and on a Mac that exports
//! `KASTELLAN_PG_BIN_DIR`.

use kastellan_db::tasks::reply_claim::{
    claim_reply, unsettled_channel_replies, ReplyDisposition, REPLIED_STATES,
};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// The last migration before 0027. The backfill can only be observed on rows
/// that existed when 0027 ran, so that test stops here, seeds, then finishes.
const BEFORE_0027: i64 = 26;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn channel_payload() -> serde_json::Value {
    serde_json::json!({"kind": "channel", "instruction": "hi", "channel": "matrix",
                       "peer": "@me:srv", "conversation": "!room:srv"})
}

/// Insert a channel task and force it into `state` with raw SQL (the
/// production writers stamp `now()` and only walk the legal transitions).
async fn seed(pool: &sqlx::PgPool, payload: serde_json::Value, state: &str) -> i64 {
    let id = kastellan_db::tasks::insert_pending(pool, kastellan_db::tasks::Lane::Fast, payload)
        .await
        .expect("insert pending");
    sqlx::query("UPDATE tasks SET state = $2, finished_at = now() WHERE id = $1")
        .bind(id)
        .bind(state)
        .execute(pool)
        .await
        .expect("force state");
    id
}

async fn disposition(pool: &sqlx::PgPool, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT reply_disposition FROM tasks WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read disposition")
}

/// Insert a task straight into `state` at schema 0026, where neither
/// `insert_pending` nor the claim columns can be assumed.
async fn insert_at_0026(pool: &sqlx::PgPool, state: &str, payload: serde_json::Value) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO tasks (state, payload, finished_at) \
         VALUES ($1::text, $2, CASE WHEN $1::text = 'running' THEN NULL ELSE now() END) \
         RETURNING id",
    )
    .bind(state)
    .bind(payload)
    .fetch_one(pool)
    .await
    .expect("seed at 0026")
}

/// The state literals in a partial-index predicate as Postgres renders it —
/// `… (state = ANY (ARRAY['completed'::text, …]))` — sorted.
fn index_states(predicate: &str) -> Vec<String> {
    let start = predicate.find("state = ANY (ARRAY[").expect("a state list") + "state = ANY (ARRAY[".len();
    let list = &predicate[start..start + predicate[start..].find(']').expect("closing bracket")];
    let mut states: Vec<String> = list
        .split(',')
        .map(|item| item.trim().trim_end_matches("::text").trim_matches('\'').to_string())
        .collect();
    states.sort();
    states
}

fn sorted(states: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = states.iter().map(|s| (*s).to_string()).collect();
    v.sort();
    v
}

/// D4: history is settled at deploy, honestly labelled, and nothing else is.
#[test]
fn the_backfill_settles_only_finished_channel_tasks() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rc-bd",
        "rc-bl",
        &format!("kastellan-supervisor-test-pg-rcb-{suffix}"),
    );
    runtime().block_on(async {
        kastellan_db::probe::ensure_database_exists(&cluster.conn_spec).await.expect("db");
        let admin = kastellan_db::pool::connect_admin_pool(&cluster.conn_spec).await.expect("admin");
        kastellan_db::MIGRATOR.run_to(BEFORE_0027, &admin).await.expect("migrate to 0026");

        // One finished channel task per terminal state: pins 0027's backfill
        // list against the const, state by state.
        let mut finished = Vec::new();
        for state in REPLIED_STATES {
            finished.push((state, insert_at_0026(&admin, state, channel_payload()).await));
        }
        let running = insert_at_0026(&admin, "running", channel_payload()).await;
        let not_channel = insert_at_0026(&admin, "completed", serde_json::json!({"kind": "ask"})).await;

        kastellan_db::MIGRATOR.run(&admin).await.expect("migrate 0027+");

        for (state, id) in finished {
            assert_eq!(disposition(&admin, id).await.as_deref(), Some("backfilled"), "{state}");
        }
        assert_eq!(disposition(&admin, running).await, None, "an unfinished task is not history");
        assert_eq!(disposition(&admin, not_channel).await, None, "not a channel task");
        let backlog = unsettled_channel_replies(&admin, 0, 100).await.expect("backlog");
        assert!(backlog.is_empty(), "nothing replays at deploy: {backlog:?}");
        admin.close().await;
    });
}

#[test]
fn the_claim_has_one_winner_and_only_for_finished_tasks() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rc-cd",
        "rc-cl",
        &format!("kastellan-supervisor-test-pg-rcc-{suffix}"),
    );
    runtime().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "reply-claim-e2e"}),
        )
        .await
        .expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool_with_max(&cluster.conn_spec, 8)
            .await
            .expect("runtime pool");

        // Every terminal state is in the backlog, in id order (pins the
        // partial index's predicate against the const).
        let mut by_state = Vec::new();
        for state in REPLIED_STATES {
            by_state.push(seed(&pool, channel_payload(), state).await);
        }
        let pending = kastellan_db::tasks::insert_pending(
            &pool,
            kastellan_db::tasks::Lane::Fast,
            channel_payload(),
        )
        .await
        .expect("pending");
        assert_eq!(unsettled_channel_replies(&pool, 0, 100).await.unwrap(), by_state);

        // The partial index itself holds exactly these states. The query above
        // carries its own WHERE, so it would pass with no index at all; this
        // reads the predicate Postgres stored.
        let predicate: String = sqlx::query_scalar(
            "SELECT pg_get_expr(indpred, indrelid) FROM pg_index \
              WHERE indexrelid = 'tasks_unsettled_channel_replies'::regclass",
        )
        .fetch_one(&pool)
        .await
        .expect("the partial index exists");
        assert_eq!(index_states(&predicate), sorted(&REPLIED_STATES), "{predicate}");

        // Paging: `after_id` is exclusive, `limit` is honoured.
        let page = unsettled_channel_replies(&pool, by_state[1], 2).await.unwrap();
        assert_eq!(page, by_state[2..4].to_vec());

        // A task that has not finished can never be settled.
        assert_eq!(claim_reply(&pool, pending, ReplyDisposition::Routed).await.unwrap(), None);
        assert_eq!(disposition(&pool, pending).await, None);

        // Eight concurrent claimers, one winner.
        let target = by_state[0];
        let claims = futures::future::join_all(
            (0..8).map(|_| claim_reply(&pool, target, ReplyDisposition::Routed)),
        )
        .await;
        let winners = claims.iter().filter(|c| matches!(c, Ok(Some(_)))).count();
        assert_eq!(winners, 1, "{claims:?}");
        let won = claims.into_iter().find_map(|c| c.ok().flatten()).unwrap();
        assert!(won.finished_at.is_some());
        assert_eq!(disposition(&pool, target).await.as_deref(), Some("routed"));

        // The other disposition is stored as itself.
        claim_reply(&pool, by_state[1], ReplyDisposition::Unroutable).await.unwrap().unwrap();
        assert_eq!(disposition(&pool, by_state[1]).await.as_deref(), Some("unroutable"));

        // Settled tasks leave the backlog.
        assert_eq!(
            unsettled_channel_replies(&pool, 0, 100).await.unwrap(),
            by_state[2..].to_vec()
        );
        pool.close().await;
    });
}
