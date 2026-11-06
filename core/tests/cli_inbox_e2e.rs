//! Subprocess-level pin for `kastellan-cli inbox {list,show,resolve}`
//! (#564 slice 1b).
//!
//! Modelled on `core/tests/cli_tools_allowlist_e2e.rs`: each subtest brings
//! up its own per-test PG cluster, runs the real CLI binary as a
//! subprocess, and asserts on DB row state, audit-row shape, and the CLI's
//! exit code + stdout/stderr contract. The per-test PG bring-up is
//! factored into one `bring_up_pg` helper (same recipe as
//! `core/tests/scheduler_asks_e2e.rs`, the sibling asks-package e2e file)
//! rather than repeated three times inline.
//!
//! What this pins:
//!
//! 1. `inbox list` prints the pending ask's id, its task id, AND its
//!    question text (`body`) — an inbox that hides the question is
//!    unusable. `inbox resolve` on it returns the task to `pending` and
//!    writes exactly one `actor='cli' action='ask.resolved'` audit row.
//! 2. A second `inbox resolve` on an already-resolved ask is a lost race
//!    (first-responder-wins is a DB property) and the CLI must REPORT the
//!    loss — exit non-zero, not print success — while the first answer's
//!    resolution stands untouched.
//! 3. An unoffered choice (e.g. `maybe` against an ask offering only
//!    `approve`/`deny`) is refused as a usage error (exit 2) BEFORE it
//!    reaches the database — the ask stays `pending`.
//!
//! ## Skip semantics
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; run `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use kastellan_db::pool::connect_runtime_pool;
use kastellan_db::probe::run as probe_run;
use kastellan_db::tasks::{self, insert_pending, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, unique_suffix, PgCluster,
};

/// Bring up a per-test PG cluster, apply migrations, and return a runtime
/// pool + the cluster handle. Returns `None` when PG or the supervisor is
/// unavailable — callers `let Some((pool, cluster)) = bring_up_pg("x").await
/// else { return };` to skip silently, the same idiom
/// `scheduler_asks_e2e.rs::bring_up_pg` uses for the same table family.
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ib-d",
        "ib-l",
        &format!("kastellan-postgres-cli-inbox-e2e-{label}-{suffix}"),
    );
    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": format!("cli_inbox_e2e-{label}")}),
    )
    .await
    // Postgres is up: a failure from here on is a defect, not a missing
    // precondition, so it fails rather than skipping with nothing said.
    .expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("runtime pool");
    Some((pool, cluster))
}

/// Insert a task and immediately claim it, landing it in `running` — the
/// only state `asks::raise` accepts (see `db::asks::raise`'s doc).
async fn seed_running_task(pool: &sqlx::PgPool) -> i64 {
    let id = insert_pending(pool, Lane::Fast, serde_json::json!({"instruction": "x", "max_plans": 3}))
        .await
        .expect("insert");
    tasks::claim_one(pool, Lane::Fast, 60)
        .await
        .expect("claim")
        .expect("a task");
    id
}

/// Build the env block the CLI subprocess needs to find PG via UDS —
/// verbatim from `cli_tools_allowlist_e2e.rs::cli_env`. The CLI's
/// `resolve_connect_spec` reads `KASTELLAN_DATA_DIR` and builds the socket
/// path from there.
fn cli_env(data_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut env = vec![("KASTELLAN_DATA_DIR".to_string(), data_dir.display().to_string())];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    if let Some(user) = std::env::var_os("USER") {
        env.push(("USER".to_string(), user.to_string_lossy().into_owned()));
    } else {
        env.push(("USER".to_string(), current_username()));
    }
    env
}

/// Run the built `kastellan-cli` binary with `args`, wired to `cluster`'s
/// PG via `cli_env`.
fn run_cli(cluster: &PgCluster, args: &[&str]) -> std::process::Output {
    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);
    Command::new(&bin)
        .args(args)
        .env_clear()
        .envs(env)
        .output()
        .expect("spawn cli")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_list_shows_a_pending_ask_and_resolve_returns_the_task_to_pending() {
    let Some((pool, cluster)) = bring_up_pg("list-resolve").await else { return };
    let task_id = seed_running_task(&pool).await;
    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "this sends mail to a stranger",
        &serde_json::json!(["approve", "deny"]),
        Some("digest-a"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1), None,
    )
    .await
    .expect("raise");

    let out = run_cli(&cluster, &["inbox", "list"]);
    assert!(out.status.success(), "inbox list must exit 0: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&raised.ask_id.to_string()), "the ask id must be listed: {stdout}");
    assert!(stdout.contains(&task_id.to_string()), "the task id must be listed: {stdout}");
    assert!(
        stdout.contains("this sends mail to a stranger"),
        "the question must be listed — an inbox that does not show the question is unusable: {stdout}"
    );

    let out = run_cli(&cluster, &["inbox", "resolve", &raised.ask_id.to_string(), "approve"]);
    assert!(out.status.success(), "inbox resolve must exit 0: {out:?}");

    assert_eq!(tasks::observe_state(&pool, task_id).await.unwrap(), "pending");
    let ask = kastellan_db::asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
    assert_eq!(ask.state, "resolved");
    assert_eq!(ask.resolution, Some(serde_json::json!({"choice": "approve"})));

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE actor = 'cli' AND action = 'ask.resolved' \
         AND payload->>'ask_id' = $1::text",
    )
    .bind(raised.ask_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1, "one ask.resolved row, actor=cli");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolving_an_already_resolved_ask_exits_non_zero() {
    let Some((pool, cluster)) = bring_up_pg("twice").await else { return };
    let task_id = seed_running_task(&pool).await;
    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "why",
        &serde_json::json!(["approve", "deny"]),
        Some("d"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1), None,
    )
    .await
    .expect("raise");

    let id = raised.ask_id.to_string();
    assert!(run_cli(&cluster, &["inbox", "resolve", &id, "approve"]).status.success());
    // First-responder-wins is already a DB property. What this pins is that
    // the CLI REPORTS the loss rather than printing success over it.
    let second = run_cli(&cluster, &["inbox", "resolve", &id, "deny"]);
    assert!(!second.status.success(), "a lost race must not exit 0: {second:?}");
    let ask = kastellan_db::asks::get(&pool, raised.ask_id).await.unwrap().unwrap();
    assert_eq!(
        ask.resolution,
        Some(serde_json::json!({"choice": "approve"})),
        "the first answer stands",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unoffered_choice_is_refused_before_it_reaches_the_database() {
    let Some((pool, cluster)) = bring_up_pg("bad-choice").await else { return };
    let task_id = seed_running_task(&pool).await;
    let raised = kastellan_db::asks::raise(
        &pool,
        task_id,
        "plan_approval",
        "why",
        &serde_json::json!(["approve", "deny"]),
        Some("d"),
        time::OffsetDateTime::now_utc() + time::Duration::hours(1), None,
    )
    .await
    .expect("raise");

    let out = run_cli(&cluster, &["inbox", "resolve", &raised.ask_id.to_string(), "maybe"]);
    assert_eq!(out.status.code(), Some(2), "a usage error exits 2: {out:?}");
    assert_eq!(
        kastellan_db::asks::get(&pool, raised.ask_id).await.unwrap().unwrap().state,
        "pending",
        "nothing was written",
    );
}
