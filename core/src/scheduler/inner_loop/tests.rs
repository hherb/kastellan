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
        lane: hhagent_db::tasks::Lane::Fast,
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
        },
        vec![StepOutcome::Ok(serde_json::json!("x")), StepOutcome::Err {
            code: "POLICY_DENIED".into(), detail: "no".into(),
        }],
    ));
    let s = c.plans_so_far_summary();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0]["decision"], "act");
    assert_eq!(s[0]["step_outcomes"], serde_json::json!(["ok", "err"]));
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
    };
    assert!(result.terminal_l1_insight.is_none());
}
