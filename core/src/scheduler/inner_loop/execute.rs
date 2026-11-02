//! Review and dispatch of one formulated plan — moved verbatim out of
//! `run_to_terminal` (over the 500-line cap) so the notebook loop (#922)
//! runs every call through the **same** floor raise, ceiling resolution,
//! audit rows, invoke expansion, method qualification, CASSANDRA review,
//! verdict handling, terminal check and dispatch as a classic plan.
//!
//! What was a `return finish!(…)` in the loop is [`Executed::Finished`];
//! what was a `continue` is [`Executed::Again`]; falling off the end (the
//! record pushed onto `ctx.plans`) is [`Executed::Dispatched`].

use sqlx::PgPool;

use super::*;
use crate::cassandra::types::Plan;
use crate::scheduler::agent::FormulationMeta;

/// The loop counters the moved code reads and bumps.
#[derive(Debug, Default)]
pub(crate) struct ExecState {
    /// Every `StepDispatcher::dispatch_step` call this task makes (success
    /// or failure). Reported back in `InnerLoopResult` for the spec §7
    /// `task.finalize` summary row.
    pub dispatch_count: u32,
    /// Set once any iteration expands an `invoke_skill` directive. ANDed
    /// into the terminal `l3_skill` capture so an invoke-driven task never
    /// re-crystallises the skill it just ran (forecloses a crystallise →
    /// pin → invoke → re-crystallise cycle).
    pub invoke_used: bool,
    /// Set once the agent gathers ≥1 successful tool observation. Gates the
    /// forced-synthesis fallback: with nothing gathered there is nothing to
    /// synthesize, so the cap fails hard.
    pub gathered: bool,
}

/// How a plan's steps are dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepPolicy {
    /// Any plan: at most [`MAX_STEPS_PER_PLAN`] steps, and the first failed
    /// step ends the plan (the next one may depend on it).
    Plan,
    /// A sift's reads (#922, K3): independent reads of one item each, so a
    /// failed read is counted and the rest still run; capped by the
    /// operator's `KASTELLAN_SIFT_MAX_READS` instead (K3c).
    SiftReads { cap: usize },
}

impl StepPolicy {
    /// The most steps one plan may carry under this policy.
    pub(crate) fn cap(self) -> usize {
        match self {
            StepPolicy::Plan => MAX_STEPS_PER_PLAN,
            StepPolicy::SiftReads { cap } => cap,
        }
    }

    /// Does a failed step end the plan?
    pub(crate) fn stops_on_error(self) -> bool {
        self == StepPolicy::Plan
    }
}

/// How one plan ended.
pub(crate) enum Executed {
    /// The task ends with this outcome. Boxed: a `Finished` is far larger
    /// than the other variants (`clippy::large_enum_variant`).
    Finished(Box<Finished>),
    /// The plan was blocked or its invoke refused; formulate again.
    Again,
    /// The plan's steps ran; their record is the last of `ctx.plans`.
    Dispatched,
}

/// A task's end, with what a `Completed` plan raised for memory.
pub(crate) struct Finished {
    pub outcome: Outcome,
    pub insight: Option<String>,
    pub skill: Option<crate::cassandra::types::L3SkillCandidate>,
    pub python_skill: Option<crate::cassandra::types::PythonSkillCandidate>,
}

impl Executed {
    fn finished(outcome: Outcome) -> Self {
        Executed::Finished(Box::new(Finished { outcome, insight: None, skill: None, python_skill: None }))
    }
}

/// Review `plan` and, when it is approved and not terminal, dispatch its
/// steps under `policy`. `synth_turn` and `exhausted` are the loop's: on a
/// forced-synthesis turn a non-terminal plan fails instead of running.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn review_and_dispatch(
    pool: &PgPool,
    review: &ChainReviewStage,
    dispatcher: &dyn StepDispatcher,
    ctx: &mut TaskContext,
    mut plan: Plan,
    meta: &FormulationMeta,
    synth_turn: bool,
    exhausted: Option<ceiling::Exhausted>,
    st: &mut ExecState,
    outbox: Option<&crate::channel::outbox::ChannelOutbox>,
    policy: StepPolicy,
) -> Result<Executed, InnerLoopError> {
    use kastellan_db::tasks;

    // Agent-side floor-raise: if the plan requests a higher floor than
    // the producer set, elevate ctx BEFORE the audit row is written
    // (so the row reflects the elevated floor + AgentRaised source)
    // and BEFORE the reviewer chain runs (so DP sees the new floor
    // for I1 + I2 checks).
    if apply_floor_raise(ctx, &plan) {
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

    write_audit_plan_formulate(pool, ctx, &plan, meta, resolved.source).await?;

    // 1b. L3 autonomous invoke expansion (before review, so the
    // reviewer governs the concrete steps). Presence of `invoke_skill`
    // triggers this branch; a malformed directive or a refused gate is
    // audited + fed back as a block so the agent replans — never a
    // silent fall-through to dispatching co-supplied steps. `plan` is
    // `mut`; we resolve the directive to OWNED data first so the borrow
    // from `validate_invoke` ends before we assign `plan.steps`.
    let mut current_invoke: Option<(i64, String)> = None;
    if plan.invoke_skill.is_some() {
        match expand_invoke_skill(pool, dispatcher, &plan, resolved.ceiling).await? {
            InvokeExpansion::Refused(reasons) => {
                for r in &reasons {
                    ctx.blocks.push(format!("invoke_rejected: {r}"));
                }
                return Ok(Executed::Again); // bounded by plan_count cap on next iter
            }
            InvokeExpansion::Expanded { steps, memory_id, name } => {
                plan.steps = steps;
                st.invoke_used = true;
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
    qualify_plan_methods(dispatcher, ctx.task_id, &mut plan.steps);

    // 2. CASSANDRA review
    let rctx = ReviewStageContext {
        task_id: ctx.task_id,
        instruction: &ctx.instruction,
        classification_floor: ctx.classification_floor,
        plan_count: ctx.plan_count,
    };
    let verdict_start = std::time::Instant::now();
    let verdict = review.review(&plan, &rctx).await;
    write_audit_verdict(pool, ctx, &verdict, verdict_start.elapsed().as_millis() as u64).await?;

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
            return Ok(Executed::finished(Outcome::Blocked { principle: *principle, reason: reason.clone() })),
        Verdict::Block(reason) => {
            // When the agent self-refused, Block does not loop back —
            // the refusal is already terminal. Fall through to the
            // if-let-Some check below. For normal (non-refusal) plans,
            // continue so the agent can revise.
            if plan.refused.is_none() {
                ctx.blocks.push(reason.clone());
                return Ok(Executed::Again);  // bounded by plan_count cap on next iter
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
                            return Ok(Executed::finished(Outcome::AwaitingOperator { ask_id }));
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
                            return Ok(Executed::finished(Outcome::Failed(format!(
                                "escalation could not be raised: {e}"
                            ))));
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
        return Ok(Executed::finished(Outcome::Refused {
            principle: refused.principle,
            reason: refused.reason,
            body,
        }));
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
            if st.dispatch_count >= 1 && !st.invoke_used && !synth_turn {
                plan.completion_skill().cloned()
            } else {
                None
            };
        let captured_python_skill: Option<crate::cassandra::types::PythonSkillCandidate> =
            if st.dispatch_count >= 1 && !st.invoke_used && !synth_turn {
                plan.completion_python_skill().cloned()
            } else {
                None
            };
        return Ok(Executed::Finished(Box::new(Finished {
            outcome: Outcome::Completed(result),
            insight: captured_l1_insight,
            skill: captured_l3_skill,
            python_skill: captured_python_skill,
        })));
    }

    // Forced-synthesis turn: the model was told to answer now. A
    // terminal plan already returned `Outcome::Completed` above (and a
    // self-refusal returned `Outcome::Refused`). If it STILL returned a
    // non-terminal plan, do NOT execute more tool steps — fail at the
    // cap rather than spending another gather round.
    if synth_turn {
        // A synthesis turn that a TIMEOUT forced (#774) never reached the
        // cap, so it must not say it did — operators count cap rows.
        let Some(which) = exhausted else {
            return Ok(Executed::finished(Outcome::Failed(llm_failure::forced_synthesis_no_answer_detail(
                &llm_failure::GatheredWork::from_plans(&ctx.plans),
            ))));
        };
        // `ctx.plan_count` is now `max_plans + 1` (the synthesis turn spent
        // one extra formulation), so the detail prints `max_plans` twice
        // rather than read as an off-by-one (`6>=5`).
        return Ok(Executed::finished(Outcome::Failed(ceiling::exhausted_detail(which, ctx.max_plans, true))));
    }

    // 4. Execute steps
    // Per-plan step budget (security audit 2026-09-02, F5): nothing bounded
    // how many tool dispatches ONE plan could carry — only the number of
    // plan iterations (`max_plans`) — so a planner steered by injected
    // content could enqueue hundreds of fetches/sends in a single turn,
    // each retained in the plan record. A plan over the cap is refused
    // whole, like an iteration-cap overrun, rather than truncated (a
    // truncated plan would execute a prefix the planner never reviewed).
    let cap = policy.cap();
    if plan.steps.len() > cap {
        return Ok(Executed::finished(Outcome::Failed(format!(
            "plan_step_cap_exceeded ({} > {cap}); refusing to execute a plan with more than \
             {cap} steps in one iteration",
            plan.steps.len(),
        ))));
    }

    let mut outcomes: Vec<StepOutcome> = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        if tasks::observe_state(pool, ctx.task_id).await? == "cancelled" {
            return Ok(Executed::finished(Outcome::Cancelled));
        }
        let outcome = dispatcher.dispatch_step(ctx.task_id, step).await;
        st.dispatch_count = st.dispatch_count.saturating_add(1);
        let is_err = outcome.is_err();
        outcomes.push(outcome);
        if is_err && policy.stops_on_error() { break; }
    }

    let steps_total = plan.steps.len();
    let steps_executed = outcomes.len();
    let any_err = outcomes.iter().any(|o| o.is_err());
    // Arm the forced-synthesis fallback once any step actually succeeds:
    // there is now a real observation to synthesize an answer from.
    if outcomes.iter().any(|o| matches!(o, StepOutcome::Ok(_))) {
        st.gathered = true;
    }
    write_audit_plan_outcome(
        pool, ctx, steps_executed, steps_total, any_err,
    ).await?;

    if let Some((memory_id, skill_name)) = &current_invoke {
        let payload = build_l3_invoke_outcome_payload(
            *memory_id, skill_name, steps_executed, steps_total, any_err,
        );
        kastellan_db::audit::insert(
            pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_INVOKE_OUTCOME, payload,
        ).await?;
    }

    let record = PlanRecord::new(plan, outcomes);
    // A block only the planner-summary sink made is otherwise invisible:
    // the planner sees `withheld` and no source screen wrote a row (#702
    // review). Written here, at the one live append point, and never on
    // resume, which rebuilds records it already recorded. Best-effort, like
    // `post_process`'s rows: losing the forensic row must not fail the task.
    for payload in record.sink_block_audit_payloads(ctx.task_id, ctx.plan_count) {
        if let Err(e) = kastellan_db::audit::insert(pool, "policy", "injection.blocked", payload).await {
            tracing::error!(task_id = ctx.task_id, error = %e, "sink injection.blocked audit insert failed");
        }
    }
    ctx.plans.push(record);
    Ok(Executed::Dispatched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_is_capped_by_the_per_plan_cap_and_stops_at_a_failure() {
        assert_eq!(StepPolicy::Plan.cap(), MAX_STEPS_PER_PLAN);
        assert!(StepPolicy::Plan.stops_on_error());
    }

    #[test]
    fn a_sifts_reads_take_their_own_cap_and_run_past_a_failure() {
        assert_eq!(StepPolicy::SiftReads { cap: 200 }.cap(), 200);
        assert!(!StepPolicy::SiftReads { cap: 200 }.stops_on_error());
    }
}
