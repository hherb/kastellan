//! The selective-thinking rewrite (#1019, #1023): a routine turn's answer
//! is rewritten by a thinking turn, and the draft is the answer when the
//! rewrite gives none. Moved out of `tests.rs` (size cap); runs on the
//! cluster `super::tests::the_notebook_loop` brings up.

use kastellan_llm_router::ThinkingPolicy;
use serde_json::{json, Value};

use super::*;
use crate::scheduler::notebook::test_harness::*;

/// With thinking off in the config, only the plan, the replan after a failed
/// call and the answer think; the answer a routine (non-thinking) turn wrote
/// is replaced by one a thinking turn writes, and its tick is kept.
pub(super) async fn selective_thinking_thinks_on_judgement_turns_and_rewrites_the_answer(pool: &sqlx::PgPool) {
    use crate::scheduler::notebook::thinking::WRITE_ANSWER_DIRECTIVE;
    use ThinkingPolicy::*;
    let mut script = Script::new(&[
        json!({"steps": steps(&[1, 2, 3]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f1"), "call": call(2, "fail")}),
        json!({"steps": steps(&[3]), "call": call(3, "c")}),
        json!({"tick": tick(3, "f3"), "answer": {"body": "a quick draft"}}),
        json!({"tick": tick(3, "again"), "answer": {"body": "the considered answer"}}),
    ]);
    script.thinks = false;
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "the considered answer");
    assert_eq!(*script.thinking.lock().unwrap(), [Allow, RouterDefault, Allow, RouterDefault, Allow]);
    let last: Value = serde_json::from_str(script.messages().last().unwrap()).unwrap();
    assert_eq!(last["turn_budget"], json!({"directive": WRITE_ANSWER_DIRECTIVE}));
    use EntryKind::*;
    assert_eq!(
        kinds(pool, r.task_id).await,
        vec![Steps, Tick, FailedStep, Steps, Tick, Answer],
        "the draft's tick is kept; the rewrite's repeated tick is dropped"
    );
    let answer = db::entries(pool, r.task_id, 0).await.unwrap().pop().unwrap();
    assert_eq!(answer.payload["body"], "the considered answer");
    let rows = discarded(pool, r.task_id).await;
    assert_eq!(rows.len(), 1, "the draft's turn is accounted for: {rows:?}");
    assert_eq!(rows[0]["reason"], "answer_rewritten");

    // A backend that thinks anyway: no rewrite, the first answer stands.
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f"), "answer": {"body": "first"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "first");
    assert_eq!(*script.thinking.lock().unwrap(), [Allow, RouterDefault]);

    // The rewrite turn calls, and calls on the re-ask (#1019 review): the
    // draft it was to rewrite is the answer (#1023), its tick kept once.
    let mut script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f"), "answer": {"body": "a quick draft"}}),
        json!({"call": call(1, "b")}),
        json!({"call": call(1, "c")}),
    ]);
    script.thinks = false;
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "a quick draft");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["a"], "neither rewrite-turn call ran");
    assert_eq!(kinds(pool, r.task_id).await, vec![Steps, Tick, Answer]);

    // The rewrite turn times out: the draft is the answer, not a forced turn.
    let mut script = Script::raw(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}).to_string(),
        json!({"tick": tick(1, "f"), "answer": {"body": "a quick draft"}}).to_string(),
        TIMEOUT.into(),
    ]);
    script.thinks = false;
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(body(&r.result), "a quick draft");
    assert_eq!(script.messages().len(), 3, "no forced-answer turn after the timed-out rewrite");
    let reasons: Vec<Value> = discarded(pool, r.task_id).await.into_iter().map(|r| r["reason"].clone()).collect();
    assert_eq!(reasons, ["answer_rewritten", "answer_rewrite_failed_draft_used"], "the audit says the draft shipped");
    assert_eq!(r.result.plan_count, 2, "the failed rewrite is not counted as a turn");

    // The draft spends the last turn (review finding): no turn is left to
    // rewrite it, so it is the answer — not a failed task, nor a forced turn.
    let mut script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "f"), "answer": {"body": "a quick draft"}}),
    ]);
    script.thinks = false;
    let r = run(pool, &script, new_task(pool, 1).await).await;
    assert_eq!(body(&r.result), "a quick draft");
    assert_eq!(script.messages().len(), 2, "no turn after the cap");
    assert_eq!(kinds(pool, r.task_id).await, vec![Steps, Tick, Answer]);
}
