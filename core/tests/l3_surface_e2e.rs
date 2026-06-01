//! Live-PG e2e for L3 skill recall surfacing (`load_l3_skills_*`).
//!
//! Verifies the trust gate (only user_approved/pinned surface), fail-safe
//! skip of a malformed-template row, and the row cap — against a real
//! Postgres cluster. Skips-as-pass without a usable PG (`[SKIP]`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::memory::l3_surface::{
    load_l3_skills_default, load_l3_skills_for_prompt, L3_SKILLS_CAP_BYTES,
};
use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};
use hhagent_db::pool::connect_runtime_pool;
use hhagent_db::probe::run as probe_run;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};
use serde_json::json;

/// Insert an L3 row with the given trust + a one-step template naming `tool`.
async fn seed_l3(pool: &sqlx::PgPool, name: &str, trust: &str, tool: &str) -> i64 {
    let metadata = json!({
        "source": "agent_raised",
        "task_id": 1,
        "trust": trust,
        "body_sha256": format!("sha-{name}"),
        "created_at": "2026-06-01T00:00:00Z",
        "template": {
            "name": name,
            "description": format!("desc for {name}"),
            "parameters": [{ "name": "x", "description": "the x" }],
            "steps": [
                { "tool": tool, "method": "do.it", "parameters": { "v": "{{x}}" } }
            ]
        }
    });
    insert_memory_at_layer(pool, &format!("desc for {name}"), &metadata, None, MemoryLayer::Skill)
        .await
        .expect("seed L3 row")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn surfaces_only_approved_and_pinned() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l3s-tg-d", "l3s-tg-l",
        &format!("hhagent-postgres-l3-surface-trust-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        json!({"test": "l3_surface_trust_gate"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    seed_l3(&pool, "untrusted_skill", "untrusted", "shell-exec").await;
    seed_l3(&pool, "approved_skill", "user_approved", "shell-exec").await;
    seed_l3(&pool, "pinned_skill", "pinned", "shell-exec").await;

    let surfaced = load_l3_skills_default(&pool).await.expect("load surfaced");
    let names: Vec<&str> = surfaced.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"approved_skill"), "approved must surface");
    assert!(names.contains(&"pinned_skill"), "pinned must surface");
    assert!(!names.contains(&"untrusted_skill"), "untrusted must never surface");

    drop(pool);
    drop(cluster);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_template_row_is_skipped_not_surfaced() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l3s-mf-d", "l3s-mf-l",
        &format!("hhagent-postgres-l3-surface-malformed-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        json!({"test": "l3_surface_malformed"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    // Approved row whose template has a non-array `parameters` → parse None.
    let bad = json!({
        "trust": "user_approved",
        "template": { "name": "broken", "description": "x", "parameters": "nope", "steps": [] }
    });
    insert_memory_at_layer(&pool, "broken", &bad, None, MemoryLayer::Skill)
        .await.expect("seed malformed row");
    seed_l3(&pool, "good_skill", "user_approved", "shell-exec").await;

    let surfaced = load_l3_skills_default(&pool).await.expect("load surfaced");
    let names: Vec<&str> = surfaced.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"good_skill"), "good skill surfaces");
    assert!(!names.contains(&"broken"), "malformed row must be skipped, not error");

    drop(pool);
    drop(cluster);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_cap_is_honoured() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l3s-rc-d", "l3s-rc-l",
        &format!("hhagent-postgres-l3-surface-rowcap-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        json!({"test": "l3_surface_rowcap"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    for i in 0..5 {
        seed_l3(&pool, &format!("skill_{i}"), "user_approved", "shell-exec").await;
    }
    let surfaced = load_l3_skills_for_prompt(&pool, 3, L3_SKILLS_CAP_BYTES)
        .await.expect("load with row cap 3");
    assert_eq!(surfaced.len(), 3, "row cap of 3 honoured");
    // load_layer returns (created_at DESC, id DESC) and cap_surfaced keeps the
    // first cap_rows, so the newest seeded skill must lead — anchors the
    // newest-first ordering contract, not just the count.
    assert_eq!(surfaced[0].name, "skill_4", "newest-first ordering preserved");

    drop(pool);
    drop(cluster);
}
