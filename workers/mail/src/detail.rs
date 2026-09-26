//! Shaping a `GET /v1/messages/{id}` response for the planner.
//!
//! localmail addresses an attachment by its **position** in the message's
//! `attachments` array (`/v1/messages/{id}/attachments/{index}`, slice D,
//! `api_minor` 2) — the only per-message identity that is both unique and
//! already on the wire. But localmail does not *write* that position into the
//! entries (its spec: "position in the array already is the index"), and the
//! planner reads a pruned JSON view in which an array element's position is not
//! a label it can copy. So this worker writes it in: each entry gains
//! `"index": <n>`, and `mail.get_attachment_text` / `mail.get_attachment` take
//! that `index` back (#760).
//!
//! Why a position rather than the filename or hash the tools already take:
//! measured on the live archive, **200** `(message, filename)` groups span more
//! than one distinct blob (one message carries `attachment` 22 times over 22
//! blobs), so a filename cannot always say which one; and a sha256 is 64
//! characters the planner has been caught mistyping (task 160). An index is
//! one or two digits and exact.

use serde_json::Value;

/// The key each attachment entry gains. Named for the route segment it fills.
pub const INDEX_KEY: &str = "index";

/// Write each attachment's position into it as [`INDEX_KEY`].
///
/// Only object entries are numbered; anything else is left exactly as served,
/// because inventing a shape for a malformed entry would hide the fault from
/// the reader who might diagnose it. A message without an `attachments` array
/// is returned unchanged for the same reason — `attach` refuses it loudly when
/// an attachment tool actually needs one.
///
/// An `index` key localmail served itself would be **overwritten**, knowingly:
/// the position is what `attach::pick` and the index route resolve, so it is
/// the only value the planner can safely copy back — a served `index` meaning
/// anything else would be a trap. The live shape gate
/// (`mock_localmail_shapes_match_real_localmail`) pins the served key set, so a
/// localmail that starts sending one is noticed there rather than here.
pub fn number_attachments(mut message: Value) -> Value {
    if let Some(Value::Array(entries)) = message.get_mut("attachments") {
        for (i, entry) in entries.iter_mut().enumerate() {
            if let Value::Object(map) = entry {
                map.insert(INDEX_KEY.to_string(), Value::from(i));
            }
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_attachment_carries_its_position_in_the_array() {
        let msg = json!({
            "id": "7",
            "attachments": [
                {"filename": "attachment", "sha256": "a"},
                {"filename": "attachment", "sha256": "b"},
                {"filename": "logo.png", "sha256": "c"}
            ]
        });
        let out = number_attachments(msg);
        let idx: Vec<&Value> = out["attachments"].as_array().unwrap().iter().map(|a| &a["index"]).collect();
        assert_eq!(idx, vec![&json!(0), &json!(1), &json!(2)]);
        // The numbering is the only change: every served key survives.
        assert_eq!(out["attachments"][1]["sha256"], "b");
        assert_eq!(out["id"], "7");
    }

    /// The position is the *array* position, counting entries this worker
    /// cannot use (a `null` sha256) — because that is what localmail's route
    /// counts. Skipping them would shift every later index by one and fetch the
    /// wrong attachment.
    #[test]
    fn unusable_and_malformed_entries_still_occupy_their_position() {
        let msg = json!({"attachments": [
            {"filename": "unstored.pdf", "sha256": null},
            "not-an-object",
            {"filename": "real.pdf", "sha256": "d"}
        ]});
        let out = number_attachments(msg);
        assert_eq!(out["attachments"][0]["index"], 0);
        assert_eq!(out["attachments"][1], "not-an-object", "left exactly as served");
        assert_eq!(out["attachments"][2]["index"], 2);
    }

    #[test]
    fn a_message_without_an_attachments_array_is_unchanged() {
        for msg in [json!({"id": "1"}), json!({"attachments": null}), json!({"attachments": {}})] {
            assert_eq!(number_attachments(msg.clone()), msg);
        }
    }
}
