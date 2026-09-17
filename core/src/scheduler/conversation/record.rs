//! What a finished channel turn leaves for the next turn (#701).
//!
//! Pure and infallible: a deterministic function of the plans a task
//! accumulated and the floor it ended at. No I/O and no failure mode — a task
//! must never fail because its record could not be built.

use serde::{Deserialize, Serialize};

use crate::cassandra::types::DataClass;
use crate::scheduler::inner_loop::result_view;
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};

/// Most bytes of one call's `parameters`, as [`result_view::render`] measures
/// them. Large enough for the `{message_id, filename}` shape that motivated
/// this, and for a short query string.
pub(crate) const CALL_PARAMS_CAP: usize = 1024;

/// Most characters of the planner's own `returns` note. The note is what ties
/// a call to the thing it produced ("The extracted text from the FHZ4XR
/// e-ticket PDF"), so it earns a line, but not a paragraph.
pub(crate) const RETURNS_MAX_CHARS: usize = 256;

/// Most calls one turn records, oldest dropped first.
pub(crate) const CALLS_PER_TURN: usize = 16;

/// Most serialised bytes one turn's record may occupy in `tasks.turn_record`.
pub(crate) const TURN_RECORD_CAP: usize = 8 * 1024;

/// One tool call a turn made successfully.
///
/// `parameters` is the planner's own input to the call, pruned by
/// [`result_view::render`] so identifiers stay whole. The **result** is
/// deliberately absent: the next turn needs the id that was passed, not the
/// body that came back, and keeping results out means no tool output crosses a
/// task boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TurnCall {
    pub(crate) tool: String,
    pub(crate) method: String,
    pub(crate) parameters: serde_json::Value,
    pub(crate) returns: String,
}

/// The record one finished channel turn leaves behind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TurnRecord {
    pub(crate) calls: Vec<TurnCall>,
    /// How many calls were dropped to fit the caps. Serialised as
    /// `_omitted_calls`, and omitted entirely when zero: absence and loss must
    /// not render identically, so a reader that sees no key knows nothing was
    /// cut.
    #[serde(rename = "_omitted_calls", default, skip_serializing_if = "is_zero")]
    pub(crate) omitted_calls: usize,
    /// The most sensitive class this turn touched: the max of the task's final
    /// floor and every dispatched step's declared classification. The next
    /// turn inherits it (see [`super::floor`]).
    pub(crate) data_class: DataClass,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// `s` cut to `max_chars` characters plus `…` when longer. Character
/// boundaries, not bytes: cutting a multi-byte character in half would panic.
fn clamp_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…")
}

/// Build the record for a finished turn.
///
/// One entry per step whose outcome was `Ok`, in dispatch order. A failed step
/// is left out: an error is not a referent. A step whose *result* the sink
/// screen withheld still contributes its call — the parameters are
/// planner-authored, and the result is not carried either way.
///
/// `final_floor` is the task's floor at the end, which already includes any
/// agent raise and any floor this task itself inherited, so inheritance chains
/// correctly across turns.
pub(crate) fn from_plans(plans: &[PlanRecord], final_floor: DataClass) -> TurnRecord {
    let mut calls: Vec<TurnCall> = Vec::new();
    let mut data_class = final_floor;

    for record in plans {
        // `outcomes` is index-aligned with `plan.steps`; zip stops at the
        // shorter one, so a plan whose steps were not all dispatched (an early
        // terminal, a cap) contributes only what actually ran.
        for (step, outcome) in record.plan.steps.iter().zip(record.outcomes()) {
            if step.classification.rank() > data_class.rank() {
                data_class = step.classification;
            }
            if matches!(outcome, StepOutcome::Err { .. }) {
                continue;
            }
            calls.push(TurnCall {
                tool: step.tool.clone(),
                method: step.method.clone(),
                parameters: result_view::render(&step.parameters, CALL_PARAMS_CAP),
                returns: clamp_chars(&step.returns, RETURNS_MAX_CHARS),
            });
        }
    }

    // The count cap first: drop the OLDEST calls, because the referent a
    // follow-up asks about is nearly always the most recent thing that
    // happened.
    let over = calls.len().saturating_sub(CALLS_PER_TURN);
    if over > 0 {
        calls.drain(0..over);
    }

    // Then the byte cap, measured rather than estimated: drop the oldest call
    // until the serialised record fits. Terminates because an empty `calls`
    // serialises well under the cap.
    let mut record = TurnRecord { calls, omitted_calls: over, data_class };
    while !record.calls.is_empty() && serialised_len(&record) > TURN_RECORD_CAP {
        record.calls.remove(0);
        record.omitted_calls += 1;
    }
    record
}

/// Serialised size of `record`, as the prompt and storage budgets count bytes.
/// An unserialisable record is impossible here (no non-string keys, no NaN);
/// were it ever possible it reports `usize::MAX`, which fails closed by
/// dropping calls.
fn serialised_len(record: &TurnRecord) -> usize {
    serde_json::to_value(record)
        .map(|v| result_view::serialised_len(&v))
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests;
