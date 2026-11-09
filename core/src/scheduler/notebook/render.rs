//! The user message of one notebook turn (#922): the goal (and the
//! conversation), the notebook so far, any new operator corrections, the
//! reviewer's advisories and blocks, the last failed call, the current step,
//! the turn budget, and `now`.
//!
//! **Its size does not grow with the number of tool results.** Only the
//! current step's last result (or the last failed call's) is shown, never the
//! earlier ones; what earlier steps found survives as the model's own notes
//! and findings. (`blocks` and `advisories` do grow, one line per block.)
//!
//! **Key order is the prompt cache's order** (#943), as in the classic
//! `agent::context`: what is fixed for the task, then the notebook (which
//! changes in place as steps finish), then what changes every turn, the turn
//! budget, and `now` last.
//! A `serde_json::Map` would sort the keys, so every level is built as
//! ordered JSON text (`obj`).
//!
//! **Trust.** Notes, findings, the summary and the step list (a replan is
//! written after the model read tool output) are written by the model from
//! tool output, so they carry tool-output trust: each goes through the
//! planner-text screen ([`screen_model_text`]) on every render, and a blocked
//! one is shown as [`WITHHELD_MARKER`]. A withheld finding is still counted
//! (`withheld_findings`), so the answer can say a fact was withheld (C6).
//! The step's last result is the summary's stored, screened view
//! ([`PlanRecord::last_step_view`]). Corrections are the operator's own text.

use serde_json::{json, Value};

use super::state::{NotebookState, StepSpec, StepStatus};
use crate::scheduler::inner_loop::{ceiling, screen_model_text, PlanRecord, TaskContext, WITHHELD_MARKER};

/// How many estimated tokens of findings the notebook holds before it tells
/// the model it is full (`findings_full: true`): time to answer. Findings are
/// never dropped past it — the answer is written from them (D6).
pub(crate) const FINDINGS_TOKEN_BUDGET: usize = 8_000;

/// Bytes per token for the estimate above, measured on the DGX's planner
/// traffic (#940: a 96 KiB summary was ~49k tokens).
pub(crate) const BYTES_PER_TOKEN: usize = 2;

/// The step the turn works on, and what happened the last time it was tried.
pub(crate) struct CurrentStep<'a> {
    pub step: &'a StepSpec,
    /// The step's tool's full doc (`prompt_assembly::render_tool_doc`);
    /// `None` when the tool is not advertised.
    pub tool_doc: Option<String>,
    /// The record of the call made for this step last turn, if any.
    pub last: Option<&'a PlanRecord>,
}

/// Where this turn sits in the task's budget. `directive` replaces the
/// counts on the forced-answer turn and on the turn that rewrites an answer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnBudget {
    pub this_turn: u32,
    pub max_turns: u32,
    pub directive: Option<&'static str>,
}

/// The user message for one notebook turn. `seen_corrections` is the seq of
/// the last correction already shown; only later ones are rendered.
/// `last_failure` is the step whose call failed last turn and that call's
/// record: the step is no longer current, so without it the replan turn
/// would not see why it failed.
pub(crate) fn render_turn(
    ctx: &TaskContext,
    state: &NotebookState,
    seen_corrections: i64,
    last_failure: Option<(u32, &PlanRecord)>,
    current: Option<CurrentStep<'_>>,
    turn: TurnBudget,
    now: Option<&str>,
) -> String {
    let mut fields: Vec<(&str, String)> = vec![
        ("instruction", json!(ctx.instruction).to_string()),
        ("classification_floor", json!(ctx.classification_floor).to_string()),
    ];
    if !ctx.conversation.is_empty() {
        fields.push(("conversation", Value::Array(ctx.conversation.clone()).to_string()));
    }
    fields.push(("notebook", notebook_view(state)));
    let unseen = state.unseen_corrections(seen_corrections);
    if !unseen.is_empty() {
        let items: Vec<Value> = unseen.iter().map(|c| json!({"text": c.text})).collect();
        fields.push(("corrections", Value::Array(items).to_string()));
    }
    // The reviewer's word on earlier calls, as the classic loop shows it:
    // without `blocks` a blocked call would be tried again blind. Both only
    // grow, so they sit before the parts that change every turn.
    if !ctx.advisories.is_empty() {
        fields.push(("advisories", json!(ctx.advisories).to_string()));
    }
    if !ctx.blocks.is_empty() {
        fields.push(("blocks", json!(ctx.blocks).to_string()));
    }
    // Every step of an invoked skill, the failed one last: the repeat guard
    // counts the earlier ones as seen, so hiding them would strand their facts.
    if let Some(view) = last_failure.and_then(|(step, r)| Some((step, r.last_result_view()?))) {
        fields.push(("last_failure", obj(&[("step", view.0.to_string()), ("result", view.1.to_string())])));
    }
    if let Some(cur) = current {
        fields.push(("current_step", current_step_view(&cur)));
    }
    fields.push(("turn_budget", budget_view(ctx, turn)));
    if let Some(now) = now {
        fields.push(("now", json!(now).to_string()));
    }
    obj(&fields)
}

/// A compact JSON object whose keys appear exactly in `fields`' order; each
/// value is already JSON text. Nested objects are built the same way, so the
/// order holds at every level (a `Value::Object` would sort its keys, and
/// `findings`, which grows, would then sort ahead of `steps`).
fn obj(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields.iter().map(|(k, v)| format!("{}:{v}", Value::from(*k))).collect();
    format!("{{{}}}", body.join(","))
}

fn arr(items: Vec<String>) -> String {
    format!("[{}]", items.join(","))
}

/// `text` as JSON, or the withheld marker when the planner-text screen
/// blocks it.
fn screened(text: &str) -> String {
    let shown = if screen_model_text(text).is_some() { WITHHELD_MARKER } else { text };
    json!(shown).to_string()
}

fn notebook_view(state: &NotebookState) -> String {
    let steps: Vec<String> = state
        .steps
        .iter()
        .map(|s| {
            let status = match s.status {
                StepStatus::Pending => "pending",
                StepStatus::Done => "done",
                StepStatus::Failed => "failed",
            };
            let mut fields: Vec<(&str, String)> = vec![
                ("id", s.spec.id.to_string()),
                ("purpose", screened(&s.spec.purpose)),
                ("tool", screened(&s.spec.tool)),
                ("done_when", screened(&s.spec.done_when)),
                ("status", json!(status).to_string()),
            ];
            if let Some(note) = &s.note {
                fields.push(("note", screened(note)));
            }
            obj(&fields)
        })
        .collect();

    let mut withheld = 0usize;
    let mut finding_bytes = 0usize;
    let findings: Vec<String> = state
        .findings
        .iter()
        .map(|f| {
            finding_bytes += f.text.len() + f.source.len();
            if screen_model_text(&f.text).is_some() || screen_model_text(&f.source).is_some() {
                withheld += 1;
                obj(&[("text", json!(WITHHELD_MARKER).to_string()), ("step", f.step.to_string())])
            } else {
                obj(&[
                    ("text", json!(f.text).to_string()),
                    ("source", json!(f.source).to_string()),
                    ("step", f.step.to_string()),
                ])
            }
        })
        .collect();

    let mut fields: Vec<(&str, String)> = vec![("steps", arr(steps))];
    if let Some(summary) = &state.summary {
        fields.push(("summary", screened(summary)));
    }
    fields.push(("findings", arr(findings)));
    if finding_bytes / BYTES_PER_TOKEN > FINDINGS_TOKEN_BUDGET {
        fields.push(("findings_full", "true".into()));
    }
    if withheld > 0 {
        fields.push(("withheld_findings", withheld.to_string()));
    }
    obj(&fields)
}

fn current_step_view(cur: &CurrentStep<'_>) -> String {
    let mut fields: Vec<(&str, String)> = vec![
        ("id", cur.step.id.to_string()),
        ("purpose", screened(&cur.step.purpose)),
        ("done_when", screened(&cur.step.done_when)),
    ];
    if let Some(doc) = &cur.tool_doc {
        fields.push(("tool_doc", json!(doc).to_string()));
    }
    if let Some(view) = cur.last.and_then(PlanRecord::last_result_view) {
        fields.push(("last_result", view.to_string()));
    }
    obj(&fields)
}

fn budget_view(ctx: &TaskContext, turn: TurnBudget) -> String {
    if let Some(d) = turn.directive {
        return obj(&[("directive", json!(d).to_string())]);
    }
    let mut fields: Vec<(&str, String)> =
        vec![("this_turn", turn.this_turn.to_string()), ("max_turns", turn.max_turns.to_string())];
    if let Some(s) = ceiling::seconds_left(ctx.deadline, std::time::Instant::now()) {
        fields.push(("seconds_left", s.to_string()));
    }
    obj(&fields)
}

#[cfg(test)]
mod tests;
