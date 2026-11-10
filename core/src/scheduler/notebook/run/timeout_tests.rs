//! A notebook turn that times out, and a task run past the fast lane's
//! deadline (#993: ported from the classic loop's `timeout_tests` before it
//! was retired). The timeouts are **real** `reqwest` request timeouts
//! ([`real_request_timeout`]: a router pointed at a listener that accepts and
//! never answers), because a hand-built error could satisfy
//! `is_request_timeout` in a way production never produces. Each runs on the
//! cluster `super::tests::the_notebook_loop` brings up.
//!
//! The decisions themselves are unit-tested in `inner_loop/llm_failure/tests.rs`.

use std::time::{Duration, Instant};

use serde_json::json;

use super::ANSWER_DIRECTIVE;
use crate::scheduler::notebook::test_harness::*;

fn failed(r: &InnerLoopResult) -> &str {
    match &r.outcome {
        Outcome::Failed(d) => d,
        o => panic!("expected Failed, got {o:?}"),
    }
}

/// A turn that times out after something was gathered spends the
/// forced-answer turn, which answers from what was gathered; the timed-out
/// call is counted and audited, and produced no turn.
pub(super) async fn a_timeout_after_gathering_answers_from_what_was_gathered(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        TIMEOUT.into(),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A"}}).to_string(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!((r.result.plan_count, r.result.dispatch_count), (2, 1), "the timed-out call produced no turn");
    assert_eq!(r.result.failed_llm_calls, 1, "…but it is a model call, counted separately");
    let last: serde_json::Value = serde_json::from_str(script.messages().last().unwrap()).unwrap();
    assert_eq!(last["turn_budget"], json!({"directive": ANSWER_DIRECTIVE}), "the answer came from the forced turn");
    let failed_rows = rows(pool, r.task_id, "agent", "plan.formulate_failed").await;
    assert_eq!(failed_rows.len(), 1, "{failed_rows:?}");
    assert_eq!(failed_rows[0]["request_timeout"], true);
    assert_eq!(failed_rows[0]["synth_turn"], false);
    assert!(failed_rows[0].get("llm_usage").is_none(), "a timeout got no completion to cost: {}", failed_rows[0]);
}

/// The forced-answer turn itself times out (classic case 3): it is not forced
/// again, and the task fails in the timeout's words, naming what was gathered.
pub(super) async fn a_timeout_on_the_answer_turn_fails_naming_what_was_gathered(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        TIMEOUT.into(),
        TIMEOUT.into(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    let d = failed(&r.result);
    assert!(d.starts_with("The language model ran out of time before it could write an answer"), "{d}");
    assert!(d.contains("1 tool call (1 succeeded)"), "{d}");
    assert_eq!(script.messages().len(), 3, "the answer turn was asked once, not again");
    assert_eq!(r.result.failed_llm_calls, 2);
    let failed_rows = rows(pool, r.task_id, "agent", "plan.formulate_failed").await;
    assert_eq!(failed_rows.len(), 2, "{failed_rows:?}");
    assert_eq!(failed_rows[1]["synth_turn"], true, "{}", failed_rows[1]);
}

/// A timeout before anything was gathered has nothing to answer from: the
/// task fails in words, and no forced-answer turn is asked for.
pub(super) async fn a_timeout_before_gathering_fails_in_words(pool: &sqlx::PgPool) {
    let script = Script::raw(&[TIMEOUT.into(), json!({"answer": {"body": "unused"}}).to_string()]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    let d = failed(&r.result);
    assert!(d.starts_with("The language model ran out of time"), "{d}");
    assert!(d.contains("no tool was called"), "{d}");
    assert!(d.contains("timed out"), "the timeout query still finds it: {d}");
    assert_eq!(script.messages().len(), 1, "no second turn");
    assert_eq!((r.result.failed_llm_calls, r.result.plan_count), (1, 0));
}

/// A re-asked reply (#945) that times out after a gather is like any timeout:
/// the forced-answer turn answers from what was gathered.
pub(super) async fn a_re_ask_that_times_out_after_gathering_spends_the_answer_turn(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}).to_string(),
        "Sure! Here is what I found.".into(),
        TIMEOUT.into(),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "from the forced turn"}}).to_string(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "from the forced turn");
    assert_eq!(script.reasks.lock().unwrap().len(), 1, "the prose was shown back once");
    assert_eq!(r.result.failed_llm_calls, 2, "the prose and the re-ask's timeout");
    let failed_rows = rows(pool, r.task_id, "agent", "plan.formulate_failed").await;
    assert_eq!(failed_rows.len(), 2, "{failed_rows:?}");
    assert!(failed_rows[0]["raw_head"].as_str().is_some_and(|h| h.starts_with("Sure!")), "{}", failed_rows[0]);
    assert!(failed_rows[0].get("latency_ms").is_some(), "{}", failed_rows[0]);
    assert_eq!(failed_rows[1]["request_timeout"], true, "{}", failed_rows[1]);
    assert_eq!(failed_rows[1]["thinking_suppressed_retry"], true, "{}", failed_rows[1]);
    let thinking = script.thinking.lock().unwrap();
    assert_eq!(thinking[2], kastellan_llm_router::ThinkingPolicy::Suppress, "the re-ask runs with thinking off: {thinking:?}");
}

/// The re-ask times out with nothing gathered: the task fails in the
/// timeout's words, and says the first reply could not be used.
pub(super) async fn a_re_ask_that_times_out_before_gathering_fails_in_words(pool: &sqlx::PgPool) {
    let script = Script::raw(&["Sure! Here is what I would do.".into(), TIMEOUT.into()]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    let d = failed(&r.result);
    assert!(d.starts_with("The language model's first reply could not be used, so it was asked again. "), "{d}");
    assert!(d.contains("ran out of time before it finished planning"), "the timeout's own wording: {d}");
    assert!(d.contains("timed out"), "the timeout query still finds it: {d}");
    assert_eq!(r.result.failed_llm_calls, 2);
}

/// The fast lane's deadline passes during a gathering turn: the next turn is
/// the forced answer, exactly as at the turn cap (#922), far below the cap.
pub(super) async fn the_deadline_passing_after_a_gather_spends_the_answer_turn(pool: &sqlx::PgPool) {
    let mut script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "out of time"}}),
    ]);
    script.turn_delay = Duration::from_millis(300);
    let mut ctx = new_task(pool, 5).await;
    ctx.deadline = Some(Instant::now() + Duration::from_millis(100));
    let r = run(pool, &script, ctx).await;
    assert_eq!(body(&r.result), "out of time");
    assert_eq!((r.result.plan_count, r.result.dispatch_count), (2, 1), "gather turn + answer turn");
    let last: serde_json::Value = serde_json::from_str(script.messages().last().unwrap()).unwrap();
    assert_eq!(last["turn_budget"], json!({"directive": ANSWER_DIRECTIVE}));
}

/// Nothing gathered when the deadline passes: the task fails like the cap,
/// naming the ceiling, and the model is asked nothing.
pub(super) async fn the_deadline_with_nothing_gathered_fails_naming_the_ceiling(pool: &sqlx::PgPool) {
    let script = Script::new(&[json!({"answer": {"body": "never reached"}})]);
    let mut ctx = new_task(pool, 5).await;
    ctx.deadline = Some(Instant::now());
    let r = run(pool, &script, ctx).await;
    assert_eq!(failed(&r.result), "plan_time_ceiling_exceeded");
    assert_eq!(r.result.plan_count, 0);
    assert!(script.messages().is_empty(), "no turn after the deadline");
}
