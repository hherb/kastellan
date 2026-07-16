//! Per-task iterative replanning loop.
//!
//! Called by the lane runner once a task is claimed. Owns the
//! per-task `Workspace` and the `TaskContext` that accumulates state
//! across plan iterations.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use thiserror::Error;

use crate::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use crate::cassandra::types::{DataClass, PlannedStep, Verdict};
use crate::scheduler::audit::{
    build_l3_invoke_outcome_payload, ACTION_L3_INVOKE_OUTCOME, SCHEDULER_AUDIT_ACTOR,
};

use self::floor::apply_floor_raise;
pub use self::floor::ClassificationFloorSource;
use self::invoke_expand::{expand_invoke_skill, InvokeExpansion};
use self::summary::{render_plans_summary, PlanRecord};
// Re-exported only so the `#[cfg(test)] mod tests` below can reach these
// `summary`-owned bounds via `use super::*`; no non-test code in this module
// references them, hence the `cfg(test)` gate (else they read as unused).
#[cfg(test)]
pub(crate) use self::summary::{STEP_ERR_DETAIL_MAX, STEP_OK_SUMMARY_MAX};
use super::agent::{AgentError, PlanFormulator};
use super::inner_loop_audit::{
    write_audit_plan_formulate, write_audit_plan_outcome, write_audit_verdict,
};

mod floor;
mod invoke_expand;
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
}

impl Outcome {
    pub fn final_state(&self) -> &'static str {
        match self {
            Outcome::Completed(_) => "completed",
            Outcome::Failed(_)    => "failed",
            Outcome::Cancelled    => "cancelled",
            Outcome::TimedOut     => "timed_out",
            Outcome::Blocked { .. } => "blocked",
            Outcome::Refused { .. } => "refused",
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

    /// Drop any per-task state this dispatcher holds (e.g. the handoff
    /// cache) once the task reaches a terminal state. Default no-op; the
    /// production dispatcher overrides it. Called once per task by the lane
    /// runner after [`run_to_terminal`].
    fn purge_task(&self, _task_id: i64) {}
}

/// Run the inner loop until terminal. Returns an [`InnerLoopResult`]
/// carrying the terminal [`Outcome`] plus the per-task counters the
/// lane runner needs for the spec §7 `task.finalize` audit row.
pub async fn run_to_terminal(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    mut ctx: TaskContext,
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

        write_audit_plan_formulate(pool, &ctx, &plan, &meta).await?;

        // 1b. L3 autonomous invoke expansion (before review, so the
        // reviewer governs the concrete steps). Presence of `invoke_skill`
        // triggers this branch; a malformed directive or a refused gate is
        // audited + fed back as a block so the agent replans — never a
        // silent fall-through to dispatching co-supplied steps. `plan` is
        // `mut`; we resolve the directive to OWNED data first so the borrow
        // from `validate_invoke` ends before we assign `plan.steps`.
        let mut current_invoke: Option<(i64, String)> = None;
        if plan.invoke_skill.is_some() {
            match expand_invoke_skill(pool, dispatcher.as_ref(), &plan).await? {
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
                // No channel bus in this scope — for non-refusal plans,
                // treat as Block so the agent gets a chance to revise.
                // The audit row above already records `verdict_kind=escalate`,
                // but the runtime degradation (escalate → block) is invisible
                // to anyone not reading the audit log; a warn keeps it
                // grep-able in the daemon journal.
                //
                // TODO(channel-bus): when the channel-bus lands, route
                //   the Escalate verdict to the operator channel and
                //   await a verdict from there. The site to update is
                //   this match arm. See HANDOVER §"channel bus".
                if plan.refused.is_none() {
                    tracing::warn!(
                        task_id = ctx.task_id,
                        plan_count = ctx.plan_count,
                        severity = ?sev,
                        reason = %reason,
                        "Verdict::Escalate degraded to Block (channel-bus not wired)"
                    );
                    ctx.blocks.push(format!("escalate(no-channel): {reason}"));
                    continue;
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
