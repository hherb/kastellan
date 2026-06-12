//! Pure outbound mapping: turn a finalized `tasks` row (its `payload` routing
//! metadata + its `result`) into the [`OutgoingMessage`] reply, or `None` if the
//! task did not originate from a channel. No DB, no I/O.
//!
//! The result body shown to the user is derived from `Outcome::result_payload()`
//! (`core/src/scheduler/inner_loop.rs`). A **completed** task's result is the
//! agent's `plan.result` — default shape `{"kind":"text","body":"..."}` — so the
//! reply surfaces its `body` (then a `message` alias, then compact JSON for a
//! structured result). `error`/`blocked`/`refused` carry the fixed `kind`s
//! `result_payload()` stamps and map to safe, user-facing sentences. Replies go
//! only to the *paired* user, so error detail is acceptable to surface (the
//! recipient is the authorized operator).

use serde_json::Value;

use super::{ChannelId, ConversationId, OutgoingMessage, PeerId};

/// Build the reply for a finalized channel task. Returns `None` (with no error)
/// when `payload.kind != "channel"` (an `ask`/`l3_run` completion the bus must
/// ignore) or routing metadata is missing/malformed (the caller logs a warn).
pub fn reply_for_completed_task(payload: &Value, result: Option<&Value>) -> Option<OutgoingMessage> {
    if payload.get("kind").and_then(Value::as_str) != Some("channel") {
        return None;
    }
    let channel = payload.get("channel").and_then(Value::as_str)?;
    let peer = payload.get("peer").and_then(Value::as_str)?;
    let conversation = payload.get("conversation").and_then(Value::as_str)?;

    Some(OutgoingMessage {
        channel: ChannelId(channel.to_string()),
        peer: PeerId(peer.to_string()),
        conversation: ConversationId(conversation.to_string()),
        body: reply_body(result),
    })
}

/// Map a finalized task `result` to a user-facing body.
pub fn reply_body(result: Option<&Value>) -> String {
    let Some(result) = result else {
        return "Task finished, but produced no result.".to_string();
    };
    // The non-completion outcomes carry the fixed `kind`s that
    // `Outcome::result_payload()` (`scheduler/inner_loop.rs`) stamps.
    match result.get("kind").and_then(Value::as_str) {
        Some("error") => format!(
            "Sorry — that failed: {}",
            result.get("detail").and_then(Value::as_str).unwrap_or("unknown error")
        ),
        Some("blocked") => format!(
            "I can't do that (policy: {}).",
            result.get("principle").and_then(Value::as_str).unwrap_or("blocked")
        ),
        Some("refused") => str_field(result, "body")
            .unwrap_or_else(|| "I have to decline that request.".to_string()),
        // Anything else is a successful completion: the agent's `plan.result`,
        // whose default shape is `{"kind":"text","body":"..."}` (a custom kind is
        // also possible). Surface the human-facing `body`, then a `message` alias,
        // then compact JSON for a structured result with neither.
        _ => completion_body(result),
    }
}

/// Extract the user-facing text from a successful completion result.
fn completion_body(result: &Value) -> String {
    str_field(result, "body")
        .or_else(|| str_field(result, "message"))
        .unwrap_or_else(|| compact(result))
}

/// A non-empty string field, trimmed-checked (an empty `body` is treated as
/// absent so an empty default completion doesn't surface as a blank reply).
fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "(unserializable result)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn channel_payload() -> Value {
        json!({"kind":"channel","channel":"matrix","peer":"@me:srv","conversation":"!room:srv","instruction":"hi"})
    }

    #[test]
    fn non_channel_task_yields_no_reply() {
        let p = json!({"kind":"ask","instruction":"hi"});
        assert!(reply_for_completed_task(&p, Some(&json!({"kind":"completed"}))).is_none());
    }

    #[test]
    fn missing_routing_yields_no_reply() {
        let p = json!({"kind":"channel","instruction":"hi"}); // no channel/peer/conversation
        assert!(reply_for_completed_task(&p, Some(&json!({"kind":"completed"}))).is_none());
    }

    #[test]
    fn completed_with_message_routes_to_origin() {
        let out = reply_for_completed_task(
            &channel_payload(),
            Some(&json!({"kind":"completed","message":"It's sunny."})),
        )
        .expect("reply");
        assert_eq!(out.channel, ChannelId("matrix".into()));
        assert_eq!(out.peer, PeerId("@me:srv".into()));
        assert_eq!(out.conversation, ConversationId("!room:srv".into()));
        assert_eq!(out.body, "It's sunny.");
    }

    #[test]
    fn completed_without_message_falls_back_to_compact_json() {
        let out = reply_for_completed_task(
            &channel_payload(),
            Some(&json!({"kind":"completed","answer":42})),
        )
        .unwrap();
        assert!(out.body.contains("42"));
    }

    #[test]
    fn real_completion_shape_surfaces_the_agent_body() {
        // The actual finalized result for a completed task: plan.result, default
        // shape {"kind":"text","body":...}. Must surface `body`, not "Task
        // finished (text)." (the slice-#1 bug this slice fixes).
        let out = reply_for_completed_task(
            &channel_payload(),
            Some(&json!({"kind":"text","body":"You have 2 meetings today."})),
        )
        .unwrap();
        assert_eq!(out.body, "You have 2 meetings today.");
    }

    #[test]
    fn empty_text_body_falls_through_not_blank() {
        // The empty default completion {"kind":"text","body":""} must not produce
        // a blank reply — an empty body is treated as absent → compact fallback.
        let body = reply_body(Some(&json!({"kind":"text","body":"   "})));
        assert!(!body.trim().is_empty(), "reply must not be blank: {body:?}");
    }

    #[test]
    fn custom_completion_kind_with_body_surfaces_body() {
        let body = reply_body(Some(&json!({"kind":"summary","body":"3 items."})));
        assert_eq!(body, "3 items.");
    }

    #[test]
    fn error_blocked_refused_map_to_safe_sentences() {
        let err = reply_body(Some(&json!({"kind":"error","detail":"db down"})));
        assert!(err.contains("db down"));
        let blk = reply_body(Some(&json!({"kind":"blocked","principle":"privacy"})));
        assert!(blk.contains("privacy"));
        let refused = reply_body(Some(&json!({"kind":"refused","body":"No."})));
        assert_eq!(refused, "No.");
    }
}
