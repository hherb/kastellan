//! Unit tests for the CASSANDRA type vocabulary (`DataClass` ordering,
//! `Severity`, `Verdict`, and their serde shapes).
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! (item 9b over-cap test-lift). Production logic lives in the parent
//! `types.rs`; this file is `mod tests;` from there and is only compiled
//! under `#[cfg(test)]`.

use super::*;

#[test]
fn data_class_total_order_is_consistent() {
    assert!(DataClass::Public.rank() < DataClass::Personal.rank());
    assert!(DataClass::Personal.rank() < DataClass::ClinicalConfidential.rank());
    assert!(DataClass::ClinicalConfidential.rank() < DataClass::Secret.rank());
}

#[test]
fn data_class_as_pascal_str_matches_serde_wire_form() {
    // Pin the audit-log contract: `as_pascal_str` MUST stay
    // byte-identical to the serde wire form so the rendered class
    // name in `Verdict::Block` reasons (and any other audit string)
    // can be cross-grepped with task payloads.
    for c in [
        DataClass::Public,
        DataClass::Personal,
        DataClass::ClinicalConfidential,
        DataClass::Secret,
    ] {
        let wire = serde_json::to_value(c).unwrap();
        let wire_str = wire.as_str().expect("DataClass serialises as JSON string");
        assert_eq!(
            c.as_pascal_str(),
            wire_str,
            "as_pascal_str must equal serde wire form for {c:?}",
        );
    }
}

#[test]
fn plan_is_terminal_requires_all_three_conditions() {
    let mut p = Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "r".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };
    assert!(p.is_terminal(), "all three present");

    p.result = None;
    assert!(!p.is_terminal(), "missing result");

    p.result = Some(serde_json::json!("ok"));
    p.steps = vec![PlannedStep {
        tool: "x".into(),
        method: "y".into(),
        parameters: serde_json::json!({}),
        returns: "".into(),
        done_when: "".into(),
        classification: DataClass::Public,
    }];
    assert!(!p.is_terminal(), "non-empty steps");

    p.steps = vec![];
    p.decision = "act".into();
    assert!(!p.is_terminal(), "wrong decision string");
}

#[test]
fn plan_serialises_skipping_none_result() {
    let p = Plan {
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
    };
    let s = serde_json::to_string(&p).unwrap();

    // skip_serializing_if must omit the key entirely when result is None.
    let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert!(
        parsed.get("result").is_none(),
        "expected `result` key absent in JSON, got: {s}"
    );

    // Round-trip is still lossless: Plan { result: None } deserialises
    // back to Plan { result: None } via the #[serde(default)] hint.
    let p2: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(p, p2);
}

#[test]
fn plan_round_trips_refused_field_some() {
    let p = Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "r".into(),
        steps: vec![],
        result: Some(serde_json::json!({
            "kind": "text",
            "body": "Principle 1 would be violated."
        })),
        data_ceiling: DataClass::Public,
        refused: Some(RefusedReason {
            principle: 1,
            reason: "physical_harm".into(),
        }),
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };
    let s = serde_json::to_string(&p).unwrap();
    let p2: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(p, p2, "Plan with refused: Some(...) must round-trip");
}

#[test]
fn plan_omits_refused_key_when_none() {
    let p = Plan {
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
    };
    let s = serde_json::to_string(&p).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert!(
        parsed.get("refused").is_none(),
        "expected `refused` key absent when None; got JSON: {s}"
    );

    // Round-trip remains lossless via #[serde(default)].
    let p2: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(p, p2);
}

#[test]
fn plan_floor_request_round_trips_when_absent() {
    // Absent floor_request must serde-skip via `skip_serializing_if`,
    // matching the existing `refused` / `result` shape.
    let p = Plan {
        context:       "c".into(),
        decision:      "task_complete".into(),
        rationale:     "r".into(),
        steps:         vec![],
        result:        None,
        data_ceiling:  DataClass::Public,
        refused:       None,
        floor_request: None,
        l1_insight:    None,
        l3_skill: None,
        invoke_skill: None,
    };
    let s = serde_json::to_string(&p).unwrap();
    assert!(
        !s.contains("floor_request"),
        "absent floor_request must not serialise (skip_serializing_if); got: {s}",
    );
    let back: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(back.floor_request, None);
}

#[test]
fn plan_floor_request_round_trips_when_set() {
    let p = Plan {
        context:       "c".into(),
        decision:      "task_complete".into(),
        rationale:     "r".into(),
        steps:         vec![],
        result:        None,
        data_ceiling:  DataClass::Public,
        refused:       None,
        floor_request: Some(DataClass::ClinicalConfidential),
        l1_insight:    None,
        l3_skill: None,
        invoke_skill: None,
    };
    let s = serde_json::to_string(&p).unwrap();
    assert!(
        s.contains(r#""floor_request":"ClinicalConfidential""#),
        "set floor_request must serialise as PascalCase string; got: {s}",
    );
    let back: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(back.floor_request, Some(DataClass::ClinicalConfidential));
}

#[test]
fn plan_is_refused_is_independent_of_is_terminal() {
    // The four corners of the (is_refused × is_terminal) matrix.
    let base = Plan {
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
    };

    // Neither
    assert!(!base.is_refused() && !base.is_terminal());

    // Terminal only
    let mut p = base.clone();
    p.decision = "task_complete".into();
    p.result = Some(serde_json::json!({"kind": "text", "body": "done"}));
    assert!(!p.is_refused() && p.is_terminal());

    // Refused only (non-terminal — malformed shape, but the helper is independent)
    let mut p = base.clone();
    p.refused = Some(RefusedReason { principle: 2, reason: "fraud".into() });
    assert!(p.is_refused() && !p.is_terminal());

    // Both (the well-formed refusal case)
    let mut p = base.clone();
    p.decision = "task_complete".into();
    p.result = Some(serde_json::json!({"kind": "text", "body": "I cannot."}));
    p.refused = Some(RefusedReason { principle: 1, reason: "physical_harm".into() });
    assert!(p.is_refused() && p.is_terminal());
}

#[test]
fn verdict_serialises_all_variants() {
    for v in [
        Verdict::Approve,
        Verdict::Advisory("x".into()),
        Verdict::Escalate("y".into(), Severity::Medium),
        Verdict::Block("z".into()),
        Verdict::ConstitutionalBlock { principle: 1, reason: "harm".into() },
    ] {
        let s = serde_json::to_string(&v).unwrap();
        let v2: Verdict = serde_json::from_str(&s).unwrap();
        assert_eq!(v, v2);
    }
}

#[test]
fn completion_insight_returns_some_when_terminal_and_insight_present() {
    let plan = Plan {
        context: "".into(),
        decision: DECISION_TERMINAL.into(),
        rationale: "".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "answer"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: Some("shell-exec /bin/ls works for dirs".into()),
        l3_skill: None,
        invoke_skill: None,
    };
    assert_eq!(plan.completion_insight(), Some("shell-exec /bin/ls works for dirs"));
}

#[test]
fn completion_insight_returns_none_when_not_terminal() {
    let plan = Plan {
        context: "".into(),
        decision: "step_required".into(),  // not DECISION_TERMINAL
        rationale: "".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "x"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: Some("foo".into()),
        l3_skill: None,
        invoke_skill: None,
    };
    assert!(plan.completion_insight().is_none());
}

#[test]
fn completion_insight_returns_none_when_insight_absent() {
    let plan = Plan {
        context: "".into(),
        decision: DECISION_TERMINAL.into(),
        rationale: "".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "x"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };
    assert!(plan.completion_insight().is_none());
}

#[test]
fn plan_l1_insight_serde_round_trip_omits_none() {
    let plan = Plan {
        context: "c".into(),
        decision: DECISION_TERMINAL.into(),
        rationale: "r".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "x"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    };
    let s = serde_json::to_string(&plan).expect("serialize");
    assert!(!s.contains("l1_insight"), "None should be omitted via skip_serializing_if; got: {s}");

    // And the round-trip survives deserialization with the field absent.
    let plan2: Plan = serde_json::from_str(&s).expect("deserialize");
    assert!(plan2.l1_insight.is_none());
}

#[test]
fn plan_l1_insight_serde_round_trip_carries_some_value() {
    let plan = Plan {
        context: "c".into(),
        decision: DECISION_TERMINAL.into(),
        rationale: "r".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "x"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: Some("a useful insight".into()),
        l3_skill: None,
        invoke_skill: None,
    };
    let s = serde_json::to_string(&plan).expect("serialize");
    assert!(s.contains("\"l1_insight\":\"a useful insight\""), "Some value should serialize; got: {s}");

    let plan2: Plan = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(plan2.l1_insight.as_deref(), Some("a useful insight"));
}

#[test]
fn completion_insight_returns_none_when_terminal_decision_but_result_missing() {
    let plan = Plan {
        context: "".into(),
        decision: DECISION_TERMINAL.into(),
        rationale: "".into(),
        steps: vec![],
        result: None,  // missing result -> is_terminal() = false
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: Some("insight".into()),
        l3_skill: None,
        invoke_skill: None,
    };
    assert!(plan.completion_insight().is_none());
}

// ── L3 helpers ────────────────────────────────────────────────────────────

/// Build a minimal terminal `Plan` (decision == DECISION_TERMINAL,
/// empty steps, result present). Used by L3 tests below.
fn make_terminal_plan() -> Plan {
    Plan {
        context: "ctx".into(),
        decision: DECISION_TERMINAL.into(),
        rationale: "r".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "done"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
    }
}

/// Build a non-terminal `Plan` (decision != DECISION_TERMINAL, has steps, no result). Used by L3 tests.
fn make_action_plan() -> Plan {
    Plan {
        context: "ctx".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({}),
            returns: "stdout".into(),
            done_when: "exit 0".into(),
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

// ── L3SkillCandidate / completion_skill() tests ────────────────────────

#[test]
fn completion_skill_some_on_terminal_with_skill() {
    let mut plan = make_terminal_plan();
    plan.l3_skill = Some(L3SkillCandidate {
        name: "summarise_repo_readme".into(),
        description: "Read a repo README and summarise".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
        }],
    });
    let skill = plan.completion_skill().expect("should be Some on terminal");
    assert_eq!(skill.name, "summarise_repo_readme");
    assert_eq!(skill.steps.len(), 1);
}

#[test]
fn completion_skill_none_when_not_terminal() {
    let mut plan = make_action_plan();
    plan.l3_skill = Some(L3SkillCandidate {
        name: "x".into(), description: "y".into(), parameters: vec![], steps: vec![],
    });
    assert!(plan.completion_skill().is_none());
}

#[test]
fn completion_skill_none_when_unset() {
    let plan = make_terminal_plan();
    assert!(plan.completion_skill().is_none());
}

#[test]
fn l3_skill_none_is_omitted_from_wire_form() {
    let plan = make_terminal_plan(); // l3_skill: None
    let v = serde_json::to_value(&plan).expect("serialize");
    assert!(v.get("l3_skill").is_none(), "skip_serializing_if must omit None");
}

#[test]
fn l3_skill_candidate_round_trips_through_serde() {
    let c = L3SkillCandidate {
        name: "n".into(), description: "d".into(),
        parameters: vec![L3Param { name: "p".into(), description: "pd".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(), method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["echo", "{{p}}"] }),
        }],
    };
    let v = serde_json::to_value(&c).expect("ser");
    let back: L3SkillCandidate = serde_json::from_value(v).expect("de");
    assert_eq!(c, back);
}

// ── invoke_skill / validate_invoke() tests ─────────────────────────────

#[test]
fn invoke_directive_deserializes_from_plan_json() {
    let json = r#"{
        "context":"c","decision":"act","rationale":"r","steps":[],
        "data_ceiling":"Public",
        "invoke_skill":{"name":"summarise_repo_readme","args":{"repo_path":"/tmp/x"}}
    }"#;
    let plan: Plan = serde_json::from_str(json).unwrap();
    let dir = plan.validate_invoke().expect("well-formed invoke");
    assert_eq!(dir.name, "summarise_repo_readme");
    assert_eq!(dir.args.get("repo_path").map(String::as_str), Some("/tmp/x"));
}

#[test]
fn validate_invoke_rejects_invoke_with_nonempty_steps() {
    let plan = Plan {
        context: "c".into(), decision: "act".into(), rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: "shell-exec".into(), method: "shell.exec".into(),
            parameters: serde_json::json!({}), returns: String::new(),
            done_when: String::new(), classification: DataClass::Public,
        }],
        result: None, data_ceiling: DataClass::Public, refused: None,
        floor_request: None, l1_insight: None, l3_skill: None,
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default() }),
    };
    assert!(matches!(plan.validate_invoke(), Err(MalformedInvoke::HasSteps)));
}

#[test]
fn validate_invoke_rejects_invoke_on_terminal_plan() {
    let plan = Plan {
        context: "c".into(), decision: "task_complete".into(), rationale: "r".into(),
        steps: vec![], result: Some(serde_json::json!({"body":"x"})),
        data_ceiling: DataClass::Public, refused: None, floor_request: None,
        l1_insight: None, l3_skill: None,
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default() }),
    };
    assert!(matches!(plan.validate_invoke(), Err(MalformedInvoke::Terminal)));
}

#[test]
fn validate_invoke_rejects_invoke_with_l3_skill() {
    let plan = Plan {
        context: "c".into(), decision: "act".into(), rationale: "r".into(),
        steps: vec![], result: None, data_ceiling: DataClass::Public, refused: None,
        floor_request: None, l1_insight: None,
        l3_skill: Some(L3SkillCandidate {
            name: "s".into(), description: "d".into(),
            parameters: vec![], steps: vec![],
        }),
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default() }),
    };
    assert!(matches!(plan.validate_invoke(), Err(MalformedInvoke::HasL3Skill)));
}

#[test]
fn validate_invoke_precedence_has_steps_wins_over_terminal() {
    // A plan that is BOTH terminal and carries steps must report HasSteps
    // (the first check), pinning the documented precedence order.
    let plan = Plan {
        context: "c".into(), decision: "task_complete".into(), rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: "shell-exec".into(), method: "shell.exec".into(),
            parameters: serde_json::json!({}), returns: String::new(),
            done_when: String::new(), classification: DataClass::Public,
        }],
        result: Some(serde_json::json!({"body":"x"})),
        data_ceiling: DataClass::Public, refused: None, floor_request: None,
        l1_insight: None, l3_skill: None,
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default() }),
    };
    assert!(matches!(plan.validate_invoke(), Err(MalformedInvoke::HasSteps)));
}

#[test]
fn plan_without_invoke_skill_round_trips_without_the_key() {
    // skip_serializing_if keeps existing fixtures byte-stable.
    let plan = Plan {
        context: "c".into(), decision: "act".into(), rationale: "r".into(),
        steps: vec![], result: None, data_ceiling: DataClass::Public, refused: None,
        floor_request: None, l1_insight: None, l3_skill: None, invoke_skill: None,
    };
    let s = serde_json::to_string(&plan).unwrap();
    assert!(!s.contains("invoke_skill"), "absent directive must not serialize a key");
}
