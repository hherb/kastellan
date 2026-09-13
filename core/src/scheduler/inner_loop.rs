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
pub(crate) use self::summary::{STEP_ERR_DETAIL_MAX, STEP_OK_SUMMARY_MAX};
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

mod floor;
mod invoke_expand;
mod result_view;
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
/// increment per formulator call) and is the natural value for the
/// finalize payload's `total_llm_calls` field. `dispatch_count` is
/// incremented once per `StepDispatcher::dispatch_step` call —
/// regardless of whether the call returned `Ok` or `Err` — so the
/// audit row reflects how often the host actually tried to dispatch
/// a step, not how often it succeeded.
#[derive(Clone, Debug)]
pub struct InnerLoopResult {
    pub outcome: Outcome,
    pub plan_count: u32,
    pub dispatch_count: u32,
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

    // Tracks every `StepDispatcher::dispatch_step` call this task makes
    // (success or failure). Reported back in `InnerLoopResult` for the
    // spec §7 `task.finalize` summary row.
    let mut dispatch_count: u32 = 0;

    // Set true once any iteration expands an `invoke_skill` directive.
    // ANDed into the terminal `l3_skill` capture so an invoke-driven task
    // never re-crystallises the skill it just ran (forecloses a
    // crystallise → pin → invoke → re-crystallise cycle).
    let mut invoke_used = false;

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
                dispatch_count,
                terminal_l1_insight: $insight,
                terminal_l3_skill: $skill,
                terminal_python_skill: $pyskill,
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

    // Set true once the agent gathers ≥1 successful tool observation. Gates
    // the forced-synthesis fallback below: with nothing gathered there is
    // nothing to synthesize, so the cap fails hard (unchanged behaviour).
    let mut gathered = false;
    // Set true once the single forced-synthesis turn has been spent, so we
    // never loop back into it (belt-and-suspenders — a synth turn always
    // returns a terminal outcome anyway).
    let mut synth_attempted = false;

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
        // which is why the existing cap tests are unaffected.
        let over_cap = ctx.plan_count >= ctx.max_plans;
        let synth_turn = over_cap && gathered && !synth_attempted;
        if over_cap && !synth_turn {
            return finish!(Outcome::Failed(format!(
                "plan_iteration_cap_exceeded ({}>={})", ctx.plan_count, ctx.max_plans
            )));
        }
        if synth_turn {
            synth_attempted = true;
        }

        // 1. Formulate plan (forced-synthesis variant on the synth turn).
        //
        // No loop-level retry: replanning IS the retry shape (the agent
        // sees the prior failure on the next iteration, bounded by
        // `max_plans`). A transient HTTP/transport error that escapes
        // the formulator's own retry is therefore terminal here.
        let formulation = if synth_turn {
            formulator.formulate_synthesis(&ctx).await
        } else {
            formulator.formulate_plan(&ctx).await
        };
        let (mut plan, meta) = match formulation {
            Ok(x) => x,
            Err(e) => return finish!(Outcome::Failed(format!("llm: {e}"))),
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

        // Agent-side floor-raise: if the plan requests a higher floor than
        // the producer set, elevate ctx BEFORE the audit row is written
        // (so the row reflects the elevated floor + AgentRaised source)
        // and BEFORE the reviewer chain runs (so DP sees the new floor
        // for I1 + I2 checks).
        if apply_floor_raise(&mut ctx, &plan) {
            tracing::info!(
                task_id = ctx.task_id,
                plan_count = ctx.plan_count,
                new_floor = ctx.classification_floor.as_pascal_str(),
                "agent raised classification floor"
            );
        }

        // Resolve an absent `data_ceiling` against the task floor (#506).
        //
        // Position is load-bearing on all three sides:
        //   * AFTER `apply_floor_raise`, so an agent-raised floor is the one
        //     resolved against — resolving first would pin the ceiling to the
        //     producer's lower floor and let I1 pass on a plan the agent itself
        //     said needed a higher one.
        //   * BEFORE the audit write, so the `plan.formulate` row records the
        //     value policy actually enforced, together with its provenance.
        //   * BEFORE L3 expansion and review, which are the two readers of the
        //     resolved value (expansion stamps it onto generated steps; the
        //     deterministic screen enforces I1/I3 with it).
        //
        // Written back into `plan` so there is exactly ONE resolved value in
        // play. Recomputing it per reader is how a numerator and a denominator
        // end up disagreeing with no way to tell which is wrong.
        let resolved = resolve_data_ceiling(plan.data_ceiling, ctx.classification_floor);
        plan.data_ceiling = Some(resolved.ceiling);
        if resolved.source == DataCeilingSource::FloorResolved {
            tracing::warn!(
                task_id = ctx.task_id,
                plan_count = ctx.plan_count,
                resolved_to = resolved.ceiling.as_pascal_str(),
                "plan omitted `data_ceiling`; resolved to the task classification floor. \
                 The model should emit the field explicitly."
            );
        }

        write_audit_plan_formulate(pool, &ctx, &plan, &meta, resolved.source).await?;

        // 1b. L3 autonomous invoke expansion (before review, so the
        // reviewer governs the concrete steps). Presence of `invoke_skill`
        // triggers this branch; a malformed directive or a refused gate is
        // audited + fed back as a block so the agent replans — never a
        // silent fall-through to dispatching co-supplied steps. `plan` is
        // `mut`; we resolve the directive to OWNED data first so the borrow
        // from `validate_invoke` ends before we assign `plan.steps`.
        let mut current_invoke: Option<(i64, String)> = None;
        if plan.invoke_skill.is_some() {
            match expand_invoke_skill(pool, dispatcher.as_ref(), &plan, resolved.ceiling).await? {
                InvokeExpansion::Refused(reasons) => {
                    for r in &reasons {
                        ctx.blocks.push(format!("invoke_rejected: {r}"));
                    }
                    continue; // bounded by plan_count cap on next iter
                }
                InvokeExpansion::Expanded { steps, memory_id, name } => {
                    plan.steps = steps;
                    invoke_used = true;
                    current_invoke = Some((memory_id, name));
                }
            }
        }

        // 1c. Complete an omitted method namespace (`get_attachment_text` →
        // `mail.get_attachment_text`) on the plan itself, once.
        //
        // Placed after invoke expansion so expanded steps are covered too, and
        // before review so CASSANDRA, the plan digest, the per-step output cap
        // and both audit payloads all see the method that will actually be put
        // on the wire. The planner's own wording is not lost: `plan.formulate`
        // was already written above, from the plan as it arrived.
        //
        // The dispatcher re-checks at the chokepoint, which is a no-op once a
        // name is qualified (an exactly-advertised method is never rewritten) —
        // kept because the chokepoint, not this loop, is what every dispatch
        // path goes through.
        qualify_plan_methods(dispatcher.as_ref(), ctx.task_id, &mut plan.steps);

        // 2. CASSANDRA review
        let rctx = ReviewStageContext {
            task_id: ctx.task_id,
            instruction: &ctx.instruction,
            classification_floor: ctx.classification_floor,
            plan_count: ctx.plan_count,
        };
        let verdict_start = std::time::Instant::now();
        let verdict = review.review(&plan, &rctx).await;
        write_audit_verdict(pool, &ctx, &verdict, verdict_start.elapsed().as_millis() as u64).await?;

        // Precedence (issue #23 spec §2):
        //   Verdict CB                       → Outcome::Blocked   (reviewer wins)
        //   Escalate, no CB, no refusal      → Outcome::AwaitingOperator (#564 slice 1b),
        //                                      unless the operator already approved
        //                                      this exact plan, which proceeds.
        //                                      With a refusal present the row below
        //                                      wins — Escalate does NOT suspend a
        //                                      refusal plan (see the arm's else).
        //   plan.refused.is_some(), no CB    → Outcome::Refused   (agent's refusal stands)
        //   plan terminal, neither           → Outcome::Completed
        //   non-terminal                     → execute steps
        match &verdict {
            Verdict::ConstitutionalBlock { principle, reason } =>
                return finish!(Outcome::Blocked { principle: *principle, reason: reason.clone() }),
            Verdict::Block(reason) => {
                // When the agent self-refused, Block does not loop back —
                // the refusal is already terminal. Fall through to the
                // if-let-Some check below. For normal (non-refusal) plans,
                // continue so the agent can revise.
                if plan.refused.is_none() {
                    ctx.blocks.push(reason.clone());
                    continue;  // bounded by plan_count cap on next iter
                }
            }
            Verdict::Escalate(reason, sev) => {
                // Same rationale as Block: a refusal plan must not loop.
                //
                // For a non-refusal plan the reviewer said a human must
                // decide, so this raises an operator ask and suspends the
                // task — unless the operator already approved *this exact
                // plan*, in which case it proceeds.
                if plan.refused.is_none() {
                    // Digest the plan as it stands — after the floor raise,
                    // the `data_ceiling` resolution, invoke expansion and
                    // namespace completion — because that is what would
                    // execute, and the replan runs the same normalisations
                    // so the two digests are comparable.
                    let digest = crate::cassandra::plan_digest::plan_digest(&plan);
                    // ANY resolved ask may carry the approval, not just the
                    // newest: a task that escalated at an earlier plan and
                    // then at a later one holds two approvals, and this plan
                    // is covered by whichever one names its digest.
                    let approved = ctx.resolved_asks.iter().find(|a| {
                        matches!(asks::decide(a, &digest), asks::AskDecision::Approved)
                    });
                    if let Some(approval) = approved {
                        tracing::info!(
                            task_id = ctx.task_id,
                            ask_id = approval.id,
                            plan_count = ctx.plan_count,
                            severity = ?sev,
                            "Verdict::Escalate covered by a resolved operator approval for this \
                             exact plan; proceeding"
                        );
                        // The audit trail otherwise reads
                        // `cassandra.verdict{kind=escalate}` → step dispatch
                        // with nothing in between, and the digest of the plan
                        // that RAN appears only in the much earlier
                        // `ask.raised` row. This row is what lets a reader
                        // show that the plan which executed is the plan that
                        // was approved — the single property the digest
                        // binding exists to provide.
                        asks::emit_approval_applied(
                            pool, approval.id, ctx.task_id, &digest,
                        ).await;
                        // Falls through to the refusal check below and then
                        // to the terminal check / step execution, exactly as
                        // an `Approve` verdict does.
                    } else {
                        // The run's history travels with the suspension
                        // (spec D11). Without it the resumed task rebuilds
                        // an EMPTY context and re-formulates every
                        // iteration it already ran — re-executing their
                        // steps, so an escalation the operator APPROVED
                        // would send the same email twice.
                        let resume_state = asks::resume_state_from(
                            &ctx.plans, &ctx.advisories, &ctx.blocks,
                        );
                        match asks::raise_and_suspend(
                            pool, ctx.task_id, &plan, reason, *sev, Some(&resume_state),
                            outbox, ctx.origin.as_ref(),
                        )
                        .await
                        {
                            Ok(ask_id) => {
                                tracing::info!(
                                    task_id = ctx.task_id,
                                    ask_id,
                                    plan_count = ctx.plan_count,
                                    severity = ?sev,
                                    reason = %reason,
                                    "Verdict::Escalate raised an operator ask; task suspended"
                                );
                                return finish!(Outcome::AwaitingOperator { ask_id });
                            }
                            // Fail the task; do NOT fall back to Block.
                            // Degrading silently is the behaviour this slice
                            // deletes, and doing it on the one path where the
                            // reviewer said a human must decide is the worst
                            // place to keep it. If the ask row really was
                            // cancelled underneath us, `finalize` is a no-op
                            // for it anyway.
                            //
                            // COVERAGE, stated plainly so nobody assumes more
                            // than there is: the *precondition* that makes this
                            // arm fire is pinned by
                            // `scheduler_asks_e2e::raising_against_a_task_that_is_not_running_is_an_error`
                            // — `raise_and_suspend` really does return `Err`
                            // rather than orphaning an ask. **This arm itself is
                            // not exercised end-to-end**, because reaching it
                            // through the lane runner needs the task to stop
                            // being `running` between the claim and the review,
                            // which no test stages. So a regression that swapped
                            // these two lines back to `ctx.blocks.push(...);
                            // continue` — the exact degrade #564 slice 1b deletes
                            // — would go green. Keep the `Failed` return.
                            Err(e) => {
                                tracing::error!(
                                    task_id = ctx.task_id,
                                    error = %e,
                                    "Verdict::Escalate could not raise an operator ask"
                                );
                                return finish!(Outcome::Failed(format!(
                                    "escalation could not be raised: {e}"
                                )));
                            }
                        }
                    }
                } else {
                    // Escalate on a refusal plan: refusal stands and no
                    // degradation happens (the loop terminates). Surface
                    // a journal line so operators grepping for Escalate
                    // events don't silently miss this case.
                    tracing::info!(
                        task_id = ctx.task_id,
                        plan_count = ctx.plan_count,
                        severity = ?sev,
                        reason = %reason,
                        "Verdict::Escalate on refusal plan — refusal stands, no degradation"
                    );
                }
            }
            Verdict::Advisory(c) => {
                // Only record advisory when the plan is not a refusal;
                // no point accumulating advisories we are about to discard.
                if plan.refused.is_none() {
                    ctx.advisories.push(c.clone());
                }
                // proceed in both cases — falls through to the refusal check
            }
            Verdict::Approve => { /* proceed */ }
        }

        // Agent self-declared a constitutional refusal. Reviewer's non-CB
        // verdict (Approve / Advisory / Block / Escalate) does NOT override —
        // refusal is terminal. The verdict row is already audit-logged above.
        // Steps (if any) are dropped: execution is unsafe under a self-declared
        // violation, and looping would spin until the plan cap (wrong).
        if let Some(refused) = plan.refused.clone() {
            let body = plan.result.as_ref()
                .and_then(|v| v.get("body"))
                .and_then(|b| b.as_str())
                .map(String::from)
                .unwrap_or_default();
            return finish!(Outcome::Refused {
                principle: refused.principle,
                reason: refused.reason,
                body,
            });
        }

        // 3. Terminal check
        if plan.is_terminal() {
            let result = plan.result.clone()
                .unwrap_or_else(|| serde_json::json!({"kind": "text", "body": ""}));
            // Capture the agent-raised l1_insight on the EXACT iteration where
            // Outcome::Completed will fire. We use plan.completion_insight()
            // which encapsulates the gate (is_terminal && l1_insight.is_some()).
            let captured_l1_insight: Option<String> = plan.completion_insight().map(|s| s.to_string());
            // Grounding gate: only crystallise a skill if the task
            // actually executed >= 1 tool step (dispatch_count is the
            // running per-task counter). A pure-text-answer task
            // (terminal on plan 1, zero dispatches) emits no skill.
            // Also never crystallise off the forced-synthesis turn: that
            // answer is a best-effort wrap-up produced under the plan cap,
            // not a demonstrated-good procedure, so it must not seed a
            // reusable skill even if the model volunteers one.
            let captured_l3_skill: Option<crate::cassandra::types::L3SkillCandidate> =
                if dispatch_count >= 1 && !invoke_used && !synth_turn {
                    plan.completion_skill().cloned()
                } else {
                    None
                };
            let captured_python_skill: Option<crate::cassandra::types::PythonSkillCandidate> =
                if dispatch_count >= 1 && !invoke_used && !synth_turn {
                    plan.completion_python_skill().cloned()
                } else {
                    None
                };
            return finish!(
                Outcome::Completed(result),
                captured_l1_insight,
                captured_l3_skill,
                captured_python_skill
            );
        }

        // Forced-synthesis turn: the model was told to answer now. A
        // terminal plan already returned `Outcome::Completed` above (and a
        // self-refusal returned `Outcome::Refused`). If it STILL returned a
        // non-terminal plan, do NOT execute more tool steps — fail at the
        // cap rather than spending another gather round.
        if synth_turn {
            // Report the cap the same way as the primary cap message above
            // (`max_plans>=max_plans`). `ctx.plan_count` is now `max_plans + 1`
            // — the synthesis turn spent one extra formulation — so printing it
            // here read as an off-by-one (`6>=5`) against the cap of 5.
            return finish!(Outcome::Failed(format!(
                "plan_iteration_cap_exceeded ({}>={}); forced synthesis did not produce a final answer",
                ctx.max_plans, ctx.max_plans
            )));
        }

        // 4. Execute steps
        // Per-plan step budget (security audit 2026-09-02, F5): nothing bounded
        // how many tool dispatches ONE plan could carry — only the number of
        // plan iterations (`max_plans`) — so a planner steered by injected
        // content could enqueue hundreds of fetches/sends in a single turn,
        // each retained in the plan record. A plan over the cap is refused
        // whole, like an iteration-cap overrun, rather than truncated (a
        // truncated plan would execute a prefix the planner never reviewed).
        if plan.steps.len() > MAX_STEPS_PER_PLAN {
            return finish!(Outcome::Failed(format!(
                "plan_step_cap_exceeded ({} > {}); refusing to execute a plan with more than \
                 {} steps in one iteration",
                plan.steps.len(),
                MAX_STEPS_PER_PLAN,
                MAX_STEPS_PER_PLAN
            )));
        }

        let mut outcomes: Vec<StepOutcome> = Vec::with_capacity(plan.steps.len());
        for step in &plan.steps {
            if tasks::observe_state(pool, ctx.task_id).await? == "cancelled" {
                return finish!(Outcome::Cancelled);
            }
            let outcome = dispatcher.dispatch_step(ctx.task_id, step).await;
            dispatch_count = dispatch_count.saturating_add(1);
            let is_err = outcome.is_err();
            outcomes.push(outcome);
            if is_err { break; }
        }

        let steps_total = plan.steps.len();
        let steps_executed = outcomes.len();
        let any_err = outcomes.iter().any(|o| o.is_err());
        // Arm the forced-synthesis fallback once any step actually succeeds:
        // there is now a real observation to synthesize an answer from.
        if outcomes.iter().any(|o| matches!(o, StepOutcome::Ok(_))) {
            gathered = true;
        }
        write_audit_plan_outcome(
            pool, &ctx, steps_executed, steps_total, any_err,
        ).await?;

        if let Some((memory_id, skill_name)) = &current_invoke {
            let payload = build_l3_invoke_outcome_payload(
                *memory_id, skill_name, steps_executed, steps_total, any_err,
            );
            kastellan_db::audit::insert(
                pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_INVOKE_OUTCOME, payload,
            ).await?;
        }

        ctx.plans.push(PlanRecord::new(plan, outcomes));
        // loop back: agent reflects on the outcomes for the next plan
    }
}

#[cfg(test)]
mod tests;
