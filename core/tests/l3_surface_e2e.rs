//! Live-PG e2e for L3 skill recall surfacing (`load_l3_skills_*`).
//!
//! Verifies the trust gate (only user_approved/pinned surface), fail-safe
//! skip of a malformed-template row, the row cap, that a large pile of
//! untrusted rows surfaces nothing (the SQL trust push-down), and that
//! `PgSystemPromptBuilder::build_with_recalled` emits a `<skills>` block
//! and populates `skill_count` — against a real Postgres cluster.
//! Skips-as-pass without a usable PG (`[SKIP]`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::memory::l3_surface::{
    load_l3_skills_default, load_l3_skills_for_prompt, L3_SKILLS_CAP_BYTES,
};
use hhagent_core::prompt_assembly::{PgSystemPromptBuilder, SystemPromptBuilder};
use hhagent_core::recall_assembly::RecalledContext;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_with_recalled_emits_skills_block_and_counts() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l3s-e2e-d", "l3s-e2e-l",
        &format!("hhagent-postgres-l3-surface-build-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "l3_surface_build"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    seed_l3(&pool, "approved_skill", "user_approved", "shell-exec").await;

    let builder = PgSystemPromptBuilder::new(pool.clone());
    let recalled = RecalledContext::empty();
    let assembled = builder.build_with_recalled("BASE", &recalled).await.expect("assemble");

    assert!(assembled.system_prompt.contains("<skills>"), "skills block present");
    assert!(assembled.system_prompt.contains("approved_skill"), "approved skill rendered");
    assert_eq!(assembled.skill_count, 1, "skill_count counts surfaced skills");

    drop(pool);
    drop(cluster);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_untrusted_pile_surfaces_nothing() {
    // The L3 crystallisation writer appends a `trust:"untrusted"` row on
    // every completed task, so the Skill layer grows with task history.
    // The loader's SQL trust push-down (`load_layer_by_trust`) must keep
    // surfacing bounded to the *approved* rows — a pile of untrusted rows,
    // however large, surfaces nothing and never reaches the planner.
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "l3s-pile-d", "l3s-pile-l",
        &format!("hhagent-postgres-l3-surface-pile-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        json!({"test": "l3_surface_untrusted_pile"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    for i in 0..40 {
        seed_l3(&pool, &format!("untrusted_{i}"), "untrusted", "shell-exec").await;
    }

    let surfaced = load_l3_skills_default(&pool).await.expect("load surfaced");
    assert!(surfaced.is_empty(), "an all-untrusted layer must surface nothing; got {surfaced:?}");

    // And one approved row among the pile still surfaces — exactly one.
    seed_l3(&pool, "the_one_approved", "user_approved", "shell-exec").await;
    let surfaced = load_l3_skills_default(&pool).await.expect("load surfaced");
    assert_eq!(surfaced.len(), 1, "only the single approved row surfaces");
    assert_eq!(surfaced[0].name, "the_one_approved");

    drop(pool);
    drop(cluster);
}
