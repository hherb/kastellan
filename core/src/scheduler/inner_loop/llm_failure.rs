//! What the inner loop does when a planning call to the model fails
//! (issue #774).
//!
//! Before this, any error out of the formulator ended the task with
//! `llm: <router error>`. On the DGX that threw away whole tasks: a mail
//! question gathered 13 tool results correctly, then the forced-synthesis
//! call — the one meant to *write the answer* — ran past
//! `KASTELLAN_LLM_TIMEOUT_MS`, and the user got a raw transport error after
//! nine minutes and nothing else.
//!
//! Three changes, the decisions for all of which live here as pure
//! functions so they are testable without a model or a database:
//!
//! 1. **A timed-out planning call after tools have gathered results jumps
//!    straight to the forced-synthesis turn** ([`should_force_synthesis`])
//!    instead of failing: the work is in hand, so ask for an answer from it.
//! 2. **A timed-out synthesis turn is retried once with thinking
//!    suppressed** — decided in the loop, because only the formulator knows
//!    whether a cheaper attempt exists
//!    (`PlanFormulator::formulate_synthesis_without_thinking`).
//! 3. **A timeout that still ends the task says so in words**
//!    ([`timeout_failure_detail`]): the model ran out of time, what had been
//!    gathered, and what to try. The raw router error stays on the end, both
//!    for the operator and so the existing `detail like '%timed out%'` query
//!    keeps finding these rows.
//!
//! Every other formulator error (a decode failure, an HTTP 4xx, a refused
//! connection) is unchanged: a cheaper request cannot fix it.

use std::collections::BTreeMap;

use super::{PlanRecord, StepOutcome};

/// Should a failed planning call be answered by spending the
/// forced-synthesis turn now, rather than failing the task?
///
/// Only for a **request timeout** (the model was reached and was slow —
/// the case a shorter "just answer" request can beat), only once there is
/// something to synthesize from (`gathered`: at least one successful tool
/// observation), and only if the synthesis turn has not been spent — the
/// synthesis turn's own timeout has its retry and then fails. Pure.
pub(super) fn should_force_synthesis(timed_out: bool, gathered: bool, synth_attempted: bool) -> bool {
    timed_out && gathered && !synth_attempted
}

/// Longest method name the failure message repeats verbatim.
///
/// Method names are *model-emitted* (a plan step's `method`), so this
/// message is one of the few places planner text reaches the user outside
/// the synthesized answer. Anything that is not a plausible method name is
/// counted under [`OTHER_METHOD`] instead of being echoed.
const MAX_METHOD_CHARS: usize = 64;

/// The bucket for method names too long or too odd to repeat.
const OTHER_METHOD: &str = "other";

/// The tool calls a task made before it failed, for telling the user what
/// the timeout threw away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct GatheredWork {
    /// Tool calls actually dispatched (steps after a failing one never are).
    pub calls: usize,
    /// How many of them returned a result.
    pub succeeded: usize,
    /// Dispatched calls per method, sorted by name for a stable message.
    pub by_method: BTreeMap<String, usize>,
}

impl GatheredWork {
    /// Count the dispatched calls in `plans`. Pure.
    ///
    /// A record's outcomes are index-aligned with its plan's steps and never
    /// outnumber them; a step with no outcome was never dispatched.
    pub(super) fn from_plans(plans: &[PlanRecord]) -> Self {
        let mut work = GatheredWork::default();
        for record in plans {
            for (step, outcome) in record.plan.steps.iter().zip(record.outcomes()) {
                work.calls += 1;
                if matches!(outcome, StepOutcome::Ok(_)) {
                    work.succeeded += 1;
                }
                *work.by_method.entry(displayable_method(&step.method)).or_default() += 1;
            }
        }
        work
    }
}

/// `method` if it looks like a method name (`mail.get_message`), else
/// [`OTHER_METHOD`]. Pure.
fn displayable_method(method: &str) -> String {
    let plausible = !method.is_empty()
        && method.chars().count() <= MAX_METHOD_CHARS
        && method.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if plausible { method.to_string() } else { OTHER_METHOD.to_string() }
}

/// The task's failure detail when a planning call ran out of time and
/// nothing more can be done. Pure.
///
/// This text reaches the user (a Matrix or email reply), so it leads with
/// what happened in plain words; `router_error` goes last, in brackets,
/// for the operator.
pub(super) fn timeout_failure_detail(work: &GatheredWork, router_error: &str) -> String {
    let what = if work.calls == 0 {
        "The language model ran out of time before it finished planning, so no tool was \
         called and no answer was produced."
            .to_string()
    } else {
        let methods = work
            .by_method
            .iter()
            .map(|(m, n)| format!("{m} ×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "The language model ran out of time before it could write an answer, so this task \
             produced none. It had already made {} tool call{} ({} succeeded): {methods}.",
            work.calls,
            if work.calls == 1 { "" } else { "s" },
            work.succeeded,
        )
    };
    format!(
        "{what} Asking for less at once (fewer items, a narrower question) usually fits; the \
         operator can also raise KASTELLAN_LLM_TIMEOUT_MS. [llm: {router_error}]"
    )
}

#[cfg(test)]
mod tests;
