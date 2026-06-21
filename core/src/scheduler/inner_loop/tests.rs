//! Unit tests for [`super`] — the per-task inner replanning loop.
//!
//! Lifted verbatim (de-indented one level) from the inline
//! `#[cfg(test)] mod tests` block that used to live at the tail of
//! `inner_loop.rs`, following the established Rust-2018 sibling-module
//! pattern (cf. `inner_loop_audit.rs`, `injection_guard/tests.rs`,
//! `macos_container/tests.rs`). `use super::*` resolves to the parent
//! `inner_loop` module, so every production item these tests exercise
//! (`TaskContext`, `ClassificationFloorSource`, `apply_floor_raise`,
//! the `inner_loop_audit` writer re-exports, …) stays reachable exactly
//! as before. The audit-payload-shape pins live with their builder in
//! `inner_loop_audit.rs`; these pin the state-machine / floor-raise
//! orchestration that remains in `inner_loop.rs`.

use super::*;
use crate::cassandra::types::DataClass;

fn ctx() -> TaskContext {
    TaskContext {
        task_id: 1,
        lane: kastellan_db::tasks::Lane::Fast,
        instruction: "ping".into(),
        classification_floor: DataClass::Public,
        classification_floor_source: ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: 3,
    }
}

#[test]
fn classification_floor_source_as_snake_str_matches_serde_wire_form() {
    // Pin the audit-log contract: `as_snake_str` MUST stay
    // byte-identical to the serde wire form so the rendered token
    // in the `classification_floor_source` payload key can be
    // cross-grepped with operator-visible logs. Mirrors the
    // `data_class_as_pascal_str_matches_serde_wire_form` pin.
    for s in [
        ClassificationFloorSource::Operator,
        ClassificationFloorSource::CliInferred,
        ClassificationFloorSource::AgentRaised,
        ClassificationFloorSource::Default,
    ] {
        let wire = serde_json::to_value(s).unwrap();
        let wire_str = wire.as_str()
            .expect("ClassificationFloorSource serialises as JSON string");
        assert_eq!(
            s.as_snake_str(),
            wire_str,
            "as_snake_str must equal serde wire form for {s:?}",
        );
    }
}

#[test]
fn outcome_final_state_mapping() {
    assert_eq!(Outcome::Completed(serde_json::json!("x")).final_state(), "completed");
    assert_eq!(Outcome::Failed("e".into()).final_state(), "failed");
    assert_eq!(Outcome::Cancelled.final_state(), "cancelled");
    assert_eq!(Outcome::TimedOut.final_state(), "timed_out");
    assert_eq!(Outcome::Blocked { principle: 1, reason: "r".into() }.final_state(), "blocked");
    assert_eq!(
        Outcome::Refused { principle: 1, reason: "harm".into(), body: "explanation".into() }
            .final_state(),
        "refused",
    );
}

#[test]
fn outcome_refused_result_payload_carries_principle_reason_and_body() {
    let o = Outcome::Refused {
        principle: 2,
        reason: "fraud_or_impersonation".into(),
        body: "Signing under your identity would impersonate you.".into(),
    };
    let p = o.result_payload().unwrap();
    assert_eq!(p["kind"], "refused");
    assert_eq!(p["principle"], 2);
    assert_eq!(p["reason"], "fraud_or_impersonation");
    assert_eq!(p["body"], "Signing under your identity would impersonate you.");

    // Exact key set — guards against accidental payload bloat.
    let keys: std::collections::BTreeSet<String> = p.as_object().unwrap()
        .keys().cloned().collect();
    let expected: std::collections::BTreeSet<String> =
        ["kind", "principle", "reason", "body"].iter().map(|s| s.to_string()).collect();
    assert_eq!(keys, expected);
}

#[test]
fn outcome_result_payload_for_failed_includes_detail() {
    let p = Outcome::Failed("oops".into()).result_payload().unwrap();
    assert_eq!(p["kind"], "error");
    assert_eq!(p["detail"], "oops");
}

#[test]
fn step_outcome_is_err_classifier() {
    let ok = StepOutcome::Ok(serde_json::json!("x"));
    let err = StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "no".into() };
    assert!(!ok.is_err());
    assert!(err.is_err());
}

#[test]
fn agent_floor_request_higher_than_producer_elevates_ctx() {
    let mut c = ctx();
    // Start at Public (Default source).
    assert_eq!(c.classification_floor, DataClass::Public);
    assert_eq!(c.classification_floor_source, ClassificationFloorSource::Default);

    let plan = Plan {
        context: "c".into(), decision: "d".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::ClinicalConfidential, refused: None,
        floor_request: Some(DataClass::ClinicalConfidential),
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    };
    let raised = apply_floor_raise(&mut c, &plan);
    assert!(raised);
    assert_eq!(c.classification_floor, DataClass::ClinicalConfidential);
    assert_eq!(c.classification_floor_source, ClassificationFloorSource::AgentRaised);
    assert!(c.classification_floor_signals.is_empty());
}

#[test]
fn agent_floor_request_lower_than_producer_is_ignored() {
    let mut c = ctx();
    c.classification_floor = DataClass::ClinicalConfidential;
    c.classification_floor_source = ClassificationFloorSource::Operator;

    let plan = Plan {
        context: "c".into(), decision: "d".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::Public, refused: None,
        // floor_request below current floor — must NOT lower:
        floor_request: Some(DataClass::Public),
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    };
    let raised = apply_floor_raise(&mut c, &plan);
    assert!(!raised, "lower floor_request must be ignored");
    assert_eq!(c.classification_floor, DataClass::ClinicalConfidential);
    assert_eq!(c.classification_floor_source, ClassificationFloorSource::Operator);
}

#[test]
fn agent_floor_request_equal_to_producer_is_no_op() {
    let mut c = ctx();
    c.classification_floor = DataClass::Personal;
    c.classification_floor_source = ClassificationFloorSource::CliInferred;
    c.classification_floor_signals = vec!["my_email".into()];

    let plan = Plan {
        context: "c".into(), decision: "d".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::Personal, refused: None,
        floor_request: Some(DataClass::Personal),
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    };
    let raised = apply_floor_raise(&mut c, &plan);
    assert!(!raised, "equal-rank floor_request must be a no-op");
    assert_eq!(c.classification_floor, DataClass::Personal);
    assert_eq!(c.classification_floor_source, ClassificationFloorSource::CliInferred);
    assert_eq!(c.classification_floor_signals, vec!["my_email".to_string()]);
}

#[test]
fn agent_floor_request_none_is_no_op() {
    let mut c = ctx();
    c.classification_floor = DataClass::Public;
    c.classification_floor_source = ClassificationFloorSource::CliInferred;
    c.classification_floor_signals = vec!["patient".into()];

    let plan = Plan {
        context: "c".into(), decision: "d".into(), rationale: "r".into(),
        steps: vec![], result: None,
        data_ceiling: DataClass::Public, refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    };
    let raised = apply_floor_raise(&mut c, &plan);
    assert!(!raised);
    // CLI inference state is preserved when there's no raise request.
    assert_eq!(c.classification_floor_source, ClassificationFloorSource::CliInferred);
    assert_eq!(c.classification_floor_signals, vec!["patient".to_string()]);
}

#[test]
fn task_context_plans_so_far_summary_is_compact() {
    let mut c = ctx();
    c.plans.push((
        crate::cassandra::types::Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![],
            result: None,
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
            invoke_skill: None,
            python_skill: None,
        },
        vec![StepOutcome::Ok(serde_json::json!("x")), StepOutcome::Err {
            code: "POLICY_DENIED".into(), detail: "no".into(),
        }],
    ));
    let s = c.plans_so_far_summary();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0]["decision"], "act");
    // An Ok step stays the compact "ok" scalar; an Err step now surfaces
    // its code + detail so the agent can diagnose and replan instead of
    // seeing a bare "err" and flailing.
    assert_eq!(
        s[0]["step_outcomes"],
        serde_json::json!(["ok", "err: POLICY_DENIED: no"])
    );
}

#[test]
fn plans_so_far_summary_truncates_long_error_detail() {
    let mut c = ctx();
    let long_detail = "x".repeat(500);
    c.plans.push((
        crate::cassandra::types::Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![],
            result: None,
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
            invoke_skill: None,
            python_skill: None,
        },
        vec![StepOutcome::Err {
            code: "OPERATION_FAILED".into(),
            detail: long_detail,
        }],
    ));
    let s = c.plans_so_far_summary();
    let surfaced = s[0]["step_outcomes"][0].as_str().unwrap();
    // Prefix is intact; the unbounded detail is clamped so a single
    // chatty worker error can't blow up the always-in-context prompt.
    assert!(surfaced.starts_with("err: OPERATION_FAILED: "));
    let prefix_chars = "err: OPERATION_FAILED: ".chars().count();
    assert!(
        surfaced.chars().count() <= prefix_chars + STEP_ERR_DETAIL_MAX + 1,
        "detail not truncated: {} chars",
        surfaced.chars().count()
    );
    assert!(surfaced.ends_with('…'));
}

// ── Slice E: InnerLoopResult field pin (l1_insight payload-key
//    tests live in `inner_loop_audit::tests`) ───────────────────

#[test]
fn inner_loop_result_terminal_l1_insight_default_is_none() {
    // Structural pin: any newly-built InnerLoopResult should default
    // terminal_l1_insight to None unless explicitly set.
    let result = InnerLoopResult {
        outcome: Outcome::Failed("test".into()),
        plan_count: 0,
        dispatch_count: 0,
        terminal_l1_insight: None,
        terminal_l3_skill: None,
        terminal_python_skill: None,
    };
    assert!(result.terminal_l1_insight.is_none());
}

// ── Python-skill grounding gate: unit harness ──────────────────

/// Scripted formulator for use in inner-loop unit tests that need a
/// PgPool-backed `run_to_terminal`. Mirrors the one in
/// `scheduler_inner_loop_e2e.rs` but lives here so `inner_loop/tests.rs`
/// can import it without crossing the integration-test boundary.
#[cfg(test)]
mod inner_loop_test_stubs {
    use super::*;
    use crate::cassandra::types::{DataClass, Plan, PlannedStep};
    use crate::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
    use std::sync::Mutex;

    pub struct ScriptedFormulator {
        pub script: Mutex<std::collections::VecDeque<Plan>>,
    }

    impl ScriptedFormulator {
        pub fn new(script: Vec<Plan>) -> Self {
            Self { script: Mutex::new(script.into()) }
        }
    }

    #[async_trait::async_trait]
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
                    graph_seed_source: crate::entity_extraction::SeedSource::None,
                },
            ))
        }
    }

    pub struct OkDispatcher;

    #[async_trait::async_trait]
    impl StepDispatcher for OkDispatcher {
        async fn dispatch_step(
            &self,
            _task_id: i64,
            _step: &PlannedStep,
        ) -> StepOutcome {
            StepOutcome::Ok(serde_json::json!("ok"))
        }
    }

    /// A non-terminal plan that dispatches one step (increments dispatch_count).
    pub fn one_step_plan() -> Plan {
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
            invoke_skill: None,
            python_skill: None,
        }
    }

    /// A terminal `task_complete` plan carrying a `python_skill` candidate.
    pub fn complete_plan_with_python_skill(
        body: &str,
        cand: crate::cassandra::types::PythonSkillCandidate,
    ) -> Plan {
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
            python_skill: Some(cand),
        }
    }
}

/// Python-skill grounding gate: a task that dispatches >= 1 step and
/// terminates with `python_skill: Some(cand)` must have
/// `result.terminal_python_skill == Some(cand)`.
///
/// Mirrors the l3_skill grounding-gate test in `memory_l3_crystallise_e2e.rs`
/// but drives `run_to_terminal` directly so the assertion targets the
/// `InnerLoopResult` field itself (not the downstream writer).
///
/// Skips silently when Postgres / the supervisor are unavailable.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_python_skill_captured_under_grounding_gate() {
    use inner_loop_test_stubs::{
        complete_plan_with_python_skill, one_step_plan, OkDispatcher, ScriptedFormulator,
    };

    // Skip if PG / supervisor are not available on this host.
    if kastellan_tests_common::skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = kastellan_tests_common::pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = format!("ilpy-{}", kastellan_tests_common::unique_suffix());
    let service_name = format!("kastellan-sched-test-pg-{suffix}");
    let cluster = tokio::task::block_in_place(|| {
        kastellan_tests_common::bring_up_pg_cluster(&bin_dir, "ip-d", "ip-l", &service_name)
    });
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"purpose": "inner-loop-python-skill-unit"}),
    )
    .await
    .ok();
    let pool = match kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec).await {
        Ok(p) => p,
        Err(_) => return,
    };

    // Insert + claim a task so the inner loop can observe its state.
    let id = kastellan_db::tasks::insert_pending(
        &pool,
        kastellan_db::tasks::Lane::Fast,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let _ = kastellan_db::tasks::claim_one(&pool, kastellan_db::tasks::Lane::Fast, 60)
        .await
        .unwrap()
        .unwrap();

    let cand = crate::cassandra::types::PythonSkillCandidate {
        name: "noop".into(),
        description: "d".into(),
        code: "pass\n".into(),
    };

    // Plan 1: non-terminal, dispatches one step (dispatch_count → 1).
    // Plan 2: terminal, carries `python_skill: Some(cand)`.
    let formulator = std::sync::Arc::new(ScriptedFormulator::new(vec![
        one_step_plan(),
        complete_plan_with_python_skill("done", cand.clone()),
    ]));
    let review = std::sync::Arc::new(crate::cassandra::review::ChainReviewStage::new(vec![
        std::sync::Arc::new(crate::cassandra::review::NoopReviewStage),
    ]));
    let dispatcher = std::sync::Arc::new(OkDispatcher);

    let ctx = TaskContext {
        task_id: id,
        lane: kastellan_db::tasks::Lane::Fast,
        instruction: "ping".into(),
        classification_floor: crate::cassandra::types::DataClass::Public,
        classification_floor_source: ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: 5,
    };

    let result = super::run_to_terminal(&pool, formulator, review, dispatcher, ctx)
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, Outcome::Completed(_)),
        "expected Completed, got {:?}",
        result.outcome
    );
    assert_eq!(
        result.terminal_python_skill.as_ref(),
        Some(&cand),
        "terminal_python_skill must be captured under the grounding gate"
    );
}
