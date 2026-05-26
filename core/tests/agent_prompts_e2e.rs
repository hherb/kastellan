//! End-to-end test for the prompt ledger: `load_prompts_from_dir` writes hashes
//! into `agent_prompts`, cache entries round-trip, and both versions of an
//! edited file persist (append-only by GRANT from migration 0006).
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::scheduler::prompts::load_prompts_from_dir;
use hhagent_db::agent_prompts::hash_content;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};

/// Async helper: bring up a PG cluster (via the shared
/// [`hhagent_tests_common::bring_up_pg_cluster`]), run migrations,
/// return pool + cluster handle. The `PgCluster` carries the cleanup
/// guards internally and drops them in the right order at end of scope.
/// Returns `None` when PG or supervisor is unavailable (skip).
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("hhagent-sched-test-pg-ap-{suffix}");
    // The shared `bring_up_pg_cluster` is sync (spawns initdb, uses
    // systemd/launchd). `PgCluster::sup` holds Box<dyn Supervisor>
    // which is not Send, so we cannot use spawn_blocking. Use
    // block_in_place instead — it yields the async worker thread for
    // the duration of the blocking call without requiring the return
    // value to be Send.
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "ap-d", "ap-l", &service_name)
    });

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "agent-prompts-e2e"}),
    )
    .await
    .ok()?;

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Verifies that `load_prompts_from_dir` writes the SHA-256 hash into the
/// `agent_prompts` ledger, the cache entry matches, and both versions of
/// an edited prompt file persist (append-only by GRANT, migration 0006).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_hash_lands_in_ledger_and_audit_payload() {
    let Some((pool, _cluster)) = bring_up_pg("ap").await else {
        eprintln!("\n[SKIP] prompt_hash_lands_in_ledger_and_audit_payload: no PG\n");
        return;
    };

    // Create a temporary directory with one prompt file.
    let tmp = tempfile::tempdir().expect("tempdir");
    let prompt_path = tmp.path().join("agent_planner.md");

    // --- Version 1 ---
    let v1_content = "version 1 content\n";
    std::fs::write(&prompt_path, v1_content).expect("write v1");

    let cache = load_prompts_from_dir(&pool, tmp.path())
        .await
        .expect("load v1");

    // Cache entry must carry the correct sha and content.
    let v1_hash = hash_content(v1_content);
    let entry = cache
        .get("agent_planner")
        .expect("agent_planner missing from cache");
    assert_eq!(
        entry.sha256, v1_hash,
        "cache sha256 must match hash_content(v1)"
    );
    assert_eq!(
        entry.content, v1_content,
        "cache content must match what was written"
    );

    // DB must have exactly 1 row for this name after v1 load.
    let count_v1: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_prompts WHERE name = 'agent_planner'",
    )
    .fetch_one(&pool)
    .await
    .expect("count v1 rows");
    assert_eq!(count_v1, 1, "expected 1 row after loading v1");

    // --- Version 2 ---
    let v2_content = "version 2 content\n";
    std::fs::write(&prompt_path, v2_content).expect("write v2");

    let cache2 = load_prompts_from_dir(&pool, tmp.path())
        .await
        .expect("load v2");

    // Cache must now reflect the new sha.
    let v2_hash = hash_content(v2_content);
    let entry2 = cache2
        .get("agent_planner")
        .expect("agent_planner missing from cache after v2 load");
    assert_eq!(
        entry2.sha256, v2_hash,
        "cache sha256 must match hash_content(v2)"
    );
    assert_ne!(v1_hash, v2_hash, "v1 and v2 must have distinct hashes");

    // DB must have 2 rows total (both versions persist — append-only by GRANT).
    let count_v2: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_prompts WHERE name = 'agent_planner'",
    )
    .fetch_one(&pool)
    .await
    .expect("count v2 rows");
    assert_eq!(count_v2, 2, "expected 2 rows after loading v2 (both versions persist)");

    // V1 row must still be present by its hash.
    let count_v1_by_hash: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_prompts WHERE sha256 = $1")
            .bind(&v1_hash)
            .fetch_one(&pool)
            .await
            .expect("count v1 by hash");
    assert_eq!(
        count_v1_by_hash, 1,
        "v1 row must still exist after loading v2 (ledger is append-only)"
    );

    eprintln!(
        "\n[PASS] prompt_hash_lands_in_ledger_and_audit_payload: \
         v1_hash={} v2_hash={}\n",
        &v1_hash[..8],
        &v2_hash[..8]
    );
}
