//! A thinking turn that spends its whole `max_tokens` thinking and says
//! nothing (#1047), and what a failed call's row then records, through the
//! whole loop. Each runs on the cluster `super::tests::the_notebook_loop`
//! brings up.

use kastellan_llm_router::ThinkingPolicy;
use serde_json::{json, Value};

use super::ANSWER_DIRECTIVE;
use crate::scheduler::notebook::partial::PARTIAL_HEAD;
use crate::scheduler::notebook::test_harness::*;

/// The task's `plan.formulate_failed` rows, oldest first.
async fn failed_rows(pool: &sqlx::PgPool, task_id: i64) -> Vec<Value> {
    rows(pool, task_id, "agent", "plan.formulate_failed").await
}

/// DGX task 491 (#1047): a thinking turn spent all 16384 tokens thinking and
/// said nothing. The same turn is asked again once with thinking suppressed —
/// it had the room for a reply, only not for the thinking — instead of
/// spending the forced-answer turn (after gathering) or failing (before).
pub(super) async fn a_thinking_runaway_is_asked_again_without_thinking(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        RUNAWAY.into(),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A"}}).to_string(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.failed_llm_calls, 1);
    let thinking = script.thinking.lock().unwrap().clone();
    assert_eq!(thinking, [ThinkingPolicy::Allow, ThinkingPolicy::RouterDefault, ThinkingPolicy::Suppress]);
    assert!(script.reasks.lock().unwrap().is_empty(), "asked again as it was, not shown back with a reason");
    let m = script.messages();
    assert_eq!(m[1], m[2], "the same turn is asked again");
    let last: Value = serde_json::from_str(&m[2]).unwrap();
    assert_ne!(last["turn_budget"], json!({"directive": ANSWER_DIRECTIVE}), "not the forced-answer turn: {last}");
    let failed = failed_rows(pool, r.task_id).await;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!((&failed[0]["thinking"], &failed[0]["thinking_suppressed_retry"]), (&json!(true), &json!(false)));
    assert_eq!(failed[0]["llm_usage"]["finish_reason"], "length");

    // Before anything was gathered the first turn is asked again too: the
    // plan is written without thinking, and later turns think again.
    let script = Script::raw(&[
        RUNAWAY.into(),
        json!({"steps": steps(&[1]), "call": call(1, "a")}).to_string(),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A"}}).to_string(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "A");
    let thinking = script.thinking.lock().unwrap().clone();
    assert_eq!(thinking, [ThinkingPolicy::Allow, ThinkingPolicy::Suppress, ThinkingPolicy::RouterDefault]);
}

/// The re-ask without thinking ran out too: that is out of room, as a
/// runaway was before #1047 — after gathering the forced-answer turn is
/// spent, before it the task fails saying so.
pub(super) async fn a_runaway_re_ask_that_runs_out_again_is_out_of_room(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        RUNAWAY.into(),
        RUNAWAY.into(),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A"}}).to_string(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.failed_llm_calls, 2);
    let last: Value = serde_json::from_str(script.messages().last().unwrap()).unwrap();
    assert_eq!(last["turn_budget"], json!({"directive": ANSWER_DIRECTIVE}));

    let script = Script::raw(&[RUNAWAY.into(), RUNAWAY.into()]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    let Outcome::Failed(d) = &r.result.outcome else { panic!("{:?}", r.result.outcome) };
    assert!(d.contains("The language model used its whole reply budget"), "{d}");
    assert_eq!(script.messages().len(), 2, "asked again once, not twice");
    let failed = failed_rows(pool, r.task_id).await;
    let flags: Vec<_> = failed.iter().map(|v| (v["thinking"].clone(), v["thinking_suppressed_retry"].clone())).collect();
    assert_eq!(flags, [(json!(true), json!(false)), (json!(false), json!(true))], "{failed:?}");
}

/// The forced-answer turn never thinks, so a blank cut-off there is no
/// thinking runaway: it is not asked again, and code answers from the
/// findings (#1044).
pub(super) async fn a_runaway_forced_answer_turn_is_not_asked_again(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        json!({"tick": tick(1, "invoice A"), "call": call(2, "b")}).to_string(),
        RUNAWAY.into(),
    ]);
    // max_plans 1 → 2 turns; turn 3 is the forced answer.
    let r = run(pool, &script, new_task(pool, 1).await).await;
    assert!(body(&r.result).starts_with(PARTIAL_HEAD), "{}", body(&r.result));
    assert_eq!(script.messages().len(), 3, "the forced-answer turn is not asked again");
    let failed = failed_rows(pool, r.task_id).await;
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0]["thinking"], false);
}

/// #1047's three 600 s timeouts carried no `llm_usage` (a timed-out call
/// gets no body back), so the row could not say what the call was. It now
/// says how long the call ran and whether it let the model think.
pub(super) async fn a_timed_out_calls_row_says_how_long_it_ran_and_whether_it_thought(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        TIMEOUT.into(),
        TIMEOUT.into(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert!(matches!(r.result.outcome, Outcome::Failed(_)), "{:?}", r.result.outcome);
    let failed = failed_rows(pool, r.task_id).await;
    assert_eq!(failed.len(), 2, "the routine turn, then the forced answer: {failed:?}");
    for (row, thought) in failed.iter().zip([true, false]) {
        assert_eq!((&row["request_timeout"], &row["thinking"]), (&json!(true), &json!(thought)), "{row}");
        // The scripted router times out after 150 ms.
        assert!(row["elapsed_ms"].as_u64().is_some_and(|ms| ms >= 150), "{row}");
        assert!(row.get("llm_usage").is_none(), "a timeout has no completion: {row}");
    }
}
