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
use crate::cassandra::types::{DataClass, Plan, PlannedStep, Verdict};

use super::agent::{AgentError, PlanFormulator};
use super::inner_loop_audit::{
    write_audit_plan_formulate, write_audit_plan_outcome, write_audit_verdict,
};

/// Provenance of the current `classification_floor` value.
///
/// Carried in [`TaskContext`] and emitted into the
/// `agent/plan.formulate` audit-row payload so operators can trace
/// any DP-blocked plan back to how the floor was set.
///
/// Wire form (lowercase snake_case via serde) matches the
/// operator-visible audit-log token — renaming any branch is an
/// audit-trail contract break. Mirrors the `as_pascal_str` shape on
/// `DataClass`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationFloorSource {
    /// Operator explicitly passed `--classification-floor X`.
    Operator,
    /// CLI keyword classifier elevated above Public.
    CliInferred,
    /// Agent raised the floor mid-task via `Plan.floor_request`.
    AgentRaised,
    /// No inference matched and no operator flag was set.
    Default,
}

impl ClassificationFloorSource {
    /// Canonical lowercase snake_case string, identical to the serde wire
    /// form. Used by audit-log payload emitters so the rendered tag is a
    /// formal contract instead of relying on the de-facto stability of
    /// `Debug`. Renaming any branch is an audit-trail contract break.
    pub fn as_snake_str(self) -> &'static str {
        match self {
            ClassificationFloorSource::Operator    => "operator",
            ClassificationFloorSource::CliInferred => "cli_inferred",
            ClassificationFloorSource::AgentRaised => "agent_raised",
            ClassificationFloorSource::Default     => "default",
        }
    }
}

/// Per-task accumulator state passed to the agent each iteration.
#[derive(Debug)]
pub struct TaskContext {
    pub task_id: i64,
    pub lane: hhagent_db::tasks::Lane,
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
    pub plans: Vec<(Plan, Vec<StepOutcome>)>,
    pub advisories: Vec<String>,
    pub blocks: Vec<String>,
    pub plan_count: u32,
    pub max_plans: u32,
}

impl TaskContext {
    /// Compact summary of completed plans, for inclusion in the
    /// agent's input. Avoids dumping unbounded `serde_json::Value`
    /// blobs into the prompt; gives just enough for the agent to
    /// reflect.
    pub fn plans_so_far_summary(&self) -> Vec<serde_json::Value> {
        self.plans.iter().map(|(p, outcomes)| {
            serde_json::json!({
                "decision":      p.decision,
                "step_outcomes": outcomes.iter().map(|o| match o {
                    StepOutcome::Ok(_) => "ok",
                    StepOutcome::Err { .. } => "err",
                }).collect::<Vec<_>>(),
            })
        }).collect()
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
    Db(#[from] hhagent_db::DbError),
}

/// Trait for executing a single `PlannedStep`. The production impl
/// is a thin wrapper around `tool_host::dispatch`; the test impl
/// returns scripted `StepOutcome`s.
#[async_trait::async_trait]
pub trait StepDispatcher: Send + Sync {
    async fn dispatch_step(&self, step: &PlannedStep) -> StepOutcome;

    /// Live tool-name set this dispatcher can reach. Used by the agent
    /// L3-invoke path to re-validate a skill against the registry as it is
    /// *now* (the TOCTOU close). Default: empty — only the production
    /// [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`] holds a
    /// registry; non-loop / test doubles that never expand an invoke can
    /// keep the empty default.
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }
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
    use hhagent_db::tasks;

    // Tracks every `StepDispatcher::dispatch_step` call this task makes
    // (success or failure). Reported back in `InnerLoopResult` for the
    // spec §7 `task.finalize` summary row.
    let mut dispatch_count: u32 = 0;

    /// Local helper: wrap an `Outcome` with the counters captured so
    /// far. Cuts the boilerplate at every early-return point.
    /// `$insight` is the `terminal_l1_insight` value and `$skill` is
    /// the `terminal_l3_skill` value — both `None` for all
    /// non-Completed outcomes; the Completed arm passes
    /// `captured_l1_insight` and `captured_l3_skill`.
    macro_rules! finish {
        ($outcome:expr, $insight:expr, $skill:expr) => {
            Ok(InnerLoopResult {
                outcome: $outcome,
                plan_count: ctx.plan_count,
                dispatch_count,
                terminal_l1_insight: $insight,
                terminal_l3_skill: $skill,
            })
        };
        // Convenience form for all non-Completed arms: both None.
        ($outcome:expr) => {
            finish!($outcome, None, None)
        };
    }

    loop {
        // Cancellation poll — top of loop.
        if tasks::observe_state(pool, ctx.task_id).await? == "cancelled" {
            return finish!(Outcome::Cancelled);
        }

        if ctx.plan_count >= ctx.max_plans {
            return finish!(Outcome::Failed(format!(
                "plan_iteration_cap_exceeded ({}>={})", ctx.plan_count, ctx.max_plans
            )));
        }

        // 1. Formulate plan
        //
        // No loop-level retry: replanning IS the retry shape (the agent
        // sees the prior failure on the next iteration, bounded by
        // `max_plans`). A transient HTTP/transport error that escapes
        // the formulator's own retry is therefore terminal here.
        let (plan, meta) = match formulator.formulate_plan(&ctx).await {
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
            let captured_l3_skill: Option<crate::cassandra::types::L3SkillCandidate> =
                if dispatch_count >= 1 {
                    plan.completion_skill().cloned()
                } else {
                    None
                };
            return finish!(Outcome::Completed(result), captured_l1_insight, captured_l3_skill);
        }

        // 4. Execute steps
        let mut outcomes: Vec<StepOutcome> = Vec::with_capacity(plan.steps.len());
        for step in &plan.steps {
            if tasks::observe_state(pool, ctx.task_id).await? == "cancelled" {
                return finish!(Outcome::Cancelled);
            }
            let outcome = dispatcher.dispatch_step(step).await;
            dispatch_count = dispatch_count.saturating_add(1);
            let is_err = outcome.is_err();
            outcomes.push(outcome);
            if is_err { break; }
        }

        let steps_total = plan.steps.len();
        let steps_executed = outcomes.len();
        let any_err = outcomes.iter().any(|o| o.is_err());
        write_audit_plan_outcome(
            pool, &ctx, steps_executed, steps_total, any_err,
        ).await?;

        ctx.plans.push((plan, outcomes));
        // loop back: agent reflects on the outcomes for the next plan
    }
}

/// Apply `plan.floor_request` to `ctx` if it raises the current floor.
/// Pure side-effect on `ctx`. Returns true iff `ctx` was mutated.
///
/// Never lowers the floor: a `floor_request` whose rank is ≤ the
/// current floor is a no-op (pinned by
/// `agent_floor_request_lower_than_producer_is_ignored`).
///
/// On a successful raise, also flips
/// `ctx.classification_floor_source` to `AgentRaised` and clears
/// `ctx.classification_floor_signals` (the signals explained the
/// original CLI inference, not the elevated floor).
fn apply_floor_raise(ctx: &mut TaskContext, plan: &Plan) -> bool {
    if let Some(req) = plan.floor_request {
        if req.rank() > ctx.classification_floor.rank() {
            ctx.classification_floor = req;
            ctx.classification_floor_source = ClassificationFloorSource::AgentRaised;
            ctx.classification_floor_signals.clear();
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests;
