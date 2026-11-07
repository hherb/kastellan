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

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kastellan_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use kastellan_core::cassandra::types::{DataClass, PlannedStep};
use kastellan_core::memory::embedder::NoOpEmbedder;
use kastellan_core::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use kastellan_core::scheduler::spawn_scheduler;
use kastellan_db::tasks::{insert_pending, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};
use kastellan_tests_common::scripted_notebook::{answer_reply, call_reply, ScriptedNotebook};
use sqlx::postgres::PgListener;

/// Async helper: bring up a PG cluster (via the shared
/// [`kastellan_tests_common::bring_up_pg_cluster`]), run migrations,
/// return pool + cluster handle. The `PgCluster` carries the cleanup
/// guards internally and drops them in the right order at end of scope.
/// Returns `None` when PG or supervisor is unavailable (skip).
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("kastellan-sched-test-pg-ln-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "lnd", "lnl", &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-lanes"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Scripted stubs
// ---------------------------------------------------------------------------

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
// The planner
// ---------------------------------------------------------------------------

/// Each task's script (#993: a notebook planner): one sleeping step, then an
/// answer naming the task's lane.
fn sleep_then_answer(ctx: &TaskContext) -> String {
    if ctx.plans.is_empty() {
        let step = PlannedStep {
            tool: "sleep".into(),
            method: "doit".into(),
            parameters: serde_json::json!({}),
            returns: "x".into(),
            done_when: "x".into(),
            classification: DataClass::Public,
        };
        return call_reply(1, &step);
    }
    answer_reply(match ctx.lane {
        Lane::Fast => "fast-done",
        Lane::Long => "long-done",
    })
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

    // Each task's script: one sleeping step, then an answer.
    // The SleepyDispatcher will sleep ~1 s per step.
    let formulator = Arc::new(ScriptedNotebook::new(sleep_then_answer));

    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));

    // ~1 s sleep per step; two tasks in parallel → both done ~1 s.
    let dispatcher = Arc::new(SleepyDispatcher {
        delay: Duration::from_millis(800),
    });

    let entity_extractor: Arc<dyn kastellan_core::entity_extraction::EntityExtractor> =
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new());
    let embedder: Arc<dyn kastellan_core::memory::Embedder> =
        Arc::new(NoOpEmbedder::new());
    let scheduler = spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        entity_extractor,
        embedder,
        None,
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

    // `tasks_completed` fires on every terminal state, a failed one too. Two
    // tasks that fail at once (a reply the notebook refuses) would beat the
    // 1.7 s bound below without sleeping at all, so first prove each ran its
    // step and answered.
    for (id, body) in [(id_fast, "fast-done"), (id_long, "long-done")] {
        let (state, result): (String, Option<serde_json::Value>) =
            sqlx::query_as("SELECT state, result FROM tasks WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "completed", "task {id}: {result:?}");
        assert_eq!(result.as_ref().and_then(|r| r["body"].as_str()), Some(body), "task {id}");
    }
    assert!(elapsed >= Duration::from_millis(800), "a step slept: elapsed={elapsed:.2?}");

    // If serial: ≥1.6 s. If concurrent: ≈0.8 s. Allow 1.7 s headroom.
    assert!(
        elapsed < Duration::from_millis(1700),
        "expected both tasks to complete within 1.7 s (concurrently), but elapsed={elapsed:.2?}",
    );
}
