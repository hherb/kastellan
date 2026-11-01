//! Reshaping `mail.search` hits for the planner (#947).
//!
//! localmail answers two of the projected [`HIT_FIELDS`] in a shape that costs
//! the planner more than it tells it, so this module rewrites them in place
//! after every search:
//!
//! * **`attachments`** arrives as `{filename, sha256, content_type, size}` per
//!   attachment. The planner needs only the names: they are what
//!   `mail.get_attachment_text` takes (with the `message_id`), and they are how
//!   it tells which of 50 hits holds a file the user named. Task 242 of the
//!   #922 re-measure answered wrongly because a hit said only
//!   `has_attachments: true`. The sha256s are dropped on purpose — an opaque
//!   64-hex id is one this planner retypes wrongly (see `crate::ids`), and
//!   each one would cost ~70 bytes of the step's 16 KiB view.
//! * **`date_local`** is the instant of the message's `Date:` header on the
//!   sender's own clock (`2023-07-12T09:58:24+10:00`), where `date` is UTC
//!   (`2023-07-11T23:58:24+00:00`). Same instant, but the user remembers the
//!   first, so it replaces `date` when localmail has one. localmail sends
//!   `null` without a usable header; `date` then stays as it was.
//!
//! Nothing else in a hit is touched — above all not `message_id`, so a
//! fabricated id stays attributable to the planner, never to this worker.
//!
//! [`HIT_FIELDS`]: crate::localmail_contract::HIT_FIELDS

use serde_json::Value;

/// Rewrite every hit under `response["results"]` as the module doc describes.
/// A response without a `results` array is left alone.
pub fn shape(response: &mut Value) {
    let Some(hits) = response.get_mut("results").and_then(Value::as_array_mut) else { return };
    for hit in hits.iter_mut().filter_map(Value::as_object_mut) {
        if let Some(Value::Array(list)) = hit.get_mut("attachments") {
            let names: Vec<Value> = list
                .iter()
                .filter_map(|a| a.get("filename").and_then(Value::as_str))
                .map(|n| Value::String(n.to_owned()))
                .collect();
            *list = names;
        }
        // `remove` either way: the hit keeps exactly one date.
        if let Some(Value::String(local)) = hit.remove("date_local") {
            hit.insert("date".to_owned(), Value::String(local));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one_hit(hit: Value) -> Value {
        let mut out = json!({"results": [hit], "next_cursor": null});
        shape(&mut out);
        out["results"][0].clone()
    }

    #[test]
    fn attachments_become_their_file_names_in_order() {
        let hit = one_hit(json!({
            "message_id": "4242",
            "attachments": [
                {"filename": "Fee Proposal 230712.pdf", "sha256": "209a", "content_type": "application/pdf", "size": 247579},
                {"filename": "guide.docx", "sha256": "b3f3", "content_type": "application/x", "size": 9},
            ],
        }));
        assert_eq!(hit["attachments"], json!(["Fee Proposal 230712.pdf", "guide.docx"]));
        assert_eq!(hit["message_id"], json!("4242"), "ids are never touched");
    }

    /// A nameless attachment has nothing the planner could select it by, so it
    /// is left out rather than shown as a `null` it might copy into a call.
    #[test]
    fn an_attachment_without_a_string_name_is_left_out() {
        let hit = one_hit(json!({"attachments": [{"sha256": "aa"}, {"filename": null}, {"filename": "a.pdf"}]}));
        assert_eq!(hit["attachments"], json!(["a.pdf"]));
    }

    #[test]
    fn no_attachments_stays_an_empty_list() {
        assert_eq!(one_hit(json!({"attachments": []}))["attachments"], json!([]));
    }

    #[test]
    fn the_senders_local_time_replaces_the_utc_date() {
        let hit = one_hit(json!({
            "date": "2023-07-11T23:58:24+00:00",
            "date_local": "2023-07-12T09:58:24+10:00",
        }));
        assert_eq!(hit["date"], json!("2023-07-12T09:58:24+10:00"));
        assert!(hit.get("date_local").is_none(), "one date per hit: {hit}");
    }

    #[test]
    fn without_a_local_time_the_utc_date_stays() {
        let hit = one_hit(json!({"date": "2023-07-11T23:58:24+00:00", "date_local": null}));
        assert_eq!(hit["date"], json!("2023-07-11T23:58:24+00:00"));
        assert!(hit.get("date_local").is_none(), "{hit}");
    }

    #[test]
    fn a_response_without_results_is_left_alone() {
        for mut v in [json!({"detail": "x"}), json!({"results": "not a list"}), json!([1, 2])] {
            let before = v.clone();
            shape(&mut v);
            assert_eq!(v, before);
        }
    }
}
