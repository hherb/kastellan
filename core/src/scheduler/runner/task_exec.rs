//! Task execution entry for the lane runner: build a [`TaskContext`]
//! from the claimed task's payload, run it to a terminal outcome, and
//! purge the per-task handoff cache.
//!
//! [`run_one`] is the single agent-task entry [`super::drain_lane`]
//! calls (the operator L3-run path is handled inline in `drain_lane`).
//! The payload-shape validation is factored into the pure
//! [`parse_classification_floor_source_from_payload`] so it can be
//! unit-tested without seeding a task in Postgres.

use std::sync::Arc;

use sqlx::PgPool;

use kastellan_db::asks::Ask;
use kastellan_db::tasks::Task;

use crate::cassandra::review::ChainReviewStage;
use crate::cassandra::types::DataClass;
use crate::channel::outbox::ChannelOutbox;
use crate::scheduler::agent::PlanFormulator;
use crate::scheduler::asks::{resolution_choice, Choice};
use crate::scheduler::inner_loop::{
    run_to_terminal, ClassificationFloorSource, InnerLoopResult, Outcome, StepDispatcher,
    TaskContext,
};

/// The terminal outcome a task's already-resolved ask forces before any
/// planning happens, or `None` to proceed.
///
/// Only a **denial** is terminal here. `asks::resolve` re-enqueues the task
/// on any resolution, so a denied task returns to `pending`, is claimed,
/// and would otherwise replan from scratch — and if denial only bound to
/// the plan digest, the agent could replan around it: the operator denies
/// plan P, the agent produces P′, P′ passes review, and the thing that was
/// just refused executes. An operator saying "deny" means *do not do this*
/// (spec D2).
///
/// `reason` is the ask's `body` — the concern the operator was answering.
/// Their optional free-text note is deliberately not copied here; it lives
/// in `asks.resolution` and the `ask.resolved` audit row (spec D10).
///
/// A malformed resolution proceeds rather than terminating: failing toward
/// running costs an escalation, where failing toward refusal kills a task
/// for a reason nobody can see in the plan trail.
///
/// **Scans every resolution, not just the newest.** A denial ends the task,
/// so in practice it is always the last decision a task can hold and looking
/// only at the newest would give the same answer — but that is a property of
/// today's terminal handling, not of this function, and a scan costs nothing
/// over a list bounded by how many times a human answered.
pub(crate) fn pre_plan_outcome(resolved: &[Ask]) -> Option<Outcome> {
    let denied = resolved
        .iter()
        .find(|ask| matches!(resolution_choice(ask), Some(Choice::Deny)))?;
    Some(Outcome::Denied {
        ask_id: denied.id,
        reason: denied.body.clone(),
    })
}

/// `(starting plan_count, max_plans)` for a claimed task, given the value
/// its DB column holds.
///
/// Two separable things the old code conflated by starting every run at 0
/// (spec D6):
///
/// * **the column is a historical fact.** `inner_loop` mirrors
///   `ctx.plan_count` back with `increment_plan_count`, which writes the
///   **absolute** value — so a task that escalated after 4 plans and
///   resumed at 0 rewrote its own column 4 → 1. Seeding from the column
///   keeps it monotonic and true.
/// * **the budget is a policy.** An approved plan gets a full further
///   allowance, because the alternative — spending from the original
///   budget — leaves a task that escalated on its last allowed plan
///   resuming with none, so the operator's approval buys nothing. Runaway
///   is not the risk it looks like: each additional allowance costs one
///   human interaction.
///
/// A negative column value (impossible today; `plan_count` is a signed
/// column with a non-negative writer) clamps to 0 rather than wrapping
/// through `as u32` to ~4 billion, which would make the cap check pass
/// forever.
pub(crate) fn resume_budget(db_plan_count: i32, max_plans: u32) -> (u32, u32) {
    let carried = u32::try_from(db_plan_count).unwrap_or(0);
    (carried, carried.saturating_add(max_plans))
}

pub(super) async fn run_one(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    task: &Task,
    max_plans: u32,
    outbox: Option<&ChannelOutbox>,
) -> InnerLoopResult {
    let instruction = task.payload.get("instruction")
        .and_then(|v| v.as_str()).unwrap_or("").to_string();

    // SECURITY: an unrecognised classification_floor is a hard error —
    // silently defaulting to Public would downgrade clinically-classified
    // data into the lowest review band. Field absence is the producer
    // opting out (treated as no floor / Public); a present-but-bad value
    // is a producer bug that must surface.
    let classification_floor = match task.payload.get("classification_floor") {
        None => DataClass::Public,
        Some(v) => {
            let Some(s) = v.as_str() else {
                return failed_result(format!(
                    "classification_floor in payload is not a string: {v:?}"
                ));
            };
            match serde_json::from_str::<DataClass>(&format!("\"{}\"", s)) {
                Ok(dc) => dc,
                Err(_) => return failed_result(format!(
                    "unknown classification_floor: {s:?} (expected one of \
                     Public, Personal, ClinicalConfidential, Secret)"
                )),
            }
        }
    };
    // Provenance: source defaults to "default" when absent. Validation
    // lives in the pure helper `parse_classification_floor_source_from_payload`
    // so it can be unit-tested without seeding a task in Postgres.
    let classification_floor_source = match parse_classification_floor_source_from_payload(
        task.payload.get("classification_floor_source"),
    ) {
        Ok(src) => src,
        Err(detail) => return failed_result(detail),
    };
    // Signals: empty array iff absent or not an array. Each entry must
    // be a string; non-string entries are skipped silently (better than
    // failing the task on a non-load-bearing presentation field).
    let classification_floor_signals: Vec<String> = task.payload
        .get("classification_floor_signals")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect())
        .unwrap_or_default();

    // Bound the override by u32::MAX explicitly: an `as u32` cast would
    // silently roll over a producer-supplied 2^33 to a small number,
    // which then *under*shoots the lane default. Falling back to the
    // lane default on any out-of-range value keeps behaviour predictable.
    let max_plans_override = task.payload.get("max_plans")
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(max_plans);

    // One read, two consumers (spec D4): the denial check just below, and
    // the `Escalate` arm's digest comparison via `ctx.resolved_asks`.
    //
    // A read failure FAILS THE TASK CLOSED. Proceeding with an empty list
    // would skip the denial check as surely as the approval check, and those
    // two are not equally safe to get wrong: an unseen approval costs one
    // more operator interaction, while an unseen DENIAL lets a task the
    // operator explicitly refused go on to plan and dispatch real steps
    // before it escalates again. With the read failed there is no way to
    // tell "nobody has decided yet" from "somebody decided and I could not
    // read it", so this cannot proceed on the optimistic reading. The task
    // ends `failed` and is re-submittable; a `Denied` outcome is not used
    // because no denial was actually read.
    let resolved_asks = match kastellan_db::asks::resolved_for_task(pool, task.id).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                task_id = task.id, error = %e,
                "could not read the task's resolved asks; failing the task closed, because \
                 a failed read cannot distinguish 'no decision yet' from 'a decision I \
                 could not see' — and the second one may be a denial"
            );
            return failed_result(format!(
                "could not read this task's resolved operator asks: {e}"
            ));
        }
    };

    if let Some(outcome) = pre_plan_outcome(&resolved_asks) {
        tracing::info!(task_id = task.id, "operator denied this task's ask; terminating without planning");
        return InnerLoopResult {
            outcome,
            plan_count: u32::try_from(task.plan_count).unwrap_or(0),
            dispatch_count: 0,
            terminal_l1_insight: None,
            terminal_l3_skill: None,
            terminal_python_skill: None,
            // No plans ran, so there is nothing for the next turn to read.
            turn_record: None,
        };
    }

    let (start_plan_count, max_plans_for_run) = resume_budget(task.plan_count, max_plans_override);

    // Restore the run this task was suspended in the middle of (spec D11).
    //
    // The state rides on the ask that suspended it, and `resolved_asks` is
    // newest-first, so `[0]` is the suspension the operator most recently
    // answered — the one whose history is current. A task that never
    // suspended has no resolved asks and restores nothing, which is the same
    // empty context a first run has always had.
    //
    // Without this the resumed task re-formulates every iteration it already
    // ran and dispatches their steps a second time: plan 1 sends an email,
    // plan 2 escalates, the operator approves, and the email goes out again
    // — a duplicate side effect caused *by* the human oversight step.
    //
    // What it does NOT do is make re-execution impossible. A planner that
    // re-emits an identical step still dispatches it; the guarantee is that
    // the planner has the information not to, exactly as it does between two
    // ordinary iterations of the same run.
    let restored = crate::scheduler::asks::restore_resume_state(
        resolved_asks.first().and_then(|a| a.resume_state.as_ref()),
    );
    if !restored.plans.is_empty() {
        tracing::info!(
            task_id = task.id,
            plans = restored.plans.len(),
            "resuming a suspended task with the plan history it had at suspend time",
        );
    }

    // #701: the earlier turns of this conversation, if this task came from one.
    // Loading, screening, budgeting and classification all live in
    // `scheduler::conversation`; this is the wiring only.
    let origin = crate::channel::ask_message::destination_from_task_payload(&task.payload);
    let loaded = crate::scheduler::conversation::load_for_task(
        pool,
        task,
        origin.as_ref(),
        classification_floor,
        classification_floor_source,
    )
    .await;
    let (classification_floor, classification_floor_source) = loaded.floor;

    // A block only this screen made is otherwise invisible: the planner sees
    // `withheld` and no other screen wrote a row. Best-effort, like the sink
    // rows — losing a forensic row must not fail the task.
    for block in &loaded.blocks {
        let payload = crate::scheduler::conversation::block_audit_payload(task.id, block);
        if let Err(e) =
            kastellan_db::audit::insert(pool, "policy", "injection.blocked", payload).await
        {
            tracing::error!(
                task_id = task.id, error = %e,
                "conversation injection.blocked audit insert failed"
            );
        }
    }

    let ctx = TaskContext {
        task_id: task.id,
        lane: task.lane,
        instruction,
        classification_floor,
        classification_floor_source,
        classification_floor_signals,
        plans: restored.plans,
        advisories: restored.advisories,
        blocks: restored.blocks,
        plan_count: start_plan_count,
        max_plans: max_plans_for_run,
        resolved_asks,
        origin,
        conversation: loaded.turns,
        conversation_task_ids: loaded.task_ids,
    };

    let task_id = ctx.task_id;
    let dispatcher_for_purge = std::sync::Arc::clone(&dispatcher);
    let result = match run_to_terminal(pool, formulator, review, dispatcher, ctx, outbox).await {
        Ok(r) => r,
        Err(e) => failed_result(format!("inner_loop: {e}")),
    };
    dispatcher_for_purge.purge_task(task_id);
    result
}

/// Build an `InnerLoopResult` representing a `Failed` outcome with
/// zero counters. Used at the pre-loop validation points in
/// [`run_one`] (bad payload shape, classification override) where the
/// inner loop never runs — counters are 0 in those branches.
fn failed_result(detail: String) -> InnerLoopResult {
    InnerLoopResult {
        outcome: Outcome::Failed(detail),
        plan_count: 0,
        dispatch_count: 0,
        terminal_l1_insight: None,
        terminal_l3_skill: None,
        terminal_python_skill: None,
        turn_record: None,
    }
}

// Production `StepDispatcher`: see [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`]
// (moved out of this file 2026-05-11 when the placeholder was replaced
// with the real `tool_host::dispatch` wiring — Task 3.2.bis).

/// Parse the producer-supplied `classification_floor_source` payload
/// field at task-entry time.
///
/// Semantics:
/// - **Absent (`None`)** → `Ok(Default)`. The producer opted out of
///   provenance; the floor was not set by inference or operator flag.
/// - **`"operator"`** → `Ok(Operator)`. Operator pinned the floor via
///   `kastellan-cli ask --classification-floor X`.
/// - **`"cli_inferred"`** → `Ok(CliInferred)`. The CLI's
///   `classification_inference` keyword classifier elevated above Public.
/// - **`"default"`** → `Ok(Default)`. Explicit "no provenance" — same
///   semantic as absent.
/// - **`"agent_raised"`** → `Err`. Reserved for the inner loop's
///   [`crate::scheduler::inner_loop::apply_floor_raise`]; any producer that writes
///   it directly is forging audit-trail provenance ([issue #71]).
///   The producer cannot raise the floor — only the agent can, via
///   `Plan.floor_request`, and the inner loop is the only legitimate
///   writer of `AgentRaised`. Fail-closed at entry so the audit-log
///   contract cannot be silently misattributed.
/// - **Non-string JSON value** → `Err`. Payload shape error.
/// - **Unknown string** → `Err`. Producer-bug surface; surfaces the
///   bad value so a misspelt token is easy to spot in the failure
///   message.
///
/// All `Err` variants carry a human-readable diagnostic suitable for
/// passing straight into [`failed_result`].
///
/// Pure function: no I/O, no side effects. Renaming any branch of
/// [`ClassificationFloorSource`] is an audit-trail contract break; the
/// reject here matches on the parsed variant (not the wire string) so a
/// rename of `AgentRaised` + its serde tag propagates automatically.
///
/// [issue #71]: https://github.com/hherb/kastellan/issues/71
fn parse_classification_floor_source_from_payload(
    value: Option<&serde_json::Value>,
) -> Result<ClassificationFloorSource, String> {
    let Some(v) = value else {
        return Ok(ClassificationFloorSource::Default);
    };
    let Some(s) = v.as_str() else {
        return Err(format!(
            "classification_floor_source in payload is not a string: {v:?}"
        ));
    };
    // Parse first, then reject the `AgentRaised` variant on a structural
    // match. Binding the reject to the enum variant (rather than a
    // string literal) means a future rename of `AgentRaised` + its
    // serde tag + `as_snake_str` continues to be rejected here without
    // a parallel edit. The dedicated diagnostic is preserved so an
    // operator grepping the daemon journal for "reserved" still finds
    // this site.
    match serde_json::from_value::<ClassificationFloorSource>(v.clone()) {
        Ok(ClassificationFloorSource::AgentRaised) => Err(format!(
            "classification_floor_source = {:?} is reserved for the inner \
             loop's apply_floor_raise — producers must not supply it. \
             Use operator / cli_inferred / default at submission time.",
            ClassificationFloorSource::AgentRaised.as_snake_str(),
        )),
        Ok(src) => Ok(src),
        Err(_) => Err(format!(
            "unknown classification_floor_source: {s:?} (expected one of \
             operator, cli_inferred, default)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::OffsetDateTime;

    #[test]
    fn absent_payload_field_parses_as_default() {
        // The producer opted out of provenance — no operator flag, no
        // CLI inference matched. `Default` is the documented absent-case
        // sentinel.
        let got = parse_classification_floor_source_from_payload(None).unwrap();
        assert_eq!(got, ClassificationFloorSource::Default);
    }

    #[test]
    fn operator_string_parses_as_operator() {
        let v = json!("operator");
        let got = parse_classification_floor_source_from_payload(Some(&v)).unwrap();
        assert_eq!(got, ClassificationFloorSource::Operator);
    }

    #[test]
    fn cli_inferred_string_parses_as_cli_inferred() {
        let v = json!("cli_inferred");
        let got = parse_classification_floor_source_from_payload(Some(&v)).unwrap();
        assert_eq!(got, ClassificationFloorSource::CliInferred);
    }

    #[test]
    fn default_string_parses_as_default() {
        let v = json!("default");
        let got = parse_classification_floor_source_from_payload(Some(&v)).unwrap();
        assert_eq!(got, ClassificationFloorSource::Default);
    }

    #[test]
    fn agent_raised_string_is_rejected_as_reserved() {
        // Issue #71: producers must not be able to forge `agent_raised`
        // provenance. The inner loop's `apply_floor_raise` is the only
        // legitimate writer. The error string must mention "reserved"
        // so an operator searching the daemon journal can find this
        // site without reading the code.
        let v = json!("agent_raised");
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains("agent_raised"),
            "error must echo the rejected value: {err}",
        );
        assert!(
            err.contains("reserved") || err.contains("apply_floor_raise"),
            "error must mention why the value is rejected: {err}",
        );
    }

    #[test]
    fn non_string_payload_value_is_rejected_as_shape_error() {
        let v = json!(42);
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains("not a string"),
            "shape error must surface as 'not a string': {err}",
        );
    }

    #[test]
    fn unknown_string_value_is_rejected_with_value_echoed() {
        let v = json!("garbage");
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains("garbage"),
            "error must echo the bad value: {err}",
        );
        assert!(
            err.contains("unknown") || err.contains("expected one of"),
            "error must name the contract: {err}",
        );
    }

    #[test]
    fn agent_raised_reject_binds_to_enum_variant_not_string_literal() {
        // Defense-in-depth pin: the reject inside
        // `parse_classification_floor_source_from_payload` matches on
        // the parsed `ClassificationFloorSource::AgentRaised` variant.
        // Feeding it the canonical wire form via `as_snake_str()`
        // exercises the same path a forging producer would. If a future
        // refactor rewires the reject to a hard-coded string literal,
        // and someone separately renames `AgentRaised` + its serde tag
        // (which `as_snake_str_matches_serde_wire_form` in `inner_loop`
        // forces to stay in lockstep), the literal would silently go
        // out of date — this test would still catch it because the
        // input is derived from the enum, not a constant.
        let wire = ClassificationFloorSource::AgentRaised.as_snake_str();
        let v = json!(wire);
        let err = parse_classification_floor_source_from_payload(Some(&v)).unwrap_err();
        assert!(
            err.contains(wire),
            "error must echo the rejected wire form {wire:?}: {err}",
        );
        assert!(
            err.contains("reserved") || err.contains("apply_floor_raise"),
            "error must name the contract: {err}",
        );
    }

    fn resolved_ask(choice: &str) -> Ask {
        Ask {
            id: 9,
            task_id: 3,
            kind: "plan_approval".to_string(),
            body: "this sends mail to a stranger".to_string(),
            options: serde_json::json!(["approve", "deny"]),
            plan_digest: Some("digest-a".to_string()),
            state: "resolved".to_string(),
            created_at: OffsetDateTime::now_utc(),
            deadline_at: OffsetDateTime::now_utc(),
            resolved_at: Some(OffsetDateTime::now_utc()),
            resolved_by: Some("operator".to_string()),
            resolution: Some(serde_json::json!({"choice": choice})),
            resume_state: None,
        }
    }

    #[test]
    fn a_denial_terminates_the_task_before_any_planning() {
        let out = pre_plan_outcome(&[resolved_ask("deny")]).expect("a denial is terminal");
        match out {
            Outcome::Denied { ask_id, reason } => {
                assert_eq!(ask_id, 9);
                // The ask's body — the question the operator answered —
                // not the operator's own note (spec D10).
                assert_eq!(reason, "this sends mail to a stranger");
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn an_approval_does_not_terminate_the_task() {
        assert!(pre_plan_outcome(&[resolved_ask("approve")]).is_none());
    }

    #[test]
    fn no_resolved_ask_does_not_terminate_the_task() {
        assert!(pre_plan_outcome(&[]).is_none());
    }

    #[test]
    fn a_denial_anywhere_in_the_list_terminates_the_task() {
        // The list is newest-first, so an older denial sits BEHIND newer
        // approvals. Only the newest being consulted would let a task the
        // operator refused go on to plan and dispatch.
        let mut approved = resolved_ask("approve");
        approved.id = 11;
        let mut denied = resolved_ask("deny");
        denied.id = 9;
        let out = pre_plan_outcome(&[approved, denied]).expect("a denial is terminal");
        match out {
            Outcome::Denied { ask_id, .. } => assert_eq!(ask_id, 9, "the denying ask is named"),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn several_approvals_and_no_denial_do_not_terminate_the_task() {
        // The two-escalation shape: a task holding two live approvals must
        // proceed to planning, where the digest comparison decides.
        let mut first = resolved_ask("approve");
        first.id = 9;
        let mut second = resolved_ask("approve");
        second.id = 11;
        assert!(pre_plan_outcome(&[second, first]).is_none());
    }

    #[test]
    fn a_malformed_resolution_does_not_terminate_the_task() {
        // Fail toward running rather than toward a silent refusal: a task
        // killed by an unparseable row is far harder to diagnose than one
        // that escalates again.
        let mut a = resolved_ask("deny");
        a.resolution = Some(serde_json::json!({"choice": "maybe"}));
        assert!(pre_plan_outcome(&[a]).is_none());
    }

    #[test]
    fn resume_budget_carries_the_count_and_extends_the_allowance() {
        // A fresh task: nothing carried, budget is the lane default.
        assert_eq!(resume_budget(0, 5), (0, 5));
        // A task that escalated after 4 plans and was approved: the column
        // keeps its 4 (so `increment_plan_count`'s absolute write no longer
        // rewinds it to 1) and the approved plan gets a full 5 more.
        assert_eq!(resume_budget(4, 5), (4, 9));
    }

    #[test]
    fn resume_budget_survives_impossible_column_values() {
        // plan_count is a signed column; a negative would make `as u32`
        // wrap to ~4 billion and the cap check pass forever.
        assert_eq!(resume_budget(-1, 5), (0, 5));
        // And the extension must not overflow into a tiny budget.
        assert_eq!(resume_budget(i32::MAX, u32::MAX).1, u32::MAX);
    }

    #[test]
    fn rejected_agent_raised_diagnostic_does_not_list_it_in_the_expected_set() {
        // Defense-in-depth pin: the "unknown value" message lists the
        // producer-legal set (operator / cli_inferred / default).
        // A future refactor that drops the explicit `agent_raised`
        // reject and falls back to the generic parser would silently
        // re-allow producer-supplied `agent_raised` — pin the contract
        // here. Asserts the dedicated reject message does NOT contain
        // the substring "expected one of" (it lives in a different code
        // path), and that the generic "unknown" message does NOT list
        // `agent_raised`.
        let agent_raised_err = parse_classification_floor_source_from_payload(
            Some(&json!("agent_raised")),
        )
        .unwrap_err();
        assert!(
            !agent_raised_err.contains("expected one of"),
            "agent_raised reject must use the dedicated message, not the generic \
             'expected one of': {agent_raised_err}",
        );
        let unknown_err =
            parse_classification_floor_source_from_payload(Some(&json!("nope"))).unwrap_err();
        assert!(
            !unknown_err.contains("agent_raised"),
            "the 'unknown' diagnostic must not advertise agent_raised as a \
             producer-legal value: {unknown_err}",
        );
    }
}
