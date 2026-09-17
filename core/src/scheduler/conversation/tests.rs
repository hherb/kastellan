//! Unit tests for [`super::turn_from_row`] — how one stored row becomes a
//! [`Turn`]. The windowed query itself is PG-gated and lives in
//! `db/tests/conversation_turns_e2e.rs`.

use super::*;
use crate::cassandra::types::DataClass;
use kastellan_db::tasks::turns::ConversationTurnRow;

fn row(result: Option<serde_json::Value>, turn_record: Option<serde_json::Value>) -> ConversationTurnRow {
    ConversationTurnRow {
        task_id: 187,
        finished_at: time::OffsetDateTime::from_unix_timestamp(1_757_000_000).expect("timestamp"),
        payload: serde_json::json!({
            "kind": "channel",
            "instruction": "What are my 3 most recent flight bookings?",
            "channel": "matrix",
            "peer": "@horst:example.org",
            "conversation": "!room:example.org",
        }),
        result,
        turn_record,
    }
}

#[test]
fn the_answer_is_what_the_user_was_actually_sent() {
    // `reply_body` is the same pure function the bus delivered with, so the
    // planner reads the sentence the user saw — not a second rendering that
    // could drift from it.
    let turn = turn_from_row(row(
        Some(serde_json::json!({"kind": "text", "body": "Booking FHZ4XR - 1,076.97 AUD"})),
        None,
    ));
    assert_eq!(turn.answer, "Booking FHZ4XR - 1,076.97 AUD");
    assert_eq!(turn.user, "What are my 3 most recent flight bookings?");
    assert_eq!(turn.task_id, 187);
}

#[test]
fn a_refused_turn_carries_the_sentence_the_user_saw() {
    let turn = turn_from_row(row(
        Some(serde_json::json!({
            "kind": "refused", "principle": 2, "reason": "policy",
            "body": "I have to decline that request.",
        })),
        None,
    ));
    assert_eq!(turn.answer, "I have to decline that request.");
}

#[test]
fn a_turn_with_no_result_still_renders_an_answer() {
    // A crashed task never reached finalize, so its result is NULL. The user
    // still got a reply; the turn must say the same thing.
    let turn = turn_from_row(row(None, None));
    assert!(!turn.answer.is_empty());
    assert!(turn.record.is_none());
}

#[test]
fn a_well_formed_stored_record_is_parsed() {
    let turn = turn_from_row(row(
        Some(serde_json::json!({"kind": "text", "body": "ok"})),
        Some(serde_json::json!({
            "calls": [{"tool": "mail", "method": "mail.get_attachment_text",
                       "parameters": {"message_id": 38036},
                       "returns": "The e-ticket text."}],
            "data_class": "Personal",
        })),
    ));
    let record = turn.record.expect("a well-formed record must parse");
    assert_eq!(record.calls.len(), 1);
    assert_eq!(record.calls[0].method, "mail.get_attachment_text");
    assert_eq!(record.data_class, DataClass::Personal);
    assert_eq!(record.omitted_calls, 0, "an absent _omitted_calls key means none were dropped");
}

#[test]
fn the_class_survives_a_record_whose_calls_no_longer_parse() {
    // The class and the calls must not share a failure mode: if a future shape
    // change to `calls` could throw the class away, a clinical turn would be
    // rendered into a follow-up running at Public.
    let turn = turn_from_row(row(
        Some(serde_json::json!({"kind": "text", "body": "ok"})),
        Some(serde_json::json!({
            "calls": "not an array",
            "data_class": "ClinicalConfidential",
        })),
    ));
    assert!(turn.record.is_none(), "the calls are lost");
    assert_eq!(
        turn.data_class,
        Some(DataClass::ClinicalConfidential),
        "but the class, which governs what may be done with the text, is not",
    );
}

#[test]
fn a_turn_from_before_the_migration_has_no_class_at_all() {
    // turn_record IS NULL for every turn that finished before 0026. The view
    // refuses to show such a turn's text; here we pin that the class is absent
    // rather than silently defaulting to the bottom of the lattice.
    let turn = turn_from_row(row(Some(serde_json::json!({"kind": "text", "body": "ok"})), None));
    assert_eq!(turn.data_class, None);
}

#[test]
fn a_malformed_stored_record_renders_without_calls() {
    // Fail-safe, not fail-closed: a schema change must not make a whole
    // conversation unreadable. The test above is the positive control — it
    // proves a well-formed record does NOT take this arm, so this test is not
    // passing vacuously.
    let turn = turn_from_row(row(
        Some(serde_json::json!({"kind": "text", "body": "ok"})),
        Some(serde_json::json!({"calls": "not an array", "data_class": "Personal"})),
    ));
    assert!(turn.record.is_none(), "unparseable record is dropped");
    assert_eq!(turn.answer, "ok", "but the turn's text survives");
}

#[test]
fn a_payload_without_an_instruction_renders_an_empty_user_line() {
    // Fail-safe for the same reason: a malformed payload must not take the
    // conversation down with it.
    let mut r = row(Some(serde_json::json!({"kind": "text", "body": "ok"})), None);
    r.payload = serde_json::json!({"kind": "channel"});
    let turn = turn_from_row(r);
    assert_eq!(turn.user, "");
}

#[test]
fn a_turn_that_renders_its_text_always_contributes_its_class() {
    // ⚠️ The cross-check between the two halves of the split parse, and the
    // regression a review found shipped: `view::render` gates on
    // `Turn::data_class` while `floor::inherit_floor` read the class out of
    // `Turn::record`. For a stored record whose `calls` no longer parse those
    // two disagree — the text was rendered and the class was not inherited,
    // which is a clinical answer reaching a follow-up running at `Public`.
    //
    // Stated as the invariant rather than as the one shape that broke it: for
    // ANY row, if the rendered turn carries text, the floor must have risen.
    // A future change that reintroduces the divergence fails here whatever
    // shape it takes.
    for record in [
        // The shape that broke: readable class, unparseable calls.
        Some(serde_json::json!({"calls": "not an array", "data_class": "ClinicalConfidential"})),
        // A well-formed record, so the success path is covered by the same rule.
        Some(serde_json::json!({"calls": [], "data_class": "ClinicalConfidential"})),
        // No record at all: no class, and no text either.
        None,
    ] {
        let turn = turn_from_row(row(
            Some(serde_json::json!({"kind": "text", "body": "the CT report says ..."})),
            record.clone(),
        ));
        let rendered = super::view::render(
            std::slice::from_ref(&turn),
            super::view::CONVERSATION_BUDGET,
        );
        let shows_text = rendered.turns[0].get("answer").is_some();
        let (floor, source) = super::floor::inherit_floor(
            DataClass::Public,
            crate::scheduler::inner_loop::ClassificationFloorSource::Default,
            std::slice::from_ref(&turn),
        );

        if shows_text {
            assert_eq!(
                floor,
                DataClass::ClinicalConfidential,
                "a turn whose text is rendered must raise the floor: {record:?}",
            );
            assert_eq!(
                source,
                crate::scheduler::inner_loop::ClassificationFloorSource::ConversationInherited,
            );
        } else {
            assert_eq!(floor, DataClass::Public, "nothing shown, nothing inherited");
        }
    }
}

#[test]
fn the_query_binds_every_field_from_the_asking_task() {
    // ⚠️ The wiring a review found entirely unpinned. Each of these bindings
    // had a mutant that survived the whole suite, because the db tests pass
    // their own literals and never see what the caller actually sends:
    //
    //  * `window_anchor` ← the asking task's `created_at`, NOT `now()`. This
    //    is the one the db module argues for at length: a task suspended on an
    //    operator ask must see at resume every turn it saw at first planning.
    //  * `window_hours` ← `WINDOW_HOURS`, not a literal that can drift from it.
    //  * `limit` ← `MAX_TURNS`, same reason.
    //  * `exclude_task_id` ← the asking task, so it cannot read itself.
    //  * the three conversation keys, each in its own field — a transposition
    //    of `peer` and `conversation` hands one peer another peer's turns, and
    //    the named-field struct only makes that a compile error if the
    //    assignment here is right in the first place.
    use crate::channel::ask_message::AskDestination;
    use crate::channel::{ChannelId, ConversationId, PeerId};

    let dest = AskDestination {
        channel: ChannelId("matrix".into()),
        peer: PeerId("@horst:example.org".into()),
        conversation: ConversationId("!room:example.org".into()),
    };
    let created_at = time::OffsetDateTime::from_unix_timestamp(1_757_000_000).expect("timestamp");
    let q = super::conversation_query_for(&dest, 187, created_at);

    assert_eq!(q.window_anchor, created_at, "the anchor is the asking task's arrival");
    assert_eq!(q.window_hours, super::WINDOW_HOURS);
    assert_eq!(q.limit, super::MAX_TURNS);
    assert_eq!(q.exclude_task_id, 187);
    assert_eq!(q.channel, "matrix");
    assert_eq!(q.peer, "@horst:example.org");
    assert_eq!(q.conversation, "!room:example.org");
}

#[test]
fn a_block_audit_payload_carries_the_forensics_and_never_the_text() {
    // `block_audit_payload`'s doc says it is "pure, so the shape is testable
    // without a pool" — and then nothing tested it. Its only coverage was a
    // PG-gated e2e asserting three of its eight keys, so on any host where
    // Postgres skips, the function was wholly uncovered. Its sibling
    // `sink_block_audit_payloads` has three unit tests.
    let block = super::view::ConversationBlock {
        task_id: 186,
        score: 0.93,
        reason_codes: vec!["instruction_override"],
        body_sha256: "a".repeat(64),
        body_byte_len: 4096,
    };
    let payload = super::block_audit_payload(187, &block);

    assert_eq!(payload["task_id"], 187, "the task that was planning");
    assert_eq!(payload["turn_task_id"], 186, "the earlier turn that was blocked");
    assert_eq!(payload["decision"], "block");
    assert_eq!(payload["tier"], super::view::TIER_CONVERSATION);
    assert_eq!(payload["reason_codes"][0], "instruction_override");
    assert_eq!(payload["body_byte_len"], 4096);
    assert_eq!(payload["body_sha256"].as_str().map(str::len), Some(64));
    assert!(payload["score"].as_f64().is_some_and(|s| (s - 0.93).abs() < 1e-6));

    // The screened text itself must never reach the row — the hash and the
    // length are the whole point of carrying those two fields instead.
    let serialised = payload.to_string();
    assert_eq!(
        serialised.matches("instruction").count(),
        1,
        "only the reason code mentions it; no screened body leaked in: {serialised}",
    );
}
