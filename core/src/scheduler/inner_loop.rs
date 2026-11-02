//! Per-task iterative replanning loop.
//!
//! Called by the lane runner once a task is claimed. Owns the
//! per-task `Workspace` and the `TaskContext` that accumulates state
//! across plan iterations.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

use crate::cassandra::data_ceiling::{resolve_data_ceiling, DataCeilingSource};
use crate::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use crate::cassandra::types::{DataClass, PlannedStep, Verdict};
use crate::scheduler::audit::{
    build_l3_invoke_outcome_payload, ACTION_L3_INVOKE_OUTCOME, SCHEDULER_AUDIT_ACTOR,
};

use self::floor::apply_floor_raise;
pub use self::floor::ClassificationFloorSource;
use self::invoke_expand::{expand_invoke_skill, InvokeExpansion};
use self::summary::render_plans_summary;
// `pub use` rather than a private import: `TaskContext::plans` is a public
// field of this type, and the suspend/restore path
// (`scheduler::asks::resume_state_from` / `restore_resume_state`) names it
// directly. Without a public path it would be reachable only through the
// leaked-type loophole.
pub use self::summary::PlanRecord;
// Re-exported only so the `#[cfg(test)] mod tests` below can reach these
// `summary`-owned bounds via `use super::*`; no non-test code in this module
// references them, hence the `cfg(test)` gate (else they read as unused).
#[cfg(test)]
pub(crate) use self::summary::STEP_OK_SUMMARY_MAX;
// The notebook loop (#922) renders model text and step results through the
// summary's own screens.
pub(crate) use self::summary::{
    notebook_block_audit_payload, screen_model_text, sorter_view, STEP_ERR_DETAIL_MAX, WITHHELD_MARKER,
};
use super::agent::{AgentError, PlanFormulator};
use super::asks;

/// Hard cap on tool dispatches a single plan iteration may request. Sized far
/// above any legitimate plan (the planner prompt asks for a handful of steps)
/// and well below the point where retained outcomes (up to 256 KiB each for a
/// fetch) become a memory problem for the daemon.
pub const MAX_STEPS_PER_PLAN: usize = 64;
use super::inner_loop_audit::{
    write_audit_plan_formulate, write_audit_plan_outcome, write_audit_verdict,
};

pub(crate) mod ceiling;
pub(crate) mod execute;
mod floor;
mod invoke_expand;
// `pub(crate)` so the notebook loop (#922) recovers from a failed model
// call with the same decisions and the same audit row.
pub(crate) mod llm_failure;
// `pub(crate)` so `scheduler::conversation` can reuse the same pruning,
// clamping and screen-text extraction for the conversation block (#701).
// A second copy of that logic is exactly the drift #669 warns about.
pub(crate) mod result_view;
mod summary;

/// Per-task accumulator state passed to the agent each iteration.
#[derive(Debug)]
pub struct TaskContext {
    pub task_id: i64,
    pub lane: kastellan_db::tasks::Lane,
    pub instruction: String,
    pub classification_floor: DataClass,
    /// Provenance of `classification_floor`. Set at task entry by
    /// `runner::run_inner_loop_for_task`; mutated to `AgentRaised` on
    /// successful agent floor-raise (see `apply_floor_raise`).
    pub classification_floor_source: ClassificationFloorSource,
    /// Matched signal tags from CLI keyword inference. Non-empty iff
    /// `classification_floor_source == CliInferred`. Cleared on agent
    /// raise (the tags explained the original CLI inference, not the
    /// elevated floor).
    pub classification_floor_signals: Vec<String>,
    pub plans: Vec<PlanRecord>,
    pub advisories: Vec<String>,
    pub blocks: Vec<String>,
    pub plan_count: u32,
    pub max_plans: u32,
    /// When this run stops gathering, whatever `max_plans` says (#922): the
    /// fast lane's 15-minute ceiling, set from the claim instant by
    /// `runner::task_exec::run_one`. `None` = no wall-clock limit (the long
    /// lane). Checked between plans — see [`ceiling`].
    pub deadline: Option<std::time::Instant>,
    /// Every operator ask this task has already had answered, newest first.
    /// Empty for a task nobody has been asked about.
    ///
    /// Read **once** by `runner::task_exec::run_one` before the first
    /// formulation and threaded in here, so the `Escalate` arm compares
    /// digests against in-memory values rather than issuing a second query
    /// from inside the loop — and so a test can construct the decisions
    /// without a live Postgres (spec D4).
    ///
    /// **A `Vec`, not an `Option`, because approvals bind to plan digests
    /// and a task can hold several at once.** One live approval was enough
    /// only for a task that escalates once. A task that escalates at two
    /// different plans needs both: with only the newest kept, the earlier
    /// plan re-asks a question the operator already answered, approving that
    /// makes the other one stale, and the pair alternates until the ask
    /// deadline. See `db::asks::resolved_for_task`.
    ///
    /// Never holds a denial in practice: `run_one` terminates a denied task
    /// before building this context. `asks::decide` still handles that case
    /// correctly rather than assuming it away.
    pub resolved_asks: Vec<kastellan_db::asks::Ask>,
    /// Where this task came from, if it came from a channel — the routing
    /// an escalation's question is delivered to (#564 slice 2, spec D13).
    ///
    /// Computed once in `runner::task_exec::run_one` from the payload it
    /// already holds, rather than re-read from the DB on the escalation
    /// path. `None` for a `kastellan-cli ask` or scheduled task, whose ask
    /// is answered through `kastellan-cli inbox`.
    pub origin: Option<crate::channel::ask_message::AskDestination>,
    /// The earlier turns of this task's conversation, rendered, screened and
    /// budgeted (#701). Empty for a task that is not channel-originated, that
    /// has no earlier turns, or whose history could not be read.
    ///
    /// Held as rendered JSON rather than as `Turn`s because it is loaded and
    /// screened **once** per run, in `runner::task_exec::run_one`, and read on
    /// every planner iteration — the same memoize-at-the-append-point reasoning
    /// as `PlanRecord::rendered` (#344).
    pub conversation: Vec<serde_json::Value>,
    /// The ids of the turns behind `conversation`, for the `plan.formulate`
    /// row. `Some(vec![])` means there were none; **`None` means the read
    /// failed** — absence and loss must not render identically.
    pub conversation_task_ids: Option<Vec<i64>>,
}

impl TaskContext {
    /// Compact summary of completed plans, for inclusion in the agent's
    /// input. Avoids dumping unbounded `serde_json::Value` blobs into the
    /// prompt; gives just enough for the agent to reflect — including each
    /// failed step's `code` + clamped `detail`. Rendering, screening, and the
    /// global size budget all live in [`summary::render_plans_summary`].
    pub fn plans_so_far_summary(&self) -> Vec<serde_json::Value> {
        render_plans_summary(&self.plans)
    }
}

/// The `tasks.turn_record` value for a finished task, or `None` when the task
/// did not come from a channel (#701).
///
/// Only a channel task has a conversation for a later turn to read, so only a
/// channel task stores a record: for a CLI or scheduled task it would be data
/// nothing can ever look up.
///
/// Serialisation of a `TurnRecord` cannot fail (no non-string keys, no NaN); a
/// failure would mean no record rather than a failed task, because the record
/// is a convenience for the *next* turn and never a reason to lose this one.
pub(crate) fn turn_record_for(ctx: &TaskContext) -> Option<serde_json::Value> {
    ctx.origin.as_ref()?;
    let record = crate::scheduler::conversation::record::from_plans(
        &ctx.plans,
        ctx.classification_floor,
    );
    serde_json::to_value(record).ok()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StepOutcome {
    Ok(serde_json::Value),
    Err { code: String, detail: String },
}

impl StepOutcome {
    pub fn is_err(&self) -> bool { matches!(self, StepOutcome::Err { .. }) }
}

/// Bundle returned by [`run_to_terminal`] so the lane runner can
/// build the spec §7 `task.finalize` summary row without re-querying.
///
/// `plan_count` is the final value of `TaskContext::plan_count` (one
/// increment per formulator call that produced a plan);
/// `failed_llm_calls` counts the calls that produced none (#774), so the
/// finalize payload's `total_llm_calls` is their sum. `dispatch_count` is
/// incremented once per `StepDispatcher::dispatch_step` call —
/// regardless of whether the call returned `Ok` or `Err` — so the
/// audit row reflects how often the host actually tried to dispatch
/// a step, not how often it succeeded.
#[derive(Clone, Debug)]
pub struct InnerLoopResult {
    pub outcome: Outcome,
    pub plan_count: u32,
    pub dispatch_count: u32,
    /// Formulator calls in this run that produced no plan — a failed
    /// attempt the loop retried or recovered from, or the final failure
    /// (#774, #923).
    pub failed_llm_calls: u32,
    /// `l1_insight` from the terminal plan, captured only when the
    /// inner loop reaches `Outcome::Completed`. The lane runner reads
    /// this in `drain_lane` and writes one `actor='scheduler'
    /// action='l1.promoted'` audit row if `Some`. `None` on every
    /// other outcome (Failed / Cancelled — Refused / Blocked are
    /// also not Outcome::Completed).
    pub terminal_l1_insight: Option<String>,
    /// `l3_skill` from the terminal plan, captured only when the inner
    /// loop reaches `Outcome::Completed` AND the task executed >= 1 tool
    /// step (`dispatch_count >= 1`). The lane runner reads this in
    /// `drain_lane` and writes one `actor='scheduler'
    /// action='l3.crystallised'` audit row if `Some`. `None` otherwise.
    pub terminal_l3_skill: Option<crate::cassandra::types::L3SkillCandidate>,
    /// `python_skill` from the terminal plan, captured only when the inner
    /// loop reaches `Outcome::Completed` AND `dispatch_count >= 1` (the same
    /// grounding gate as `terminal_l3_skill`). The lane runner writes one
    /// `action='l3.crystallised'` (`kind: "python"`) audit row if `Some`.
    pub terminal_python_skill: Option<crate::cassandra::types::PythonSkillCandidate>,
    /// The conversational-continuity record for a channel task (#701), for the
    /// lane runner to hand to `tasks::finalize`. `None` for every other task
    /// kind, and for the pre-loop failure paths, which ran no plans.
    pub turn_record: Option<serde_json::Value>,
}

/// Terminal result of the inner loop. The lane runner translates
/// these into `tasks.state` + `tasks.result` via `db::tasks::finalize`.
#[derive(Clone, Debug)]
pub enum Outcome {
    Completed(serde_json::Value),
    Failed(String),
    Cancelled,
    TimedOut,
    Blocked { principle: u8, reason: String },
    /// Agent self-declared a constitutional refusal. Sourced from
    /// `plan.refused` in the inner loop. Distinct from `Blocked`
    /// (which is the reviewer-detected `Verdict::ConstitutionalBlock`
    /// path). `body` carries the planner's prose `result.body` so the
    /// user-facing explanation is preserved in the audit + DB result.
    Refused { principle: u8, reason: String, body: String },
    /// **Non-terminal.** The reviewer escalated and an operator ask was
    /// raised; `db::asks::raise` has already moved the task to
    /// `awaiting_operator` inside its own transaction, so the lane runner
    /// must NOT finalize. Resolution re-enqueues the task through the
    /// `tasks_resumed` NOTIFY and it runs again from the top.
    AwaitingOperator { ask_id: i64 },
    /// An operator answered a raised ask with `deny`. Terminal.
    ///
    /// `reason` is the ask's own `body` — the reviewer's escalation
    /// concern, i.e. the question the operator was answering. The
    /// operator's optional free-text note deliberately does NOT travel
    /// here; it lives in `asks.resolution` and the `ask.resolved` audit
    /// row (spec D10).
    Denied { ask_id: i64, reason: String },
}

impl Outcome {
    /// The `tasks.state` this outcome finalizes to, or `None` when the
    /// task has not finished.
    ///
    /// **An `Option` rather than a pseudo-terminal string, and that is
    /// load-bearing.** `AwaitingOperator` is the first non-terminal
    /// outcome the loop can return. Answering `"awaiting_operator"` here
    /// would send it to `tasks::finalize`, which matches
    /// `WHERE state = 'running'` — the row is already `awaiting_operator`,
    /// so the UPDATE silently no-ops — and would then write a terminal
    /// lifecycle row and a `task.finalize` row asserting an end that did
    /// not happen. `Option` makes every call site say what it does with an
    /// unfinished task, and the compiler finds them all.
    pub fn final_state(&self) -> Option<&'static str> {
        match self {
            Outcome::Completed(_) => Some("completed"),
            Outcome::Failed(_)    => Some("failed"),
            Outcome::Cancelled    => Some("cancelled"),
            Outcome::TimedOut     => Some("timed_out"),
            Outcome::Blocked { .. } => Some("blocked"),
            Outcome::Refused { .. } => Some("refused"),
            // `blocked` is shared with the reviewer-detected path on
            // purpose: a third terminal state would need a migration to
            // widen `tasks_state_check` and would partition every existing
            // observation query grouping on terminal states. The
            // `ask.resolved` audit row's `choice` is what separates the two
            // populations.
            Outcome::Denied { .. } => Some("blocked"),
            Outcome::AwaitingOperator { .. } => None,
        }
    }

    pub fn result_payload(&self) -> Option<serde_json::Value> {
        match self {
            Outcome::Completed(v) => Some(v.clone()),
            Outcome::Failed(s)    => Some(serde_json::json!({"kind": "error", "detail": s})),
            Outcome::Blocked { principle, reason } =>
                Some(serde_json::json!({"kind": "blocked", "principle": principle, "reason": reason})),
            Outcome::Refused { principle, reason, body } => Some(serde_json::json!({
                "kind": "refused",
                "principle": principle,
                "reason": reason,
                "body": body,
            })),
            Outcome::Denied { ask_id, reason } => Some(serde_json::json!({
                "kind": "denied",
                "ask_id": ask_id,
                "reason": reason,
            })),
            _ => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum InnerLoopError {
    #[error("agent: {0}")]
    Agent(#[from] AgentError),
    #[error("db: {0}")]
    Db(#[from] kastellan_db::DbError),
}

/// Trait for executing a single `PlannedStep`. The production impl
/// is a thin wrapper around `tool_host::dispatch`; the test impl
/// returns scripted `StepOutcome`s.
#[async_trait::async_trait]
pub trait StepDispatcher: Send + Sync {
    async fn dispatch_step(&self, task_id: i64, step: &PlannedStep) -> StepOutcome;

    /// Live tool-name set this dispatcher can reach. Used by the agent
    /// L3-invoke path to re-validate a skill against the registry as it is
    /// *now* (the TOCTOU close). Default: empty — only the production
    /// [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`] holds a
    /// registry; non-loop / test doubles that never expand an invoke can
    /// keep the empty default.
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    /// The method to dispatch in place of `method`, or `None` to leave it
    /// unchanged — namespace completion against `tool`'s advertised methods
    /// (see `tool_dispatch::method_qualify`).
    ///
    /// On the trait rather than only inside the dispatcher because the plan is
    /// normalised *once*, up here, where `plan.steps` is still owned: the
    /// method is read by more than the dispatch call (the per-step output cap
    /// in `summary::ok_summary_cap`, the plan digest, the audit payloads), and
    /// qualifying only at the dispatch site left every one of those reading a
    /// name that was never on the wire. Default: `None` — only the production
    /// dispatcher holds a registry.
    fn qualify_method(&self, _tool: &str, _method: &str) -> Option<String> {
        None
    }

    /// Drop any per-task state this dispatcher holds (e.g. the handoff
    /// cache) once the task reaches a terminal state. Default no-op; the
    /// production dispatcher overrides it. Called once per task by the lane
    /// runner after [`run_to_terminal`].
    fn purge_task(&self, _task_id: i64) {}

    /// Register a per-task workspace `out/` dir so `dispatch_step` can bind it
    /// into opt-in workers (durable file output). Called once per task by the
    /// lane runner before dispatching, when a `Workspace` was constructed.
    /// Default no-op; only the production dispatcher holds workspace state.
    fn set_task_out_dir(&self, _task_id: i64, _out_dir: std::path::PathBuf) {}
}

/// Rewrite each step's `method` in place to the namespace-completed form the
/// dispatcher will actually put on the wire.
///
/// Extracted from the loop body so the substitution has a test that does not
/// need Postgres: the pure completion rule and the registry lookup each had
/// one, and the line joining them to the plan did not — which is how a live
/// `-32601 unknown method` regression could survive a green suite.
///
/// Idempotent: an already-qualified method is never rewritten, so running this
/// and the dispatcher's own chokepoint check over the same plan is a no-op the
/// second time.
fn qualify_plan_methods(dispatcher: &dyn StepDispatcher, task_id: i64, steps: &mut [PlannedStep]) {
    for step in steps {
        if let Some(qualified) = dispatcher.qualify_method(&step.tool, &step.method) {
            tracing::info!(
                task_id, tool = %step.tool,
                requested = %step.method, dispatched = %qualified,
                "inner_loop: completed an omitted method namespace"
            );
            step.method = qualified;
        }
    }
}

/// Run the inner loop until terminal. Returns an [`InnerLoopResult`]
/// carrying the terminal [`Outcome`] plus the per-task counters the
/// lane runner needs for the spec §7 `task.finalize` audit row.
///
/// `outbox` is where a raised ask is delivered (#564 slice 2, spec D13);
/// `None` on a daemon built or configured without channels, in which case
/// every escalation is undelivered but still durable — see
/// `asks::deliver_ask`.
pub async fn run_to_terminal(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    mut ctx: TaskContext,
    outbox: Option<&crate::channel::outbox::ChannelOutbox>,
) -> Result<InnerLoopResult, InnerLoopError> {
    use kastellan_db::tasks;

    // `dispatch_count`, `invoke_used` and `gathered`: the counters the
    // review-and-dispatch half bumps; see `execute::ExecState`.
    let mut st = execute::ExecState::default();

    // Formulator calls that produced NO plan (#774, #923): a failed attempt
    // the loop retried or recovered from, or the final failure. `plan_count` counts only
    // the calls that produced a plan, so `task.finalize`'s `total_llm_calls`
    // is `plan_count + failed_llm_calls`.
    let mut failed_llm_calls: u32 = 0;

    /// Local helper: wrap an `Outcome` with the counters captured so
    /// far. Cuts the boilerplate at every early-return point.
    /// `$insight` is the `terminal_l1_insight` value and `$skill` is
    /// the `terminal_l3_skill` value — both `None` for all
    /// non-Completed outcomes; the Completed arm passes
    /// `captured_l1_insight` and `captured_l3_skill`.
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
                // Every `InnerLoopResult` is built here, so the record is
                // built at each one from the plans accumulated so far —
                // including the outcomes that are not `Completed`, whose calls
                // are just as real a referent for the next turn.
                //
                // NOT every *exit*: the loop also has `?` propagations (a
                // Postgres blip on `observe_state`, on an audit insert), which
                // return past this macro and become `failed_result`, whose
                // `turn_record` is `None` even though `ctx.plans` was
                // populated. That loss is #710.
                turn_record: turn_record_for(&ctx),
            })
        };
        // 3-arg form (existing call sites): python skill None.
        ($outcome:expr, $insight:expr, $skill:expr) => {
            finish!($outcome, $insight, $skill, None)
        };
        // Convenience form for all non-Completed arms: all None.
        ($outcome:expr) => {
            finish!($outcome, None, None, None)
        };
    }

    // Set true once the single forced-synthesis turn has been spent, so we
    // never loop back into it (belt-and-suspenders — a synth turn always
    // returns a terminal outcome anyway).
    let mut synth_attempted = false;
    // Set true when a planning call timed out after something was gathered
    // (#774): the next iteration spends the forced-synthesis turn instead of
    // the task failing with everything it gathered thrown away.
    let mut force_synth = false;

    loop {
        // Cancellation poll — top of loop.
        if tasks::observe_state(pool, ctx.task_id).await? == "cancelled" {
            return finish!(Outcome::Cancelled);
        }

        // Plan-iteration cap. When the agent has already gathered at least
        // one successful observation, spend ONE final "forced-synthesis"
        // turn — instruct the model to stop gathering and answer from what
        // it has — before failing. This converts the common
        // kept-searching-never-answered cap-hit (e.g. an open-ended "what
        // happened today?" news query, where a deterministic local planner
        // keeps chasing fresher results) into a best-effort answer instead
        // of a bare `plan_iteration_cap_exceeded` error. With nothing
        // gathered (every step denied / errored / blocked-before-execution)
        // there is nothing to synthesize, so the cap fails hard as before —
        // which is why the existing cap tests are unaffected. The fast lane's
        // wall-clock ceiling (#922) is handled exactly like the cap.
        let exhausted = ceiling::exhausted(
            ctx.plan_count, ctx.max_plans, ctx.deadline, std::time::Instant::now(),
        );
        let synth_turn = (exhausted.is_some() || force_synth) && st.gathered && !synth_attempted;
        if let (Some(which), false) = (exhausted, synth_turn) {
            return finish!(Outcome::Failed(ceiling::exhausted_detail(which, ctx.max_plans, false)));
        }
        if synth_turn {
            synth_attempted = true;
        }

        // 1. Formulate plan (forced-synthesis variant on the synth turn).
        //
        // No general loop-level retry: replanning IS the retry shape (the
        // agent sees the prior failure on the next iteration, bounded by
        // `max_plans`). The two exceptions are a REQUEST TIMEOUT (#774), the
        // failure a cheaper request can beat, and an EMPTY completion
        // (#923), asked again once — `llm_failure::formulate_turn` owns that
        // recovery, and the counting/audit of every failed call.
        let turn = llm_failure::TurnState { synth_turn, gathered: st.gathered, synth_attempted };
        let (plan, meta) = match llm_failure::formulate_turn(
            pool, formulator.as_ref(), &ctx, turn, &mut failed_llm_calls,
        ).await {
            llm_failure::Turn::Planned(planned) => *planned,
            llm_failure::Turn::ForceSynthesis => {
                force_synth = true;
                continue;
            }
            llm_failure::Turn::Failed(detail) => return finish!(Outcome::Failed(detail)),
        };

        ctx.plan_count += 1;
        // Best-effort mirror — the in-memory `ctx.plan_count` is the
        // source of truth, the DB column is for operator visibility
        // (`tasks status`). A real DB error here doesn't change loop
        // behaviour but is worth surfacing in the daemon log.
        if let Err(e) = tasks::increment_plan_count(pool, ctx.task_id, ctx.plan_count as i32).await {
            tracing::warn!(
                task_id = ctx.task_id, plan_count = ctx.plan_count, error = %e,
                "tasks::increment_plan_count failed (mirror only; loop continues)"
            );
        }

        match execute::review_and_dispatch(
            pool, review.as_ref(), dispatcher.as_ref(), &mut ctx, plan, &meta, synth_turn, exhausted, &mut st, outbox,
            execute::StepPolicy::Plan,
        )
        .await?
        {
            execute::Executed::Finished(f) => return finish!(f.outcome, f.insight, f.skill, f.python_skill),
            execute::Executed::Again | execute::Executed::Dispatched => {}
        }
        // loop back: agent reflects on the outcomes for the next plan
    }
}

// `pub(crate)` so other scheduler tests can reuse `inner_loop_test_stubs`.
#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod timeout_tests;
