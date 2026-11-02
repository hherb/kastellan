//! The notebook loop (#922): one step per turn, working memory in
//! `task_notebook`, every call reviewed and dispatched by the **same** code
//! as a classic plan ([`execute::review_and_dispatch`]).
//!
//! Per turn:
//!
//! 1. cancel check, then the turn cap and the fast lane's ceiling — as the
//!    classic loop does, including one forced-answer turn once something was
//!    gathered;
//! 2. read and fold the notebook; append the operator's corrections to the
//!    instruction the reviewer sees (D11);
//! 3. render the turn ([`render_turn`]) and ask the model, with the classic
//!    recoveries: an empty answer is asked again once, a reply that is not a
//!    usable notebook reply is shown back once (#945), a timeout after
//!    gathering spends the forced-answer turn;
//! 4. write the reply's `tick` and `steps` entries;
//! 5. turn the reply into a one-step plan (a `call`) or a terminal one (an
//!    `answer` or `infeasible`), and review and dispatch it;
//! 6. record a failed or blocked call as a `failed_step`, so the next turn
//!    replans (D4), and the end of the task as its terminal entry.
//!
//! Every turn counts against one cap, `max_plans × NOTEBOOK_TURNS_PER_PLAN`
//! (C5): the injection-bounding role of the classic plan cap is kept.

use std::sync::Arc;

use kastellan_db::notebook::{self as db, EntryKind};
use kastellan_llm_router::ThinkingPolicy;
use serde_json::json;
use sqlx::PgPool;

use super::formulator::NotebookFormulator;
use super::render::{render_turn, CurrentStep, TurnBudget};
use super::reply::{answer_ceiling, answer_plan, call_plan, parse_reply, require_answer, validate, NotebookReply, TickSpec};
use super::state::{fold, Correction, NotebookState};
use crate::cassandra::review::ChainReviewStage;
use crate::cassandra::types::DataClass;
use crate::scheduler::agent::{AgentError, FormulationMeta};
use crate::scheduler::inner_loop::execute::{self, ExecState, Executed};
use crate::scheduler::inner_loop::{
    ceiling, llm_failure, notebook_block_audit_payload, screen_model_text, turn_record_for, InnerLoopError,
    InnerLoopResult, Outcome, PlanRecord, StepDispatcher, StepOutcome, TaskContext, STEP_ERR_DETAIL_MAX,
};

/// Turns per classic plan in the cap (C5): a notebook step costs about one
/// turn, a classic plan often batched several.
pub(crate) const NOTEBOOK_TURNS_PER_PLAN: u32 = 2;

/// Replaces the turn counts on the single forced-answer turn.
pub(crate) const ANSWER_DIRECTIVE: &str = "Your turn budget is spent. Do NOT call another tool. \
Reply now with `answer`, written from the findings (and the current step's last result, if it has \
one); if they are incomplete, say what is missing. If nothing can be said, reply `infeasible` with \
the reason.";

/// The step the last dispatched call was for, its data's classification,
/// and whether it failed.
#[derive(Clone, Copy)]
struct LastCall {
    step: u32,
    classification: DataClass,
    failed: bool,
}

/// Run `ctx`'s task to an end on the notebook loop.
pub(crate) async fn run_notebook_to_terminal(
    pool: &PgPool,
    nb: &dyn NotebookFormulator,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    mut ctx: TaskContext,
    outbox: Option<&crate::channel::outbox::ChannelOutbox>,
) -> Result<InnerLoopResult, InnerLoopError> {
    use kastellan_db::tasks;

    ctx.max_plans = ctx.max_plans.saturating_mul(NOTEBOOK_TURNS_PER_PLAN);
    // A resumed run brings its plans back (C3); what they gathered counts.
    let mut st = ExecState {
        gathered: ctx.plans.iter().any(|r| r.outcomes().iter().any(|o| matches!(o, StepOutcome::Ok(_)))),
        ..ExecState::default()
    };
    let mut failed_llm_calls: u32 = 0;
    let mut force_answer = false;
    let mut answer_attempted = false;
    let mut seen_corrections: i64 = 0;
    let mut last_call: Option<LastCall> = None;
    let base_instruction = ctx.instruction.clone();

    macro_rules! finish {
        ($outcome:expr, $insight:expr, $skill:expr, $pyskill:expr) => {
            Ok(InnerLoopResult {
                outcome: $outcome,
                plan_count: ctx.plan_count,
                dispatch_count: st.dispatch_count,
                failed_llm_calls,
                terminal_l1_insight: $insight,
                terminal_l3_skill: $skill,
                terminal_python_skill: $pyskill,
                turn_record: turn_record_for(&ctx),
            })
        };
        ($outcome:expr) => {
            finish!($outcome, None, None, None)
        };
    }

    loop {
        if tasks::observe_state(pool, ctx.task_id).await? == "cancelled" {
            append_terminal(pool, ctx.task_id, EntryKind::Cancelled, json!({})).await;
            return finish!(Outcome::Cancelled);
        }
        let exhausted = ceiling::exhausted(ctx.plan_count, ctx.max_plans, ctx.deadline, std::time::Instant::now());
        let answer_turn = (exhausted.is_some() || force_answer) && st.gathered && !answer_attempted;
        if let (Some(which), false) = (exhausted, answer_turn) {
            return finish!(Outcome::Failed(ceiling::exhausted_detail(which, ctx.max_plans, false)));
        }
        if answer_turn {
            answer_attempted = true;
        }

        let state = match fold(&db::entries(pool, ctx.task_id, 0).await?) {
            Ok(s) => s,
            Err(e) => return finish!(Outcome::Failed(format!("the task notebook could not be read: {e}"))),
        };
        ctx.instruction = with_corrections(&base_instruction, &state.corrections);

        let turn = {
            let current = state.current_step();
            let record = ctx.plans.last();
            let last_failure = last_call.filter(|l| l.failed).and_then(|l| Some((l.step, record?)));
            let last = last_call.filter(|l| !l.failed && current.is_some_and(|c| c.id == l.step)).and(record);
            let tool_doc = current.and_then(|c| nb.tool_doc(&c.tool));
            let budget = TurnBudget {
                this_turn: ctx.plan_count.saturating_add(1),
                max_turns: ctx.max_plans,
                directive: answer_turn.then_some(ANSWER_DIRECTIVE),
            };
            let msg = |now: Option<&str>| {
                let cur = current.map(|step| CurrentStep { step, tool_doc: tool_doc.clone(), last });
                render_turn(&ctx, &state, seen_corrections, last_failure, cur, budget, now)
            };
            let guard = TurnGuard { gathered: st.gathered, answer_attempted, answer_turn };
            ask(pool, nb, &ctx, &state, &msg, guard, &mut failed_llm_calls).await
        };
        seen_corrections = state.corrections.last().map_or(seen_corrections, |c| c.seq);
        let (reply, meta) = match turn {
            Asked::Reply(b) => *b,
            Asked::ForceAnswer => {
                force_answer = true;
                continue;
            }
            Asked::Failed(detail) => return finish!(Outcome::Failed(detail)),
        };

        ctx.plan_count += 1;
        if let Err(e) = tasks::increment_plan_count(pool, ctx.task_id, ctx.plan_count as i32).await {
            tracing::warn!(task_id = ctx.task_id, error = %e, "tasks::increment_plan_count failed (mirror only)");
        }

        // The reply's own entries, in the order `validate` checked them.
        let mut after = state.clone();
        let tick_class = |step: u32| last_call.filter(|l| l.step == step).map_or(ctx.classification_floor, |l| l.classification);
        if let Some((tick, payload)) =
            reply.tick.as_ref().and_then(|t| Some((t, reply.tick_payload(tick_class(t.step))?)))
        {
            audit_screened_tick(pool, ctx.task_id, tick).await;
            let value = serde_json::to_value(&payload).expect("a TickPayload is plain JSON (strings, numbers, an enum)");
            db::append(pool, ctx.task_id, EntryKind::Tick, value).await?;
            // `validate` applied the same tick to a copy, so this cannot fail;
            // if it did, the notebook and the task would disagree.
            if let Err(e) = after.apply_tick(payload) {
                return finish!(Outcome::Failed(format!("the task notebook rejected its own tick: {e}")));
            }
        }
        if let Some(steps) = &reply.steps {
            db::append(pool, ctx.task_id, EntryKind::Steps, json!({"steps": steps})).await?;
            if let Err(e) = after.apply_steps(steps.clone()) {
                return finish!(Outcome::Failed(format!("the task notebook rejected its own steps: {e}")));
            }
        }

        let plan = match (&reply.call, &reply.answer, &reply.infeasible) {
            (Some(call), _, _) => call_plan(&reply, call, ctx.classification_floor),
            (None, Some(a), _) => {
                let ceiling = answer_ceiling(ctx.classification_floor, &touched(&after, &ctx.plans), a.data_ceiling);
                answer_plan(&a.body, ceiling, a.refused.clone())
            }
            (None, None, Some(i)) => {
                answer_plan(&i.reason, answer_ceiling(ctx.classification_floor, &touched(&after, &ctx.plans), None), None)
            }
            // `validate` refuses a reply with none of the three.
            (None, None, None) => return finish!(Outcome::Failed("notebook reply had no call or answer".into())),
        };

        match execute::review_and_dispatch(
            pool, review.as_ref(), dispatcher.as_ref(), &mut ctx, plan, &meta, answer_turn, exhausted, &mut st, outbox,
        )
        .await?
        {
            Executed::Finished(f) => {
                if let Some((kind, payload)) = terminal_entry(&f.outcome, &reply) {
                    append_terminal(pool, ctx.task_id, kind, payload).await;
                }
                return finish!(f.outcome, f.insight, f.skill, f.python_skill);
            }
            // The reviewer blocked the call (its reason is in `ctx.blocks`):
            // the step failed, and the next turn replans.
            Executed::Again => {
                if let Some(call) = &reply.call {
                    let detail = ctx.blocks.last().cloned().unwrap_or_default();
                    let payload = json!({"step": call.step, "code": "BLOCKED", "detail": clamp(&detail),
                                         "classification": ctx.classification_floor});
                    db::append(pool, ctx.task_id, EntryKind::FailedStep, payload).await?;
                    last_call = None;
                }
            }
            Executed::Dispatched => {
                let Some(call) = &reply.call else { continue };
                let prev = last_call.filter(|l| l.step == call.step && !l.failed).map(|l| l.classification);
                let classification = prev.map_or(call.classification, |p| p.max(call.classification));
                let failed = match ctx.plans.last().and_then(|r| r.outcomes().first()) {
                    Some(StepOutcome::Err { code, detail }) => {
                        let payload = json!({"step": call.step, "code": code, "detail": clamp(detail),
                                             "classification": classification});
                        db::append(pool, ctx.task_id, EntryKind::FailedStep, payload).await?;
                        true
                    }
                    _ => false,
                };
                last_call = Some(LastCall { step: call.step, classification, failed });
            }
        }
    }
}

/// What a notebook turn produced.
enum Asked {
    Reply(Box<(NotebookReply, FormulationMeta)>),
    /// The call timed out after gathering: spend the forced-answer turn next.
    ForceAnswer,
    Failed(String),
}

#[derive(Clone, Copy)]
struct TurnGuard {
    gathered: bool,
    answer_attempted: bool,
    answer_turn: bool,
}

/// Ask the model for one turn and check the reply, with the classic loop's
/// recoveries (`llm_failure::formulate_turn`): an empty answer is asked again
/// once; a reply that does not parse or breaks a rule is shown back once with
/// the reason (#945); every failed call is counted and audited once.
async fn ask(
    pool: &PgPool,
    nb: &dyn NotebookFormulator,
    ctx: &TaskContext,
    state: &NotebookState,
    msg: &super::formulator::UserMessage<'_>,
    guard: TurnGuard,
    failed_llm_calls: &mut u32,
) -> Asked {
    let check = |sent: Result<(String, FormulationMeta), AgentError>| {
        let (raw, meta) = sent?;
        let checked = parse_reply(&raw)
            .and_then(|r| validate(&r, state).map(|()| r))
            .and_then(|r| if guard.answer_turn { require_answer(&r).map(|()| r) } else { Ok(r) });
        match checked {
            Ok(reply) => Ok((reply, meta)),
            Err(detail) => Err(AgentError::Decode { detail, raw, usage: meta.usage, latency_ms: meta.latency_ms }),
        }
    };
    // The forced-answer turn writes from findings already extracted, and it
    // is the last chance: thinking off, so it cannot spend its whole budget
    // reasoning (DGX task 269 lost 408 s and its answer that way).
    let thinking = if guard.answer_turn { ThinkingPolicy::Suppress } else { ThinkingPolicy::RouterDefault };
    let first = check(nb.notebook_turn(ctx, msg, None, thinking).await);
    let mut retried = false;
    let mut reasked = false;
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
        Ok((reply, mut meta)) => {
            if retried {
                meta.retry_count = meta.retry_count.saturating_add(1);
            }
            Asked::Reply(Box::new((reply, meta)))
        }
        Err(e) => {
            record(pool, ctx, guard, failed_llm_calls, &e, reasked).await;
            let timed_out = e.is_request_timeout();
            // A blank reply cut off by `max_tokens` (#941) ran out of room the
            // way a timeout runs out of time: the same request fails the same
            // way, so after gathering it spends the forced-answer turn too.
            let out_of_room = timed_out || ran_out_of_tokens(&e);
            if llm_failure::should_force_synthesis(out_of_room, guard.gathered, guard.answer_attempted) {
                return Asked::ForceAnswer;
            }
            let detail = if timed_out {
                llm_failure::timeout_failure_detail(&llm_failure::GatheredWork::from_plans(&ctx.plans), &e.to_string())
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

/// A blank reply the backend cut off at `max_tokens` (`finish_reason:
/// length`): the model spent the whole budget thinking.
fn ran_out_of_tokens(e: &AgentError) -> bool {
    matches!(e, AgentError::Decode { raw, usage, .. }
        if raw.trim().is_empty() && usage.finish_reason.as_deref() == Some("length"))
}

/// Count one failed model call and write its `plan.formulate_failed` row.
async fn record(pool: &PgPool, ctx: &TaskContext, guard: TurnGuard, failed: &mut u32, e: &AgentError, suppressed: bool) {
    *failed = failed.saturating_add(1);
    llm_failure::record_failed_formulation(pool, ctx.task_id, ctx.plan_count, guard.answer_turn, suppressed, e).await;
}

/// The instruction the reviewer sees: the task's own, then every operator
/// correction so far (D11), so a correction can widen the task like a new
/// instruction would.
fn with_corrections(base: &str, corrections: &[Correction]) -> String {
    corrections.iter().fold(base.to_string(), |s, c| format!("{s}\n\nOperator correction: {}", c.text))
}

/// Every class of data the task touched (C1): each ticked or failed step's,
/// and every dispatched call's — a further call for one step, or a step later
/// replanned away, included.
fn touched(state: &NotebookState, plans: &[PlanRecord]) -> Vec<DataClass> {
    let dispatched = plans.iter().flat_map(|r| r.plan.steps.iter().take(r.outcomes().len()).map(|s| s.classification));
    state.touched.iter().copied().chain(dispatched).collect()
}

/// The entry that records how the task ended, for the outcomes that end it
/// with an answer or a cancel. A suspended, blocked or failed task writes
/// none: an `awaiting_operator` task resumes and needs its notebook open.
fn terminal_entry(outcome: &Outcome, reply: &NotebookReply) -> Option<(EntryKind, serde_json::Value)> {
    match outcome {
        Outcome::Completed(_) | Outcome::Refused { .. } => match (&reply.answer, &reply.infeasible) {
            (Some(a), _) => Some((EntryKind::Answer, json!({"body": a.body}))),
            (None, Some(i)) => Some((EntryKind::Infeasible, json!({"reason": i.reason}))),
            (None, None) => None,
        },
        Outcome::Cancelled => Some((EntryKind::Cancelled, json!({}))),
        _ => None,
    }
}

/// A terminal entry is best-effort: losing it must not lose the answer the
/// task already has.
async fn append_terminal(pool: &PgPool, task_id: i64, kind: EntryKind, payload: serde_json::Value) {
    if let Err(e) = db::append(pool, task_id, kind, payload).await {
        tracing::error!(task_id, kind = %kind, error = %e, "task_notebook terminal entry not written");
    }
}

/// Write a `policy / injection.blocked` row for each part of a tick the
/// planner-text screen blocks, once, when the tick lands — the render
/// withholds it on every turn after, and must not audit it every turn.
/// Best-effort, like the sink rows.
async fn audit_screened_tick(pool: &PgPool, task_id: i64, tick: &TickSpec) {
    let parts = [("note", tick.note.as_str()), ("summary", tick.summary.as_str())]
        .into_iter()
        .chain(tick.findings.iter().flat_map(|f| [("finding", f.text.as_str()), ("finding", f.source.as_str())]));
    for (part, text) in parts {
        if let Some(block) = screen_model_text(text) {
            let payload = notebook_block_audit_payload(task_id, tick.step, part, &block);
            if let Err(e) = kastellan_db::audit::insert(pool, "policy", "injection.blocked", payload).await {
                tracing::error!(task_id, error = %e, "notebook injection.blocked audit insert failed");
            }
        }
    }
}

/// A failed step's detail, as long as the plan summary shows one.
fn clamp(detail: &str) -> String {
    detail.chars().take(STEP_ERR_DETAIL_MAX).collect()
}

#[cfg(test)]
mod tests;
