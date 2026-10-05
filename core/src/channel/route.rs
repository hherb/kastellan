//! Pure outbound mapping: turn a finalized `tasks` row (its `payload` routing
//! metadata + its `result`) into the [`OutgoingMessage`] reply, or `None` if the
//! task did not originate from a channel. No DB, no I/O.
//!
//! The result body shown to the user is derived from `Outcome::result_payload()`
//! (`core/src/scheduler/inner_loop.rs`). A **completed** task's result is the
//! agent's `plan.result` — default shape `{"kind":"text","body":"..."}` — so the
//! reply surfaces its `body` (then a `message` alias, then compact JSON for a
//! structured result). `error`/`blocked`/`refused`/`denied` carry the fixed `kind`s
//! `result_payload()` stamps and map to safe, user-facing sentences. Replies go
//! only to the *paired* user, so error detail is acceptable to surface (the
//! recipient is the authorized operator).

use serde_json::Value;

use super::OutgoingMessage;

/// Build the reply for a finalized channel task. Returns `None` (with no error)
/// when `payload.kind != "channel"` (an `ask`/`l3_run` completion the bus must
/// ignore) or routing metadata is missing/malformed (the caller, which can
/// tell the two apart by `kind`, warns for the second).
pub fn reply_for_completed_task(payload: &Value, result: Option<&Value>) -> Option<OutgoingMessage> {
    // The same four keys the ask-delivery path reads, through the same
    // function (spec D10) — so where an ask is asked and where its task's
    // answer is delivered cannot drift apart.
    let dest = super::ask_message::destination_from_task_payload(payload)?;
    Some(OutgoingMessage {
        channel: dest.channel,
        peer: dest.peer,
        conversation: dest.conversation,
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
        // An operator ask timed out (#564 slice 2, spec D14). The generic
        // error arm below renders this as "Sorry — that failed:
        // ask_timeout", which is true and tells the user nothing: their
        // request stalled because a question *about* it went unanswered.
        // Matched on the exact detail string `db::asks` defines, so a
        // different error carrying a similar-looking detail is unaffected.
        Some("error")
            if result.get("detail").and_then(Value::as_str)
                == Some(kastellan_db::asks::ASK_TIMEOUT_DETAIL) =>
        {
            "I needed an operator to approve something before continuing, and nobody \
             answered in time, so I stopped."
                .to_string()
        }
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
        // An operator was asked to decide and said no (#564 slice 1b).
        // Without this arm the `{"kind":"denied", …}` payload falls into
        // the completion arm below and the user is shown raw JSON as if it
        // were their answer. `reason` is the escalation concern the
        // operator was answering — never their private free-text note,
        // which `Outcome::Denied` deliberately does not carry (spec D10).
        Some("denied") => match str_field(result, "reason") {
            Some(reason) => format!("An operator declined that: {reason}."),
            None => "An operator declined that request.".to_string(),
        },
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

    use super::super::{ChannelId, ConversationId, PeerId};

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

    #[test]
    fn a_denied_task_reads_as_a_refusal_not_as_an_answer() {
        // `Outcome::Denied` stamps {"kind":"denied","ask_id":N,"reason":…}.
        // Before this arm existed it fell into the completion branch, so a
        // user whose task an operator DENIED was handed raw JSON presented
        // as their answer.
        let body = reply_body(Some(&json!({
            "kind": "denied",
            "ask_id": 41,
            "reason": "sends mail to a stranger",
        })));
        assert!(
            body.contains("sends mail to a stranger"),
            "the concern the operator answered must reach the user: {body:?}",
        );
        assert!(!body.contains("ask_id"), "never raw JSON: {body:?}");
        assert!(!body.contains('{'), "never raw JSON: {body:?}");
    }

    #[test]
    fn a_denial_without_a_reason_still_reads_as_a_refusal() {
        // Defence in depth: `Outcome::Denied` always carries `reason`, but a
        // missing or empty one must not fall through to the completion arm
        // and hand the user a JSON blob.
        for r in [json!({"kind":"denied","ask_id":41}),
                  json!({"kind":"denied","ask_id":41,"reason":"  "})] {
            let body = reply_body(Some(&r));
            assert!(!body.contains('{'), "never raw JSON: {body:?}");
            assert!(body.contains("declined"), "must read as a refusal: {body:?}");
        }
    }

    /// D14. An expired ask already reaches the room — `notify_task_completed`
    /// is an `AFTER UPDATE OF state` trigger and `awaiting_operator → failed`
    /// crosses into its terminal set — so the only question is what it says.
    /// "Sorry — that failed: ask_timeout" is true and useless; the user's
    /// question stalled because nobody answered a question about it.
    #[test]
    fn an_ask_timeout_reads_as_an_unanswered_question_not_a_crash() {
        let body = reply_body(Some(&json!({"kind": "error", "detail": "ask_timeout"})));
        assert!(!body.contains("ask_timeout"), "the raw detail string is not user-facing: {body}");
        let lowered = body.to_lowercase();
        assert!(lowered.contains("answer"), "must say nobody answered: {body}");
    }

    /// Every other error detail keeps the existing generic rendering — the new
    /// arm must be exactly one detail string wide, not a prefix match that
    /// swallows unrelated failures.
    #[test]
    fn other_error_details_are_unchanged_by_the_timeout_arm() {
        let body = reply_body(Some(&json!({"kind": "error", "detail": "ask_timeout_but_not_really"})));
        assert!(body.contains("ask_timeout_but_not_really"), "{body}");
    }

    /// D10: the ask's destination and the reply's routing are read off the same
    /// payload by the same function. Asserted directly, because the failure
    /// mode is silent — the two drift, and an ask is delivered to a
    /// conversation the answer never returns to.
    #[test]
    fn the_reply_route_and_the_ask_destination_agree() {
        use crate::channel::ask_message::destination_from_task_payload;
        let p = channel_payload();
        let reply = reply_for_completed_task(&p, Some(&json!({"kind": "completed"}))).expect("reply");
        let dest = destination_from_task_payload(&p).expect("destination");
        assert_eq!(reply.channel, dest.channel);
        assert_eq!(reply.peer, dest.peer);
        assert_eq!(reply.conversation, dest.conversation);
    }
}
