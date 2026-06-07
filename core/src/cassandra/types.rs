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
mod tests;
