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
//! Three changes. [`formulate_turn`] is the thin async shell the loop calls
//! (it talks to the formulator and writes audit rows); every *decision* it
//! makes is a pure function here, testable without a model or a database:
//!
//! 1. **A timed-out planning call after tools have gathered results jumps
//!    straight to the forced-synthesis turn** ([`should_force_synthesis`])
//!    instead of failing: the work is in hand, so ask for an answer from it.
//! 2. **A timed-out synthesis turn is retried once with thinking
//!    suppressed** — only the formulator knows whether a cheaper attempt
//!    exists (`PlanFormulator::formulate_synthesis_without_thinking`).
//! 3. **A timeout that still ends the task says so in words**
//!    ([`timeout_failure_detail`]): the model ran out of time, what had been
//!    gathered, and what to try. The raw router error stays on the end, both
//!    for the operator and so the existing `detail like '%timed out%'` query
//!    keeps finding these rows.
//!
//! Every other formulator error (a decode failure, an HTTP 4xx, a refused
//! connection) is unchanged: a cheaper request cannot fix it.

use std::collections::BTreeMap;

use super::{PlanRecord, StepOutcome, TaskContext};
use crate::cassandra::types::Plan;
use crate::scheduler::agent::{FormulationMeta, PlanFormulator};

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

/// The task's failure detail when a synthesis turn that a **timeout**
/// forced (not the plan cap) still returned tool steps instead of an answer.
/// Pure.
///
/// Distinct from the cap's `plan_iteration_cap_exceeded` wording on
/// purpose: the cap was never reached, and operators count cap-exceeded
/// rows (#774 review).
pub(super) fn forced_synthesis_no_answer_detail(work: &GatheredWork) -> String {
    format!(
        "A planning call ran out of time, so the task asked the language model to answer from \
         the {} tool result{} already gathered — and it asked for more tools instead of \
         answering. No answer was produced. Asking for less at once usually fits; the operator \
         can also raise KASTELLAN_LLM_TIMEOUT_MS. [llm: forced synthesis after a timeout did not \
         produce a final answer]",
        work.succeeded,
        if work.succeeded == 1 { "" } else { "s" },
    )
}

/// Audit action for a formulator call that produced no plan.
pub(super) const ACTION_FORMULATE_FAILED: &str = "plan.formulate_failed";

/// Longest router-error text an `agent/plan.formulate_failed` row carries.
/// The error can embed a backend's response body (already capped at 1 KiB
/// by the router); the row needs the kind of failure, not the body.
const FAILED_ROW_ERROR_CHARS: usize = 512;

/// Payload for one `agent/plan.formulate_failed` row. Pure.
///
/// A timed-out call writes no `plan.formulate` row (there is no plan), so
/// without this the minutes it cost are visible only in the daemon log —
/// and #774's point is that "why was this task slow?" is a query.
/// `plan_count` is the count *before* the call (a failed call adds no
/// plan); `thinking_suppressed_retry` marks the synthesis retry.
pub(super) fn formulate_failed_payload(
    task_id: i64,
    plan_count: u32,
    synth_turn: bool,
    thinking_suppressed_retry: bool,
    request_timeout: bool,
    error: &str,
) -> serde_json::Value {
    serde_json::json!({
        "task_id": task_id,
        "plan_count": plan_count,
        "synth_turn": synth_turn,
        "thinking_suppressed_retry": thinking_suppressed_retry,
        "request_timeout": request_timeout,
        "error": error.chars().take(FAILED_ROW_ERROR_CHARS).collect::<String>(),
    })
}

/// What one planning turn produced.
pub(super) enum Turn {
    /// A plan to act on.
    Planned(Plan, FormulationMeta),
    /// The call timed out after tools had gathered results: spend the
    /// forced-synthesis turn next instead of failing ([`should_force_synthesis`]).
    ForceSynthesis,
    /// The task fails with this user-facing detail.
    Failed(String),
}

/// The three loop facts the recovery decisions read.
pub(super) struct TurnState {
    /// This is the forced-synthesis turn.
    pub synth_turn: bool,
    /// At least one tool step has succeeded, so there is something to answer from.
    pub gathered: bool,
    /// The forced-synthesis turn has been spent (or is this one).
    pub synth_attempted: bool,
}

/// Run one planning turn, with #774's timeout recovery around it.
///
/// Every failed formulator call — the first attempt, and a synthesis retry
/// — adds one to `failed_llm_calls` and writes exactly one
/// `plan.formulate_failed` row; `failure_recorded` is what stops the final
/// match from writing a second row for the one error already recorded on
/// the no-retry path.
pub(super) async fn formulate_turn(
    pool: &sqlx::PgPool,
    formulator: &dyn PlanFormulator,
    ctx: &TaskContext,
    state: TurnState,
    failed_llm_calls: &mut u32,
) -> Turn {
    let TurnState { synth_turn, gathered, synth_attempted } = state;
    let first = if synth_turn {
        formulator.formulate_synthesis(ctx).await
    } else {
        formulator.formulate_plan(ctx).await
    };
    // A synthesis turn that ran out of time gets one more attempt with
    // thinking suppressed, when the formulator has such an attempt. Every
    // failed call is counted and gets an audit row exactly once:
    // `failure_recorded` stops the row below from repeating one already
    // written here.
    let mut retried = false;
    let mut failure_recorded = false;
    let formulation = match first {
        Err(e) if synth_turn && e.is_request_timeout() => {
            *failed_llm_calls = failed_llm_calls.saturating_add(1);
            record_failed_formulation(
                pool, ctx.task_id, ctx.plan_count, true, false, &e,
            ).await;
            match formulator.formulate_synthesis_without_thinking(ctx).await {
                Some(retry) => {
                    tracing::warn!(
                        task_id = ctx.task_id,
                        plan_count = ctx.plan_count,
                        retry_ok = retry.is_ok(),
                        "forced-synthesis call timed out; retried once with thinking suppressed"
                    );
                    retried = true;
                    retry
                }
                None => {
                    tracing::warn!(
                        task_id = ctx.task_id,
                        plan_count = ctx.plan_count,
                        "forced-synthesis call timed out; no cheaper retry exists \
                         (thinking is already suppressed by config)"
                    );
                    failure_recorded = true;
                    Err(e)
                }
            }
        }
        other => other,
    };
    let (plan, mut meta) = match formulation {
        Ok(x) => x,
        Err(e) => {
            if !failure_recorded {
                *failed_llm_calls = failed_llm_calls.saturating_add(1);
                record_failed_formulation(
                    pool, ctx.task_id, ctx.plan_count, synth_turn, retried, &e,
                ).await;
            }
            let timed_out = e.is_request_timeout();
            if should_force_synthesis(timed_out, gathered, synth_attempted) {
                tracing::warn!(
                    task_id = ctx.task_id,
                    plan_count = ctx.plan_count,
                    error = %e,
                    "planning call timed out after tools had gathered results; \
                     spending the forced-synthesis turn now instead of failing the task"
                );
                return Turn::ForceSynthesis;
            }
            let detail = if timed_out {
                timeout_failure_detail(
                    &GatheredWork::from_plans(&ctx.plans),
                    &e.to_string(),
                )
            } else {
                format!("llm: {e}")
            };
            return Turn::Failed(detail);
        }
    };
    // The retry's `plan.formulate` row says it was one: the existing
    // `retry_count` column, which the formulator itself leaves at 0.
    if retried {
        meta.retry_count = meta.retry_count.saturating_add(1);
    }
    Turn::Planned(plan, meta)
}

/// Write the `agent/plan.formulate_failed` row for one failed formulator
/// call. **Best-effort**: a lost forensic row must not fail a task the
/// loop may still rescue — logged at ERROR instead, like the sink-block rows.
pub(super) async fn record_failed_formulation(
    pool: &sqlx::PgPool,
    task_id: i64,
    plan_count: u32,
    synth_turn: bool,
    thinking_suppressed_retry: bool,
    error: &crate::scheduler::agent::AgentError,
) {
    let payload = formulate_failed_payload(
        task_id,
        plan_count,
        synth_turn,
        thinking_suppressed_retry,
        error.is_request_timeout(),
        &error.to_string(),
    );
    if let Err(e) = kastellan_db::audit::insert(pool, "agent", ACTION_FORMULATE_FAILED, payload).await {
        tracing::error!(task_id, error = %e, "plan.formulate_failed audit insert failed");
    }
}

#[cfg(test)]
mod tests;
