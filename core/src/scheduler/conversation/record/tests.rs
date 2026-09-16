//! Unit tests for [`super::from_plans`] — the record a finished channel turn
//! leaves for the next turn in its conversation.

use super::*;
use crate::cassandra::types::{DataClass, Plan, PlannedStep};
use crate::scheduler::inner_loop::{PlanRecord, StepOutcome};

fn step(tool: &str, method: &str, params: serde_json::Value, class: DataClass) -> PlannedStep {
    PlannedStep {
        tool: tool.into(),
        method: method.into(),
        parameters: params,
        returns: "what this step returns".into(),
        done_when: "d".into(),
        classification: class,
    }
}

fn plan_with(steps: Vec<PlannedStep>) -> Plan {
    Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps,
        result: None,
        data_ceiling: Some(DataClass::Public),
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}

#[test]
fn a_successful_step_becomes_a_call_and_a_failed_one_does_not() {
    let plan = plan_with(vec![
        step(
            "mail",
            "mail.get_message",
            serde_json::json!({"message_id": 38036}),
            DataClass::Personal,
        ),
        step(
            "mail",
            "mail.search",
            serde_json::json!({"query": "flight"}),
            DataClass::Personal,
        ),
    ]);
    let outcomes = vec![
        StepOutcome::Ok(serde_json::json!({"subject": "Booking FHZ4XR"})),
        StepOutcome::Err { code: "UPSTREAM".into(), detail: "boom".into() },
    ];
    let record = from_plans(&[PlanRecord::new(plan, outcomes)], DataClass::Public);

    assert_eq!(record.calls.len(), 1, "a failed step is not a referent");
    assert_eq!(record.calls[0].method, "mail.get_message");
    assert_eq!(record.calls[0].parameters, serde_json::json!({"message_id": 38036}));
    assert_eq!(record.calls[0].returns, "what this step returns");
}

#[test]
fn calls_keep_their_dispatch_order_across_plans() {
    let first = PlanRecord::new(
        plan_with(vec![step("mail", "mail.search", serde_json::json!({"n": 1}), DataClass::Public)]),
        vec![StepOutcome::Ok(serde_json::json!({}))],
    );
    let second = PlanRecord::new(
        plan_with(vec![step("mail", "mail.get_message", serde_json::json!({"n": 2}), DataClass::Public)]),
        vec![StepOutcome::Ok(serde_json::json!({}))],
    );
    let record = from_plans(&[first, second], DataClass::Public);

    let ns: Vec<u64> = record
        .calls
        .iter()
        .filter_map(|c| c.parameters.get("n").and_then(|v| v.as_u64()))
        .collect();
    assert_eq!(ns, vec![1, 2], "oldest plan first, in dispatch order");
}

#[test]
fn the_data_class_is_the_max_of_the_floor_and_every_dispatched_step() {
    let plan = plan_with(vec![step(
        "mail",
        "mail.search",
        serde_json::json!({}),
        DataClass::Personal,
    )]);
    let record = from_plans(
        &[PlanRecord::new(plan, vec![StepOutcome::Ok(serde_json::json!({}))])],
        DataClass::Public,
    );
    assert_eq!(record.data_class, DataClass::Personal, "a step raises it above the floor");

    let plan = plan_with(vec![step(
        "mail",
        "mail.search",
        serde_json::json!({}),
        DataClass::Public,
    )]);
    let record = from_plans(
        &[PlanRecord::new(plan, vec![StepOutcome::Ok(serde_json::json!({}))])],
        DataClass::ClinicalConfidential,
    );
    assert_eq!(
        record.data_class,
        DataClass::ClinicalConfidential,
        "the floor wins when it is higher",
    );
}

#[test]
fn a_failed_step_still_raises_the_data_class() {
    // The step ran and touched the data; only its *call* is uninteresting to
    // the next turn. Classification must not depend on whether it worked.
    let plan = plan_with(vec![step(
        "mail",
        "mail.search",
        serde_json::json!({}),
        DataClass::Personal,
    )]);
    let record = from_plans(
        &[PlanRecord::new(
            plan,
            vec![StepOutcome::Err { code: "UPSTREAM".into(), detail: "boom".into() }],
        )],
        DataClass::Public,
    );
    assert!(record.calls.is_empty());
    assert_eq!(record.data_class, DataClass::Personal);
}

#[test]
fn a_long_returns_note_is_clamped_on_a_char_boundary() {
    let mut s = step("mail", "m", serde_json::json!({}), DataClass::Public);
    s.returns = "é".repeat(RETURNS_MAX_CHARS + 50);
    let record = from_plans(
        &[PlanRecord::new(plan_with(vec![s]), vec![StepOutcome::Ok(serde_json::json!({}))])],
        DataClass::Public,
    );
    let returns = &record.calls[0].returns;
    assert_eq!(
        returns.chars().count(),
        RETURNS_MAX_CHARS + 1,
        "clamped to the cap, plus the ellipsis",
    );
    assert!(returns.ends_with('…'));
}

#[test]
fn an_identifier_parameter_is_never_cut_in_half() {
    // A file name the next turn must pass back verbatim. `result_view::render`
    // keeps a space-free string whole or drops it; half an id is a trap.
    let filename = "Download-478886674-e-ticket-FHZ4XR.pdf";
    let s = step(
        "mail",
        "mail.get_attachment_text",
        serde_json::json!({"filename": filename, "message_id": 38036}),
        DataClass::Personal,
    );
    let record = from_plans(
        &[PlanRecord::new(plan_with(vec![s]), vec![StepOutcome::Ok(serde_json::json!({}))])],
        DataClass::Public,
    );
    assert_eq!(
        record.calls[0].parameters.get("filename").and_then(|v| v.as_str()),
        Some(filename),
    );
    assert_eq!(
        record.calls[0].parameters.get("message_id").and_then(|v| v.as_u64()),
        Some(38036),
        "a number stays a number, so the next turn can pass it back typed",
    );
}

#[test]
fn too_many_calls_drop_the_oldest_and_say_so() {
    let steps: Vec<PlannedStep> = (0..CALLS_PER_TURN + 3)
        .map(|i| {
            step(
                "mail",
                "mail.get_message",
                serde_json::json!({"message_id": i}),
                DataClass::Public,
            )
        })
        .collect();
    let outcomes = vec![StepOutcome::Ok(serde_json::json!({})); steps.len()];
    let record = from_plans(&[PlanRecord::new(plan_with(steps), outcomes)], DataClass::Public);

    assert_eq!(record.calls.len(), CALLS_PER_TURN);
    assert_eq!(record.omitted_calls, 3);
    assert_eq!(
        record.calls[0].parameters.get("message_id").and_then(|v| v.as_u64()),
        Some(3),
        "the OLDEST calls are the ones dropped",
    );
}

#[test]
fn the_record_stays_within_its_byte_cap() {
    // One call with a large parameter blob per step, enough to exceed the cap.
    let big = serde_json::json!({"note": "x ".repeat(4096)});
    let steps: Vec<PlannedStep> = (0..CALLS_PER_TURN)
        .map(|_| step("mail", "mail.search", big.clone(), DataClass::Public))
        .collect();
    let outcomes = vec![StepOutcome::Ok(serde_json::json!({})); steps.len()];
    let record = from_plans(&[PlanRecord::new(plan_with(steps), outcomes)], DataClass::Public);

    let value = serde_json::to_value(&record).expect("serialise");
    assert!(
        result_view::serialised_len(&value) <= TURN_RECORD_CAP,
        "record must fit its cap; got {} bytes",
        result_view::serialised_len(&value),
    );
    assert!(record.omitted_calls > 0, "and it must say what it dropped");
}

#[test]
fn a_turn_that_dispatched_nothing_records_no_calls() {
    let record = from_plans(&[PlanRecord::new(plan_with(vec![]), vec![])], DataClass::Public);
    assert!(record.calls.is_empty());
    assert_eq!(record.omitted_calls, 0);
    assert_eq!(record.data_class, DataClass::Public);
}

#[test]
fn the_omitted_calls_key_is_absent_when_nothing_was_dropped() {
    // Absence and loss must not render identically: a reader that sees no
    // `_omitted_calls` key knows the record is complete.
    let record = from_plans(&[PlanRecord::new(plan_with(vec![]), vec![])], DataClass::Public);
    let value = serde_json::to_value(&record).expect("serialise");
    assert!(value.get("_omitted_calls").is_none());

    let steps: Vec<PlannedStep> = (0..CALLS_PER_TURN + 1)
        .map(|_| step("mail", "m", serde_json::json!({}), DataClass::Public))
        .collect();
    let outcomes = vec![StepOutcome::Ok(serde_json::json!({})); steps.len()];
    let dropped = from_plans(&[PlanRecord::new(plan_with(steps), outcomes)], DataClass::Public);
    let value = serde_json::to_value(&dropped).expect("serialise");
    assert_eq!(value.get("_omitted_calls").and_then(|v| v.as_u64()), Some(1));
}

#[test]
fn a_record_round_trips_through_json() {
    // It is stored as JSONB and read back by the next turn, so the two
    // directions must agree — including the renamed `_omitted_calls` field.
    let steps: Vec<PlannedStep> = (0..CALLS_PER_TURN + 2)
        .map(|i| step("mail", "m", serde_json::json!({"i": i}), DataClass::Personal))
        .collect();
    let outcomes = vec![StepOutcome::Ok(serde_json::json!({})); steps.len()];
    let record = from_plans(&[PlanRecord::new(plan_with(steps), outcomes)], DataClass::Public);

    let value = serde_json::to_value(&record).expect("serialise");
    let back: TurnRecord = serde_json::from_value(value).expect("deserialise");
    assert_eq!(back, record);
}
