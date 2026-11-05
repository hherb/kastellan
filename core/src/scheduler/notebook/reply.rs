//! One notebook turn's reply (#922): parse it, check it against the
//! notebook, and turn it into the `Plan` the review chain sees.
//!
//! A reply is one JSON object with up to five keys:
//!
//! * `tick` — the current step (or a failed one, found another way) is
//!   done: a 1–2 line note, the facts found (each naming its source), and
//!   the new progress summary; with a `call` for the same step it is
//!   progress and the step stays open;
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
    /// A scrapbook to show after `body` as a table rendered by code (#922,
    /// K10): the model never retypes the rows.
    #[serde(default)]
    pub table: Option<TableSpec>,
}

/// `answer.table`: one scrapbook ref or a list of them (one per sift of the
/// same criterion; their rows are joined), the columns (default: the sift's
/// fields, then `source`), an optional order, and an optional `limit` on
/// the rows shown (the first, after the order and the merge; 0 is no
/// limit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableSpec {
    pub scrapbook: Refs,
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    #[serde(default)]
    pub order_by: Option<super::sift::table::OrderBy>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One ref or a list: models write both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Refs {
    One(String),
    Many(Vec<String>),
}

impl Refs {
    pub(crate) fn refs(&self) -> &[String] {
        match self {
            Refs::One(r) => std::slice::from_ref(r),
            Refs::Many(rs) => rs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InfeasibleSpec {
    pub reason: String,
}

impl NotebookReply {
    /// The `tick` entry this reply writes, with the classification the loop
    /// knows. A tick for the step the reply also calls is progress
    /// (`partial`): the step stays open for that call.
    pub(crate) fn tick_payload(&self, classification: DataClass) -> Option<TickPayload> {
        let t = self.tick.as_ref()?;
        Some(TickPayload {
            step: t.step,
            note: t.note.clone(),
            findings: t.findings.clone(),
            summary: t.summary.clone(),
            classification,
            partial: self.call.as_ref().is_some_and(|c| c.step == t.step),
        })
    }
}

/// The fields of a [`CallSpec`]: the ones a model writes at a reply's top
/// level when it leaves out the `call` wrapper (#980).
const CALL_FIELDS: [&str; 7] = ["step", "tool", "method", "parameters", "returns", "done_when", "classification"];

/// Parse a raw reply, tolerating a fence or a preamble like a classic plan.
///
/// A reply with no `call` key that names a `tool` and a `method` at its top
/// level is a call written without its wrapper (DGX tasks 350 and 371,
/// #980): the model mirrors the shapes it is shown, so its call fields are
/// read as `call` and the other keys (`steps`, `tick`) stay. Any other
/// refusal over a stray call field names the fix, which the re-ask passes
/// on to the model.
pub(crate) fn parse_reply(raw: &str) -> Result<NotebookReply, String> {
    let refused = |e: &dyn std::fmt::Display, hint: bool| {
        let hint = if hint { " (a tool call goes inside `call`: {\"call\": {\"step\": …, \"tool\": …}})" } else { "" };
        format!("not a notebook reply: {e}{hint}")
    };
    let strict = match parse_json_lenient::<NotebookReply>(raw) {
        Ok(reply) => return Ok(reply),
        Err(e) => e,
    };
    let Ok(serde_json::Value::Object(mut top)) = parse_json_lenient::<serde_json::Value>(raw) else {
        return Err(refused(&strict, false));
    };
    let stray = CALL_FIELDS.iter().any(|f| top.contains_key(*f));
    if top.contains_key("call") || !top.contains_key("tool") || !top.contains_key("method") {
        return Err(refused(&strict, stray));
    }
    let call: serde_json::Map<_, _> = CALL_FIELDS.iter().filter_map(|f| top.remove_entry(*f)).collect();
    top.insert("call".into(), call.into());
    serde_json::from_value(top.into()).map_err(|e| refused(&e, false))
}

/// A replan that leaves out the pending step the same reply calls keeps it,
/// first: the model meant "these come after the one I am on" (DGX task 292
/// listed only the new steps while calling the current one). Applied before
/// [`validate`], so the step list written is the one checked.
pub(crate) fn keep_called_step(reply: &mut NotebookReply, state: &NotebookState) {
    let (Some(call), Some(steps)) = (&reply.call, &mut reply.steps) else { return };
    if steps.iter().any(|s| s.id == call.step) {
        return;
    }
    if let Some(kept) = state.steps.iter().find(|s| s.spec.id == call.step && s.status == StepStatus::Pending) {
        steps.insert(0, kept.spec.clone());
    }
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
        let failed = state.steps.iter().any(|s| s.spec.id == tick.step && s.status == StepStatus::Failed);
        let progress = reply.call.as_ref().is_some_and(|c| c.step == tick.step);
        match state.current_step() {
            Some(cur) if cur.id == tick.step => {}
            // A full tick may close a failed step it found another way.
            _ if failed && !progress => {}
            Some(cur) => {
                return Err(format!("`tick.step` {} is not the current step {} (nor a failed one)", tick.step, cur.id))
            }
            None => return Err(format!("`tick.step` {}: there is no current step to tick", tick.step)),
        }
        if let Some(payload) = reply.tick_payload(DataClass::Public) {
            after.apply_tick(payload)?;
        }
    }
    if let Some(steps) = &reply.steps {
        after.apply_steps(steps.clone())?;
    }
    if let Some(call) = &reply.call {
        let pending = after.steps.iter().any(|s| s.spec.id == call.step && s.status == StepStatus::Pending);
        if !pending {
            return Err(not_pending(call.step, &after));
        }
        // `returns` becomes the call plan's `decision`, and that prefix is
        // what marks a sift's own reads: the repeat guard and the turn
        // record skip such a plan.
        if call.returns.trim_start().starts_with(super::sift::SIFT_READS_DECISION.trim_end()) {
            return Err(format!("`call.returns` may not begin `{}`", super::sift::SIFT_READS_DECISION.trim_end()));
        }
    }
    Ok(())
}

/// Why `step` cannot be called, and how to replan it. DGX task 337 called a
/// done search again; told only "is not a pending step", the re-ask sent the
/// same reply and the task failed (#964). A done step is never reopened:
/// a later `{"step": n}` would then name two results. Pure.
fn not_pending(step: u32, after: &NotebookState) -> String {
    let next = after.steps.iter().map(|s| s.spec.id).max().unwrap_or(0) + 1;
    let how = match after.steps.iter().find(|s| s.spec.id == step).map(|s| &s.status) {
        Some(StepStatus::Done) => format!(
            "it is done, and a done step is never run again: use its result (a `sift` can go `over` it), or \
to make the call again with other parameters, list it in `steps` under a new id (e.g. {next}) and call that id"
        ),
        Some(StepStatus::Failed) => "it failed. To retry it, list it in `steps` again".into(),
        _ => "it is not listed. List it in `steps` first".into(),
    };
    format!("`call.step` {step} is not a pending step: {how}")
}

/// On the forced-answer turn a `call` is not a usable reply: the turn
/// budget is spent. Refusing it here sends it through the same re-ask as any
/// unusable reply, with the reason, instead of failing the task outright (DGX
/// task 273 called again on that turn, with 17 turns of findings in hand).
pub(crate) fn require_answer(reply: &NotebookReply) -> Result<(), String> {
    match reply.call {
        Some(_) => Err("this turn must end the task: reply with `answer` (or `infeasible`), not `call`".into()),
        None => Ok(()),
    }
}

/// What a notebook call's plan says in `rationale`, before its step id. The
/// plan digest leaves `rationale` out (`plan_digest`), so this changes no
/// approval; the reviewer reads it as what it is.
pub(crate) const NOTEBOOK_STEP_RATIONALE: &str = "notebook step ";

/// The notebook step a call's plan was made for (`call_plan`'s stamp), so a
/// `sift` over `{"step": n}` can find that step's result. `None` for any
/// other plan.
pub(crate) fn notebook_step_of(plan: &Plan) -> Option<u32> {
    plan.rationale.strip_prefix(NOTEBOOK_STEP_RATIONALE)?.parse().ok()
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
        rationale: format!("{NOTEBOOK_STEP_RATIONALE}{}", call.step),
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
