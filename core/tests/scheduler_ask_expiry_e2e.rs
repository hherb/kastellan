//! End-to-end: an unanswered operator ask expires on the PERIODIC sweep and
//! fails its task, without a daemon restart (#564 slice 1b, closes #571).
//!
//! Deliberately a SEPARATE test binary from `scheduler_ask_path_e2e.rs`,
//! not one more `#[tokio::test]` appended there. This test's whole point is
//! to set `KASTELLAN_ASK_DEADLINE_S=1` so its ask expires almost
//! immediately — but `std::env::set_var` is process-global, and
//! `scheduler_ask_path_e2e.rs` runs five tests in parallel inside one
//! process. A leaked one-second deadline would give THEIR asks a one-second
//! deadline too, and the periodic sweep this file exists to exercise would
//! then expire those asks and fail those tasks mid-test — a genuine flake,
//! the same shape this repo already paid for with `KASTELLAN_WORKER_OUT`
//! (see HANDOVER). A separate `tests/*.rs` file is a separate process, so
//! the race cannot occur; that is worth an extra copy of the fixtures.
//!
//! Skips silently with `[SKIP]` on hosts without Postgres or a reachable
//! supervisor; run with `-- --nocapture` to see whether it ran.
//!
//! This test is SLOW BY DESIGN: it waits for a real `ASK_SWEEP_INTERVAL`
//! (60 s) tick rather than a shortened one, because the brief deliberately
//! does not add a test-only knob for the sweep interval — one fewer
//! configuration surface. An instant pass would mean the assertion was
//! satisfied some other way, not by the sweep.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kastellan_core::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use kastellan_core::cassandra::types::{DataClass, Plan, PlannedStep, Severity, Verdict};
use kastellan_core::memory::embedder::NoOpEmbedder;
use kastellan_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use kastellan_core::scheduler::asks::ASK_DEADLINE_ENV;
use kastellan_core::scheduler::audit::{
    action_task_terminal, ACTION_TASK_FINALIZE, FINALIZE_PROVENANCE_ASK_EXPIRY,
};
use kastellan_core::scheduler::inner_loop::{StepDispatcher, StepOutcome, TaskContext};
use kastellan_core::scheduler::{spawn_scheduler, SchedulerHandle};
use kastellan_db::tasks::{self, insert_pending, Lane};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix, PgCluster,
};

/// Copied from `scheduler_ask_path_e2e.rs` (issue #15 will hoist this into
/// a shared fixture). Brings up a PG cluster, runs migrations, returns the
/// pool and the cluster handle. `None` means PG or the supervisor is
/// unavailable (skip).
async fn bring_up_pg(label: &str) -> Option<(sqlx::PgPool, PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = format!("{}-{}", label, unique_suffix());
    let service_name = format!("kastellan-sched-test-pg-askexpiry-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "aed", "ael", &service_name)
    });

    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-ask-expiry"}),
    )
    .await
    .ok()?;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .ok()?;

    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Stubs (copied + trimmed from scheduler_ask_path_e2e.rs — this file needs
// only "always escalate" and "always the same terminal plan", never a
// resolved approval, since nobody answers the ask here).
// ---------------------------------------------------------------------------

/// Escalates every plan it sees. Unlike the sibling file's version this
/// test never lets it fall through to `Verdict::Approve` — `remaining`
/// only ever needs to outlast the one plan this test runs before the ask
/// is raised.
struct EscalatingReview {
    remaining: Mutex<u32>,
}

#[async_trait]
impl ReviewStage for EscalatingReview {
    fn name(&self) -> &str {
        "escalating"
    }

    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        let mut n = self.remaining.lock().unwrap();
        if *n > 0 {
            *n -= 1;
            Verdict::Escalate("needs a human".to_string(), Severity::High)
        } else {
            Verdict::Approve
        }
    }
}

/// Returns `StepOutcome::Ok` for every step without doing anything. This
/// test never reaches step execution (the task suspends on the ask before
/// any step runs); it exists only so the scheduler can be built.
struct NoopDispatcher;

#[async_trait]
impl StepDispatcher for NoopDispatcher {
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
        StepOutcome::Ok(serde_json::json!("done"))
    }
}

/// Returns the same terminal plan on every call — this test needs only
/// enough of a plan for the review stage to escalate it.
struct StubFormulator;

#[async_trait]
impl PlanFormulator for StubFormulator {
    async fn formulate_plan(
        &self,
        _ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        Ok((task_complete_plan("ok"), test_meta()))
    }
}

/// The `FormulationMeta` fixture, copied from `scheduler_ask_path_e2e.rs`.
/// Nothing under test reads it; it only has to be well-formed enough for
/// the `plan.formulate` audit row.
fn test_meta() -> FormulationMeta {
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
        recall_query_sha256:
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        graph_seed_entity_ids: Vec::new(),
        graph_seed_count: 0,
        graph_seed_source: kastellan_core::entity_extraction::SeedSource::None,
        usage: Default::default(),
    }
}

fn task_complete_plan(body: &str) -> Plan {
    Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": body})),
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// Spawn the scheduler with the stub formulator + the given review chain.
fn spawn_test_scheduler(pool: &sqlx::PgPool, review: Arc<ChainReviewStage>) -> SchedulerHandle {
    spawn_scheduler(
        pool.clone(),
        Arc::new(StubFormulator),
        review,
        Arc::new(NoopDispatcher),
        Arc::new(kastellan_core::entity_extraction::NoOpEntityExtractor::new()),
        Arc::new(NoOpEmbedder::new()),
        None,
    )
}

/// Poll `tasks.state` until it equals `want`, or give up after `secs` and
/// return whatever it last saw. A fixed sleep would pass or fail on
/// machine speed rather than on behaviour. Copied from
/// `scheduler_ask_path_e2e.rs`.
async fn await_state(pool: &sqlx::PgPool, task_id: i64, want: &str, secs: u64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let s = tasks::observe_state(pool, task_id).await.expect("state");
        if s == want || std::time::Instant::now() > deadline {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_ask_expires_and_fails_its_task_without_a_restart() {
    let Some((pool, _cluster)) = bring_up_pg("expire").await else {
        return; // [SKIP]
    };
    let task_id = insert_pending(
        &pool,
        Lane::Fast,
        serde_json::json!({"instruction": "x", "max_plans": 3}),
    )
    .await
    .expect("insert");

    // One-second deadline through the documented knob, set before the
    // scheduler starts so the raise inside it picks it up. This is the
    // one env-var-setting test in this binary, so there is no
    // same-process collision to serialise against — see the module doc
    // comment for why that guarantee needs a whole separate file.
    std::env::set_var(ASK_DEADLINE_ENV, "1");
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(EscalatingReview {
        remaining: Mutex::new(5),
    })]));
    let handle = spawn_test_scheduler(&pool, review);

    assert_eq!(await_state(&pool, task_id, "awaiting_operator", 20).await, "awaiting_operator");

    // Nobody answers. The periodic sweep must reach it while this process
    // keeps running — a startup-only sweep would leave it suspended
    // forever. `ASK_SWEEP_INTERVAL` is 60 s, so this genuinely takes
    // roughly a minute; that wait is the point of the test.
    assert_eq!(await_state(&pool, task_id, "failed", 90).await, "failed");
    let t = tasks::get(&pool, task_id).await.unwrap().unwrap();
    assert_eq!(
        t.result.as_ref().and_then(|r| r.get("detail")).and_then(|d| d.as_str()),
        Some(kastellan_db::asks::ASK_TIMEOUT_DETAIL),
    );

    // The sweep moved the TASK too (`awaiting_operator -> failed`), and
    // observation-phase SQL pivots on the audit log — a bare `tasks.state`
    // UPDATE is invisible to it. So the timeout must leave the same two
    // task-lifecycle rows `crash_recovery` writes for the same reason;
    // without them every query grouping on `task.finalize` silently drops
    // the whole timed-out population.
    let lifecycle = audit_payloads(&pool, &action_task_terminal("failed"), task_id).await;
    assert_eq!(lifecycle.len(), 1, "one task.failed row for the expired task: {lifecycle:?}");
    assert_eq!(lifecycle[0].get("lane").and_then(|v| v.as_str()), Some("fast"));

    let finalize = audit_payloads(&pool, ACTION_TASK_FINALIZE, task_id).await;
    assert_eq!(finalize.len(), 1, "one task.finalize row for the expired task: {finalize:?}");
    assert_eq!(finalize[0].get("state").and_then(|v| v.as_str()), Some("failed"));
    // Its own provenance, not a borrowed one. Reusing `crash_recovery`
    // would put a cause into the audit log that did not happen, and would
    // merge timeouts into the crashed population for every query that
    // groups by provenance.
    assert_eq!(
        finalize[0].get("provenance").and_then(|v| v.as_str()),
        Some(FINALIZE_PROVENANCE_ASK_EXPIRY),
    );
    // The task really did run before it escalated, so these are facts, not
    // the never-claimed zeros a producer-cancel row would carry.
    assert!(
        finalize[0].get("started_at").map(|v| v.is_string()).unwrap_or(false),
        "a suspended task WAS claimed, so started_at is a real timestamp: {finalize:?}",
    );
    assert!(
        finalize[0].get("plan_count").and_then(|v| v.as_i64()).unwrap_or(0) >= 1,
        "at least one plan ran before the escalation: {finalize:?}",
    );

    std::env::remove_var(ASK_DEADLINE_ENV);
    handle.shutdown().await;
}

/// Every `audit_log` payload with this `action` naming `task_id`, oldest
/// first.
async fn audit_payloads(
    pool: &sqlx::PgPool,
    action: &str,
    task_id: i64,
) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT payload FROM audit_log \
         WHERE action = $1 AND payload->>'task_id' = $2::text ORDER BY id",
    )
    .bind(action)
    .bind(task_id)
    .fetch_all(pool)
    .await
    .expect("read audit payloads")
}
