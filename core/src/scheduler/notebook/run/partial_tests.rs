//! The answer code writes when the forced-answer turn fails (#1044), through
//! the whole loop. Each runs on the cluster `super::tests::the_notebook_loop`
//! brings up.

use kastellan_db::notebook::EntryKind;
use serde_json::json;

use crate::cassandra::types::Verdict;
use crate::scheduler::notebook::partial::{CODE_AUTHOR, PARTIAL_HEAD};
use crate::scheduler::notebook::sift::chain::tests::{params, sift_record};
use crate::scheduler::notebook::test_harness::*;

/// DGX task 464's shape after a resume: no finding, a sift that kept rows,
/// and no usable turn in this run (it timed out, then so did the forced
/// turn). Code answers, the kept rows' table attached; its audit row names
/// no model.
pub(super) async fn a_resumed_run_with_no_usable_turn_answers_from_kept_rows(pool: &sqlx::PgPool) {
    let mut ctx = new_task(pool, 5).await;
    let sb = format!("sb:{}:1", ctx.task_id);
    ctx.plans = vec![sift_record(params(0, "an invoice"), json!({"scrapbook": sb, "kept": 7}))];
    let script = Script::raw(&[TIMEOUT.into(), TIMEOUT.into()]);
    let r = run(pool, &script, ctx).await;
    let b = body(&r.result);
    assert!(b.starts_with(PARTIAL_HEAD), "{b}");
    assert!(b.len() > PARTIAL_HEAD.len(), "the kept rows' table is attached, not the head alone: {b}");
    let why: Vec<_> = discarded(pool, r.task_id).await.iter().map(|v| v["reason"].clone()).collect();
    assert_eq!(why, [json!("forced_answer_failed_partial_used")]);
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    let last = formulated.last().expect("the code-written answer's row");
    assert_eq!((&last["llm_model"], &last["llm_backend"]), (&json!(CODE_AUTHOR), &json!(CODE_AUTHOR)), "{last}");
}

/// Only the forced-answer turn is answered by code: a routine turn that
/// fails after a tick fails the task, as before.
pub(super) async fn a_routine_turn_that_fails_after_a_tick_still_fails(pool: &sqlx::PgPool) {
    let script = Script::raw(&[
        json!({"steps": steps(&[1, 2]), "call": call(1, "a")}).to_string(),
        json!({"tick": tick(1, "f"), "call": call(2, "b")}).to_string(),
        "Sure! Here is what I found.".into(),
        "Sure! Here it is again.".into(),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert!(matches!(r.result.outcome, Outcome::Failed(_)), "{:?}", r.result.outcome);
    assert!(discarded(pool, r.task_id).await.is_empty(), "no answer was written by code");
}

/// The code-written answer is reviewed like any answer: a block on it means
/// no answer, and the notebook stays open.
pub(super) async fn a_blocked_partial_answer_is_not_the_answer(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2, 3]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f"), "call": call(2, "b")}),
        json!({"tick": tick(2, "g"), "call": call(3, "c")}),
        json!({"tick": tick(2, "g"), "call": call(3, "d")}),
    ]);
    let block = Verdict::Block("reviewer flagged the partial answer".into());
    let r = run_reviewed(pool, &script, new_task(pool, 1).await, Some(block)).await;
    assert!(matches!(r.result.outcome, Outcome::Failed(_)), "{:?}", r.result.outcome);
    assert_ne!(kinds(pool, r.task_id).await.last(), Some(&EntryKind::Answer), "a blocked answer does not close the notebook");
    let verdicts = rows(pool, r.task_id, "cassandra:chain", "verdict").await;
    assert_eq!(verdicts.last().expect("the answer was reviewed")["verdict_kind"], "block");
}
