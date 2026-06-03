//! End-to-end test for the inner loop with a scripted-router stub.
//!
//! Four scenarios:
//!   (a) one-plan happy path: agent emits task_complete, loop returns
//!       Completed.
//!   (b) tool-fail-then-recover: plan 1's first step fails, agent
//!       sees the error in plan 2 and emits task_complete.
//!   (c) plan-iteration-cap exhausted: agent emits 3 non-terminal
//!       plans, loop returns Failed with the cap message.
//!   (d) cancel mid-execution: while plan is executing steps, the
//!       test plants `state='cancelled'`, loop returns Cancelled.
//!
//! Each scenario asserts the final Outcome is the expected variant.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor. `cargo test -- --nocapture` to see them.
//!
//! Issue #15 will eventually hoist the bring-up helpers into a shared
//! fixture; until then we copy and adapt the recipe from
//! `core/tests/audit_dispatch_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hhagent_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use hhagent_core::cassandra::types::{DataClass, Plan, PlannedStep};
use hhagent_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use hhagent_core::scheduler::inner_loop::{
    run_to_terminal, Outcome, StepDispatcher, StepOutcome, TaskContext,
};
use hhagent_db::tasks::{self, insert_pending, Lane};
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
    let service_name = format!("hhagent-sched-test-pg-ilp-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, "ilp-d", "ilp-l", &service_name)
    });

    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "scheduler-inner-loop"}),
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

/// Returns plans from a pre-loaded queue. Out-of-script reads return
/// `AgentError::Decode` to make missing-script bugs loud.
struct ScriptedFormulator {
    script: Mutex<std::collections::VecDeque<Plan>>,
}

impl ScriptedFormulator {
    fn new(script: Vec<Plan>) -> Self {
        Self { script: Mutex::new(script.into()) }
    }
}

#[async_trait]
impl PlanFormulator for ScriptedFormulator {
    async fn formulate_plan(
        &self,
        _ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let plan = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(AgentError::Decode {
                detail: "scripted formulator out of plans".into(),
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
                // SHA-256 of empty string — matches StaticSystemPromptBuilder::empty()
                assembled_prompt_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
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

/// Looks up the step in a table; missing keys return a
/// `POLICY_DENIED`-shaped error so unscripted calls are loud.
struct ScriptedDispatcher {
    table: std::collections::HashMap<(String, String), StepOutcome>,
}

#[async_trait]
impl StepDispatcher for ScriptedDispatcher {
    async fn dispatch_step(&self, step: &PlannedStep) -> StepOutcome {
        self.table
            .get(&(step.tool.clone(), step.method.clone()))
            .cloned()
            .unwrap_or(StepOutcome::Err {
                code: "POLICY_DENIED".into(),
                detail: format!("no script for {}::{}", step.tool, step.method),
            })
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

fn make_ctx(task_id: i64, max_plans: u32) -> TaskContext {
    TaskContext {
        task_id,
        lane: Lane::Fast,
        instruction: "ping".into(),
        classification_floor: DataClass::Public,
        classification_floor_source: hhagent_core::scheduler::inner_loop::ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// (a) Agent emits task_complete on the first plan; loop returns
///     Completed("pong").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn happy_path_one_plan_returns_completed() {
    let Some((pool, _cluster)) = bring_up_pg("ihp").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    let formulator = Arc::new(ScriptedFormulator::new(vec![task_complete_plan("pong")]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    match result.outcome {
        Outcome::Completed(v) => assert_eq!(v["body"], "pong"),
        o => panic!("expected Completed, got {:?}", o),
    }
    // Spec §7 counter pin: one terminal plan, zero dispatch.
    assert_eq!(result.plan_count, 1);
    assert_eq!(result.dispatch_count, 0);

    // Issue #23 spec §3: the `refused` audit-row key is always present.
    // On a non-refusal plan the value is explicit JSON null — distinct
    // from key-absent so JSONB queries can rely on the key existing.
    let rows = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch audit rows");
    let plan_rows: Vec<_> = rows.iter()
        .filter(|r| r.actor == "agent" && r.action == "plan.formulate")
        .collect();
    assert_eq!(plan_rows.len(), 1, "expected exactly 1 agent/plan.formulate row");
    let payload = &plan_rows[0].payload;
    assert_eq!(payload["decision_kind"], "task_complete");
    assert!(
        payload.get("refused").is_some_and(|v| v.is_null()),
        "refused key must be present with JSON null on non-refusal rows; got payload = {payload:#?}"
    );

    // Slice A (2026-05-15): payload carries full Plan + classification_floor.
    let plan_back: Plan =
        serde_json::from_value(payload["plan"].clone())
            .expect("plan payload key must deserialise into a Plan");
    assert_eq!(plan_back.decision, "task_complete",
        "plan round-trip must preserve decision");
    assert_eq!(plan_back.steps.len(), 0,
        "plan round-trip must preserve steps");
    assert_eq!(
        payload["classification_floor"], "Public",
        "classification_floor must serialise as PascalCase string (Public for unset producer floor)"
    );

    // Slice C (prompt assembler, 2026-05-16): mid-tier
    // regression gate for the 3 new audit keys. The cli_ask_e2e
    // happy path also asserts these end-to-end, but that test
    // requires the full sandbox + worker stack; this one runs
    // wherever Postgres is reachable.
    assert!(payload.get("system_prompt_sha256")
        .and_then(|v| v.as_str())
        .map(|s| s.len() == 64)
        .unwrap_or(false),
        "plan.formulate must carry system_prompt_sha256 as a 64-char hex string; got {payload:?}");
    assert!(payload.get("l0_count").and_then(|v| v.as_u64()).is_some(),
        "plan.formulate must carry numeric l0_count; got {payload:?}");
    assert!(payload.get("l1_count").and_then(|v| v.as_u64()).is_some(),
        "plan.formulate must carry numeric l1_count; got {payload:?}");
    assert!(payload.get("recall_count").and_then(|v| v.as_u64()).is_some(),
        "plan.formulate must carry numeric recall_count; got {payload:?}");
    assert!(payload.get("recalled_memory_ids").and_then(|v| v.as_array()).is_some(),
        "plan.formulate must carry array recalled_memory_ids; got {payload:?}");
    let sha = payload.get("recall_query_sha256")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("plan.formulate must carry string recall_query_sha256; got {payload:?}"));
    assert_eq!(sha.len(), 64, "recall_query_sha256 must be 64 hex chars; got {sha}");
    // Cross-key consistency: count must equal the ids array length.
    let n = payload["recall_count"].as_u64().unwrap();
    let ids_len = payload["recalled_memory_ids"].as_array().unwrap().len() as u64;
    assert_eq!(n, ids_len,
        "recall_count must equal recalled_memory_ids.len(); got {n} vs {ids_len}");

    // l1_insight key: ScriptedFormulator produces a Plan without l1_insight,
    // so the payload key MUST be present-and-null (JSONB ? operator finds it).
    assert!(
        payload.get("l1_insight").is_some(),
        "plan.formulate payload must include l1_insight key (got payload: {payload:?})"
    );
    assert!(
        payload.get("l1_insight").unwrap().is_null(),
        "ScriptedFormulator emits no l1_insight; payload should be JSON null"
    );

    // l3_skill key: present as explicit null when the plan didn't emit one.
    assert!(payload.as_object().unwrap().contains_key("l3_skill"),
        "plan.formulate payload must include l3_skill key (got payload: {payload:?})");
    assert_eq!(payload["l3_skill"], serde_json::Value::Null,
        "ScriptedFormulator emits no l3_skill; payload should be JSON null");
}

/// (b) Plan 1 dispatches a step that fails (no entry in dispatcher
///     table); plan 2 emits task_complete; loop returns
///     Completed("recovered").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_fail_then_recover_returns_completed() {
    let Some((pool, _cluster)) = bring_up_pg("itf").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    let formulator = Arc::new(ScriptedFormulator::new(vec![
        one_step_plan("does-not-exist", "x"), // dispatcher returns Err
        task_complete_plan("recovered"),
    ]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    match result.outcome {
        Outcome::Completed(v) => assert_eq!(v["body"], "recovered"),
        o => panic!("expected Completed (after recovery), got {:?}", o),
    }
    // Spec §7 counter pin: 2 plans (failing + recovery), 1 dispatch
    // attempt (the failing step under plan 1; plan 2 is terminal).
    assert_eq!(result.plan_count, 2);
    assert_eq!(result.dispatch_count, 1);
}

/// (c) Formulator returns 3 non-terminal plans; cap is 3. After
///     formulating the 3rd plan and failing its step, the 4th
///     iteration's cap-check fires → Failed("plan_iteration_cap_exceeded …").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_iteration_cap_exhausted_returns_failed() {
    let Some((pool, _cluster)) = bring_up_pg("icap").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    // Three non-terminal plans (each step fails because the dispatcher
    // table is empty). On iter 4 the cap fires.
    let formulator = Arc::new(ScriptedFormulator::new(vec![
        one_step_plan("never", "a"),
        one_step_plan("never", "a"),
        one_step_plan("never", "a"),
    ]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    match result.outcome {
        Outcome::Failed(s) => assert!(
            s.contains("plan_iteration_cap_exceeded"),
            "expected cap message, got: {s}"
        ),
        o => panic!("expected Failed, got {:?}", o),
    }
    // Spec §7 counter pin: cap=3 plans each ran a failing step.
    assert_eq!(result.plan_count, 3);
    assert_eq!(result.dispatch_count, 3);
}

/// (d) The inner loop is running in a spawned task. While iteration 1
///     is mid-step, the test marks the task cancelled in the DB; the
///     loop detects it at the top of the next iteration and returns
///     Cancelled.
///
/// Synchronisation: the test uses a `BarrierDispatcher` that signals
/// when the first step is being processed and waits for an explicit
/// release. This avoids the timing-race a sleep-based test would have:
/// on fast hardware (DGX-class), 150 ms is enough time for the loop to
/// run iter 1 + iter 2 and complete plan 2 before the cancellation
/// lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_mid_execution_returns_cancelled() {
    use tokio::sync::Notify;

    let Some((pool, _cluster)) = bring_up_pg("ican").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    // Plan 1 dispatches a step that pauses on the barrier; while it
    // pauses, the test plants state='cancelled'. Plan 2 must NOT run.
    let formulator = Arc::new(ScriptedFormulator::new(vec![
        one_step_plan("ok-tool", "ok-method"),
        task_complete_plan("never seen"),
    ]));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dispatcher = Arc::new(BarrierDispatcher {
        entered: entered.clone(),
        release: release.clone(),
    });
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));

    let pool2 = pool.clone();
    let h = tokio::spawn(async move {
        run_to_terminal(&pool2, formulator, review, dispatcher, make_ctx(id, 3)).await
    });

    // Wait for the dispatcher to signal that iter 1's step is in flight.
    entered.notified().await;
    // Plant the cancellation while the step is paused on the barrier.
    tasks::mark_cancelled(&pool, id).await.unwrap();
    // Release the step. The for-step `observe_state` poll fires on the
    // next iteration of the step loop (none in this 1-step plan), then
    // the top-of-loop `observe_state` for iter 2 catches the cancellation.
    release.notify_one();

    let result = h.await.unwrap().unwrap();
    assert!(
        matches!(result.outcome, Outcome::Cancelled),
        "expected Cancelled, got: {:?}",
        result.outcome
    );
    // Spec §7 counter pin: plan 1 was formulated and its step ran
    // (paused on the barrier, then completed Ok before the top-of-loop
    // cancellation check fired on iter 2). dispatch_count == 1.
    assert_eq!(result.plan_count, 1);
    assert_eq!(result.dispatch_count, 1);
}

/// Dispatcher that signals on first call, waits for a release, then
/// returns Ok. Used by the cancel-mid-execution test to make the race
/// deterministic.
struct BarrierDispatcher {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl StepDispatcher for BarrierDispatcher {
    async fn dispatch_step(&self, _step: &PlannedStep) -> StepOutcome {
        self.entered.notify_one();
        self.release.notified().await;
        StepOutcome::Ok(serde_json::json!("step-ok"))
    }
}

/// Returns a scripted `ConstitutionalBlock` verdict. Used to pin the
/// precedence rule: reviewer's CB overrides the agent's refusal field.
struct ScriptedConstitutionalBlockStage {
    principle: u8,
    reason: String,
}

#[async_trait]
impl hhagent_core::cassandra::review::ReviewStage for ScriptedConstitutionalBlockStage {
    fn name(&self) -> &str { "scripted-cb" }
    async fn review(
        &self,
        _plan: &hhagent_core::cassandra::types::Plan,
        _ctx: &hhagent_core::cassandra::review::ReviewStageContext<'_>,
    ) -> hhagent_core::cassandra::types::Verdict {
        hhagent_core::cassandra::types::Verdict::ConstitutionalBlock {
            principle: self.principle,
            reason: self.reason.clone(),
        }
    }
}

/// Returns a scripted non-CB `Block` verdict. Used to pin the precedence
/// rule that `Verdict::Block` on a refusal plan does NOT loop the agent
/// back via `continue` — the refusal is already terminal.
struct ScriptedBlockStage {
    reason: String,
}

#[async_trait]
impl hhagent_core::cassandra::review::ReviewStage for ScriptedBlockStage {
    fn name(&self) -> &str { "scripted-block" }
    async fn review(
        &self,
        _plan: &hhagent_core::cassandra::types::Plan,
        _ctx: &hhagent_core::cassandra::review::ReviewStageContext<'_>,
    ) -> hhagent_core::cassandra::types::Verdict {
        hhagent_core::cassandra::types::Verdict::Block(self.reason.clone())
    }
}

/// (e) Agent emits a refusal plan (plan.refused.is_some()); loop returns
///     Outcome::Refused with the correct principle, reason, and body.
///     Reviewer always approves — refusal field takes precedence over
///     a non-CB verdict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusal_plan_terminates_with_state_refused() {
    let Some((pool, _cluster)) = bring_up_pg("iref").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    let plan = Plan {
        context: "refusing".into(),
        decision: "task_complete".into(),
        rationale: "principle 1 violated".into(),
        steps: vec![],
        result: Some(serde_json::json!({
            "kind": "text",
            "body": "I cannot help with that; it would risk physical harm.",
        })),
        data_ceiling: DataClass::Public,
        refused: Some(hhagent_core::cassandra::types::RefusedReason {
            principle: 1,
            reason: "physical_harm".into(),
        }),
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };

    let formulator = Arc::new(ScriptedFormulator::new(vec![plan]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    // Outcome shape
    match &result.outcome {
        Outcome::Refused { principle, reason, body } => {
            assert_eq!(*principle, 1);
            assert_eq!(reason, "physical_harm");
            assert!(body.contains("physical harm"), "expected body to mention 'physical harm', got: {body}");
        }
        other => panic!("expected Outcome::Refused, got {other:?}"),
    }

    // final_state contract
    assert_eq!(result.outcome.final_state(), "refused");

    // result_payload contract — 4-key shape
    let payload = result.outcome.result_payload().expect("Refused carries a payload");
    assert_eq!(payload["kind"], "refused");
    assert_eq!(payload["principle"], 1);
    assert_eq!(payload["reason"], "physical_harm");
    assert!(
        payload["body"].as_str().unwrap().contains("physical harm"),
        "payload body should mention 'physical harm'"
    );

    // Counters
    assert_eq!(result.plan_count, 1, "single refusal plan");
    assert_eq!(result.dispatch_count, 0, "no steps to dispatch on a refusal plan");

    // Audit-row contract for refusals (issue #23 spec §3).
    //
    // Exactly one agent/plan.formulate row, with:
    //   - decision_kind == "refused"
    //   - refused == { principle: 1, reason: "physical_harm" }
    //   - plan_step_count == 0
    let rows = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch audit rows");
    let plan_rows: Vec<_> = rows.iter()
        .filter(|r| r.actor == "agent" && r.action == "plan.formulate")
        .collect();
    assert_eq!(
        plan_rows.len(),
        1,
        "expected exactly 1 agent/plan.formulate row; got rows = {rows:#?}"
    );
    let payload = &plan_rows[0].payload;
    assert_eq!(
        payload["decision_kind"], "refused",
        "decision_kind must be 'refused' when plan.refused.is_some()"
    );
    assert_eq!(payload["refused"]["principle"], 1);
    assert_eq!(payload["refused"]["reason"], "physical_harm");
    assert_eq!(payload["plan_step_count"], 0);

    // Slice A: refusal plan body round-trips including refused field.
    let plan_back: Plan =
        serde_json::from_value(payload["plan"].clone())
            .expect("refusal plan must round-trip");
    assert!(plan_back.refused.is_some(),
        "round-tripped refusal plan must carry refused: Some(..)");
    assert_eq!(plan_back.refused.as_ref().unwrap().principle, 1);
    assert_eq!(plan_back.refused.as_ref().unwrap().reason, "physical_harm");
    assert_eq!(
        payload["classification_floor"], "Public",
        "test fixture's task has no classification_floor in payload; defaults to Public"
    );

    // l1_insight key: ScriptedFormulator produces a Plan without l1_insight,
    // so the payload key MUST be present-and-null (JSONB ? operator finds it).
    assert!(
        payload.get("l1_insight").is_some(),
        "plan.formulate payload must include l1_insight key (got payload: {payload:?})"
    );
    assert!(
        payload.get("l1_insight").unwrap().is_null(),
        "ScriptedFormulator emits no l1_insight; payload should be JSON null"
    );
}

/// (f) Agent emits a refusal plan (principle 1) AND the reviewer
///     independently returns Verdict::ConstitutionalBlock (principle 3).
///     The reviewer's CB must win — outcome is Blocked with principle 3,
///     not Refused with principle 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reviewer_constitutional_block_wins_over_agent_refusal() {
    let Some((pool, _cluster)) = bring_up_pg("icbw").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    // Plan: agent claims principle 1; reviewer independently detects principle 3.
    let plan = Plan {
        context: "refusing-1".into(),
        decision: "task_complete".into(),
        rationale: "agent claims P1 violation".into(),
        steps: vec![],
        result: Some(serde_json::json!({
            "kind": "text",
            "body": "agent prose mentioning P1",
        })),
        data_ceiling: DataClass::Public,
        refused: Some(hhagent_core::cassandra::types::RefusedReason {
            principle: 1,
            reason: "physical_harm_agent_side".into(),
        }),
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };

    let formulator = Arc::new(ScriptedFormulator::new(vec![plan]));
    // Reviewer returns ConstitutionalBlock with a different principle than the agent.
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(ScriptedConstitutionalBlockStage {
        principle: 3,
        reason: "irreversible_action_no_HITL".into(),
    })]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    // Reviewer's CB wins; outcome is Blocked with the reviewer's principle.
    match &result.outcome {
        Outcome::Blocked { principle, reason } => {
            assert_eq!(*principle, 3, "reviewer's principle 3 must win over agent's principle 1");
            assert_eq!(reason, "irreversible_action_no_HITL");
        }
        other => panic!("expected Outcome::Blocked (reviewer wins), got {other:?}"),
    }
    assert_eq!(result.outcome.final_state(), "blocked");
}

/// (g) Agent emits a refusal plan AND the reviewer returns a non-CB
///     `Verdict::Block`. Spec §2 precedence: a non-CB verdict must NOT
///     override the refusal — refusal is terminal, the loop must NOT
///     `continue`, and the final outcome is `Outcome::Refused`. The
///     reviewer's block verdict is still audit-logged for forensic
///     reconstruction, but the agent's self-refusal stands.
///
///     This locks the `if plan.refused.is_none()` guard in the
///     `Verdict::Block` arm. A regression that drops the guard would
///     loop the agent back until the `max_plans` cap and end as
///     `Outcome::Failed("plan cap")` instead of `Outcome::Refused`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verdict_block_on_refusal_plan_does_not_loop() {
    let Some((pool, _cluster)) = bring_up_pg("ibrf").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    let plan = Plan {
        context: "refusing".into(),
        decision: "task_complete".into(),
        rationale: "principle 4 violated".into(),
        steps: vec![],
        result: Some(serde_json::json!({
            "kind": "text",
            "body": "I will not proceed — privacy boundary.",
        })),
        data_ceiling: DataClass::Public,
        refused: Some(hhagent_core::cassandra::types::RefusedReason {
            principle: 4,
            reason: "privacy_violation".into(),
        }),
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };

    // Only one plan is queued. If the loop incorrectly `continue`s on
    // Block-against-refusal, the next formulator call returns an error
    // (queue empty) — that would surface as a failure mode loud enough
    // to distinguish from the intended Refused outcome.
    let formulator = Arc::new(ScriptedFormulator::new(vec![plan]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(ScriptedBlockStage {
        reason: "reviewer flagged; refusal still stands".into(),
    })]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    // Outcome: Refused with the agent's principle/reason; not Blocked,
    // not Failed (which a loop would produce on the empty-queue path).
    match &result.outcome {
        Outcome::Refused { principle, reason, .. } => {
            assert_eq!(*principle, 4);
            assert_eq!(reason, "privacy_violation");
        }
        other => panic!("expected Outcome::Refused, got {other:?}"),
    }
    assert_eq!(result.outcome.final_state(), "refused");

    // Pin no-loop: exactly one plan formulated, zero steps dispatched.
    assert_eq!(result.plan_count, 1, "refusal+Block must not loop the agent");
    assert_eq!(result.dispatch_count, 0);

    // The reviewer's Block verdict is still audit-logged (forensic
    // record), even though it did not override the refusal.
    let rows = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch audit rows");
    let verdict_rows: Vec<_> = rows.iter()
        .filter(|r| r.actor == "cassandra:chain" && r.action == "verdict")
        .collect();
    assert_eq!(verdict_rows.len(), 1, "expected exactly 1 verdict row");
    assert_eq!(verdict_rows[0].payload["verdict_kind"], "block");
}

/// (h) Agent emits a plan with `floor_request: ClinicalConfidential`
///     over a task submitted with floor=Public + a single step
///     classified as Public. The inner loop must elevate ctx BEFORE
///     review, so the real `DeterministicPolicy` Stage 0 reviewer
///     sees the elevated floor and its I2 invariant (step >= floor)
///     fires — the plan is blocked.
///
///     Pins the agent-raise → DP-block chain end-to-end with the
///     PRODUCTION reviewer rule (not a scripted stub). Also pins the
///     audit-row contract: `agent/plan.formulate` carries
///     `classification_floor: "ClinicalConfidential"` and
///     `classification_floor_source: "agent_raised"` (the original
///     CLI/operator source is replaced on raise per spec §5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_floor_raise_chain_blocks_low_classification_step() {
    let Some((pool, _cluster)) = bring_up_pg("iafr").await else {
        return; // [SKIP]
    };

    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({}))
        .await
        .unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    // Plan 1: agent raises floor to Clinical + a step classified Public
    // (which is BELOW the elevated floor → DP I2 fires → Block).
    //
    // data_ceiling is set to Clinical to satisfy I1 (ceiling >= floor)
    // — if data_ceiling were Public, I1 would fire first and we'd be
    // testing I1 not I2. The test specifically targets I2 because that's
    // the invariant most likely to be silently violated by an agent that
    // raises the floor but forgets to upgrade a step's classification.
    let plan1 = Plan {
        context: "raising-floor".into(),
        decision: "act".into(),
        rationale: "this involves clinical work".into(),
        steps: vec![PlannedStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({"argv": ["/bin/echo", "hi"]}),
            returns: "stdout".into(),
            done_when: "echoed".into(),
            classification: DataClass::Public,  // BELOW the elevated floor
        }],
        result: None,
        data_ceiling: DataClass::ClinicalConfidential,
        refused: None,
        floor_request: Some(DataClass::ClinicalConfidential),  // RAISE!
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };
    // The inner loop will loop until the plan cap; queue plan1 enough
    // times to exhaust the cap, then the outcome is Failed("plan cap").
    // We're not asserting the final outcome (Completed/Failed/Blocked)
    // — we're asserting that the FIRST plan's audit row carries the
    // elevated floor and AgentRaised source.
    let formulator = Arc::new(ScriptedFormulator::new(vec![
        plan1.clone(), plan1.clone(), plan1.clone(),
    ]));
    // Use the REAL DeterministicPolicy — the rule under test is its I2
    // invariant against the elevated floor.
    let review = Arc::new(ChainReviewStage::new(vec![
        Arc::new(hhagent_core::cassandra::review::DeterministicPolicy),
    ]));
    let dispatcher = Arc::new(ScriptedDispatcher { table: Default::default() });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 3))
        .await
        .unwrap();

    // The plan looped (DP returned Block each iteration) until the cap.
    // Outcome is Failed because the agent never produced an acceptable
    // plan within budget.
    match &result.outcome {
        Outcome::Failed(msg) => {
            assert!(msg.contains("plan_iteration_cap_exceeded"),
                "expected plan-cap failure; got: {msg}");
        }
        other => panic!("expected Outcome::Failed (plan cap exhausted), got {other:?}"),
    }

    // Audit pin: every plan.formulate row carries the elevated floor
    // and the AgentRaised source.
    let rows = hhagent_db::audit::fetch_since(&pool, 0, 100)
        .await
        .expect("fetch audit rows");
    let plan_rows: Vec<_> = rows.iter()
        .filter(|r| r.actor == "agent" && r.action == "plan.formulate")
        .collect();
    assert_eq!(plan_rows.len(), 3, "expected 3 plan.formulate rows (one per cap iter)");
    for (i, r) in plan_rows.iter().enumerate() {
        assert_eq!(
            r.payload["classification_floor"], "ClinicalConfidential",
            "plan {i}: floor must be elevated to ClinicalConfidential"
        );
        assert_eq!(
            r.payload["classification_floor_source"], "agent_raised",
            "plan {i}: source must be agent_raised after the raise"
        );
        assert!(
            r.payload.get("classification_floor_signals").is_none(),
            "plan {i}: signals must be absent under agent_raised"
        );

        // l1_insight key: ScriptedFormulator produces a Plan without l1_insight,
        // so the payload key MUST be present-and-null (JSONB ? operator finds it).
        assert!(
            r.payload.get("l1_insight").is_some(),
            "plan {i}: plan.formulate payload must include l1_insight key (got payload: {:?})",
            r.payload
        );
        assert!(
            r.payload.get("l1_insight").unwrap().is_null(),
            "plan {i}: ScriptedFormulator emits no l1_insight; payload should be JSON null"
        );
    }

    // Verdict pin: every reviewer call returned a Block verdict from
    // the I2 invariant.
    let verdict_rows: Vec<_> = rows.iter()
        .filter(|r| r.actor == "cassandra:chain" && r.action == "verdict")
        .collect();
    assert_eq!(verdict_rows.len(), 3, "expected 3 verdict rows");
    for (i, r) in verdict_rows.iter().enumerate() {
        assert_eq!(r.payload["verdict_kind"], "block",
            "verdict {i}: must be block (DP I2 fired)");
        // Block-verdict detail is the raw reason string (see
        // `write_audit_verdict` in scheduler::inner_loop_audit:
        // Verdict::Block(r) → json!(r) goes into the "detail" field).
        let detail = r.payload["detail"].as_str()
            .expect("detail must be a string for Block verdict");
        assert!(
            detail.starts_with("data-classification: step_classification_below_floor"),
            "verdict {i}: detail must reference DP I2 reason_tag; got: {detail}"
        );
    }
}
