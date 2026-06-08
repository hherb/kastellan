//! End-to-end test for the two-lane concurrent scheduler.
//!
//! One scenario:
//!   two_lanes_run_concurrently — two pending tasks (one per lane),
//!   spawn the real `scheduler::spawn_scheduler`, expect both
//!   `tasks_completed` NOTIFY rows within 1.7s (proves concurrency).
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see them.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/scheduler_inner_loop_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hhagent_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use hhagent_core::cassandra::types::{DataClass, Plan, PlannedStep};
use hhagent_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use hhagent_core::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use hhagent_core::scheduler::spawn_scheduler;
use hhagent_db::tasks::{insert_pending, Lane};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};
use sqlx::postgres::PgListener;

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
    let service_name = format!("hhagent-sched-test-pg-ln-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "lnd", "lnl", &service_name)
    });

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-lanes"}),
    )
    .await
    .ok()?;

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Scripted stubs
// ---------------------------------------------------------------------------

/// Returns plans from per-task queues. Keyed by `ctx.task_id`.
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
        let queue = map.get_mut(&ctx.task_id).ok_or(AgentError::Decode {
            detail: format!("no script for task_id {}", ctx.task_id),
            raw: "".into(),
        })?;
        let plan = queue.pop_front().ok_or(AgentError::Decode {
            detail: format!("script exhausted for task_id {}", ctx.task_id),
            raw: "".into(),
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
                assembled_prompt_sha256: "test-assembled-sha".into(),
                l0_count: 0,
                l1_count: 0,
                skill_count: 0,
                recalled_memory_ids: Vec::new(),
                recall_count: 0,
                recall_query_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                graph_seed_entity_ids: Vec::new(),
                graph_seed_count: 0,
                graph_seed_source: hhagent_core::entity_extraction::SeedSource::None,
            },
        ))
    }
}

/// A dispatcher that sleeps for a fixed delay before returning
/// `StepOutcome::Ok`. Ignores `step.tool` and `step.method`.
struct SleepyDispatcher {
    delay: Duration,
}

#[async_trait]
impl StepDispatcher for SleepyDispatcher {
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
        tokio::time::sleep(self.delay).await;
        StepOutcome::Ok(serde_json::json!("done"))
    }
}

// ---------------------------------------------------------------------------
// Plan-factory helpers
// ---------------------------------------------------------------------------

fn task_complete_plan(body: &str) -> Plan {
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
        l3_skill: None,
        invoke_skill: None,
    }
}

fn one_step_plan(tool: &str, method: &str) -> Plan {
    Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: tool.into(),
            method: method.into(),
            parameters: serde_json::json!({}),
            returns: "x".into(),
            done_when: "x".into(),
            classification: DataClass::Public,
        }],
        result: None,
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two pending tasks (one per lane) must complete within 1.7 s when
/// running concurrently. Each task has one ~1 s sleeping step, so the
/// sequential time would be ~2 s+. Receiving both `tasks_completed`
/// NOTIFYs within 1.7 s proves the two lane loops ran in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_lanes_run_concurrently() {
    let Some((pool, _cluster)) = bring_up_pg("ln").await else {
        return; // [SKIP]
    };

    // Subscribe BEFORE inserting to avoid the race where both tasks
    // complete before the listener is set up.
    let mut listener = PgListener::connect_with(&pool)
        .await
        .expect("PgListener connect");
    listener
        .listen("tasks_completed")
        .await
        .expect("LISTEN tasks_completed");

    // Insert one task per lane.
    let id_fast = insert_pending(
        &pool,
        Lane::Fast,
        serde_json::json!({"instruction": "fast-task", "max_plans": 3}),
    )
    .await
    .unwrap();

    let id_long = insert_pending(
        &pool,
        Lane::Long,
        serde_json::json!({"instruction": "long-task", "max_plans": 3}),
    )
    .await
    .unwrap();

    // Each task's script: one sleeping step, then task_complete.
    // The SleepyDispatcher will sleep ~1 s per step.
    let formulator = Arc::new(ScriptedFormulator::new_per_task(vec![
        (
            id_fast,
            vec![
                one_step_plan("sleep", "doit"),
                task_complete_plan("fast-done"),
            ],
        ),
        (
            id_long,
            vec![
                one_step_plan("sleep", "doit"),
                task_complete_plan("long-done"),
            ],
        ),
    ]));

    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));

    // ~1 s sleep per step; two tasks in parallel → both done ~1 s.
    let dispatcher = Arc::new(SleepyDispatcher {
        delay: Duration::from_millis(800),
    });

    let entity_extractor: Arc<dyn hhagent_core::entity_extraction::EntityExtractor> =
        Arc::new(hhagent_core::entity_extraction::NoOpEntityExtractor::new());
    let scheduler = spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor,
    );

    // Wait for both `tasks_completed` NOTIFYs.
    // Measure wall-clock time from scheduler start; both must arrive
    // within 1.7 s (concurrent) vs. ≥1.6 s serial (two × 800 ms steps).
    let t0 = Instant::now();

    let mut completed = std::collections::HashSet::new();
    while completed.len() < 2 {
        let n = tokio::time::timeout(Duration::from_secs(10), listener.recv())
            .await
            .expect("two_lanes_run_concurrently: timed out waiting for tasks_completed")
            .unwrap();
        let id: i64 = n.payload().parse().unwrap();
        completed.insert(id);
    }

    let elapsed = t0.elapsed();
    eprintln!("[lanes_e2e] both tasks completed in {elapsed:.2?}");

    // Shutdown the scheduler before dropping guards.
    scheduler.shutdown().await;

    assert!(completed.contains(&id_fast), "fast task ({id_fast}) not in completed set");
    assert!(completed.contains(&id_long), "long task ({id_long}) not in completed set");

    // If serial: ≥1.6 s. If concurrent: ≈0.8 s. Allow 1.7 s headroom.
    assert!(
        elapsed < Duration::from_millis(1700),
        "expected both tasks to complete within 1.7 s (concurrently), but elapsed={elapsed:.2?}",
    );
}
