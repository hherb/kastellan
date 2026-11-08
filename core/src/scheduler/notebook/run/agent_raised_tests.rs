//! The agent-raised plan features on the notebook loop (#996: ported from
//! the classic loop before it is retired, #993). The reply carries each one
//! on its `answer` (or `call`); the shared `execute::review_and_dispatch`
//! acts on it exactly as on a classic plan. Each runs on the cluster
//! `super::tests::the_notebook_loop` brings up.

use serde_json::json;

use crate::scheduler::notebook::test_harness::*;

const INSIGHT: &str = "mail.search needs a date range or it pages the whole archive";

/// A completed answer's `l1_insight` reaches the runner (which promotes it to
/// L1) and its `plan.formulate` row.
pub(super) async fn an_answers_l1_insight_reaches_the_runner(pool: &sqlx::PgPool) {
    let script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "A", "l1_insight": INSIGHT}}),
    ]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_l1_insight.as_deref(), Some(INSIGHT));
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    assert_eq!(formulated.last().unwrap()["l1_insight"], INSIGHT, "{formulated:#?}");
}

/// A refused answer ends the task `Refused`, so its insight is dropped, as on
/// a classic plan: only a completed task teaches a lesson.
pub(super) async fn a_refused_answers_l1_insight_is_dropped(pool: &sqlx::PgPool) {
    let refused = json!({"principle": 1, "reason": "physical_harm"});
    let script = Script::new(&[json!({"steps": steps(&[1]), "answer": {"body": "no", "refused": refused, "l1_insight": INSIGHT}})]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(r.result.outcome.final_state(), Some("refused"), "{:?}", r.result.outcome);
    assert_eq!(r.result.terminal_l1_insight, None);
}

/// An answer set aside on a routine turn is written again on a thinking turn:
/// the insight is the rewritten answer's, never the discarded one's.
pub(super) async fn a_rewritten_answer_keeps_only_its_own_insight(pool: &sqlx::PgPool) {
    let mut script = Script::new(&[
        json!({"steps": steps(&[1]), "call": call(1, "a")}),
        json!({"tick": tick(1, "invoice A"), "answer": {"body": "draft", "l1_insight": "the draft's lesson"}}),
        json!({"answer": {"body": "A", "l1_insight": INSIGHT}}),
    ]);
    script.thinks = false;
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(body(&r.result), "A");
    assert_eq!(r.result.terminal_l1_insight.as_deref(), Some(INSIGHT));
}
