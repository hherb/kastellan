use super::*;
use serde_json::{json, Value};

fn entry(seq: i64, kind: EntryKind, payload: Value) -> Entry {
    Entry { task_id: 1, seq, kind, payload, created_at: time::OffsetDateTime::UNIX_EPOCH }
}

fn step(id: u32) -> Value {
    json!({"id": id, "purpose": format!("p{id}"), "tool": "mail", "done_when": "found"})
}

fn steps(seq: i64, ids: &[u32]) -> Entry {
    entry(seq, EntryKind::Steps, json!({"steps": ids.iter().map(|i| step(*i)).collect::<Vec<_>>()}))
}

fn tick(seq: i64, id: u32, class: &str) -> Entry {
    entry(
        seq,
        EntryKind::Tick,
        json!({"step": id, "note": format!("n{id}"), "summary": format!("s{id}"),
               "findings": [{"text": format!("f{id}"), "source": format!("msg-{id}")}],
               "classification": class}),
    )
}

fn failed(seq: i64, id: u32, class: &str) -> Entry {
    entry(seq, EntryKind::FailedStep, json!({"step": id, "code": "E", "detail": "d", "classification": class}))
}

fn ids_with(state: &NotebookState, status: StepStatus) -> Vec<u32> {
    state.steps.iter().filter(|s| s.status == status).map(|s| s.spec.id).collect()
}

#[test]
fn a_tick_marks_the_step_done_and_carries_its_findings() {
    let s = fold(&[steps(1, &[1, 2]), tick(2, 1, "Personal")]).unwrap();
    assert_eq!(ids_with(&s, StepStatus::Done), vec![1]);
    assert_eq!(s.steps[0].note.as_deref(), Some("n1"));
    assert_eq!(s.summary.as_deref(), Some("s1"));
    assert_eq!(
        s.findings,
        vec![Finding { text: "f1".into(), source: "msg-1".into(), step: 1, classification: DataClass::Personal }]
    );
    assert_eq!(s.last_seq, 2);
    assert!(!s.terminal);
}

#[test]
fn a_replan_keeps_done_steps_and_replaces_pending_ones() {
    let s = fold(&[steps(1, &[1, 2, 3]), tick(2, 1, "Public"), failed(3, 2, "Public"), steps(4, &[4, 5])]).unwrap();
    assert_eq!(s.steps.iter().map(|s| s.spec.id).collect::<Vec<_>>(), vec![1, 2, 4, 5]);
    assert_eq!(ids_with(&s, StepStatus::Done), vec![1]);
    assert_eq!(ids_with(&s, StepStatus::Failed), vec![2]);
    assert_eq!(s.findings.len(), 1, "a replan keeps the findings");
}

/// DGX task 259: the model re-sent the whole list, failed step included.
#[test]
fn a_replan_relisting_finished_steps_keeps_done_ones_and_retries_failed_ones() {
    let s = fold(&[steps(1, &[1, 2, 3]), tick(2, 1, "Public"), failed(3, 2, "Public"), steps(4, &[1, 2, 4])]).unwrap();
    assert_eq!(s.steps.iter().map(|s| s.spec.id).collect::<Vec<_>>(), vec![1, 2, 4], "step 3 replanned away");
    assert_eq!(ids_with(&s, StepStatus::Done), vec![1]);
    assert_eq!(s.steps[0].note.as_deref(), Some("n1"), "a done step keeps its note");
    assert_eq!(ids_with(&s, StepStatus::Pending), vec![2, 4]);
    assert_eq!(s.current_step().map(|c| c.id), Some(2), "the retried step is current, in its place");
    assert_eq!(s.touched.len(), 2, "the failed attempt's data still counts");
}

#[test]
fn a_step_list_naming_one_id_twice_is_an_error() {
    let err = fold(&[steps(1, &[1, 1])]).unwrap_err();
    assert!(err.detail.contains("listed twice"), "{err}");
}

#[test]
fn the_current_step_is_the_first_pending_one() {
    assert_eq!(fold(&[]).unwrap().current_step(), None);
    let s = fold(&[steps(1, &[1, 2, 3]), tick(2, 1, "Public"), failed(3, 2, "Public")]).unwrap();
    assert_eq!(s.current_step().map(|c| c.id), Some(3));
    let s = fold(&[steps(1, &[1]), tick(2, 1, "Public")]).unwrap();
    assert_eq!(s.current_step(), None, "every step done");
}

#[test]
fn a_correction_is_unseen_until_its_seq_is_passed() {
    let c = |seq, t: &str| entry(seq, EntryKind::Correction, json!({"text": t}));
    let s = fold(&[steps(1, &[1]), c(2, "other account"), tick(3, 1, "Public"), c(4, "and 2025")]).unwrap();
    let texts = |seen| s.unseen_corrections(seen).iter().map(|c| c.text.as_str()).collect::<Vec<_>>();
    assert_eq!(texts(0), vec!["other account", "and 2025"]);
    assert_eq!(texts(2), vec!["and 2025"]);
    assert_eq!(texts(4), Vec::<&str>::new());
}

#[test]
fn touched_collects_every_dispatched_steps_classification() {
    // Step 2 failed, step 3 was replanned away after its failure; both count.
    let s = fold(&[
        steps(1, &[1, 2, 3]),
        tick(2, 1, "Personal"),
        failed(3, 2, "ClinicalConfidential"),
        failed(4, 3, "Secret"),
        steps(5, &[4]),
    ])
    .unwrap();
    assert_eq!(s.touched, vec![DataClass::Personal, DataClass::ClinicalConfidential, DataClass::Secret]);
}

#[test]
fn a_malformed_payload_is_an_error_not_a_guess() {
    // Missing field, unknown field, wrong type: each refused, naming the entry.
    for bad in [
        json!({"step": 1, "note": "n", "summary": "s", "findings": []}),
        json!({"step": 1, "note": "n", "summary": "s", "findings": [], "classification": "Public", "extra": 1}),
        json!({"step": "one", "note": "n", "summary": "s", "findings": [], "classification": "Public"}),
    ] {
        let err = fold(&[steps(1, &[1]), entry(2, EntryKind::Tick, bad)]).unwrap_err();
        assert_eq!((err.seq, err.kind), (2, "tick"));
        assert!(err.detail.starts_with("malformed payload"), "{err}");
    }
}

#[test]
fn a_tick_for_an_unlisted_or_finished_step_is_an_error() {
    assert!(fold(&[steps(1, &[1]), tick(2, 9, "Public")]).unwrap_err().detail.contains("no step 9"));
    let err = fold(&[steps(1, &[1, 2]), tick(2, 1, "Public"), tick(3, 1, "Public")]).unwrap_err();
    assert!(err.detail.contains("already Done"), "{err}");
}

#[test]
fn only_a_correction_may_follow_the_end() {
    let answer = entry(3, EntryKind::Answer, json!({"body": "done"}));
    let s = fold(&[steps(1, &[1]), tick(2, 1, "Public"), answer.clone(),
                   entry(4, EntryKind::Correction, json!({"text": "late"}))]).unwrap();
    assert!(s.terminal);
    assert_eq!(s.corrections.len(), 1);
    let err = fold(&[steps(1, &[1, 2]), answer, tick(4, 2, "Public")]).unwrap_err();
    assert!(err.detail.contains("after the task ended"), "{err}");
}
