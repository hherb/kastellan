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
