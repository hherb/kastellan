//! What the planner is shown about its **own** earlier plans: the call each
//! step made (#699) and the `decision` each plan stated (#700).
//!
//! Until #699 a completed plan reached the next prompt as
//! `{decision, step_outcomes}` alone, so the planner saw what came back but not
//! what it had asked for. In task 186 it searched with
//! `has_attachment: true`, then dropped that filter on the next search, and
//! nothing in its prompt could have told it so. Each step outcome now carries
//! the call beside it, under [`CALL_KEY`].
//!
//! Both pieces are **planner-authored**, but a planner that has read injected
//! text can be led to copy it into a parameter or into its `decision`, which
//! would then re-enter every later prompt of the task. So both pass the sink
//! screen like every other string this module emits — under the `Strict`
//! profile, because they are model text rather than a tool's output, and a
//! document-fetching tool's `Relaxed` allowance for quoted chat templates does
//! not apply to what the planner wrote itself.
//!
//! Pure and deterministic in its inputs; screened **once**, when the
//! [`super::PlanRecord`] is built, like the step outcomes.

use serde_json::{json, Value};

use super::sink::{clamp_audit_label, sink_screen_with, SinkBlock};
use super::{ELIDED_KEY, ELIDED_REASON, WITHHELD_MARKER};
use crate::cassandra::injection_guard::GuardProfile;
use crate::cassandra::types::PlannedStep;
use crate::scheduler::conversation::record::CALL_PARAMS_CAP;
use crate::scheduler::inner_loop::result_view;

/// The key, inside one step outcome object, under which the call that produced
/// it is shown: `{"tool", "method", "parameters"}`, or [`WITHHELD_MARKER`].
pub(super) const CALL_KEY: &str = "call";

/// The key under which a call's pruned parameters are shown.
const PARAMETERS_KEY: &str = "parameters";

/// What a call adds to its step outcome object beyond its own serialised
/// value: the `"call":` key and the separating comma.
const CALL_FRAMING_BYTES: usize = r#","call":"#.len();

/// The profile both screens here use: planner-authored text is held to the
/// fail-closed default whatever tool it names.
const MODEL_AUTHORED_PROFILE: GuardProfile = GuardProfile::Strict;

/// A planner-bound value that has been through the sink screen, plus what the
/// screen blocked in it (for the forensic row). `value` is already the
/// replacement when `sink_block` is `Some`.
#[derive(Clone, Debug)]
pub(super) struct Screened {
    pub(super) value: Value,
    pub(super) sink_block: Option<SinkBlock>,
}

impl Screened {
    /// Screen `value` (every key and string of it, as the step views are
    /// screened) and replace it with [`WITHHELD_MARKER`] on a Block.
    ///
    /// `also` holds extra screen texts taken from *inside* `value`, screened
    /// again on their own. [`result_view::screen_text`] stops at
    /// `MAX_WALK_DEPTH` counted from the value it is handed, while
    /// [`result_view::render`] prunes from the sub-value's own root, so a
    /// sub-value nested one level down keeps a deepest level the wrapped walk
    /// never reaches — rendered for the planner but unscreened (found by
    /// review; out of reach today only because serde_json's own recursion
    /// limit is lower). Screening that sub-value at its own root puts the
    /// level back. Strictly additive: every reading of the whole `value` is
    /// still taken first, so this can only block more, never less (#702).
    fn screen(value: Value, also: &[&str]) -> Self {
        let whole = result_view::screen_text(&value);
        for text in std::iter::once(whole.as_str()).chain(also.iter().copied()) {
            if let Some(block) = sink_screen_with(MODEL_AUTHORED_PROFILE, text) {
                return Self { value: Value::from(WITHHELD_MARKER), sink_block: Some(block) };
            }
        }
        Self { value, sink_block: None }
    }
}

/// The call `step` made, as the planner will see it beside its outcome.
///
/// `tool` and `method` are clamped the way the audit row clamps them (an
/// `UNKNOWN_TOOL` step can carry an arbitrarily long invented name), and
/// `parameters` is pruned by [`result_view::render`] to [`CALL_PARAMS_CAP`] —
/// the same cap and the same identifier-preserving rules #701 uses for an
/// earlier turn's calls, so a `message_id` is never cut in half.
pub(super) fn render_call(step: &PlannedStep) -> Screened {
    let parameters = result_view::render(&step.parameters, CALL_PARAMS_CAP);
    // The parameters are pruned from their own root, so they carry one level
    // more than the screen of the whole call can walk; see [`Screened::screen`].
    let deepest = result_view::screen_text(&parameters);
    Screened::screen(
        json!({
            "tool":       clamp_audit_label(&step.tool),
            "method":     clamp_audit_label(&step.method),
            PARAMETERS_KEY: parameters,
        }),
        &[&deepest],
    )
}

/// What `call` adds to the serialised summary once it sits inside its step
/// outcome object. The unit [`apply_call_budget`] and its caller count in.
pub(super) fn call_cost(call: &Value) -> usize {
    result_view::serialised_len(call).saturating_add(CALL_FRAMING_BYTES)
}

/// `call` with its `parameters` replaced by `"elided": "summary budget"`,
/// keeping `tool` and `method`; `None` for a call with no parameters to drop
/// (the withheld marker, or one already elided).
fn without_parameters(call: &Value) -> Option<Value> {
    let obj = call.as_object().filter(|o| o.contains_key(PARAMETERS_KEY))?;
    let mut obj = obj.clone();
    obj.remove(PARAMETERS_KEY);
    obj.insert(ELIDED_KEY.into(), Value::from(ELIDED_REASON));
    Some(Value::Object(obj))
}

/// Drop the oldest calls' `parameters` until `total` — the summary's current
/// serialised size, calls included — is within `budget`. Returns how many
/// calls lost their parameters.
///
/// The second pass of the summary budget, run only once the step outputs have
/// been elided: calls are what stop the planner repeating itself, so they go
/// last, and even then `tool` and `method` stay. Oldest first (`calls[0]` is
/// the first plan), stopping the moment the total fits, and only where
/// dropping actually shrinks the call, so it is idempotent.
pub(super) fn apply_call_budget(calls: &mut [Vec<Option<Value>>], mut total: usize, budget: usize) -> usize {
    let mut elided = 0;
    for call in calls.iter_mut().flatten().flatten() {
        if total <= budget {
            break;
        }
        let Some(smaller) = without_parameters(call) else { continue };
        let (before, after) = (call_cost(call), call_cost(&smaller));
        if after < before {
            total = total.saturating_sub(before - after);
            *call = smaller;
            elided += 1;
        }
    }
    elided
}

/// The plan's `decision`, screened (#700). Not clamped: it is not counted in
/// the summary budget today (see `PLANS_SUMMARY_BUDGET`), and clamping it is
/// a separate change from screening it — filed as #729, which also has the
/// argument for why an unbounded always-in-context string wants a bound.
pub(super) fn render_decision(decision: &str) -> Screened {
    Screened::screen(Value::from(decision), &[])
}

#[cfg(test)]
mod tests;
