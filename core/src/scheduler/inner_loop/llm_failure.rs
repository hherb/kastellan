//! What the loop does when a planning call to the model fails (issue #774).
//!
//! Before this, any error out of the formulator ended the task with
//! `llm: <router error>`. On the DGX that threw away whole tasks: a mail
//! question gathered 13 tool results correctly, then the forced-synthesis
//! call — the one meant to *write the answer* — ran past
//! `KASTELLAN_LLM_TIMEOUT_MS`, and the user got a raw transport error after
//! nine minutes and nothing else.
//!
//! The notebook's `super::super::notebook::ask` is the thin async shell the
//! loop calls (it talks to the model and writes audit rows); every
//! *decision* it makes is a pure function here, testable without a model or
//! a database:
//!
//! 1. **A timed-out planning call after tools have gathered results jumps
//!    straight to the forced-answer turn** ([`should_force_synthesis`])
//!    instead of failing: the work is in hand, so ask for an answer from it.
//!    A reply cut off at `max_tokens` (#941) gets the same recovery, and
//!    with nothing gathered fails in its own words ([`token_cap_failure_detail`]).
//! 2. **A timeout that still ends the task says so in words**
//!    ([`timeout_failure_detail`]): the model ran out of time, what had been
//!    gathered, and what to try. The raw router error stays on the end, both
//!    for the operator and so the existing `detail like '%timed out%'` query
//!    keeps finding these rows.
//!
//! An **empty completion** (#923: no content, and not cut off at `length`)
//! is asked again once; a decode failure that still ends the task says so in
//! words ([`decode_failure_detail`]). A **reply that cannot be used** (#945)
//! is shown back to the model once with the reason, thinking suppressed. At
//! most one retry of any kind per turn: a retry's own failure is never
//! retried. Every other model error (an HTTP status, a refused connection, a
//! missing prompt) is `llm: <error>`: the loop does not retry it. Every
//! failed call writes one `plan.formulate_failed` row
//! ([`record_failed_formulation`]).

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
pub(crate) fn should_force_synthesis(timed_out: bool, gathered: bool, synth_attempted: bool) -> bool {
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
pub(crate) struct GatheredWork {
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
    pub(crate) fn from_plans(plans: &[PlanRecord]) -> Self {
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
pub(crate) fn timeout_failure_detail(work: &GatheredWork, router_error: &str) -> String {
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
         operator can also raise KASTELLAN_LLM_TIMEOUT_MS (a fast-lane task's time ceiling bounds \
         each call too). [llm: {router_error}]"
    )
}

/// The task's failure detail when the model's answer could not be read as a
/// plan (#923). Pure.
///
/// `empty`: the answer had no content — retried once if it was
/// [`AgentError::is_empty_completion`], not if it was cut off at `length`. Like
/// [`timeout_failure_detail`], plain words first and the decode error last,
/// in brackets, for the operator.
pub(crate) fn decode_failure_detail(empty: bool, decode_error: &str) -> String {
    let what = if empty {
        "The language model returned an empty answer, so this task produced none."
    } else {
        "The language model answered with something that is not a plan, so this task produced none."
    };
    format!("{what} You can ask again. [llm: {decode_error}]")
}

/// The task's failure detail when the model's reply was cut off at
/// [`super::ceiling::PLANNER_MAX_TOKENS`] (#941) with nothing gathered to
/// answer from. Pure.
pub(crate) fn token_cap_failure_detail(decode_error: &str) -> String {
    format!(
        "The language model used its whole reply budget ({} tokens) without finishing, so this task \
         produced none. Asking for less at once (fewer items, a narrower question) usually fits. \
         [llm: {decode_error}]",
        super::ceiling::PLANNER_MAX_TOKENS,
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
         can also raise KASTELLAN_LLM_TIMEOUT_MS (a fast-lane task's time ceiling bounds each \
         call too). [llm: forced synthesis after a timeout did not \
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

/// Longest head of the model's own reply an `agent/plan.formulate_failed`
/// row carries (#945). Enough to tell a prose answer from a refusal or a
/// preamble. It is model output and can restate tool data (on a mail task,
/// the user's mail): accepted, with eyes open, as the agent's own data at the
/// agent user's boundary. The row lands in `audit_log` **and** its plaintext
/// JSONL mirror (`audit_mirror`, mode 0600) — the same OS-user protection as
/// the daemon log, no stronger. It goes to the row rather than a `warn!` so
/// "what did the model say?" is a query against the task, not a log grep.
const FAILED_ROW_RAW_HEAD_CHARS: usize = 512;

/// Payload for one `agent/plan.formulate_failed` row. Pure.
///
/// A timed-out call writes no `plan.formulate` row (there is no plan), so
/// without this the minutes it cost are visible only in the daemon log —
/// and #774's point is that "why was this task slow?" is a query.
/// `plan_count` is the count *before* the call (a failed call adds no
/// plan); `thinking_suppressed_retry` marks a call made with thinking
/// suppressed — the synthesis retry (#774) or the re-ask (#945); a
/// `synth_turn: false` row with it set is the re-ask.
///
/// `completion` is the cost of the answer that failed to decode, when one
/// came back ([`AgentError::completion`](crate::scheduler::agent::AgentError::completion)).
/// It is written under the `plan.formulate` row's own keys (`llm_usage`,
/// `latency_ms`), so one query reads the cost of every planning call that got
/// a completion back. Unlike that row, the keys are absent (not null) when
/// none did: a router failure, timeouts included, records no latency.
/// With it go `raw_head`, the first [`FAILED_ROW_RAW_HEAD_CHARS`] chars of
/// what the model said, and `raw_chars`, the reply's whole length (#945).
pub(super) fn formulate_failed_payload(
    task_id: i64,
    plan_count: u32,
    synth_turn: bool,
    thinking_suppressed_retry: bool,
    request_timeout: bool,
    error: &str,
    completion: Option<(&kastellan_llm_router::CompletionStats, u64, &str)>,
) -> serde_json::Value {
    let mut v = serde_json::json!({
        "task_id": task_id,
        "plan_count": plan_count,
        "synth_turn": synth_turn,
        "thinking_suppressed_retry": thinking_suppressed_retry,
        "request_timeout": request_timeout,
        "error": error.chars().take(FAILED_ROW_ERROR_CHARS).collect::<String>(),
    });
    if let Some((usage, latency_ms, raw)) = completion {
        v["llm_usage"] = serde_json::to_value(usage)
            .expect("CompletionStats serialisation cannot fail (integers, a string, no maps)");
        v["latency_ms"] = serde_json::json!(latency_ms);
        v["raw_head"] = serde_json::json!(raw.chars().take(FAILED_ROW_RAW_HEAD_CHARS).collect::<String>());
        v["raw_chars"] = serde_json::json!(raw.chars().count());
    }
    v
}

/// Write the `agent/plan.formulate_failed` row for one failed formulator
/// call. **Best-effort**: a lost forensic row must not fail a task the
/// loop may still rescue — logged at ERROR instead, like the sink-block rows.
pub(crate) async fn record_failed_formulation(
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
        error.completion(),
    );
    if let Err(e) = kastellan_db::audit::insert(pool, "agent", ACTION_FORMULATE_FAILED, payload).await {
        tracing::error!(task_id, error = %e, "plan.formulate_failed audit insert failed");
    }
}

#[cfg(test)]
mod tests;
