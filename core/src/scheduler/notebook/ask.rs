//! One notebook turn's model call, checked, with its recoveries ([`ask`]).

use kastellan_llm_router::ThinkingPolicy;
use sqlx::PgPool;

use super::formulator::NotebookFormulator;
use super::reply::{keep_called_step, parse_reply, require_answer, validate, NotebookReply};
use super::sift;
use super::state::NotebookState;
use super::thinking::{self, TurnKind};
use crate::scheduler::agent::{AgentError, FormulationMeta};
use crate::scheduler::inner_loop::{llm_failure, TaskContext};

/// What a notebook turn produced.
pub(super) enum Asked {
    Reply(Box<(NotebookReply, FormulationMeta)>),
    /// The call timed out, or was cut off at `max_tokens`, after gathering:
    /// spend the forced-answer turn next.
    ForceAnswer,
    Failed(String),
}

#[derive(Clone, Copy)]
pub(super) struct TurnGuard {
    pub gathered: bool,
    pub answer_attempted: bool,
    pub answer_turn: bool,
    pub kind: TurnKind,
}

/// Ask the model for one turn and check the reply, with the classic loop's
/// recoveries (`llm_failure::formulate_turn`): an empty answer is asked again
/// once; a reply that does not parse or breaks a rule is shown back once with
/// the reason (#945); an answer with no table after a sift that kept rows is
/// shown back once (#960) and kept if that fails; every failed call is
/// counted and audited once.
pub(super) async fn ask(
    pool: &PgPool,
    nb: &dyn NotebookFormulator,
    ctx: &TaskContext,
    state: &NotebookState,
    msg: &super::formulator::UserMessage<'_>,
    guard: TurnGuard,
    failed_llm_calls: &mut u32,
) -> Asked {
    let wants_table = thinking::shows_back_a_missing_table(guard.kind, nb.thinks_by_default());
    let check = |sent: Result<(String, FormulationMeta), AgentError>| {
        let (raw, meta) = sent?;
        let answer_only = matches!(guard.kind, TurnKind::ForcedAnswer | TurnKind::WriteAnswer);
        let checked = parse_reply(&raw)
            .map(|mut r| {
                // The rewrite turn only writes: its findings were ticked on the
                // turn before, so a repeated tick or step list is dropped.
                if guard.kind == TurnKind::WriteAnswer {
                    r.tick = None;
                    r.steps = None;
                }
                keep_called_step(&mut r, state);
                r
            })
            .and_then(|r| validate(&r, state).map(|()| r))
            .and_then(|r| if answer_only { require_answer(&r).map(|()| r) } else { Ok(r) });
        match checked {
            Ok(reply) => Ok((reply, meta, raw)),
            Err(detail) => Err(AgentError::Decode { detail, raw, usage: meta.usage, latency_ms: meta.latency_ms }),
        }
    };
    // Which turns think: `thinking::turn_thinking`. The forced-answer turn
    // never does (DGX task 269 lost 408 s and its answer that way), nor does
    // the #945 re-ask below.
    let thinking = thinking::turn_thinking(guard.kind);
    let mut first = check(nb.notebook_turn(ctx, msg, None, thinking).await);
    let mut retried = false;
    let mut reasked = false;
    // #960: a first reply that answers with no table after a sift that kept
    // rows is shown back once. It passed every check, so it is no failed call,
    // and a re-ask that fails keeps it: a missing table never fails a turn.
    if let (true, Ok((reply, _, raw))) = (wants_table, &first) {
        if let Err(reason) = sift::answer::table_left_out(reply, &ctx.plans) {
            tracing::info!(task_id = ctx.task_id, %reason, "notebook answer names no table; shown back once");
            match check(nb.notebook_turn(ctx, msg, Some((raw, &reason)), ThinkingPolicy::Suppress).await) {
                Ok(again) => {
                    first = Ok(again);
                    retried = true;
                }
                Err(e) => {
                    record(pool, ctx, guard, failed_llm_calls, &e, true).await;
                    tracing::warn!(task_id = ctx.task_id, error = %e, "the table re-ask failed; the first answer is kept");
                }
            }
        }
    }
    let result = match first {
        Err(e) if e.is_empty_completion() => {
            record(pool, ctx, guard, failed_llm_calls, &e, false).await;
            tracing::warn!(task_id = ctx.task_id, "notebook turn returned an empty completion; asking again once");
            retried = true;
            check(nb.notebook_turn(ctx, msg, None, thinking).await)
        }
        Err(e) if e.non_plan_answer().is_some() => {
            record(pool, ctx, guard, failed_llm_calls, &e, false).await;
            let reply = e.non_plan_answer().unwrap_or_default().to_string();
            let reason = match &e {
                AgentError::Decode { detail, .. } => detail.clone(),
                other => other.to_string(),
            };
            tracing::warn!(task_id = ctx.task_id, %reason, "notebook reply could not be used; re-asked once");
            retried = true;
            reasked = true;
            check(nb.notebook_turn(ctx, msg, Some((&reply, &reason)), ThinkingPolicy::Suppress).await)
        }
        other => other,
    };
    match result {
        Ok((reply, mut meta, _)) => {
            if retried {
                meta.retry_count = meta.retry_count.saturating_add(1);
            }
            Asked::Reply(Box::new((reply, meta)))
        }
        Err(e) => {
            record(pool, ctx, guard, failed_llm_calls, &e, reasked).await;
            let timed_out = e.is_request_timeout();
            // A reply cut off by `max_tokens` (#941) ran out of room the way a
            // timeout runs out of time: the same request fails the same way,
            // so after gathering it spends the forced-answer turn too.
            let out_of_room = timed_out || e.ran_out_of_tokens();
            if llm_failure::should_force_synthesis(out_of_room, guard.gathered, guard.answer_attempted) {
                return Asked::ForceAnswer;
            }
            let detail = if timed_out {
                llm_failure::timeout_failure_detail(&llm_failure::GatheredWork::from_plans(&ctx.plans), &e.to_string())
            } else if e.ran_out_of_tokens() {
                llm_failure::token_cap_failure_detail(&e.to_string())
            } else if let AgentError::Decode { raw, .. } = &e {
                llm_failure::decode_failure_detail(raw.trim().is_empty(), &e.to_string())
            } else {
                format!("llm: {e}")
            };
            Asked::Failed(if reasked && e.non_plan_answer().is_none() {
                format!("The language model's first reply could not be used, so it was asked again. {detail}")
            } else {
                detail
            })
        }
    }
}

/// Count one failed model call and write its `plan.formulate_failed` row.
async fn record(pool: &PgPool, ctx: &TaskContext, guard: TurnGuard, failed: &mut u32, e: &AgentError, suppressed: bool) {
    *failed = failed.saturating_add(1);
    llm_failure::record_failed_formulation(pool, ctx.task_id, ctx.plan_count, guard.answer_turn, suppressed, e).await;
}
