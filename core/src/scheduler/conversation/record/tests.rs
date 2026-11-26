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

#[test]
fn steps_that_never_ran_are_not_calls() {
    // The inner loop BREAKS on the first Err, so `outcomes` is shorter than
    // `steps` on every plan that failed part-way — the common case, and one no
    // fixture had. Treating a missing outcome as success would tell the next
    // turn identifiers that were never passed to a tool.
    let plan = plan_with(vec![
        step("mail", "mail.search", serde_json::json!({"n": 1}), DataClass::Public),
        step("mail", "mail.get_message", serde_json::json!({"n": 2}), DataClass::Public),
        step("mail", "mail.get_attachment_text", serde_json::json!({"n": 3}), DataClass::Public),
    ]);
    // Step 1 succeeded, step 2 failed, step 3 never ran.
    let outcomes = vec![
        StepOutcome::Ok(serde_json::json!({})),
        StepOutcome::Err { code: "UPSTREAM".into(), detail: "boom".into() },
    ];
    let record = from_plans(&[PlanRecord::new(plan, outcomes)], DataClass::Public);

    assert_eq!(record.calls.len(), 1, "only the step that actually succeeded");
    assert_eq!(record.calls[0].parameters.get("n").and_then(|v| v.as_u64()), Some(1));
}

#[test]
fn a_returns_note_of_exactly_the_cap_is_not_marked_as_cut() {
    // The boundary the test above misses: it uses `RETURNS_MAX_CHARS + 50`, so
    // `<=` -> `<` in `clamp_chars` survives, and an exactly-capped note grows a
    // spurious `…` — absence and loss rendering identically, one character
    // wide. Both sides of the boundary, so neither comparison can drift.
    let note_of = |n: usize| {
        let mut st = step("mail", "m", serde_json::json!({}), DataClass::Public);
        st.returns = "x".repeat(n);
        from_plans(
            &[PlanRecord::new(plan_with(vec![st]), vec![StepOutcome::Ok(serde_json::json!({}))])],
            DataClass::Public,
        )
        .calls[0]
            .returns
            .clone()
    };

    let exact = note_of(RETURNS_MAX_CHARS);
    assert_eq!(exact.chars().count(), RETURNS_MAX_CHARS, "exactly at the cap is untouched");
    assert!(!exact.ends_with('…'), "nothing was cut, so nothing says it was");

    let over = note_of(RETURNS_MAX_CHARS + 1);
    assert!(over.ends_with('…'), "one character over the cap is cut and says so");
    assert_eq!(over.chars().count(), RETURNS_MAX_CHARS + 1, "cap plus the ellipsis");
}

/// A sift's reads are not the planner's calls, so they are not carried; the
/// sift itself is, and it leaves its scrapbook for the next turn (#922).
#[test]
fn a_sift_leaves_its_scrapbook_and_its_reads_are_not_calls() {
    use crate::scheduler::notebook::sift::{SIFT_METHOD, SIFT_READS_DECISION, SIFT_TOOL};
    let mut reads = plan_with(vec![
        step("mail", "mail.get_message", serde_json::json!({"id": "m1"}), DataClass::Personal),
        step("mail", "mail.get_message", serde_json::json!({"id": "m2"}), DataClass::Personal),
    ]);
    reads.decision = format!("{SIFT_READS_DECISION}an invoice");
    let ok = || StepOutcome::Ok(serde_json::json!({}));
    let summary = serde_json::json!({"scrapbook": "sb:7:2", "criterion": "an invoice", "fields": ["amount"], "kept": 2});
    let sift = plan_with(vec![step(SIFT_TOOL, SIFT_METHOD, serde_json::json!({"keep_if": "an invoice"}), DataClass::Personal)]);
    let record = from_plans(
        &[PlanRecord::new(reads, vec![ok(), ok()]), PlanRecord::new(sift, vec![StepOutcome::Ok(summary)])],
        DataClass::Public,
    );
    assert_eq!(record.calls.iter().map(|c| c.method.as_str()).collect::<Vec<_>>(), [SIFT_METHOD]);
    assert_eq!(
        record.scrapbooks,
        [TurnScrapbook {
            scrapbook: "sb:7:2".into(),
            criterion: "an invoice".into(),
            fields: vec!["amount".into()],
            rows: 2,
            sample_withheld: false,
            continues: None,
            from: 0,
            looked_at: None,
            remaining: 0,
            list: None,
        }]
    );
    assert_eq!(record.data_class, DataClass::Personal, "the reads still raise the class");
    let shown = serde_json::to_value(&record.scrapbooks[0]).unwrap();
    assert!(shown.get("sample_withheld").is_none(), "absent unless true: {shown}");
}

/// #956: a summary the screen withheld has no criterion or fields. The
/// record used to keep them as blanks, which read as a sift with an empty
/// criterion and no fields rather than a withheld one; it now says so.
#[test]
fn a_withheld_summary_leaves_its_ref_and_says_it_was_withheld() {
    use crate::scheduler::notebook::sift::{SIFT_METHOD, SIFT_TOOL};
    let summary = serde_json::json!({"scrapbook": "sb:7:2", "kept": 1, "sample_withheld": true});
    let sift = plan_with(vec![step(SIFT_TOOL, SIFT_METHOD, serde_json::json!({}), DataClass::Personal)]);
    let record = from_plans(&[PlanRecord::new(sift, vec![StepOutcome::Ok(summary)])], DataClass::Public);
    let shown = serde_json::to_value(&record.scrapbooks).unwrap();
    assert_eq!(shown, serde_json::json!([{"scrapbook": "sb:7:2", "rows": 1, "sample_withheld": true}]));
    let back: Vec<TurnScrapbook> = serde_json::from_value(shown).unwrap();
    assert_eq!(back, record.scrapbooks, "a record stored this way reads back");
    let old: TurnScrapbook =
        serde_json::from_value(serde_json::json!({"scrapbook": "sb:7:2", "criterion": "", "fields": [], "rows": 1})).unwrap();
    assert!(!old.sample_withheld, "a record stored before the flag still parses");
}

#[test]
fn a_record_stored_before_scrapbooks_still_parses() {
    let r: TurnRecord = serde_json::from_value(serde_json::json!({"calls": [], "data_class": "Public"})).unwrap();
    assert!(r.scrapbooks.is_empty());
    let back = serde_json::to_value(&r).unwrap();
    assert!(back.get("scrapbooks").is_none(), "an empty list is not stored");
}

/// Review finding: eight scrapbooks at their widest exceed the byte cap on
/// their own, and the loop used to drop only calls — every referent gone and
/// the record still over. Calls go first, then the oldest scrapbooks, and
/// the record says how many of each it dropped.
#[test]
fn scrapbooks_too_shrink_to_fit_the_byte_cap() {
    use crate::scheduler::notebook::sift::{SIFT_METHOD, SIFT_TOOL};
    let fields: Vec<String> = (0..16).map(|i| format!("{i:0>2}{}", "é".repeat(FIELD_NAME_MAX_CHARS - 2))).collect();
    let mut plans = vec![PlanRecord::new(
        plan_with(vec![step("mail", "mail.search", serde_json::json!({"q": "x"}), DataClass::Public)]),
        vec![StepOutcome::Ok(serde_json::json!({}))],
    )];
    for n in 1..=SCRAPBOOKS_PER_TURN {
        let summary = serde_json::json!({"scrapbook": format!("sb:7:{n}"), "criterion": "ü".repeat(RETURNS_MAX_CHARS),
                                         "fields": fields, "kept": 1});
        let sift = plan_with(vec![step(SIFT_TOOL, SIFT_METHOD, serde_json::json!({}), DataClass::Public)]);
        plans.push(PlanRecord::new(sift, vec![StepOutcome::Ok(summary)]));
    }
    let record = from_plans(&plans, DataClass::Public);
    let len = result_view::serialised_len(&serde_json::to_value(&record).unwrap());
    assert!(len <= TURN_RECORD_CAP, "{len} bytes");
    assert!(record.calls.is_empty(), "the calls went first");
    assert_eq!(record.omitted_calls, 1 + SCRAPBOOKS_PER_TURN, "the search and each sift's own call");
    assert!(record.omitted_scrapbooks > 0 && !record.scrapbooks.is_empty());
    assert_eq!(record.scrapbooks.len() + record.omitted_scrapbooks, SCRAPBOOKS_PER_TURN);
    assert_eq!(record.scrapbooks.last().unwrap().scrapbook, format!("sb:7:{SCRAPBOOKS_PER_TURN}"), "the newest stay");
}

/// #956: a later turn's table naming a chain's last link shows the whole
/// chain only if the record keeps each link's `continues`; absent when a
/// sift continues none, as the summary has it.
#[test]
fn a_sift_that_continues_another_records_the_link() {
    use crate::scheduler::notebook::sift::{SIFT_METHOD, SIFT_TOOL};
    let sift = |summary| {
        let plan = plan_with(vec![step(SIFT_TOOL, SIFT_METHOD, serde_json::json!({}), DataClass::Personal)]);
        PlanRecord::new(plan, vec![StepOutcome::Ok(summary)])
    };
    let first = sift(serde_json::json!({"scrapbook": "sb:7:2", "kept": 3, "next_from": 30}));
    let next = sift(serde_json::json!({"scrapbook": "sb:7:3", "kept": 1, "continues": "sb:7:2"}));
    let shown = serde_json::to_value(from_plans(&[first, next], DataClass::Public).scrapbooks).unwrap();
    assert!(shown[0].get("continues").is_none(), "absent when it continues none: {shown}");
    assert_eq!(shown[1]["continues"], "sb:7:2");
}

/// #956: a later turn's table counted an earlier sift's rows as what it
/// looked at. The record keeps the code-written `looked_at` and
/// `remaining`, and the sift's `from`; the last two are absent when 0.
#[test]
fn a_sift_records_the_span_of_its_list_it_looked_at() {
    use crate::scheduler::notebook::sift::{SIFT_METHOD, SIFT_TOOL};
    let sift = |params, summary| {
        let plan = plan_with(vec![step(SIFT_TOOL, SIFT_METHOD, params, DataClass::Personal)]);
        PlanRecord::new(plan, vec![StepOutcome::Ok(summary)])
    };
    let first = sift(serde_json::json!({}), serde_json::json!({"scrapbook": "sb:7:2", "kept": 3, "looked_at": 30, "remaining": 50}));
    let next = sift(serde_json::json!({"from": 30}), serde_json::json!({"scrapbook": "sb:7:3", "kept": 1, "looked_at": 50, "remaining": 0}));
    let shown = serde_json::to_value(from_plans(&[first, next], DataClass::Public).scrapbooks).unwrap();
    assert_eq!(shown[0]["looked_at"], 30);
    assert_eq!(shown[0]["remaining"], 50);
    assert!(shown[0].get("from").is_none(), "absent when 0: {shown}");
    assert_eq!(shown[1]["from"], 30);
    assert!(shown[1].get("remaining").is_none(), "absent when 0: {shown}");
    let old: TurnScrapbook = serde_json::from_value(serde_json::json!({"scrapbook": "sb:7:2", "rows": 1})).unwrap();
    assert_eq!(old.looked_at, None, "a record stored before the counts still parses, and says it has none");
}

/// Issue 1058: two separate sifts over one list were counted as two lists
/// by a later turn's table. The record keeps a code-written `list` key, the
/// same for sifts over the same `over` and `items` of one task; absent when
/// the sift's parameters do not parse.
#[test]
fn a_sift_records_which_list_it_went_over() {
    use crate::scheduler::notebook::sift::{SIFT_METHOD, SIFT_TOOL};
    let sift = |params, name: &str| {
        let plan = plan_with(vec![step(SIFT_TOOL, SIFT_METHOD, params, DataClass::Personal)]);
        PlanRecord::new(plan, vec![StepOutcome::Ok(serde_json::json!({"scrapbook": name, "kept": 1}))])
    };
    let over = |step: u32, keep_if: &str| serde_json::json!({"over": {"step": step}, "items": "hits", "keep_if": keep_if, "fields": ["a"]});
    let plans = [sift(over(1, "an invoice"), "sb:7:2"), sift(over(1, "a receipt"), "sb:7:3"), sift(over(2, "x"), "sb:7:4"), sift(serde_json::json!({}), "sb:7:5")];
    let shown = serde_json::to_value(from_plans(&plans, DataClass::Public).scrapbooks).unwrap();
    assert_eq!(shown[0]["list"], "7:step 1:hits", "{shown}");
    assert_eq!(shown[1]["list"], shown[0]["list"], "another criterion, the same list");
    assert_eq!(shown[2]["list"], "7:step 2:hits");
    assert!(shown[3].get("list").is_none(), "unparsed parameters: no key: {shown}");
}
