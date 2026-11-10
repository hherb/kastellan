//! Unit tests for [`super`] — the shared per-task types (`Outcome`,
//! `TaskContext`, the floor raise, the turn record) and the method
//! completion `execute` applies to every plan. The audit-payload-shape pins
//! live with their builder in `inner_loop_audit.rs`.

use super::*;
use crate::cassandra::types::{DataClass, Plan};

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
        deadline: None,
        resolved_asks: Vec::new(),
        origin: None,
        conversation: Vec::new(),
        conversation_task_ids: Some(Vec::new()),
    }
}

fn plan_with_decision(decision: &str) -> crate::cassandra::types::Plan {
    crate::cassandra::types::Plan {
        context: "c".into(),
        decision: decision.into(),
        rationale: "r".into(),
        steps: vec![],
        result: None,
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// Build a `PlannedStep` naming a given tool (the #701 turn-record test).
fn step_with_tool(tool: &str) -> crate::cassandra::types::PlannedStep {
    crate::cassandra::types::PlannedStep {
        tool: tool.into(),
        method: "m".into(),
        parameters: serde_json::json!({}),
        returns: "r".into(),
        done_when: "d".into(),
        classification: DataClass::Public,
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
    assert_eq!(Outcome::Completed(serde_json::json!("x")).final_state(), Some("completed"));
    assert_eq!(Outcome::Failed("e".into()).final_state(), Some("failed"));
    assert_eq!(Outcome::Cancelled.final_state(), Some("cancelled"));
    assert_eq!(Outcome::TimedOut.final_state(), Some("timed_out"));
    assert_eq!(
        Outcome::Blocked { principle: 1, reason: "r".into() }.final_state(),
        Some("blocked")
    );
    assert_eq!(
        Outcome::Refused { principle: 1, reason: "harm".into(), body: "explanation".into() }
            .final_state(),
        Some("refused"),
    );
}

#[test]
fn awaiting_operator_is_not_a_terminal_state() {
    // The whole reason `final_state` is an Option: a suspended task has
    // not finished, and `tasks::finalize` matches `WHERE state='running'`
    // so any string here would be a silent no-op UPDATE plus two audit
    // rows asserting an end that did not happen.
    let o = Outcome::AwaitingOperator { ask_id: 7 };
    assert_eq!(o.final_state(), None);
    assert_eq!(o.result_payload(), None);
}

#[test]
fn denied_is_terminal_as_blocked_and_says_so_in_the_payload() {
    let o = Outcome::Denied { ask_id: 7, reason: "sends mail to a stranger".into() };
    // `blocked` because the operator is the review authority of last
    // resort — and no fabricated `principle`, which is what reusing
    // Outcome::Blocked would have required.
    assert_eq!(o.final_state(), Some("blocked"));
    let p = o.result_payload().expect("a denied task has a payload");
    assert_eq!(p.get("kind").and_then(|v| v.as_str()), Some("denied"));
    assert_eq!(p.get("ask_id").and_then(|v| v.as_i64()), Some(7));
    assert_eq!(
        p.get("reason").and_then(|v| v.as_str()),
        Some("sends mail to a stranger")
    );
    assert!(p.get("principle").is_none(), "a denial violates no CASSANDRA principle");
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
        data_ceiling: Some(DataClass::ClinicalConfidential), refused: None,
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
        data_ceiling: Some(DataClass::Public), refused: None,
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
        data_ceiling: Some(DataClass::Personal), refused: None,
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
        data_ceiling: Some(DataClass::Public), refused: None,
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
        failed_llm_calls: 0,
        terminal_l1_insight: None,
        terminal_l3_skill: None,
        terminal_python_skill: None,
        turn_record: None,
    };
    assert!(result.terminal_l1_insight.is_none());
}

// ----- namespace completion is applied to the plan, not just at dispatch -----

/// The plan itself carries the completed method, because more than the
/// dispatch call reads it.
///
/// `summary::ok_summary_cap` keys the per-step output cap on
/// `plan.steps[i].method`; with the completion applied only at the dispatch
/// site, a plan step written as a bare `search_batch` newly *succeeded* (it
/// used to fail `-32601`) while its result head was capped at the flat budget
/// instead of the batch-scaled 24 KiB (then 4 KiB, a 6× cut; 16 KiB since
/// #677, still a third less) — a silent cut in what the planner
/// then reasons from, on a path this feature created. The audit payloads and
/// the plan digest read the same field.
#[tokio::test]
async fn qualify_plan_methods_rewrites_the_step_the_summary_and_audit_will_read() {
    /// Stands in for the registry: completes a bare method for `mail` only.
    struct QualifyingDispatcher;

    #[async_trait::async_trait]
    impl StepDispatcher for QualifyingDispatcher {
        async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
            StepOutcome::Ok(serde_json::json!("ok"))
        }
        fn qualify_method(&self, tool: &str, method: &str) -> Option<String> {
            (tool == "mail" && !method.contains('.')).then(|| format!("mail.{method}"))
        }
    }

    let mut steps = vec![
        PlannedStep {
            tool: "mail".into(),
            method: "get_attachment_text".into(),
            parameters: serde_json::json!({}),
            returns: "text".into(),
            done_when: "text".into(),
            classification: DataClass::Public,
        },
        // Already qualified — must be left exactly as written.
        PlannedStep {
            tool: "mail".into(),
            method: "mail.search".into(),
            parameters: serde_json::json!({}),
            returns: "hits".into(),
            done_when: "hits".into(),
            classification: DataClass::Public,
        },
        // A tool the dispatcher cannot complete for — untouched, and the
        // worker's own METHOD_NOT_FOUND stays the honest answer.
        PlannedStep {
            tool: "web-search".into(),
            method: "search".into(),
            parameters: serde_json::json!({}),
            returns: "hits".into(),
            done_when: "hits".into(),
            classification: DataClass::Public,
        },
    ];

    qualify_plan_methods(&QualifyingDispatcher, 1, &mut steps);

    assert_eq!(steps[0].method, "mail.get_attachment_text", "the live failure");
    assert_eq!(steps[1].method, "mail.search", "already qualified — unchanged");
    assert_eq!(steps[2].method, "search", "not this tool's to complete");

    // Idempotent: the dispatcher re-checks at the chokepoint, so a second pass
    // over an already-normalised plan must change nothing.
    let before: Vec<String> = steps.iter().map(|s| s.method.clone()).collect();
    qualify_plan_methods(&QualifyingDispatcher, 1, &mut steps);
    let after: Vec<String> = steps.iter().map(|s| s.method.clone()).collect();
    assert_eq!(before, after, "completion must be idempotent");
}

// ── #701: the conversational-continuity turn record ────────────

/// A channel-originated `TaskContext` carrying one successful `mail` step.
fn channel_ctx_with_one_call() -> TaskContext {
    let mut ctx = ctx();
    ctx.origin = Some(crate::channel::ask_message::AskDestination {
        channel: crate::channel::ChannelId("matrix".into()),
        peer: crate::channel::PeerId("@horst:example.org".into()),
        conversation: crate::channel::ConversationId("!room:example.org".into()),
    });
    let mut plan = plan_with_decision("act");
    plan.steps = vec![step_with_tool("mail")];
    ctx.plans.push(PlanRecord::new(
        plan,
        vec![StepOutcome::Ok(serde_json::json!({"subject": "hi"}))],
    ));
    ctx
}

#[test]
fn a_channel_task_carries_a_turn_record_and_a_cli_task_does_not() {
    // The record rides `InnerLoopResult` so the lane runner can hand it to
    // `tasks::finalize` without re-reading the task's plans.
    let ctx = channel_ctx_with_one_call();
    let record = turn_record_for(&ctx).expect("a channel task records its turn");
    assert_eq!(
        record.get("calls").and_then(|c| c.as_array()).map(Vec::len),
        Some(1),
    );

    let mut cli = channel_ctx_with_one_call();
    cli.origin = None;
    assert!(
        turn_record_for(&cli).is_none(),
        "a CLI task has no conversation, so a record would be data nothing can look up",
    );
}

#[test]
fn the_turn_record_carries_the_floor_the_task_ended_at() {
    // Inheritance chains across turns only because the record stores the
    // FINAL floor: a turn that inherited `Personal` must pass it on.
    let mut ctx = channel_ctx_with_one_call();
    ctx.classification_floor = DataClass::ClinicalConfidential;
    let record = turn_record_for(&ctx).expect("record");
    assert_eq!(
        record.get("data_class").and_then(|v| v.as_str()),
        Some("ClinicalConfidential"),
    );
}
