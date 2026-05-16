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

use super::agent::{AgentError, FormulationMeta, PlanFormulator};

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
    macro_rules! finish {
        ($outcome:expr) => {
            Ok(InnerLoopResult {
                outcome: $outcome,
                plan_count: ctx.plan_count,
                dispatch_count,
            })
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
            return finish!(Outcome::Completed(result));
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

/// Pure builder for the `agent/plan.formulate` audit-row payload.
///
/// Extracted from `write_audit_plan_formulate` so the wire shape is
/// unit-testable without a live Postgres pool. The 14/15-key shape pins
/// (in this file's `tests` module) defend against accidental drift —
/// 14 keys for non-`CliInferred` sources, 15 when `CliInferred` carries
/// matched signals.
///
/// Slice A (2026-05-15) added `plan` (full serialised Plan) +
/// `classification_floor` (task-level DataClass) so captures carry
/// everything the reviewer pipeline needs to be replayed offline —
/// see `core::observation::replay`.
///
/// Slice B (2026-05-16) added `classification_floor_source` (always)
/// and conditional `classification_floor_signals` (CliInferred only)
/// so audit consumers can trace how the floor was set.
pub(crate) fn build_plan_formulate_payload(
    task_id: i64,
    plan_count: u32,
    classification_floor: DataClass,
    classification_floor_source: ClassificationFloorSource,
    classification_floor_signals: &[String],
    plan: &Plan,
    meta: &FormulationMeta,
) -> serde_json::Value {
    // Issue #23 (spec §3): "refused" takes precedence over the
    // is_terminal-derived "task_complete" so a refusal payload is
    // wire-distinguishable from a successful completion via the same
    // discriminator field — including the malformed-refusal-with-steps
    // shape the inner-loop short-circuit also honours.
    let decision_kind = if plan.is_refused() {
        crate::cassandra::types::DECISION_REFUSED
    } else if plan.is_terminal() {
        crate::cassandra::types::DECISION_TERMINAL
    } else {
        "act"
    };

    // Explicit JSON null (not key-absent) so downstream JSONB queries
    // can rely on `refused` always being present.
    let refused = plan.refused.as_ref()
        .map(|r| serde_json::json!({ "principle": r.principle, "reason": r.reason }))
        .unwrap_or(serde_json::Value::Null);

    // `plan` is the full Plan JSON. Together with `classification_floor`
    // this is what enables offline replay (Slice B / observation::replay).
    // Plans are typically <1 KiB; the audit-envelope SHA-256 truncation
    // at 4 KiB is the safety net for the rare oversized case.
    let plan_json = serde_json::to_value(plan)
        .expect("Plan serialisation cannot fail (no non-string keys, no NaN)");

    // PascalCase string via DataClass's #[serde(rename_all = "PascalCase")].
    let classification_floor_json = serde_json::to_value(classification_floor)
        .expect("DataClass serialisation cannot fail (closed enum, no payloads)");

    let mut obj = serde_json::Map::new();
    obj.insert("task_id".into(),         serde_json::json!(task_id));
    obj.insert("plan_count".into(),      serde_json::json!(plan_count));
    obj.insert("prompt_name".into(),     serde_json::json!(meta.prompt_name));
    obj.insert("prompt_sha256".into(),   serde_json::json!(meta.prompt_sha256));
    obj.insert("llm_model".into(),       serde_json::json!(meta.llm_model));
    obj.insert("llm_backend".into(),     serde_json::json!(meta.llm_backend));
    obj.insert("latency_ms".into(),      serde_json::json!(meta.latency_ms));
    obj.insert("retry_count".into(),     serde_json::json!(meta.retry_count));
    obj.insert("plan_step_count".into(), serde_json::json!(plan.steps.len()));
    obj.insert("decision_kind".into(),   serde_json::json!(decision_kind));
    obj.insert("refused".into(),         refused);
    // Slice A:
    obj.insert("plan".into(),                 plan_json);
    obj.insert("classification_floor".into(), classification_floor_json);
    // Slice B (automatic floor inference, 2026-05-16):
    obj.insert(
        "classification_floor_source".into(),
        serde_json::json!(classification_floor_source.as_snake_str()),
    );
    // Signals key only appears when source is CliInferred AND we have
    // signals. Other sources (Operator / AgentRaised / Default) omit
    // the key (saving JSON payload bytes and making the absence itself
    // a wire signal that no CLI inference was the load-bearing decision).
    if classification_floor_source == ClassificationFloorSource::CliInferred
        && !classification_floor_signals.is_empty()
    {
        obj.insert(
            "classification_floor_signals".into(),
            serde_json::json!(classification_floor_signals),
        );
    }
    serde_json::Value::Object(obj)
}

async fn write_audit_plan_formulate(
    pool: &PgPool,
    ctx: &TaskContext,
    plan: &Plan,
    meta: &FormulationMeta,
) -> Result<(), InnerLoopError> {
    let payload = build_plan_formulate_payload(
        ctx.task_id,
        ctx.plan_count,
        ctx.classification_floor,
        ctx.classification_floor_source,
        &ctx.classification_floor_signals,
        plan,
        meta,
    );
    hhagent_db::audit::insert(pool, "agent", "plan.formulate", payload).await?;
    Ok(())
}

async fn write_audit_verdict(
    pool: &PgPool,
    ctx: &TaskContext,
    verdict: &Verdict,
    latency_ms: u64,
) -> Result<(), InnerLoopError> {
    let (kind, detail) = match verdict {
        Verdict::Approve => ("approve", serde_json::Value::Null),
        Verdict::Advisory(c) => ("advisory", serde_json::json!(c)),
        Verdict::Escalate(c, s) => ("escalate", serde_json::json!({"concern": c, "severity": s})),
        Verdict::Block(r) => ("block", serde_json::json!(r)),
        Verdict::ConstitutionalBlock { principle, reason } =>
            ("constitutional_block", serde_json::json!({"principle": principle, "reason": reason})),
    };
    let payload = serde_json::json!({
        "task_id":      ctx.task_id,
        "plan_count":   ctx.plan_count,
        "verdict_kind": kind,
        "detail":       detail,
        "latency_ms":   latency_ms,
    });
    hhagent_db::audit::insert(pool, "cassandra:chain", "verdict", payload).await?;
    Ok(())
}

async fn write_audit_plan_outcome(
    pool: &PgPool,
    ctx: &TaskContext,
    steps_executed: usize,
    steps_total: usize,
    any_err: bool,
) -> Result<(), InnerLoopError> {
    let payload = serde_json::json!({
        "task_id":         ctx.task_id,
        "plan_count":      ctx.plan_count,
        "terminal_kind":   if any_err { "err" } else { "ok" },
        "steps_executed":  steps_executed,
        "steps_total":     steps_total,
    });
    hhagent_db::audit::insert(pool, "scheduler", "plan.outcome", payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::DataClass;

    fn ctx() -> TaskContext {
        TaskContext {
            task_id: 1,
            lane: hhagent_db::tasks::Lane::Fast,
            instruction: "ping".into(),
            classification_floor: DataClass::Public,
            classification_floor_source: ClassificationFloorSource::Default,
            classification_floor_signals: vec![],
            plans: vec![],
            advisories: vec![],
            blocks: vec![],
            plan_count: 0,
            max_plans: 3,
        }
    }

    #[test]
    fn classification_floor_source_as_snake_str_matches_serde_wire_form() {
        // Pin the audit-log contract: `as_snake_str` MUST stay
        // byte-identical to the serde wire form so the rendered token
        // in the `classification_floor_source` payload key can be
        // cross-grepped with operator-visible logs. Mirrors the
        // `data_class_as_pascal_str_matches_serde_wire_form` pin.
        for s in [
            ClassificationFloorSource::Operator,
            ClassificationFloorSource::CliInferred,
            ClassificationFloorSource::AgentRaised,
            ClassificationFloorSource::Default,
        ] {
            let wire = serde_json::to_value(s).unwrap();
            let wire_str = wire.as_str()
                .expect("ClassificationFloorSource serialises as JSON string");
            assert_eq!(
                s.as_snake_str(),
                wire_str,
                "as_snake_str must equal serde wire form for {s:?}",
            );
        }
    }

    #[test]
    fn outcome_final_state_mapping() {
        assert_eq!(Outcome::Completed(serde_json::json!("x")).final_state(), "completed");
        assert_eq!(Outcome::Failed("e".into()).final_state(), "failed");
        assert_eq!(Outcome::Cancelled.final_state(), "cancelled");
        assert_eq!(Outcome::TimedOut.final_state(), "timed_out");
        assert_eq!(Outcome::Blocked { principle: 1, reason: "r".into() }.final_state(), "blocked");
        assert_eq!(
            Outcome::Refused { principle: 1, reason: "harm".into(), body: "explanation".into() }
                .final_state(),
            "refused",
        );
    }

    #[test]
    fn outcome_refused_result_payload_carries_principle_reason_and_body() {
        let o = Outcome::Refused {
            principle: 2,
            reason: "fraud_or_impersonation".into(),
            body: "Signing under your identity would impersonate you.".into(),
        };
        let p = o.result_payload().unwrap();
        assert_eq!(p["kind"], "refused");
        assert_eq!(p["principle"], 2);
        assert_eq!(p["reason"], "fraud_or_impersonation");
        assert_eq!(p["body"], "Signing under your identity would impersonate you.");

        // Exact key set — guards against accidental payload bloat.
        let keys: std::collections::BTreeSet<String> = p.as_object().unwrap()
            .keys().cloned().collect();
        let expected: std::collections::BTreeSet<String> =
            ["kind", "principle", "reason", "body"].iter().map(|s| s.to_string()).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn outcome_result_payload_for_failed_includes_detail() {
        let p = Outcome::Failed("oops".into()).result_payload().unwrap();
        assert_eq!(p["kind"], "error");
        assert_eq!(p["detail"], "oops");
    }

    #[test]
    fn step_outcome_is_err_classifier() {
        let ok = StepOutcome::Ok(serde_json::json!("x"));
        let err = StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "no".into() };
        assert!(!ok.is_err());
        assert!(err.is_err());
    }

    #[test]
    fn agent_floor_request_higher_than_producer_elevates_ctx() {
        let mut c = ctx();
        // Start at Public (Default source).
        assert_eq!(c.classification_floor, DataClass::Public);
        assert_eq!(c.classification_floor_source, ClassificationFloorSource::Default);

        let plan = Plan {
            context: "c".into(), decision: "d".into(), rationale: "r".into(),
            steps: vec![], result: None,
            data_ceiling: DataClass::ClinicalConfidential, refused: None,
            floor_request: Some(DataClass::ClinicalConfidential),
        };
        let raised = apply_floor_raise(&mut c, &plan);
        assert!(raised);
        assert_eq!(c.classification_floor, DataClass::ClinicalConfidential);
        assert_eq!(c.classification_floor_source, ClassificationFloorSource::AgentRaised);
        assert!(c.classification_floor_signals.is_empty());
    }

    #[test]
    fn agent_floor_request_lower_than_producer_is_ignored() {
        let mut c = ctx();
        c.classification_floor = DataClass::ClinicalConfidential;
        c.classification_floor_source = ClassificationFloorSource::Operator;

        let plan = Plan {
            context: "c".into(), decision: "d".into(), rationale: "r".into(),
            steps: vec![], result: None,
            data_ceiling: DataClass::Public, refused: None,
            // floor_request below current floor — must NOT lower:
            floor_request: Some(DataClass::Public),
        };
        let raised = apply_floor_raise(&mut c, &plan);
        assert!(!raised, "lower floor_request must be ignored");
        assert_eq!(c.classification_floor, DataClass::ClinicalConfidential);
        assert_eq!(c.classification_floor_source, ClassificationFloorSource::Operator);
    }

    #[test]
    fn agent_floor_request_equal_to_producer_is_no_op() {
        let mut c = ctx();
        c.classification_floor = DataClass::Personal;
        c.classification_floor_source = ClassificationFloorSource::CliInferred;
        c.classification_floor_signals = vec!["my_email".into()];

        let plan = Plan {
            context: "c".into(), decision: "d".into(), rationale: "r".into(),
            steps: vec![], result: None,
            data_ceiling: DataClass::Personal, refused: None,
            floor_request: Some(DataClass::Personal),
        };
        let raised = apply_floor_raise(&mut c, &plan);
        assert!(!raised, "equal-rank floor_request must be a no-op");
        assert_eq!(c.classification_floor, DataClass::Personal);
        assert_eq!(c.classification_floor_source, ClassificationFloorSource::CliInferred);
        assert_eq!(c.classification_floor_signals, vec!["my_email".to_string()]);
    }

    #[test]
    fn agent_floor_request_none_is_no_op() {
        let mut c = ctx();
        c.classification_floor = DataClass::Public;
        c.classification_floor_source = ClassificationFloorSource::CliInferred;
        c.classification_floor_signals = vec!["patient".into()];

        let plan = Plan {
            context: "c".into(), decision: "d".into(), rationale: "r".into(),
            steps: vec![], result: None,
            data_ceiling: DataClass::Public, refused: None,
            floor_request: None,
        };
        let raised = apply_floor_raise(&mut c, &plan);
        assert!(!raised);
        // CLI inference state is preserved when there's no raise request.
        assert_eq!(c.classification_floor_source, ClassificationFloorSource::CliInferred);
        assert_eq!(c.classification_floor_signals, vec!["patient".to_string()]);
    }

    #[test]
    fn build_plan_formulate_payload_carries_full_plan_and_classification_floor() {
        let plan = Plan {
            context: "ctx".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![PlannedStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({"argv": ["/bin/echo", "hi"]}),
                returns: "stdout".into(),
                done_when: "echoed".into(),
                classification: DataClass::Public,
            }],
            result: None,
            data_ceiling: DataClass::Personal,
            refused: None,
            floor_request: None,
        };
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "deadbeef".into(),
            llm_model: "gemma4:26b".into(),
            llm_backend: "local".into(),
            latency_ms: 42,
            retry_count: 0,
        };
        let payload = build_plan_formulate_payload(
            /*task_id*/ 7,
            /*plan_count*/ 1,
            /*classification_floor*/ DataClass::ClinicalConfidential,
            /*classification_floor_source*/ ClassificationFloorSource::Default,
            /*classification_floor_signals*/ &[],
            &plan,
            &meta,
        );

        // New: full Plan JSON round-trips byte-for-byte.
        let plan_back: Plan = serde_json::from_value(payload["plan"].clone())
            .expect("plan key must deserialise back into a Plan");
        assert_eq!(plan_back, plan, "plan payload field must round-trip");

        // New: task-level classification_floor stringified PascalCase.
        assert_eq!(
            payload["classification_floor"], "ClinicalConfidential",
            "classification_floor must serialise as PascalCase string"
        );

        // Existing 11 keys remain unchanged.
        assert_eq!(payload["task_id"], 7);
        assert_eq!(payload["plan_count"], 1);
        assert_eq!(payload["decision_kind"], "act");
        assert_eq!(payload["plan_step_count"], 1);
        assert!(payload["refused"].is_null());
    }

    #[test]
    fn build_plan_formulate_payload_pins_fourteen_keys_for_default_source() {
        // Pin the total key count so a future additive change to the
        // wire shape becomes a deliberate, reviewable edit instead of
        // an accidental drift. Default source: 14 keys (no signals).
        let plan = Plan {
            context: "".into(),
            decision: "task_complete".into(),
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
        };
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "x".into(),
            llm_model: "m".into(),
            llm_backend: "local".into(),
            latency_ms: 0,
            retry_count: 0,
        };
        let payload = build_plan_formulate_payload(
            1, 0, DataClass::Public,
            ClassificationFloorSource::Default, &[],
            &plan, &meta,
        );
        let keys: std::collections::BTreeSet<&str> = payload
            .as_object()
            .expect("payload is a JSON object")
            .keys()
            .map(|s| s.as_str())
            .collect();
        let expected: std::collections::BTreeSet<&str> = [
            "task_id", "plan_count", "prompt_name", "prompt_sha256",
            "llm_model", "llm_backend", "latency_ms", "retry_count",
            "plan_step_count", "decision_kind", "refused",
            // Slice A additions:
            "plan", "classification_floor",
            // Slice B (automatic floor inference, 2026-05-16):
            "classification_floor_source",
        ].into_iter().collect();
        assert_eq!(keys, expected, "payload key set drifted; update the pin deliberately");
    }

    #[test]
    fn build_plan_formulate_payload_default_source_omits_signals_key() {
        let plan = Plan {
            context: "".into(), decision: "task_complete".into(), rationale: "".into(),
            steps: vec![], result: Some(serde_json::json!({"kind":"text","body":"ok"})),
            data_ceiling: DataClass::Public, refused: None, floor_request: None,
        };
        let meta = FormulationMeta {
            prompt_name: "p".into(), prompt_sha256: "h".into(),
            llm_model: "m".into(), llm_backend: "local".into(),
            latency_ms: 1, retry_count: 0,
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default, &[], &plan, &meta,
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 14,
            "default-source payload should have 14 keys; got {} keys: {:?}",
            obj.len(), obj.keys().collect::<Vec<_>>());
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("default".into()));
        assert!(obj.get("classification_floor_signals").is_none(),
            "signals key must be ABSENT when source is not cli_inferred");
    }

    #[test]
    fn build_plan_formulate_payload_cli_inferred_source_has_15_keys_with_signals() {
        let plan = Plan {
            context: "".into(), decision: "task_complete".into(), rationale: "".into(),
            steps: vec![], result: Some(serde_json::json!({"kind":"text","body":"ok"})),
            data_ceiling: DataClass::ClinicalConfidential, refused: None, floor_request: None,
        };
        let meta = FormulationMeta {
            prompt_name: "p".into(), prompt_sha256: "h".into(),
            llm_model: "m".into(), llm_backend: "local".into(),
            latency_ms: 1, retry_count: 0,
        };
        let signals = vec!["patient".to_string(), "pathology".to_string()];
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::CliInferred, &signals,
            &plan, &meta,
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 15,
            "cli_inferred payload should have 15 keys (default 14 + signals); got {} keys: {:?}",
            obj.len(), obj.keys().collect::<Vec<_>>());
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("cli_inferred".into()));
        let arr = obj["classification_floor_signals"].as_array()
            .expect("signals key is an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], serde_json::Value::String("patient".into()));
        assert_eq!(arr[1], serde_json::Value::String("pathology".into()));
    }

    #[test]
    fn build_plan_formulate_payload_agent_raised_source_omits_signals() {
        // After an agent raise, signals are cleared — they only explain the
        // original CLI inference, not the elevated floor.
        let plan = Plan {
            context: "".into(), decision: "task_complete".into(), rationale: "".into(),
            steps: vec![], result: None,
            data_ceiling: DataClass::ClinicalConfidential, refused: None,
            floor_request: Some(DataClass::ClinicalConfidential),
        };
        let meta = FormulationMeta {
            prompt_name: "p".into(), prompt_sha256: "h".into(),
            llm_model: "m".into(), llm_backend: "local".into(),
            latency_ms: 1, retry_count: 0,
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::AgentRaised,
            &[],  // empty: signals are cleared on raise
            &plan, &meta,
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 14,
            "agent_raised should have 14 keys (no signals); got: {:?}", obj.keys().collect::<Vec<_>>());
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("agent_raised".into()));
        assert!(obj.get("classification_floor_signals").is_none());
    }

    #[test]
    fn task_context_plans_so_far_summary_is_compact() {
        let mut c = ctx();
        c.plans.push((
            crate::cassandra::types::Plan {
                context: "c".into(),
                decision: "act".into(),
                rationale: "r".into(),
                steps: vec![],
                result: None,
                data_ceiling: DataClass::Public,
                refused: None,
                floor_request: None,
            },
            vec![StepOutcome::Ok(serde_json::json!("x")), StepOutcome::Err {
                code: "POLICY_DENIED".into(), detail: "no".into(),
            }],
        ));
        let s = c.plans_so_far_summary();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0]["decision"], "act");
        assert_eq!(s[0]["step_outcomes"], serde_json::json!(["ok", "err"]));
    }
}
