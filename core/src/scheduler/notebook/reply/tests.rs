use super::*;
use crate::scheduler::notebook::state::fold;
use kastellan_db::notebook::{Entry, EntryKind};
use serde_json::{json, Value};

fn entry(seq: i64, kind: EntryKind, payload: Value) -> Entry {
    Entry { task_id: 1, seq, kind, payload, created_at: time::OffsetDateTime::UNIX_EPOCH }
}

fn step(id: u32) -> Value {
    json!({"id": id, "purpose": "p", "tool": "mail", "done_when": "d"})
}

/// A notebook with steps 1 and 2 listed, step 1 current.
fn two_steps() -> NotebookState {
    fold(&[entry(1, EntryKind::Steps, json!({"steps": [step(1), step(2)]}))]).unwrap()
}

fn call(step: u32) -> Value {
    json!({"step": step, "tool": "mail", "method": "mail.search", "parameters": {"q": "x"},
           "returns": "hits", "done_when": "listed", "classification": "Personal"})
}

fn tick(step: u32) -> Value {
    json!({"step": step, "note": "n", "summary": "s", "findings": [{"text": "f", "source": "m1"}]})
}

fn check(reply: Value, state: &NotebookState) -> Result<NotebookReply, String> {
    let r = parse_reply(&reply.to_string())?;
    validate(&r, state).map(|()| r)
}

#[test]
fn a_reply_with_both_a_call_and_an_answer_is_rejected() {
    let err = check(json!({"call": call(1), "answer": {"body": "b"}}), &two_steps()).unwrap_err();
    assert!(err.contains("exactly one"), "{err}");
}

#[test]
fn a_reply_with_no_call_answer_or_infeasible_is_rejected() {
    let err = check(json!({"tick": tick(1)}), &two_steps()).unwrap_err();
    assert!(err.contains("exactly one"), "{err}");
}

#[test]
fn ticking_a_step_that_is_not_current_is_rejected() {
    let err = check(json!({"tick": tick(2), "call": call(2)}), &two_steps()).unwrap_err();
    assert!(err.contains("not the current step 1"), "{err}");
    let empty = NotebookState::default();
    let err = check(json!({"steps": [step(1)], "tick": tick(1), "call": call(1)}), &empty).unwrap_err();
    assert!(err.contains("no current step"), "{err}");
}

#[test]
fn a_call_for_an_unknown_or_finished_step_is_rejected() {
    assert!(check(json!({"call": call(9)}), &two_steps()).unwrap_err().contains("not a pending step"));
    // Step 1 is ticked in this very reply, so calling it again is refused.
    let err = check(json!({"tick": tick(1), "call": call(1)}), &two_steps()).unwrap_err();
    assert!(err.contains("not a pending step"), "{err}");
}

#[test]
fn the_first_turn_must_list_steps() {
    let empty = NotebookState::default();
    assert!(check(json!({"call": call(1)}), &empty).unwrap_err().contains("must list the `steps`"));
    check(json!({"steps": [step(1)], "call": call(1)}), &empty).expect("steps plus the first call");
}

#[test]
fn a_tick_and_a_call_for_the_next_step_is_accepted() {
    let r = check(json!({"tick": tick(1), "call": call(2)}), &two_steps()).unwrap();
    assert_eq!(r.call.unwrap().step, 2);
}

#[test]
fn a_replan_cannot_reopen_a_done_step() {
    // Step 1 is ticked in this reply; re-listing it keeps it done, so a call
    // for it is refused.
    let err = check(json!({"tick": tick(1), "steps": [step(1), step(2)], "call": call(1)}), &two_steps()).unwrap_err();
    assert!(err.contains("not a pending step"), "{err}");
}

#[test]
fn a_replan_may_retry_a_failed_step() {
    let failed = fold(&[
        entry(1, EntryKind::Steps, json!({"steps": [step(1), step(2)]})),
        entry(2, EntryKind::FailedStep, json!({"step": 1, "code": "E", "detail": "d", "classification": "Public"})),
    ])
    .unwrap();
    check(json!({"steps": [step(1), step(2)], "call": call(1)}), &failed).expect("retry step 1");
}

#[test]
fn an_unknown_key_is_rejected() {
    assert!(parse_reply(r#"{"call": null, "answer": {"body": "b"}, "plan": []}"#).is_err());
}

#[test]
fn a_fenced_reply_parses() {
    let raw = format!("Here you go:\n```json\n{}\n```", json!({"answer": {"body": "done"}}));
    let r = parse_reply(&raw).unwrap();
    assert_eq!(r.answer.unwrap().body, "done");
}

#[test]
fn the_answer_ceiling_is_the_highest_of_floor_touched_and_declared() {
    use DataClass::*;
    assert_eq!(answer_ceiling(Public, &[Personal, ClinicalConfidential], Some(Public)), ClinicalConfidential);
    assert_eq!(answer_ceiling(Personal, &[], None), Personal);
    assert_eq!(answer_ceiling(Public, &[Public], Some(Secret)), Secret);
}

#[test]
fn the_answer_plan_is_terminal() {
    let p = answer_plan("the answer", DataClass::Personal, None);
    assert!(p.is_terminal());
    assert_eq!(p.data_ceiling, Some(DataClass::Personal));
    assert_eq!(p.result.unwrap()["body"], "the answer");
}

#[test]
fn the_call_plan_is_one_reviewable_step() {
    let r = check(json!({"tick": tick(1), "call": call(2)}), &two_steps()).unwrap();
    let p = call_plan(&r, r.call.as_ref().unwrap(), DataClass::Public);
    assert!(!p.is_terminal());
    assert_eq!(p.steps.len(), 1);
    assert_eq!((p.steps[0].tool.as_str(), p.steps[0].method.as_str()), ("mail", "mail.search"));
    assert_eq!(p.steps[0].classification, DataClass::Personal);
    assert_eq!(p.context, "s", "the tick's summary is the plan's context");
    assert_eq!(p.data_ceiling, Some(DataClass::Personal), "the call's own class");
    let p = call_plan(&r, r.call.as_ref().unwrap(), DataClass::Secret);
    assert_eq!(p.data_ceiling, Some(DataClass::Secret), "never below the floor");
}

#[test]
fn a_refusal_rides_on_the_answer() {
    let r = check(json!({"answer": {"body": "no", "refused": {"principle": 2, "reason": "impersonation"}}}), &two_steps())
        .unwrap();
    let a = r.answer.unwrap();
    let p = answer_plan(&a.body, DataClass::Public, a.refused);
    assert!(p.is_refused() && p.is_terminal());
}
