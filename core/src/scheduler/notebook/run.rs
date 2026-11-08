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
//!    usable notebook reply is shown back once (#945), a timeout or a reply
//!    cut off at `max_tokens` after gathering spends the forced-answer turn;
//!    and an answer with no table after a sift that kept rows is shown back
//!    once too (#960), and kept if that re-ask fails — except on the
//!    forced-answer turn, where code attaches the newest sift's table instead;
//! 4. write the reply's `tick` and `steps` entries;
//! 5. set aside, unreviewed, an answer a non-thinking turn wrote (the next
//!    turn rewrites it, thinking) and a call this task already made
//!    successfully ([`repeat`]) — each with a `plan.discarded` row;
//! 6. otherwise turn the reply into a one-step plan (a `call`) or a terminal
//!    one (an `answer` or `infeasible`), and review and dispatch it — except
//!    a `sift` call, which is run here ([`sift::run`]: its reads are the one
//!    reviewed plan), and an answer's `table`, rendered into the body before
//!    review;
//! 7. record a failed or blocked call as a `failed_step`, so the next turn
//!    replans (D4), and the end of the task as its terminal entry.
//!
//! Every turn counts against one cap, `max_plans × NOTEBOOK_TURNS_PER_PLAN`
//! (C5; a resumed run: the turns it carries plus that many more): the
//! injection-bounding role of the classic plan cap is kept.

use std::sync::Arc;

use kastellan_db::notebook::{self as db, EntryKind};
use serde_json::json;
use sqlx::PgPool;

use super::ask::{ask, Asked, TurnGuard};
use super::audit::{audit_discarded_turn, audit_screened_steps, audit_screened_tick};
use super::formulator::NotebookFormulator;
use super::render::{render_turn, CurrentStep, TurnBudget};
use super::reply::{
    answer_ceiling, answer_plan, call_plan, NotebookReply,
};
use super::state::{
    entry, fold, AnswerPayload, Correction, FailedStepPayload, InfeasiblePayload, NotebookState, StepsPayload,
};
use super::repeat;
use super::sift::{self, run::SiftRun, SIFT_TOOL};
use super::thinking::{self, TurnKind};
use crate::cassandra::review::ChainReviewStage;
use crate::cassandra::types::{DataClass, Plan};
use crate::scheduler::inner_loop::execute::{self, ExecState, Executed};
use crate::scheduler::inner_loop::{
    ceiling, turn_record_for, InnerLoopError,
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

    ctx.max_plans = turn_cap(ctx.plan_count, ctx.max_plans);
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
    // A failed, blocked or repeated call: the next turn replans, and thinks.
    let mut replan_due = false;
    // A non-thinking turn chose to answer: the next turn rewrites it, thinking.
    let mut write_answer = false;
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

        let kind = thinking::turn_kind(answer_turn, write_answer, state.steps.is_empty(), replan_due);
        // The turn's `now`, kept for a sift's sorter ("currently due").
        let turn_now = std::sync::Mutex::new(None::<String>);
        let turn = {
            let current = state.current_step();
            let record = ctx.plans.last();
            let last_failure = last_call.filter(|l| l.failed).and_then(|l| Some((l.step, record?)));
            let last = last_call.filter(|l| !l.failed && current.is_some_and(|c| c.id == l.step)).and(record);
            let tool_doc = current.and_then(|c| nb.tool_doc(&c.tool));
            let budget = TurnBudget {
                this_turn: ctx.plan_count.saturating_add(1),
                max_turns: ctx.max_plans,
                directive: match kind {
                    TurnKind::ForcedAnswer => Some(ANSWER_DIRECTIVE),
                    TurnKind::WriteAnswer => Some(thinking::WRITE_ANSWER_DIRECTIVE),
                    _ => None,
                },
            };
            let msg = |now: Option<&str>| {
                *turn_now.lock().expect("never poisoned: nothing panics holding it") = now.map(String::from);
                let cur = current.map(|step| CurrentStep { step, tool_doc: tool_doc.clone(), last });
                render_turn(&ctx, &state, seen_corrections, last_failure, cur, budget, now)
            };
            let guard = TurnGuard { gathered: st.gathered, answer_attempted, answer_turn, kind };
            ask(pool, nb, &ctx, &state, &msg, guard, &mut failed_llm_calls).await
        };
        seen_corrections = state.corrections.last().map_or(seen_corrections, |c| c.seq);
        write_answer = false;
        if kind == TurnKind::Replan {
            replan_due = false;
        }
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
            db::append(pool, ctx.task_id, EntryKind::Tick, entry(&payload)).await?;
            // `validate` applied the same tick to a copy, so this cannot fail;
            // if it did, the notebook and the task would disagree.
            if let Err(e) = after.apply_tick(payload) {
                return finish!(Outcome::Failed(format!("the task notebook rejected its own tick: {e}")));
            }
        }
        if let Some(steps) = &reply.steps {
            audit_screened_steps(pool, ctx.task_id, &state, steps).await;
            db::append(pool, ctx.task_id, EntryKind::Steps, entry(StepsPayload { steps: steps.clone() })).await?;
            if let Err(e) = after.apply_steps(steps.clone()) {
                return finish!(Outcome::Failed(format!("the task notebook rejected its own steps: {e}")));
            }
        }

        // The answer of a turn that did not think is not used: the findings it
        // ticked are kept, and the next turn writes the answer, thinking.
        if reply.answer.is_some() && thinking::rewrite_answer(kind, nb.thinks_by_default()) {
            audit_discarded_turn(pool, &ctx, "answer_rewritten", None, &meta).await;
            write_answer = true;
            continue;
        }

        let plan = match (&reply.call, &reply.answer, &reply.infeasible) {
            (Some(call), _, _) => {
                let plan = call_plan(&reply, call, ctx.classification_floor);
                // The same call again returns the same result: not run, and
                // the next turn replans, thinking (DGX task 291). Compared as
                // it would be dispatched, so a namespace the model left out
                // does not hide a repeat; the qualified name it then shows is
                // an advertised method, never the model's own text.
                let repeated = plan.steps.first().cloned().map(|mut s| {
                    if let Some(m) = dispatcher.qualify_method(&s.tool, &s.method) {
                        s.method = m;
                    }
                    s
                });
                let held = |h: &crate::handoff::HandoffRef| dispatcher.stashed(ctx.task_id, h).is_some();
                if let Some(s) = repeated.filter(|s| repeat::already_made(&ctx.plans, s, held)) {
                    tracing::info!(task_id = ctx.task_id, method = %s.method, "notebook: repeated call not dispatched");
                    audit_discarded_turn(pool, &ctx, "repeated_call", Some(&s.method), &meta).await;
                    ctx.blocks.push(repeat::repeated_call_reason(&s.method));
                    replan_due = true;
                    continue;
                }
                plan
            }
            (None, Some(a), _) => {
                // A table is rendered by code and reviewed with the body; its
                // rows' class joins the ceiling (C1, K7).
                let mut touched = touched(&after, &ctx.plans);
                // The forced answer is never shown back for a missing table
                // (its budget is spent): code attaches it (#960).
                let forced = (kind == TurnKind::ForcedAnswer).then(|| sift::answer::forced_table(&reply, &ctx.plans)).flatten();
                if let Some(t) = &forced {
                    tracing::info!(task_id = ctx.task_id, scrapbook = ?t.scrapbook.refs(), "notebook: forced answer names no table; attached by code");
                }
                // `forced` is only ever set when the answer's table names no refs.
                let body = match forced.as_ref().or(a.table.as_ref()) {
                    Some(t) => {
                        let (body, class) = sift::answer::with_table(pool, &ctx, &a.body, t, forced.is_some()).await;
                        touched.push(class);
                        body
                    }
                    None => a.body.clone(),
                };
                let plan = answer_plan(&body, answer_ceiling(ctx.classification_floor, &touched, a.data_ceiling), a.refused.clone());
                Plan { l1_insight: a.l1_insight.clone(), ..plan }
            }
            (None, None, Some(i)) => {
                answer_plan(&i.reason, answer_ceiling(ctx.classification_floor, &touched(&after, &ctx.plans), None), None)
            }
            // `validate` refuses a reply with none of the three.
            (None, None, None) => return finish!(Outcome::Failed("notebook reply had no call or answer".into())),
        };

        let executed = match reply.call.as_ref().filter(|c| c.tool == SIFT_TOOL) {
            // A sift is run here, not dispatched: its reads are one reviewed
            // plan, and its sorter needs the router (K2).
            Some(call) => {
                let now = turn_now.lock().expect("never poisoned").clone();
                let r = SiftRun { pool, review: review.as_ref(), dispatcher: dispatcher.as_ref(), nb, outbox, now: now.as_deref() };
                sift::run::run_sift(r, &mut ctx, call, &meta, &mut st, &mut failed_llm_calls).await?
            }
            None => {
                execute::review_and_dispatch(
                    pool, review.as_ref(), dispatcher.as_ref(), &mut ctx, plan, &meta, answer_turn, exhausted, &mut st,
                    outbox, execute::StepPolicy::Plan,
                )
                .await?
            }
        };
        match executed {
            Executed::Finished(f) => {
                if let Some((kind, payload)) = terminal_entry(&f.outcome, &reply) {
                    append_terminal(pool, ctx.task_id, kind, payload).await;
                }
                return finish!(f.outcome, f.insight, f.skill, f.python_skill);
            }
            // The reviewer blocked the call, or a sift was refused (the
            // reason is in `ctx.blocks`): the step failed, and the next turn
            // replans.
            Executed::Again => {
                if let Some(call) = &reply.call {
                    let detail = ctx.blocks.last().cloned().unwrap_or_default();
                    let payload = FailedStepPayload {
                        step: call.step,
                        code: "BLOCKED".into(),
                        detail: clamp(&detail),
                        classification: ctx.classification_floor,
                    };
                    db::append(pool, ctx.task_id, EntryKind::FailedStep, entry(payload)).await?;
                    last_call = None;
                    replan_due = true;
                }
            }
            Executed::Dispatched => {
                let Some(call) = &reply.call else { continue };
                let prev = last_call.filter(|l| l.step == call.step && !l.failed).map(|l| l.classification);
                let classification = prev.map_or(call.classification, |p| p.max(call.classification));
                let failed = match ctx.plans.last().and_then(|r| r.outcomes().first()) {
                    Some(StepOutcome::Err { code, detail }) => {
                        let payload =
                            FailedStepPayload { step: call.step, code: code.clone(), detail: clamp(detail), classification };
                        db::append(pool, ctx.task_id, EntryKind::FailedStep, entry(payload)).await?;
                        replan_due = true;
                        true
                    }
                    _ => false,
                };
                last_call = Some(LastCall { step: call.step, classification, failed });
            }
        }
    }
}

/// The turn cap (C5). A resumed run's `max_plans` is already the turns it
/// carries plus a fresh allowance in plans (`runner::task_exec::resume_budget`):
/// only the allowance is multiplied, or each approved escalation would grow
/// the cap by the turns already spent. Pure.
pub(crate) fn turn_cap(spent: u32, max_plans: u32) -> u32 {
    spent.saturating_add(max_plans.saturating_sub(spent).saturating_mul(NOTEBOOK_TURNS_PER_PLAN))
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
            (Some(a), _) => Some((EntryKind::Answer, entry(AnswerPayload { body: a.body.clone() }))),
            (None, Some(i)) => Some((EntryKind::Infeasible, entry(InfeasiblePayload { reason: i.reason.clone() }))),
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

/// A failed step's detail, as long as the plan summary shows one.
fn clamp(detail: &str) -> String {
    detail.chars().take(STEP_ERR_DETAIL_MAX).collect()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod refusal_tests;

#[cfg(test)]
mod agent_raised_tests;
