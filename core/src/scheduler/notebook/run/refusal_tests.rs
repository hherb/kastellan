//! A refused answer and the reviewer's verdict on it (#993: ported from the
//! retired classic loop). The precedence is the shared
//! `execute::review_and_dispatch`'s; these pin that the notebook reaches it.
//! Each runs on the cluster `super::tests::the_notebook_loop` brings up.

use serde_json::json;

use crate::cassandra::types::Verdict;
use crate::scheduler::notebook::test_harness::*;

/// An answer carrying a refusal (`"refused"`). The verdict is the caller's:
/// `run` approves, `run_reviewed` adds one.
fn refusal(body: &str, principle: u8, reason: &str) -> serde_json::Value {
    json!({"steps": steps(&[1]), "answer": {"body": body, "refused": {"principle": principle, "reason": reason}}})
}

/// A refused answer ends the task `Refused` with the model's principle,
/// reason and body, dispatching nothing; its `plan.formulate` row says so.
pub(super) async fn a_refused_answer_ends_the_task_refused(pool: &sqlx::PgPool) {
    let script = Script::new(&[refusal("I cannot help with that; it would risk physical harm.", 1, "physical_harm")]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    match &r.result.outcome {
        Outcome::Refused { principle, reason, body } => {
            assert_eq!((*principle, reason.as_str()), (1, "physical_harm"));
            assert!(body.contains("physical harm"), "{body}");
        }
        o => panic!("expected Refused, got {o:?}"),
    }
    assert_eq!(r.result.outcome.final_state(), Some("refused"));
    let payload = r.result.outcome.result_payload().expect("Refused carries a payload");
    assert_eq!((payload["kind"].as_str(), payload["principle"].as_u64()), (Some("refused"), Some(1)));
    assert_eq!(payload["reason"], "physical_harm");
    assert!(payload["body"].as_str().unwrap().contains("physical harm"), "{payload}");
    assert_eq!(r.result.dispatch_count, 0, "a refusal dispatches nothing");
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    assert_eq!(formulated.len(), 1, "{formulated:#?}");
    assert_eq!(formulated[0]["decision_kind"], "refused");
    assert_eq!(formulated[0]["refused"], json!({"principle": 1, "reason": "physical_harm"}));
    assert_eq!(formulated[0]["plan_step_count"], 0);
    // The plan the row stores carries the refusal too, with the task's floor.
    assert_eq!(formulated[0]["plan"]["refused"], json!({"principle": 1, "reason": "physical_harm"}));
    assert_eq!(formulated[0]["classification_floor"], "Public");
    // Present as JSON null, so a JSONB `?` query finds the key on every row.
    assert!(formulated[0].get("l1_insight").is_some_and(|v| v.is_null()), "{:#?}", formulated[0]);
}

/// An ordinary answer's `plan.formulate` row carries `refused` as JSON null
/// (present, not absent), so observation queries can rely on the key.
pub(super) async fn an_answer_row_carries_refused_as_null(pool: &sqlx::PgPool) {
    let script = Script::new(&[json!({"steps": steps(&[1]), "answer": {"body": "done"}})]);
    let r = run(pool, &script, new_task(pool, 3).await).await;
    assert_eq!(r.result.outcome.final_state(), Some("completed"), "{:?}", r.result.outcome);
    let formulated = rows(pool, r.task_id, "agent", "plan.formulate").await;
    assert_eq!(formulated.len(), 1, "{formulated:#?}");
    assert_eq!(formulated[0]["decision_kind"], "task_complete");
    assert!(formulated[0].get("refused").is_some_and(|v| v.is_null()), "{:#?}", formulated[0]);
}

/// The reviewer's constitutional block wins over the model's own refusal:
/// the task ends `Blocked` with the reviewer's principle, not `Refused`.
pub(super) async fn a_reviewer_constitutional_block_wins_over_a_refusal(pool: &sqlx::PgPool) {
    let script = Script::new(&[refusal("agent prose mentioning P1", 1, "physical_harm_agent_side")]);
    let cb = Verdict::ConstitutionalBlock { principle: 3, reason: "irreversible_action_no_HITL".into() };
    let r = run_reviewed(pool, &script, new_task(pool, 3).await, Some(cb)).await;
    match &r.result.outcome {
        Outcome::Blocked { principle, reason } => {
            assert_eq!((*principle, reason.as_str()), (3, "irreversible_action_no_HITL"));
        }
        o => panic!("expected Blocked (the reviewer wins), got {o:?}"),
    }
    assert_eq!(r.result.outcome.final_state(), Some("blocked"));
}

/// A plain `Block` does not override a refusal, and the refused answer is
/// not sent back for another turn: the task ends `Refused` after one turn,
/// and the block is still recorded.
pub(super) async fn a_block_on_a_refused_answer_does_not_loop(pool: &sqlx::PgPool) {
    let script = Script::new(&[refusal("I will not proceed — privacy boundary.", 4, "privacy_violation")]);
    let block = Verdict::Block("reviewer flagged; refusal still stands".into());
    let r = run_reviewed(pool, &script, new_task(pool, 3).await, Some(block)).await;
    match &r.result.outcome {
        Outcome::Refused { principle, reason, .. } => assert_eq!((*principle, reason.as_str()), (4, "privacy_violation")),
        o => panic!("expected Refused, got {o:?}"),
    }
    assert_eq!(script.messages().len(), 1, "one turn: a blocked refusal is not re-asked");
    assert_eq!(r.result.dispatch_count, 0);
    let verdicts = rows(pool, r.task_id, "cassandra:chain", "verdict").await;
    assert_eq!(verdicts.len(), 1, "{verdicts:#?}");
    assert_eq!(verdicts[0]["verdict_kind"], "block");
}
