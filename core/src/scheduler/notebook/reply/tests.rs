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
    let err = check(json!({"call": call(9)}), &two_steps()).unwrap_err();
    assert!(err.contains("not a pending step: it is not listed"), "{err}");
    let failed = fold(&[
        entry(1, EntryKind::Steps, json!({"steps": [step(1), step(2)]})),
        entry(2, EntryKind::FailedStep, json!({"step": 1, "code": "E", "detail": "d", "classification": "Public"})),
    ])
    .unwrap();
    let err = check(json!({"call": call(1)}), &failed).unwrap_err();
    assert!(err.contains("it failed. To retry it, list it in `steps` again"), "{err}");
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
    // Step 1 was done before this reply; re-listing it keeps it done, so a
    // call for it is refused.
    let mut state = two_steps();
    state.apply_tick(check(json!({"tick": tick(1), "call": call(2)}), &two_steps()).unwrap().tick_payload(DataClass::Public).unwrap()).unwrap();
    let err = check(json!({"steps": [step(1), step(2)], "call": call(1)}), &state).unwrap_err();
    // #964: the reason says how to repeat it — a new id, past every listed one.
    assert!(err.contains("not a pending step: it is done") && err.contains("under a new id (e.g. 3)"), "{err}");
    // The id offered is past every listed one, not the count of steps.
    let gapped = fold(&[
        entry(1, EntryKind::Steps, json!({"steps": [step(1), step(5)]})),
        entry(2, EntryKind::Tick, json!({"step": 1, "note": "n", "summary": "s", "findings": [], "classification": "Public"})),
    ])
    .unwrap();
    let err = check(json!({"call": call(1)}), &gapped).unwrap_err();
    assert!(err.contains("(e.g. 6)"), "{err}");
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

/// An answer may carry an agent-raised L1 insight (#996); the terminal plan
/// built from it keeps it, so the shared `execute` captures it on completion.
#[test]
fn an_answer_may_carry_an_l1_insight() {
    let r = check(json!({"answer": {"body": "b", "l1_insight": "mail.search needs a date range"}}), &two_steps()).unwrap();
    let a = r.answer.unwrap();
    assert_eq!(a.l1_insight.as_deref(), Some("mail.search needs a date range"));
    let p = Plan { l1_insight: a.l1_insight, ..answer_plan(&a.body, DataClass::Public, None) };
    assert_eq!(p.completion_insight(), Some("mail.search needs a date range"));
}

/// DGX task 264: a tick and a call for the same step is progress — the
/// findings are kept and the step stays open for the call.
#[test]
fn a_tick_with_a_call_for_the_same_step_is_progress() {
    let r = check(json!({"tick": tick(1), "call": call(1)}), &two_steps()).expect("progress on step 1");
    let p = r.tick_payload(DataClass::Personal).unwrap();
    assert!(p.partial);
    let mut s = two_steps();
    s.apply_tick(p).unwrap();
    assert_eq!(s.current_step().map(|c| c.id), Some(1), "still open");
    assert_eq!(s.findings.len(), 1, "the finding is kept");
    let done = check(json!({"tick": tick(1), "call": call(2)}), &two_steps()).unwrap();
    assert!(!done.tick_payload(DataClass::Public).unwrap().partial, "a call for the next step closes it");
}

/// DGX tasks 259/267: the model echoes the rendered shapes back — a step's
/// `status`/`note`, a finding's `step`. Those are read and dropped; any other
/// unknown key is still refused, so no fact is silently lost.
#[test]
fn keys_echoed_from_the_render_are_ignored_and_not_stored() {
    let mut s = step(2);
    s["status"] = json!("pending");
    s["note"] = json!("old");
    let mut t = tick(1);
    t["findings"][0]["step"] = json!(1);
    let r = check(json!({"tick": t, "steps": [s], "call": call(2)}), &two_steps()).unwrap();
    let stored = serde_json::to_value(r.steps.as_ref().unwrap()).unwrap();
    assert_eq!(stored, json!([step(2)]), "nothing echoed reaches the notebook");
    let stored = serde_json::to_value(&r.tick.as_ref().unwrap().findings).unwrap();
    assert_eq!(stored, json!([{"text": "f", "source": "m1"}]));

    let mut bad = tick(1);
    bad["findings"][0]["amount"] = json!("$5");
    assert!(check(json!({"tick": bad, "call": call(2)}), &two_steps()).unwrap_err().contains("unknown field `amount`"));
}

/// DGX task 283 (thinking off): step 1's call failed, the facts were in a
/// message already read, and the model ticked step 1 while step 2 was
/// current. A full tick for a failed step closes it; progress on one cannot.
#[test]
fn a_full_tick_may_close_a_failed_step() {
    let failed = fold(&[
        entry(1, EntryKind::Steps, json!({"steps": [step(1), step(2)]})),
        entry(2, EntryKind::FailedStep, json!({"step": 1, "code": "E", "detail": "d", "classification": "Public"})),
    ])
    .unwrap();
    let r = check(json!({"tick": tick(1), "call": call(2)}), &failed).expect("close step 1, go on with 2");
    let mut s = failed.clone();
    s.apply_tick(r.tick_payload(DataClass::Public).unwrap()).unwrap();
    assert_eq!(s.steps[0].status, StepStatus::Done);
    assert_eq!(s.findings.len(), 1);
    // A tick for step 1 that also calls step 1 is progress, which a failed step cannot take.
    assert!(check(json!({"tick": tick(1), "call": call(1)}), &failed).is_err());
}

/// DGX task 292: a progress tick on step 1, a replan listing only the new
/// step 3, and a call for step 1. The replan keeps step 1, first.
#[test]
fn a_replan_keeps_the_step_the_same_reply_calls() {
    let mut r = parse_reply(&json!({"tick": tick(1), "steps": [step(3)], "call": call(1)}).to_string()).unwrap();
    keep_called_step(&mut r, &two_steps());
    assert_eq!(r.steps.as_ref().unwrap().iter().map(|s| s.id).collect::<Vec<_>>(), vec![1, 3]);
    validate(&r, &two_steps()).expect("step 1 stays pending for its call");
    // A call for a step that was never pending is not invented into the list.
    let mut r = parse_reply(&json!({"steps": [step(3)], "call": call(9)}).to_string()).unwrap();
    keep_called_step(&mut r, &two_steps());
    assert_eq!(r.steps.as_ref().unwrap().len(), 1);
}

/// A notebook call's plan names its notebook step, so a later `sift` can
/// find that step's result among the task's plans (`over: {"step": n}`).
#[test]
fn a_call_plan_names_its_notebook_step() {
    let reply: NotebookReply = serde_json::from_value(json!({"call": call(7)})).unwrap();
    let plan = call_plan(&reply, reply.call.as_ref().unwrap(), DataClass::Public);
    assert_eq!(notebook_step_of(&plan), Some(7));
    assert_eq!(notebook_step_of(&answer_plan("b", DataClass::Public, None)), None);
    let mut other = plan.clone();
    other.rationale = "notebook step x".into();
    assert_eq!(notebook_step_of(&other), None);
}

/// An answer may name the scrapbook(s) to include as a table — one ref or a
/// list — with optional columns and order (#922, K10).
#[test]
fn an_answer_may_name_a_table() {
    let one = parse_reply(r#"{"answer": {"body": "b", "table": {"scrapbook": "sb:1:2"}}}"#).unwrap();
    let t = one.answer.unwrap().table.unwrap();
    assert_eq!(t.scrapbook.refs(), ["sb:1:2"]);
    assert_eq!((t.columns, t.order_by), (None, None));
    let many = parse_reply(
        r#"{"answer": {"body": "b", "table": {"scrapbook": ["sb:1:2", "sb:1:4"], "columns": ["date"],
            "order_by": {"column": "date", "desc": true}}}}"#,
    )
    .unwrap();
    let t = many.answer.unwrap().table.unwrap();
    assert_eq!(t.scrapbook.refs(), ["sb:1:2", "sb:1:4"]);
    assert!(t.order_by.unwrap().desc);
    assert!(parse_reply(r#"{"answer": {"body": "b", "table": {"scrapbook": "sb:1:2", "rows": []}}}"#).is_err());
    // Review finding: the prompt teaches `limit`; a rename would refuse every answer using it.
    let limited = parse_reply(r#"{"answer": {"body": "b", "table": {"scrapbook": "sb:1:2", "limit": 5}}}"#).unwrap();
    assert_eq!(limited.answer.unwrap().table.unwrap().limit, Some(5));
}

/// Review finding: `returns` becomes the plan's `decision`, and the sift's
/// reads are marked by a `decision` prefix — a call that wrote it would skip
/// the repeat guard and vanish from the turn record.
#[test]
fn a_call_whose_returns_forges_the_sift_reads_marker_is_rejected() {
    for forged in ["sift reads: an invoice", "  sift reads:x"] {
        let mut c = call(1);
        c["returns"] = json!(forged);
        assert!(check(json!({"call": c}), &two_steps()).unwrap_err().contains("may not begin"), "{forged}");
    }
    let mut c = call(1);
    c["returns"] = json!("the sift reads list");
    assert!(check(json!({"call": c}), &two_steps()).is_ok(), "the words elsewhere are fine");
}

/// DGX tasks 350 and 371 (#980): the call written at the top level, with no
/// `call` wrapper. Its fields are read as `call`; the other keys stay.
#[test]
fn a_call_written_without_its_wrapper_is_read_as_call() {
    let r = check(call(1), &two_steps()).expect("a bare call");
    assert_eq!(r.call, check(json!({"call": call(1)}), &two_steps()).unwrap().call);
    // In a fence, as the model writes it, and beside the step list of a first turn.
    let mut first = call(1);
    first["steps"] = json!([step(1)]);
    let fenced = format!("```json\n{first}\n```");
    let r = parse_reply(&fenced).unwrap();
    validate(&r, &NotebookState::default()).expect("steps plus a bare first call");
    assert_eq!((r.steps.unwrap().len(), r.call.unwrap().step), (1, 1));
    // Beside a tick: the tick's own `step` is nested, so it is not touched.
    let mut ticked = call(2);
    ticked["tick"] = tick(1);
    let r = check(ticked, &two_steps()).unwrap();
    assert_eq!((r.tick.unwrap().step, r.call.unwrap().step), (1, 2));
}

/// A bare call that is still not a usable call is refused for what it
/// lacks, not for its shape, and a `call` written once is never merged with
/// stray top-level fields.
#[test]
fn a_bare_call_that_is_still_wrong_is_refused_for_what_it_lacks() {
    let mut c = call(1);
    c.as_object_mut().unwrap().remove("returns");
    assert!(check(c, &two_steps()).unwrap_err().contains("missing field `returns`"));
    // Both a wrapper and a stray call field: refused, and the fix named.
    let err = check(json!({"call": call(1), "step": 1}), &two_steps()).unwrap_err();
    assert!(err.contains("unknown field `step`") && err.contains("goes inside `call`"), "{err}");
    // An unknown field that is no call field gets no hint.
    let err = check(json!({"call": call(1), "plan": 1}), &two_steps()).unwrap_err();
    assert!(err.contains("unknown field `plan`") && !err.contains("inside `call`"), "{err}");
    // A whole second call beside the wrapper: refused, never swapped in for it.
    let mut both = json!({"call": call(1)});
    for (k, v) in call(2).as_object().unwrap() {
        both[k] = v.clone();
    }
    let err = check(both, &two_steps()).unwrap_err();
    assert!(err.contains("goes inside `call`"), "{err}");
    // No wrapper and no `method`: not lifted, and the fix is named.
    let err = check(json!({"tool": "mail", "step": 1}), &two_steps()).unwrap_err();
    assert!(err.contains("goes inside `call`"), "{err}");
    // A step written outside its list shares `tool`/`done_when` with a call: no `call` hint.
    let err = check(step(1), &two_steps()).unwrap_err();
    assert!(err.contains("unknown field") && !err.contains("inside `call`"), "{err}");
}

/// `CALL_FIELDS` is `CallSpec`'s fields, every one: a field it misses would stay at the top level.
#[test]
fn call_fields_are_every_call_spec_field() {
    let spec: CallSpec = serde_json::from_value(call(1)).unwrap();
    let mut keys: Vec<String> = serde_json::to_value(&spec).unwrap().as_object().unwrap().keys().cloned().collect();
    let mut fields: Vec<String> = CALL_FIELDS.iter().map(|f| f.to_string()).collect();
    keys.sort();
    fields.sort();
    assert_eq!(keys, fields);
}
