//! Data types for plan review.

use serde::{Deserialize, Serialize};

/// Decision sentinel emitted by the planner to signal task completion.
/// The inner loop matches on this exact string in `Plan::is_terminal`;
/// future sites (planner prompt, audit-log schema) reference the same
/// constant.
pub const DECISION_TERMINAL: &str = "task_complete";

/// Audit-row `decision_kind` value for refusals (issue #23). Sibling
/// to `DECISION_TERMINAL`: both are wire-strings exposed in the
/// `agent/plan.formulate` payload, so they live as named constants
/// rather than inline literals to keep a rename grep-able.
pub const DECISION_REFUSED: &str = "refused";

/// Classification of data flowing through a plan step.
///
/// Outbound policy attaches to each level (see
/// `docs/cassandra_design_plan.md` §7).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DataClass {
    Public,
    Personal,
    ClinicalConfidential,
    Secret,
}

impl DataClass {
    /// Total ordering: higher is more sensitive.
    pub fn rank(self) -> u8 {
        match self {
            DataClass::Public => 0,
            DataClass::Personal => 1,
            DataClass::ClinicalConfidential => 2,
            DataClass::Secret => 3,
        }
    }

    /// Canonical PascalCase string, identical to the serde wire form.
    ///
    /// Used by audit-log reason strings (e.g. `DeterministicPolicy`'s
    /// `Verdict::Block` payload) so the rendered class name is part of
    /// a formal contract instead of relying on the de-facto stability of
    /// `Debug`. Renaming any branch here is a contract break — operators
    /// grep audit logs for these exact tokens.
    pub fn as_pascal_str(self) -> &'static str {
        match self {
            DataClass::Public => "Public",
            DataClass::Personal => "Personal",
            DataClass::ClinicalConfidential => "ClinicalConfidential",
            DataClass::Secret => "Secret",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

/// One step within a plan. Each maps 1:1 to a `tool_host::dispatch`
/// invocation when the plan executes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedStep {
    pub tool: String,
    pub method: String,
    pub parameters: serde_json::Value,
    pub returns: String,
    pub done_when: String,
    pub classification: DataClass,
}

/// Structured marker the planner attaches to a plan when self-declaring
/// a constitutional refusal. Present iff the agent refuses to proceed;
/// drives [`Outcome::Refused`] short-circuit in the inner loop and
/// surfaces verbatim in the `agent/plan.formulate` audit-row payload.
///
/// `principle` is the 1..=5 index from `prompts/agent_planner.md`.
/// `reason` is a short structured tag (lowercase snake_case) — the
/// human-readable explanation lives in `Plan.result.body`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefusedReason {
    pub principle: u8,
    pub reason:    String,
}

/// One agent-formulated plan, reviewed as a unit.
///
/// The terminal signal: `decision == "task_complete"` AND
/// `steps.is_empty()` AND `result.is_some()`. The reviewer trivially
/// approves these (no actions to evaluate); the inner loop returns
/// `Outcome::Completed(result)`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub context: String,
    pub decision: String,
    pub rationale: String,
    pub steps: Vec<PlannedStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    pub data_ceiling: DataClass,
    /// Present iff the agent self-declared a constitutional refusal.
    /// Drives `Outcome::Refused` short-circuit; surfaced in the
    /// `agent/plan.formulate` audit-row payload as the structured
    /// operator-visible signal. Absent on every non-refusal plan.
    ///
    /// When this is `Some`, the planner is also expected to emit
    /// `decision == "task_complete"`, `steps == []`, and an explanation
    /// in `result.body`. The inner loop honours the refusal even when
    /// the planner-shape is malformed (e.g. non-empty `steps`); see
    /// [`super::inner_loop::run_to_terminal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<RefusedReason>,
    /// Agent-side request to raise the producer-set classification floor
    /// for the rest of the task. `None` (the default) leaves the floor
    /// unchanged. A `Some(class)` whose rank is ≤ the current floor is
    /// honoured as a no-op (never lowers; pinned by
    /// `agent_floor_request_lower_than_producer_is_ignored` in
    /// `scheduler::inner_loop::tests`).
    ///
    /// Round-trips through serde with `skip_serializing_if = Option::is_none`
    /// so existing fixtures stay byte-stable when the agent doesn't
    /// emit a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_request: Option<DataClass>,
}

// Invariant (enforced by future Stage 0 — not by stubs in this work's scope):
//   plan.data_ceiling >= task.classification_floor
// i.e. outputs cannot be classified *below* the producer-pinned floor without
// passage through an anonymiser/declassifier (anonymiser is out of scope here).
// The floor is a producer-set minimum on outputs; the ceiling is the inferred
// maximum classification of any input the plan touches.

impl Plan {
    pub fn is_terminal(&self) -> bool {
        self.decision == DECISION_TERMINAL
            && self.steps.is_empty()
            && self.result.is_some()
    }

    /// Returns true iff the agent self-declared a constitutional
    /// refusal on this plan. Independent of `is_terminal` — the two
    /// helpers don't conflate; a well-formed refusal is both, but a
    /// malformed refusal-with-steps is `is_refused()` only and is
    /// still honoured by the inner loop.
    pub fn is_refused(&self) -> bool {
        self.refused.is_some()
    }
}

/// Reviewer verdict on one plan. The four-tier model from
/// `docs/cassandra_design_plan.md` §4.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    Approve,
    Advisory(String),
    Escalate(String, Severity),
    Block(String),
    /// Absolute, non-overridable. Numeric principle index 1..=5.
    ConstitutionalBlock { principle: u8, reason: String },
}

#[cfg(test)]
mod tests {
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
}
