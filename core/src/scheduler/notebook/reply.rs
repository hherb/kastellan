//! One notebook turn's reply (#922): parse it, check it against the
//! notebook, and turn it into the `Plan` the review chain sees.
//!
//! A reply is one JSON object with up to five keys:
//!
//! * `tick` — the current step is done: a 1–2 line note, the facts found
//!   (each naming its source), and the new progress summary;
//! * `steps` — a step list: required on the first turn, a replan after;
//! * exactly one of `call` (the next tool call), `answer` (the final answer,
//!   written from the findings) or `infeasible` (why the task cannot be
//!   done).
//!
//! A reply that breaks a rule is refused by [`validate`] with a reason, and
//! the loop treats that exactly like a reply that is not JSON: one re-ask
//! (#945), then the task fails in words.
//!
//! Every `call` becomes a one-step [`Plan`] ([`call_plan`]) and goes through
//! the same review and dispatch as a classic plan. An `answer` becomes a
//! terminal plan ([`answer_plan`]) whose data ceiling covers every step's
//! data the task touched, whatever the model declares ([`answer_ceiling`],
//! C1).

use serde::{Deserialize, Serialize};

use super::state::{FindingSpec, NotebookState, StepSpec, StepStatus, TickPayload};
use crate::cassandra::types::{DataClass, Plan, PlannedStep, RefusedReason, DECISION_TERMINAL};
use crate::scheduler::plan_parser::parse_json_lenient;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NotebookReply {
    #[serde(default)]
    pub tick: Option<TickSpec>,
    #[serde(default)]
    pub steps: Option<Vec<StepSpec>>,
    #[serde(default)]
    pub call: Option<CallSpec>,
    #[serde(default)]
    pub answer: Option<AnswerSpec>,
    #[serde(default)]
    pub infeasible: Option<InfeasibleSpec>,
}

/// The model's half of a tick. The loop adds the classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TickSpec {
    pub step: u32,
    pub note: String,
    #[serde(default)]
    pub findings: Vec<FindingSpec>,
    pub summary: String,
}

/// The tool call for step `step`, in the fields a [`PlannedStep`] carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallSpec {
    pub step: u32,
    pub tool: String,
    pub method: String,
    pub parameters: serde_json::Value,
    pub returns: String,
    pub done_when: String,
    pub classification: DataClass,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AnswerSpec {
    pub body: String,
    #[serde(default)]
    pub data_ceiling: Option<DataClass>,
    /// A constitutional refusal, as on a classic plan: the answer then
    /// explains it, and the task ends `Refused`.
    #[serde(default)]
    pub refused: Option<RefusedReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InfeasibleSpec {
    pub reason: String,
}

impl TickSpec {
    /// The `tick` entry's payload, with the classification the loop knows.
    pub(crate) fn payload(&self, classification: DataClass) -> TickPayload {
        TickPayload {
            step: self.step,
            note: self.note.clone(),
            findings: self.findings.clone(),
            summary: self.summary.clone(),
            classification,
        }
    }
}

/// Parse a raw reply, tolerating a fence or a preamble like a classic plan.
pub(crate) fn parse_reply(raw: &str) -> Result<NotebookReply, String> {
    parse_json_lenient::<NotebookReply>(raw).map_err(|e| format!("not a notebook reply: {e}"))
}

/// Check `reply` against the notebook it answers. `Ok` means every entry it
/// becomes will fold, and its call (if any) names a step that is pending.
pub(crate) fn validate(reply: &NotebookReply, state: &NotebookState) -> Result<(), String> {
    let ends = [reply.call.is_some(), reply.answer.is_some(), reply.infeasible.is_some()];
    if ends.iter().filter(|e| **e).count() != 1 {
        return Err("a reply must hold exactly one of `call`, `answer` or `infeasible`".into());
    }
    if state.steps.is_empty() && reply.steps.is_none() {
        return Err("the first reply must list the `steps`".into());
    }
    // Apply the reply to a copy, in the order the loop writes it: tick, then
    // steps. The classification is the loop's, not the model's; any value
    // checks the same rules.
    let mut after = state.clone();
    if let Some(tick) = &reply.tick {
        match state.current_step() {
            Some(cur) if cur.id == tick.step => {}
            Some(cur) => return Err(format!("`tick.step` {} is not the current step {}", tick.step, cur.id)),
            None => return Err(format!("`tick.step` {}: there is no current step to tick", tick.step)),
        }
        after.apply_tick(tick.payload(DataClass::Public))?;
    }
    if let Some(steps) = &reply.steps {
        after.apply_steps(steps.clone())?;
    }
    if let Some(call) = &reply.call {
        let pending = after.steps.iter().any(|s| s.spec.id == call.step && s.status == StepStatus::Pending);
        if !pending {
            return Err(format!("`call.step` {} is not a pending step", call.step));
        }
    }
    Ok(())
}

/// The one-step plan for `call`, reviewed and dispatched like any plan. Its
/// `data_ceiling` is the call's classification, raised to the task's
/// `floor`: left unset, it would resolve to the floor alone, and a call that
/// touches more than the floor would then be refused as touching more than
/// its own plan declared.
pub(crate) fn call_plan(reply: &NotebookReply, call: &CallSpec, floor: DataClass) -> Plan {
    Plan {
        context: reply.tick.as_ref().map(|t| t.summary.clone()).unwrap_or_default(),
        decision: call.returns.clone(),
        rationale: String::new(),
        steps: vec![PlannedStep {
            tool: call.tool.clone(),
            method: call.method.clone(),
            parameters: call.parameters.clone(),
            returns: call.returns.clone(),
            done_when: call.done_when.clone(),
            classification: call.classification,
        }],
        result: None,
        data_ceiling: Some(call.classification.max(floor)),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// The terminal plan carrying the final answer (and a refusal, if any).
pub(crate) fn answer_plan(body: &str, ceiling: DataClass, refused: Option<RefusedReason>) -> Plan {
    Plan {
        context: String::new(),
        decision: DECISION_TERMINAL.into(),
        rationale: String::new(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": body})),
        data_ceiling: Some(ceiling),
        refused,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

/// The answer's data ceiling: the highest of the task's floor, every step's
/// data it touched, and what the model declared (C1). An answer that leaves
/// a sensitive finding out still carries its class.
pub(crate) fn answer_ceiling(floor: DataClass, touched: &[DataClass], declared: Option<DataClass>) -> DataClass {
    touched.iter().copied().chain(declared).fold(floor, DataClass::max)
}

#[cfg(test)]
mod tests;
