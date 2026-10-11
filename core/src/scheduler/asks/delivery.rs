//! Delivering a raised ask to the conversation its task came from.
//!
//! **Sync and separable**, deliberately: the decision (where does this go,
//! and did it get there?) and the audit row it produces are both separated
//! from the `await`ing emitter in [`super::lifecycle`]. Not *pure* —
//! `deliver_ask` pushes into a live queue via `ChannelOutbox::try_deliver`,
//! which is the whole observable effect; only `delivery_audit_row` is a
//! pure function. That is what lets every
//! branch below — including all three failure branches — have a unit test,
//! on a path whose async half needs a live Postgres.
//!
//! **Delivery never fails the ask** (spec D2). By the time anything here
//! runs, `db::asks::raise` has committed: the ask is durable and the task
//! is suspended. A Matrix outage must not turn into a task failure on the
//! one path where the reviewer said a human must decide.
//!
//! Spec: `docs/superpowers/specs/2026-08-19-ask-channel-slice-2-design.md`.

use time::OffsetDateTime;

use crate::channel::ask_message::{render_ask, AskDestination};
use crate::channel::outbox::ChannelOutbox;
use crate::channel::OutgoingMessage;
use crate::scheduler::audit::{
    ACTION_ASK_DELIVERED, ACTION_ASK_DELIVERY_FAILED, ACTION_ASK_UNDELIVERED,
};

/// The task did not come from a channel, so there is nobody to send to.
pub const REASON_NO_ORIGIN: &str = "task_has_no_channel_origin";

/// No channel is configured on this host at all.
pub const REASON_NO_CHANNEL: &str = "no_channel_configured";

/// What happened to one delivery attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    /// Queued to the channel's own pump.
    Delivered { channel: String, peer: String },
    /// Not sent, and expected — see [`REASON_NO_ORIGIN`] / [`REASON_NO_CHANNEL`].
    Undelivered { reason: &'static str },
    /// A channel existed and refused it; `reason` is an `OutboxError` label.
    Failed { channel: String, reason: &'static str },
}

/// Render the ask and queue it to its task's own channel.
///
/// Both `Option`s are "absent is normal, not an error": no destination means
/// a non-channel task (spec D3), no outbox means a daemon built or
/// configured without channels.
pub fn deliver_ask(
    outbox: Option<&ChannelOutbox>,
    dest: Option<&AskDestination>,
    task_id: i64,
    concern: &str,
    token: &str,
    deadline_at: OffsetDateTime,
) -> DeliveryOutcome {
    let Some(dest) = dest else {
        return DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN };
    };
    let Some(outbox) = outbox else {
        return DeliveryOutcome::Undelivered { reason: REASON_NO_CHANNEL };
    };

    let msg = OutgoingMessage {
        channel: dest.channel.clone(),
        peer: dest.peer.clone(),
        conversation: dest.conversation.clone(),
        body: render_ask(task_id, concern, token, deadline_at),
    };
    match outbox.try_deliver(task_id, msg) {
        Ok(()) => DeliveryOutcome::Delivered {
            channel: dest.channel.0.clone(),
            peer: dest.peer.0.clone(),
        },
        Err(e) => DeliveryOutcome::Failed {
            channel: dest.channel.0.clone(),
            reason: e.as_str(),
        },
    }
}

/// The `(action, payload)` for one delivery outcome.
///
/// Split from [`deliver_ask`] so the mapping is testable without a pool.
/// None of the three payloads below can carry the token, the concern or the
/// rendered body: `DeliveryOutcome` itself never holds them past this
/// point. `tests::no_audit_payload_carries_the_token_the_concern_or_the_body`
/// pins the exact key set of each payload so that a future field added to
/// a `DeliveryOutcome` variant (e.g. a `concern` for debuggability) that
/// this mapping then passed through would fail the test, rather than the
/// property silently ceasing to hold.
pub fn delivery_audit_row(
    ask_id: i64,
    task_id: i64,
    outcome: &DeliveryOutcome,
) -> (&'static str, serde_json::Value) {
    match outcome {
        DeliveryOutcome::Delivered { channel, peer } => (
            ACTION_ASK_DELIVERED,
            serde_json::json!({
                "ask_id": ask_id, "task_id": task_id,
                "channel": channel, "peer": peer,
            }),
        ),
        DeliveryOutcome::Undelivered { reason } => (
            ACTION_ASK_UNDELIVERED,
            serde_json::json!({"ask_id": ask_id, "task_id": task_id, "reason": reason}),
        ),
        DeliveryOutcome::Failed { channel, reason } => (
            ACTION_ASK_DELIVERY_FAILED,
            serde_json::json!({
                "ask_id": ask_id, "task_id": task_id,
                "channel": channel, "reason": reason,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    use crate::channel::{ChannelId, ConversationId, PeerId};

    fn dest() -> AskDestination {
        AskDestination {
            channel: ChannelId("matrix".into()),
            peer: PeerId("@horst:srv".into()),
            conversation: ConversationId("!room:srv".into()),
        }
    }

    fn deadline() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(1_787_000_000).unwrap()
    }

    #[tokio::test]
    async fn a_channel_task_gets_the_rendered_ask_on_its_own_channel() {
        let outbox = ChannelOutbox::new();
        let (tx, mut rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);

        let outcome =
            deliver_ask(Some(&outbox), Some(&dest()), 412, "writes outside scratch", "tok9", deadline());
        assert_eq!(
            outcome,
            DeliveryOutcome::Delivered { channel: "matrix".into(), peer: "@horst:srv".into() }
        );

        let crate::channel::bus::Queued::Ask { task_id, out: sent } = rx.recv().await.expect("message queued") else {
            panic!("a raised ask is queued as an ask, with no reply to claim");
        };
        assert_eq!(task_id, 412, "the task id rides along, for a stop's line");
        assert_eq!(sent.conversation.0, "!room:srv");
        assert!(sent.body.contains("/approve tok9"), "{}", sent.body);
        assert!(sent.body.contains("writes outside scratch"), "{}", sent.body);
    }

    /// D3: a `kastellan-cli ask` or scheduled task has no peer to ask. That
    /// is not an error — the ask is durable and the CLI answers it — but it
    /// must leave a row, or an escalation nobody was told about is
    /// indistinguishable from one that was delivered.
    #[test]
    fn a_task_with_no_channel_origin_is_undelivered_and_says_so() {
        let outbox = ChannelOutbox::new();
        let outcome = deliver_ask(Some(&outbox), None, 412, "c", "tok", deadline());
        assert_eq!(outcome, DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN });
    }

    /// A daemon with no channel configured at all — the Matrix-less build.
    /// Distinguished from the no-origin case because they mean different
    /// things to an operator reading the trail: one is "this task came from
    /// the CLI", the other is "this host has no way to reach you".
    #[test]
    fn no_outbox_at_all_is_a_distinct_undelivered_reason() {
        let outcome = deliver_ask(None, Some(&dest()), 412, "c", "tok", deadline());
        assert_eq!(outcome, DeliveryOutcome::Undelivered { reason: REASON_NO_CHANNEL });
    }

    /// The two prior tests each leave exactly one `Option` populated, so
    /// they pass under either check order — neither observes which of
    /// `dest` / `outbox` `deliver_ask` looks at first. Only `(None, None)`
    /// does: with both absent, checking `dest` first reports
    /// `REASON_NO_ORIGIN` ("this task came from the CLI"), while checking
    /// `outbox` first would report `REASON_NO_CHANNEL` ("this host cannot
    /// reach you") instead. The order is load-bearing for a CLI-originated
    /// task running on a daemon with no channel configured at all, and this
    /// is the only input that pins it.
    #[test]
    fn with_neither_a_destination_nor_an_outbox_the_missing_origin_is_reported() {
        let outcome = deliver_ask(None, None, 412, "c", "tok", deadline());
        assert_eq!(outcome, DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN });
    }

    /// The bus restarts under the scheduler, so a registered-but-dead queue
    /// is a real state. It must be reported, not swallowed.
    #[test]
    fn a_dead_queue_is_a_failure_carrying_the_transport_reason() {
        let outbox = ChannelOutbox::new();
        let (tx, rx) = mpsc::channel(4);
        outbox.register(ChannelId("matrix".into()), tx);
        drop(rx);

        let outcome = deliver_ask(Some(&outbox), Some(&dest()), 412, "c", "tok", deadline());
        assert_eq!(
            outcome,
            DeliveryOutcome::Failed { channel: "matrix".into(), reason: "queue_closed" }
        );
    }

    #[test]
    fn a_channel_that_is_not_up_yet_is_a_failure_not_a_panic() {
        let outbox = ChannelOutbox::new();
        let outcome = deliver_ask(Some(&outbox), Some(&dest()), 412, "c", "tok", deadline());
        assert_eq!(
            outcome,
            DeliveryOutcome::Failed { channel: "matrix".into(), reason: "no_such_channel" }
        );
    }

    /// The nonce is a live approval token and `audit_log` is readable by
    /// every role that can read the trail. Same rule as `ask.raised`, which
    /// omits it for the same reason — and this is the path that actually
    /// holds the plaintext, so the omission has to be asserted.
    ///
    /// **Pins the exact key set of each payload**, not just the absence of
    /// today's literals. `DeliveryOutcome` never carries the token, the
    /// concern or the rendered body in the first place, so a `!contains`
    /// check alone is true unconditionally — it would not catch a future
    /// `DeliveryOutcome` variant that grew a `concern` field this mapping
    /// then passed straight through. The `assert_eq!` on the sorted key
    /// list is what actually guards that: it fails the moment an extra key
    /// appears, whatever it's named or contains. The `!contains` checks
    /// stay as belt-and-braces against the *known* literals.
    #[test]
    fn no_audit_payload_carries_the_token_the_concern_or_the_body() {
        let cases: [(DeliveryOutcome, &[&str]); 3] = [
            (
                DeliveryOutcome::Delivered { channel: "matrix".into(), peer: "@horst:srv".into() },
                &["ask_id", "channel", "peer", "task_id"],
            ),
            (
                DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN },
                &["ask_id", "reason", "task_id"],
            ),
            (
                DeliveryOutcome::Failed { channel: "matrix".into(), reason: "queue_closed" },
                &["ask_id", "channel", "reason", "task_id"],
            ),
        ];
        for (outcome, expected_keys) in cases {
            let (_action, payload) = delivery_audit_row(7, 412, &outcome);
            let rendered = serde_json::to_string(&payload).unwrap();
            assert!(!rendered.contains("tok9"), "token leaked: {rendered}");
            assert!(!rendered.contains("writes outside scratch"), "concern leaked: {rendered}");
            assert!(!rendered.contains("/approve"), "body leaked: {rendered}");

            let mut keys: Vec<&str> =
                payload.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(keys, expected_keys, "unexpected key set for {outcome:?}: {rendered}");

            assert_eq!(payload["ask_id"], 7);
            assert_eq!(payload["task_id"], 412);
        }
    }

    #[test]
    fn each_outcome_maps_to_its_own_action_and_keeps_its_reason() {
        let (a, _) = delivery_audit_row(
            7, 412,
            &DeliveryOutcome::Delivered { channel: "matrix".into(), peer: "@p".into() },
        );
        assert_eq!(a, ACTION_ASK_DELIVERED);

        let (a, p) =
            delivery_audit_row(7, 412, &DeliveryOutcome::Undelivered { reason: REASON_NO_ORIGIN });
        assert_eq!(a, ACTION_ASK_UNDELIVERED);
        assert_eq!(p["reason"], REASON_NO_ORIGIN);

        let (a, p) = delivery_audit_row(
            7, 412,
            &DeliveryOutcome::Failed { channel: "matrix".into(), reason: "queue_full" },
        );
        assert_eq!(a, ACTION_ASK_DELIVERY_FAILED);
        assert_eq!(p["reason"], "queue_full");
        assert_eq!(p["channel"], "matrix");
    }
}
