//! Data types for plan review.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

/// One declared parameter of an [`L3SkillCandidate`]. The skill's
/// step `parameters` abstract task-specific values behind `{{name}}`
/// placeholders that reference these.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3Param {
    pub name: String,
    pub description: String,
}

/// One step of an [`L3SkillCandidate`] template — a parameterised
/// JSON-RPC tool call. `parameters` is a JSON object that may embed
/// `{{param_name}}` placeholders.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3TemplateStep {
    pub tool: String,
    pub method: String,
    pub parameters: serde_json::Value,
}

/// Agent-emitted candidate for an L3 (skill-layer) memory. The agent
/// emits this on a TERMINAL plan, abstracting the multi-step trajectory
/// it just executed into a reusable, parameterised tool-call template.
///
/// Validation rules + caps live in [`crate::memory::l3_crystallise`];
/// a candidate that fails validation causes the crystallise write to be
/// skipped (a `tracing::warn!` is emitted by
/// `runner::write_l3_crystallised_row` but no audit row is written).
/// Stored skills are non-executable in this slice (no invocation path).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3SkillCandidate {
    pub name: String,
    pub description: String,
    pub parameters: Vec<L3Param>,
    pub steps: Vec<L3TemplateStep>,
}

/// Agent-emitted directive to autonomously invoke a pinned L3 skill.
/// Sibling to [`Plan::l3_skill`]: where `l3_skill` *crystallises* a new
/// skill on a terminal plan, `invoke_skill` *runs* an already-pinned one
/// on a non-terminal plan. The inner loop expands it into concrete
/// [`PlannedStep`]s before review; only operator-pinned skills are
/// autonomously invocable by the agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvokeDirective {
    /// snake_case skill name, exactly as surfaced in the `<skills>` block.
    pub name: String,
    /// Agent-supplied parameter values (param name → literal value). Must
    /// supply exactly the skill's declared parameters; values are guarded
    /// by `substitute_template` (no newline/control/`{{`/`}}`/over-cap).
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// Why a plan carrying an `invoke_skill` directive is structurally
/// malformed. A malformed directive is a refusal (the agent replans);
/// it is NEVER a silent fall-through to dispatching co-supplied steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedInvoke {
    /// `invoke_skill` present alongside non-empty `steps`.
    HasSteps,
    /// `invoke_skill` present on a terminal plan (`decision == "task_complete"`).
    Terminal,
    /// `invoke_skill` present alongside an `l3_skill` crystallisation candidate.
    HasL3Skill,
}

impl std::fmt::Display for MalformedInvoke {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MalformedInvoke::HasSteps => "invoke_skill may not be combined with hand-written steps",
            MalformedInvoke::Terminal => "invoke_skill may not appear on a terminal (task_complete) plan",
            MalformedInvoke::HasL3Skill => "invoke_skill may not be combined with an l3_skill crystallisation",
        };
        f.write_str(s)
    }
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
    /// Agent-raised L1 insight candidate. Only honoured on terminal
    /// plans that reach `Outcome::Completed` (i.e. reviewer didn't
    /// Block/Escalate/ConstitutionalBlock and the agent didn't refuse).
    /// The inner loop captures this into `InnerLoopResult.terminal_l1_insight`;
    /// `runner::drain_lane` writes it to `MemoryLayer::Index` with provenance
    /// `L1Source::AgentRaised { task_id }`.
    ///
    /// Validation rules + length cap live in [`crate::memory::l1_promote`];
    /// a payload that fails validation causes the write to be skipped
    /// entirely — a `tracing::warn!` is emitted by
    /// `runner::write_l1_promoted_row` but no audit row is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l1_insight: Option<String>,
    /// Agent-raised L3 skill candidate. Only honoured on terminal
    /// plans that reach `Outcome::Completed` AND whose task executed
    /// >= 1 tool step (grounding gate in `scheduler::inner_loop`).
    ///
    /// Captured into `InnerLoopResult.terminal_l3_skill`.
    /// Written to `MemoryLayer::Skill` by `runner::drain_lane` with
    /// provenance `L3Source::AgentRaised { task_id }`, trust `"untrusted"`.
    ///
    /// Round-trips through serde with `skip_serializing_if = Option::is_none`
    /// so existing fixtures stay byte-stable when the agent doesn't emit one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l3_skill: Option<L3SkillCandidate>,
    /// Agent-emitted directive to autonomously invoke a pinned L3 skill
    /// (mutually exclusive with `steps` / `l3_skill` / terminal — see
    /// [`Plan::validate_invoke`]). The inner loop expands it into concrete
    /// `steps` before the CASSANDRA review. Round-trips with
    /// `skip_serializing_if = Option::is_none` so non-invoking plans stay
    /// byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoke_skill: Option<InvokeDirective>,
}

// Invariant (enforced by Stage 0 / `DeterministicPolicy`, see
// `cassandra::deterministic::screen_plan_for_classification_violations`):
//   plan.data_ceiling >= task.classification_floor    (I1)
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

    /// Returns `Some(insight)` iff this plan would produce
    /// `Outcome::Completed` AND carries an `l1_insight`. Encapsulates
    /// the agent-raised L1-promotion gate so the inner-loop call site
    /// stays small. `is_terminal()` is the existing check
    /// (`decision == DECISION_TERMINAL && steps.is_empty() && result.is_some()`).
    ///
    /// Named `completion_insight` (noun form) rather than
    /// `is_completion_with_insight` to follow Rust convention that
    /// `is_*` methods return `bool` — contrast with the sibling helpers
    /// `is_terminal()` and `is_refused()`.
    pub fn completion_insight(&self) -> Option<&str> {
        if self.is_terminal() {
            self.l1_insight.as_deref()
        } else {
            None
        }
    }

    /// Returns `Some(candidate)` iff this plan would produce
    /// `Outcome::Completed` AND carries an `l3_skill`. The inner loop
    /// ANDs in the `dispatch_count >= 1` grounding gate at the call
    /// site. Mirrors [`Plan::completion_insight`].
    pub fn completion_skill(&self) -> Option<&L3SkillCandidate> {
        if self.is_terminal() {
            self.l3_skill.as_ref()
        } else {
            None
        }
    }

    /// Validate a plan that carries an `invoke_skill` directive. Returns
    /// the directive when the mutual-exclusivity preconditions hold
    /// (`steps == []`, not terminal, no `l3_skill`); otherwise the
    /// specific [`MalformedInvoke`] reason. Callers branch on
    /// `self.invoke_skill.is_some()` FIRST — presence triggers the invoke
    /// path; this method never lets a malformed directive fall through to
    /// normal step dispatch.
    pub fn validate_invoke(&self) -> Result<&InvokeDirective, MalformedInvoke> {
        let dir = self
            .invoke_skill
            .as_ref()
            .expect("validate_invoke called with no invoke_skill");
        if !self.steps.is_empty() {
            return Err(MalformedInvoke::HasSteps);
        }
        if self.decision == DECISION_TERMINAL {
            return Err(MalformedInvoke::Terminal);
        }
        if self.l3_skill.is_some() {
            return Err(MalformedInvoke::HasL3Skill);
        }
        Ok(dir)
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
}
