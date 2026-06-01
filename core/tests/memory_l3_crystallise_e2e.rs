//! End-to-end DB integration coverage for L3 skill crystallisation —
//! [`hhagent_core::memory::l3_crystallise`] (direct path) and the
//! scheduler-driven path via `runner::drain_lane` (which writes the
//! `actor='scheduler' action='l3.crystallised'` audit row).
//!
//! Seven scenarios, each with its own PG cluster so no state leaks:
//!
//!  1. Agent-raised happy path → inserts row + correct audit entry.
//!  2. Dedup → second task skips insert + `action='skipped_duplicate'`.
//!  3. Grounding gate → pure-text task (0 dispatches) emits no row.
//!  4. Invalid skill → validation failure, no row written.
//!  5. Remove → `l3_remove_and_audit` deletes + journals.
//!  6. Remove wrong layer → noop + `deleted=false` audit.
//!  7. List → `list_l3` returns all layer-3 rows with trust metadata.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hhagent_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use hhagent_core::cassandra::types::{DataClass, L3SkillCandidate, L3Param, L3TemplateStep, Plan, PlannedStep};
use hhagent_core::cli_audit::l3_remove_and_audit;
use hhagent_core::entity_extraction::NoOpEntityExtractor;
use hhagent_core::memory::l1_promote::{promote_l1, L1Source};
use hhagent_core::memory::l3_crystallise::{crystallise_l3, list_l3, L3Source};
use hhagent_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use hhagent_core::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use hhagent_core::scheduler::spawn_scheduler;
use hhagent_db::memories::MemoryLayer;
use hhagent_db::tasks::{insert_pending, Lane};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};
use sqlx::postgres::PgListener;

// ---------------------------------------------------------------------------
// Fixture: a valid L3SkillCandidate used across most scenarios
// ---------------------------------------------------------------------------

fn valid_skill() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "summarise_repo_readme".into(),
        description: "Read a repo README and summarise".into(),
        parameters: vec![L3Param {
            name: "repo_path".into(),
            description: "abs path".into(),
        }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
        }],
    }
}

/// A second distinct valid skill used for list_l3 scenario (different name/SHA).
fn valid_skill_2() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "count_repo_lines".into(),
        description: "Count lines of code in a repo".into(),
        parameters: vec![L3Param {
            name: "repo_path".into(),
            description: "abs path to repo root".into(),
        }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["wc", "-l", "{{repo_path}}"] }),
        }],
    }
}

// ---------------------------------------------------------------------------
// Scripted stubs — mirror of scheduler_lanes_e2e.rs
// ---------------------------------------------------------------------------

/// Per-task scripted plan formulator. Keyed by `ctx.task_id`.
/// Out-of-script reads return `AgentError::Decode` to make bugs loud.
struct ScriptedFormulator {
    per_task: Mutex<HashMap<i64, VecDeque<Plan>>>,
}

impl ScriptedFormulator {
    fn new_per_task(scripts: Vec<(i64, Vec<Plan>)>) -> Self {
        Self {
            per_task: Mutex::new(
                scripts
                    .into_iter()
                    .map(|(id, plans)| (id, plans.into()))
                    .collect(),
            ),
        }
    }
}

#[async_trait]
impl PlanFormulator for ScriptedFormulator {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let mut map = self.per_task.lock().unwrap();
        let queue = map.get_mut(&ctx.task_id).ok_or_else(|| AgentError::Decode {
            detail: format!("no script for task_id {}", ctx.task_id),
            raw: String::new(),
        })?;
        let plan = queue.pop_front().ok_or_else(|| AgentError::Decode {
            detail: format!("script exhausted for task_id {}", ctx.task_id),
            raw: String::new(),
        })?;
        Ok((
            plan,
            FormulationMeta {
                prompt_name: "agent_planner".into(),
                prompt_sha256: "test".into(),
                llm_model: "test-model".into(),
                llm_backend: "local".into(),
                latency_ms: 1,
                retry_count: 0,
                // SHA-256 of empty string — stable sentinel
                assembled_prompt_sha256:
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .into(),
                l0_count: 0,
                l1_count: 0,
                skill_count: 0,
                recalled_memory_ids: Vec::new(),
                recall_count: 0,
                recall_query_sha256:
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .into(),
                graph_seed_entity_ids: Vec::new(),
                graph_seed_count: 0,
                graph_seed_source: hhagent_core::entity_extraction::SeedSource::None,
            },
        ))
    }
}

/// Dispatcher that always returns `Ok` for any step.
struct OkDispatcher;

#[async_trait]
impl StepDispatcher for OkDispatcher {
    async fn dispatch_step(&self, _step: &PlannedStep) -> StepOutcome {
        StepOutcome::Ok(serde_json::json!("ok"))
    }
}

// ---------------------------------------------------------------------------
// Plan-factory helpers
// ---------------------------------------------------------------------------

/// A terminal `task_complete` plan carrying an `l3_skill`.
fn complete_plan_with_skill(body: &str, skill: L3SkillCandidate) -> Plan {
    Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": body})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: Some(skill),
    }
}

/// A non-terminal plan with one tool step (so dispatch_count increments).
fn one_step_plan() -> Plan {
    Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: "cat".into(),
            method: "read".into(),
            parameters: serde_json::json!({}),
            returns: "content".into(),
            done_when: "content".into(),
            classification: DataClass::Public,
        }],
        result: None,
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
    }
}

// ---------------------------------------------------------------------------
// PG bring-up helper (async, tokio context required)
// ---------------------------------------------------------------------------

async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("hhagent-l3-test-pg-{suffix}");
    // data_label and log_label must be short and unique per concurrent test.
    // Use the first 4 chars of label so the socket path stays well under
    // the macOS sockaddr_un.sun_path 104-byte cap.
    let data_label = format!("{}-d", &label[..label.len().min(4)]);
    let log_label = format!("{}-l", &label[..label.len().min(4)]);
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, &data_label, &log_label, &service_name)
    });

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"purpose": "l3-crystallise-e2e"}),
    )
    .await
    .ok()?;

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

/// Insert a task, run a scheduler with the given scripted plans, and
/// wait for the `tasks_completed` NOTIFY. Returns the task id.
///
/// `scripts` is a closure that receives the task id and returns the
/// script (plan queue) for that task. The scheduler is shut down before
/// returning.
async fn run_task_through_scheduler(
    pool: &sqlx::PgPool,
    plans: Vec<Plan>,
) -> i64 {
    // Subscribe BEFORE inserting so we don't miss the NOTIFY.
    let mut listener = PgListener::connect_with(pool)
        .await
        .expect("PgListener connect");
    listener
        .listen("tasks_completed")
        .await
        .expect("LISTEN tasks_completed");

    let task_id = insert_pending(
        pool,
        Lane::Fast,
        serde_json::json!({"instruction": "test", "max_plans": 10}),
    )
    .await
    .expect("insert_pending");

    let formulator = Arc::new(ScriptedFormulator::new_per_task(vec![(task_id, plans)]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(OkDispatcher);
    let entity_extractor: Arc<dyn hhagent_core::entity_extraction::EntityExtractor> =
        Arc::new(NoOpEntityExtractor::new());

    let scheduler = spawn_scheduler(pool.clone(), formulator, review, dispatcher, entity_extractor);

    // Wait for completion (10 s timeout so CI doesn't hang forever).
    tokio::time::timeout(Duration::from_secs(10), listener.recv())
        .await
        .expect("timed out waiting for tasks_completed NOTIFY")
        .expect("PgListener recv error");

    scheduler.shutdown().await;

    task_id
}

// ---------------------------------------------------------------------------
// Scenario 1 — agent-raised happy path inserts L3 row and audit row
// ---------------------------------------------------------------------------

/// Drive a task with one non-terminal step + a terminal plan carrying
/// `l3_skill`. Assert: one `layer=3` memory row, correct body/metadata;
/// one `actor='scheduler' action='l3.crystallised'` audit row with
/// `action='inserted'`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_raised_happy_inserts_l3_row_and_audit() {
    let Some((pool, _cluster)) = bring_up_pg("l3a").await else {
        return; // [SKIP]
    };

    let plans = vec![
        one_step_plan(),
        complete_plan_with_skill("done", valid_skill()),
    ];
    run_task_through_scheduler(&pool, plans).await;

    // --- memories table ---
    let rows: Vec<(i64, String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, body, metadata FROM memories WHERE layer = $1",
    )
    .bind(MemoryLayer::Skill.as_db())
    .fetch_all(&pool)
    .await
    .expect("fetch layer-3 memories");

    assert_eq!(rows.len(), 1, "exactly one layer-3 memory row");
    let (_, body, metadata) = &rows[0];
    assert_eq!(body, "Read a repo README and summarise", "body must equal skill description");
    assert_eq!(
        metadata["trust"].as_str(),
        Some("untrusted"),
        "trust must be untrusted"
    );
    assert_eq!(
        metadata["template"]["name"].as_str(),
        Some("summarise_repo_readme"),
        "template.name must match skill name"
    );

    // --- audit_log table ---
    let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch_since");
    let l3_rows: Vec<_> = all_audit
        .iter()
        .filter(|r| r.actor == "scheduler" && r.action == "l3.crystallised")
        .collect();
    assert_eq!(l3_rows.len(), 1, "exactly one l3.crystallised audit row");
    let payload = &l3_rows[0].payload;
    assert_eq!(
        payload["action"].as_str(),
        Some("inserted"),
        "audit payload action must be 'inserted'"
    );
    assert_eq!(
        payload["source"].as_str(),
        Some("agent_raised"),
        "audit payload source must be 'agent_raised'"
    );
    assert_eq!(
        payload["skill_name"].as_str(),
        Some("summarise_repo_readme"),
        "audit payload skill_name must match"
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 2 — dedup: second task skips insert, audit says skipped_duplicate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_raised_dedup_skips_second() {
    let Some((pool, _cluster)) = bring_up_pg("l3b").await else {
        return; // [SKIP]
    };

    // First task — inserts.
    let plans1 = vec![
        one_step_plan(),
        complete_plan_with_skill("done-1", valid_skill()),
    ];
    run_task_through_scheduler(&pool, plans1).await;

    // Second task — same skill template (same SHA), should dedup.
    let plans2 = vec![
        one_step_plan(),
        complete_plan_with_skill("done-2", valid_skill()),
    ];
    run_task_through_scheduler(&pool, plans2).await;

    // Still exactly one layer-3 row.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE layer = $1",
    )
    .bind(MemoryLayer::Skill.as_db())
    .fetch_one(&pool)
    .await
    .expect("count layer-3");
    assert_eq!(count, 1, "dedup: only one row in DB");

    // Two l3.crystallised audit rows: first=inserted, second=skipped_duplicate.
    let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch_since");
    let l3_rows: Vec<_> = all_audit
        .iter()
        .filter(|r| r.actor == "scheduler" && r.action == "l3.crystallised")
        .collect();
    assert_eq!(l3_rows.len(), 2, "two l3.crystallised audit rows");

    let actions: std::collections::HashSet<&str> = l3_rows
        .iter()
        .map(|r| r.payload["action"].as_str().expect("action str"))
        .collect();
    assert!(
        actions.contains("inserted"),
        "first task must have action=inserted; got {actions:?}"
    );
    assert!(
        actions.contains("skipped_duplicate"),
        "second task must have action=skipped_duplicate; got {actions:?}"
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 3 — grounding gate drops pure-text task (0 dispatches)
// ---------------------------------------------------------------------------

/// Drive a task whose FIRST plan is already terminal (no step was ever
/// dispatched, so `dispatch_count == 0`). The grounding gate in the inner
/// loop must suppress the l3_skill. Assert: ZERO layer-3 rows; ZERO
/// `l3.crystallised` audit rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grounding_gate_drops_pure_text_task() {
    let Some((pool, _cluster)) = bring_up_pg("l3c").await else {
        return; // [SKIP]
    };

    // Single terminal plan with l3_skill — but zero tool steps executed.
    let plans = vec![complete_plan_with_skill("pure-text", valid_skill())];
    run_task_through_scheduler(&pool, plans).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE layer = $1",
    )
    .bind(MemoryLayer::Skill.as_db())
    .fetch_one(&pool)
    .await
    .expect("count layer-3");
    assert_eq!(count, 0, "grounding gate must have suppressed the skill row");

    let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch_since");
    let l3_audit_count = all_audit
        .iter()
        .filter(|r| r.actor == "scheduler" && r.action == "l3.crystallised")
        .count();
    assert_eq!(l3_audit_count, 0, "grounding gate must have suppressed the audit row");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 4 — invalid skill: validation failure, no row written
// ---------------------------------------------------------------------------

/// Drive a task with >= 1 dispatched step, but the terminal plan carries
/// an invalid l3_skill (undeclared placeholder). The `write_l3_crystallised_row`
/// helper in `runner.rs` catches the validation error (WARN-only) and
/// writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_skill_writes_nothing() {
    let Some((pool, _cluster)) = bring_up_pg("l3d").await else {
        return; // [SKIP]
    };

    // Skill with an undeclared placeholder: step references {{missing}}
    // but `parameters` only declares `repo_path`.
    let bad_skill = L3SkillCandidate {
        name: "bad_skill".into(),
        description: "A skill with an undeclared placeholder".into(),
        parameters: vec![L3Param {
            name: "repo_path".into(),
            description: "abs path".into(),
        }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            // {{missing}} is not declared in parameters — validation must reject this.
            parameters: serde_json::json!({ "argv": ["cat", "{{missing}}"] }),
        }],
    };

    let plans = vec![
        one_step_plan(),
        complete_plan_with_skill("done-bad", bad_skill),
    ];
    run_task_through_scheduler(&pool, plans).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE layer = $1",
    )
    .bind(MemoryLayer::Skill.as_db())
    .fetch_one(&pool)
    .await
    .expect("count layer-3");
    assert_eq!(count, 0, "invalid skill must write zero layer-3 rows");

    let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch_since");
    let l3_audit_count = all_audit
        .iter()
        .filter(|r| r.actor == "scheduler" && r.action == "l3.crystallised")
        .count();
    assert_eq!(l3_audit_count, 0, "invalid skill must write zero audit rows");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 5 — remove deletes row and journals to audit_log + deleted_memories
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_deletes_and_journals() {
    let Some((pool, _cluster)) = bring_up_pg("l3e").await else {
        return; // [SKIP]
    };

    // Seed a skill directly (no scheduler needed).
    let outcome = crystallise_l3(
        &pool,
        &valid_skill(),
        L3Source::AgentRaised { task_id: 1 },
    )
    .await
    .expect("crystallise_l3");
    let memory_id = outcome.memory_id();

    // Call the CLI remove path.
    let (deleted, _audit_id) = l3_remove_and_audit(&pool, memory_id)
        .await
        .expect("l3_remove_and_audit");
    assert!(deleted, "deleted must be true");

    // No layer-3 rows remain.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE layer = $1",
    )
    .bind(MemoryLayer::Skill.as_db())
    .fetch_one(&pool)
    .await
    .expect("count remaining");
    assert_eq!(count, 0, "layer-3 row must be gone after remove");

    // One row in deleted_memories.
    let deleted_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM deleted_memories WHERE id = $1")
            .bind(memory_id)
            .fetch_one(&pool)
            .await
            .expect("count deleted_memories");
    assert_eq!(deleted_count, 1, "deleted_memories must have the removed row");

    // One l3.removed audit row with deleted=true.
    let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch_since");
    let l3_removed: Vec<_> = all_audit
        .iter()
        .filter(|r| r.actor == "cli" && r.action == "l3.removed")
        .collect();
    assert_eq!(l3_removed.len(), 1, "exactly one l3.removed audit row");
    let payload = &l3_removed[0].payload;
    assert_eq!(
        payload["memory_id"].as_i64().expect("memory_id"),
        memory_id
    );
    assert!(
        payload["deleted"].as_bool().expect("deleted bool"),
        "audit payload deleted must be true"
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 6 — remove wrong layer is a noop, audit records deleted=false
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_wrong_layer_is_noop() {
    let Some((pool, _cluster)) = bring_up_pg("l3f").await else {
        return; // [SKIP]
    };

    // Seed an L1 (Index) row.
    let l1_outcome = promote_l1(
        &pool,
        &NoOpEntityExtractor::new(),
        "some insight",
        L1Source::Operator,
    )
    .await
    .expect("promote_l1");
    let l1_id = l1_outcome.memory_id();

    // Attempt to remove it via the L3 remove path.
    let (deleted, _audit_id) = l3_remove_and_audit(&pool, l1_id)
        .await
        .expect("l3_remove_and_audit on l1 row");
    assert!(!deleted, "must not delete an L1 row via l3_remove");

    // The L1 row must still exist.
    let still_there = hhagent_db::memories::fetch_by_ids(&pool, &[l1_id])
        .await
        .expect("fetch_by_ids");
    assert_eq!(still_there.len(), 1, "L1 row must still exist after noop remove");

    // One l3.removed audit row with deleted=false.
    let all_audit = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch_since");
    let l3_removed: Vec<_> = all_audit
        .iter()
        .filter(|r| r.actor == "cli" && r.action == "l3.removed")
        .collect();
    assert_eq!(l3_removed.len(), 1, "exactly one l3.removed audit row");
    let payload = &l3_removed[0].payload;
    assert_eq!(
        payload["memory_id"].as_i64().expect("memory_id"),
        l1_id,
        "audit payload memory_id mismatch"
    );
    assert!(
        !payload["deleted"].as_bool().expect("deleted bool"),
        "audit payload deleted must be false for wrong-layer row"
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario 7 — list_l3 returns layer-3 rows with trust metadata
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_returns_layer3_with_trust() {
    let Some((pool, _cluster)) = bring_up_pg("l3g").await else {
        return; // [SKIP]
    };

    // Seed two distinct skills (different name → different SHA → two rows).
    crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 10 })
        .await
        .expect("crystallise_l3 first");
    crystallise_l3(&pool, &valid_skill_2(), L3Source::AgentRaised { task_id: 11 })
        .await
        .expect("crystallise_l3 second");

    let rows = list_l3(&pool).await.expect("list_l3");
    assert_eq!(rows.len(), 2, "list_l3 must return 2 rows");

    for row in &rows {
        assert_eq!(
            row.layer,
            MemoryLayer::Skill,
            "each row must be at Skill layer"
        );
        assert_eq!(
            row.metadata["trust"].as_str(),
            Some("untrusted"),
            "each row must have trust=untrusted in metadata"
        );
    }

    pool.close().await;
}
