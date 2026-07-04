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

use kastellan_db::tasks::Task;

use crate::cassandra::review::ChainReviewStage;
use crate::cassandra::types::DataClass;
use crate::scheduler::agent::PlanFormulator;
use crate::scheduler::inner_loop::{
    run_to_terminal, ClassificationFloorSource, InnerLoopResult, Outcome, StepDispatcher,
    TaskContext,
};

pub(super) async fn run_one(
    pool: &PgPool,
    formulator: Arc<dyn PlanFormulator>,
    review: Arc<ChainReviewStage>,
    dispatcher: Arc<dyn StepDispatcher>,
    task: &Task,
    max_plans: u32,
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

    let ctx = TaskContext {
        task_id: task.id,
        lane: task.lane,
        instruction,
        classification_floor,
        classification_floor_source,
        classification_floor_signals,
        plans: vec![],
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: max_plans_override,
    };

    let task_id = ctx.task_id;
    let dispatcher_for_purge = std::sync::Arc::clone(&dispatcher);
    let result = match run_to_terminal(pool, formulator, review, dispatcher, ctx).await {
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
