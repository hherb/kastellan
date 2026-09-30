//! Why a reply was not delivered, and what the writer of the
//! [`actions::REPLY_UNDELIVERED`] row may see of it (#782, #790). Split out of
//! [`super`] to keep it under the 500-LOC soft cap; re-exported there, so
//! every path is unchanged.

#[allow(unused_imports)] // referenced by the doc comments' intra-doc links
use super::actions;
use super::{ChannelId, ConversationId, OutgoingMessage, PeerId};

/// Why a reply was not delivered: the `reason` of an
/// [`actions::REPLY_UNDELIVERED`] row. A fixed label, never transport text, so
/// observation SQL can group on it. Each calls for a different operator
/// action, which is why the row carries it (#782 review).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndeliveredReason {
    /// The bus's `Channel::send` failed (the email channel until slice 2).
    SendFailed,
    /// A polled driver gave up on a reply its worker kept refusing: look at
    /// the conversation (a room the bot was removed from).
    GaveUp,
    /// A polled driver dropped a reply past a full conversation queue: the
    /// conversation is stuck, and more replies kept coming.
    QueueFull,
    /// A polled driver exited with the reply still queued: the channel was
    /// restarted or shut down while the reply waited.
    DriverExit,
}

impl UndeliveredReason {
    /// The label stored in the row. A committed operator-facing interface,
    /// pinned literally by a test.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SendFailed => "send_failed",
            Self::GaveUp => "gave_up",
            Self::QueueFull => "queue_full",
            Self::DriverExit => "driver_exit",
        }
    }
}

/// What the writer of an [`actions::REPLY_UNDELIVERED`] row is told about a
/// reply a polled driver dropped: whose it was, where it was going, and why —
/// **never what it said** (#790).
///
/// The row must carry channel, peer, reason and when only (a reply is conversation
/// content). Until #790 the driver's audit hook was handed the whole
/// [`OutgoingMessage`], so that rule was kept by a doc comment and by the one
/// sink happening to call [`reply_undelivered_payload`]. This view has no body
/// field, so no sink can write one.
///
/// `conversation` is not in the row. It is here so a sink whose insert fails
/// can name the conversation in its own log line, matching the driver's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UndeliveredReply<'a> {
    pub channel: &'a ChannelId,
    pub peer: &'a PeerId,
    pub conversation: &'a ConversationId,
    pub reason: UndeliveredReason,
    /// When the driver dropped it. The row is written after the fact (#789),
    /// so `audit_log.ts` is the insert's time, not this.
    pub observed_at: time::OffsetDateTime,
}

impl<'a> UndeliveredReply<'a> {
    /// Pure: the view of `out`, dropped for `reason` at `observed_at`. The
    /// body stays behind.
    pub fn of(
        out: &'a OutgoingMessage,
        reason: UndeliveredReason,
        observed_at: time::OffsetDateTime,
    ) -> Self {
        Self {
            channel: &out.channel,
            peer: &out.peer,
            conversation: &out.conversation,
            reason,
            observed_at,
        }
    }

    /// Pure: this reply's row payload ([`reply_undelivered_payload`]).
    pub fn payload(&self) -> serde_json::Value {
        reply_undelivered_payload(self.channel, self.peer, self.reason, self.observed_at)
    }
}

/// Pure: the payload of an [`actions::REPLY_UNDELIVERED`] row — the channel,
/// the peer, the [`UndeliveredReason`] and when it happened, nothing else.
/// Never the body (a reply is conversation content) and never the error
/// (transport text, not a fixed label). The one definition for every writer of
/// that row; see the action's doc.
///
/// `observed_at` is the event's time, because a polled driver's row is written
/// after the fact (#789) and `audit_log.ts` is then the insert's. The bus's own
/// writer awaits its insert, so for it the two agree; it carries the field
/// anyway, so every row of the action has one shape.
pub fn reply_undelivered_payload(
    channel: &ChannelId,
    peer: &PeerId,
    reason: UndeliveredReason,
    observed_at: time::OffsetDateTime,
) -> serde_json::Value {
    serde_json::json!({
        "channel": channel.0,
        "peer": peer.0,
        "reason": reason.as_str(),
        "observed_at": observed_at_json(observed_at),
    })
}

/// Pure: an event time as a row stores it — RFC 3339, in the value's own offset (the
/// drivers pass UTC). `null`
/// for a time RFC 3339 cannot spell (a year outside 0–9999), which no clock
/// this daemon reads will produce, rather than a panic in an audit path.
pub(crate) fn observed_at_json(t: time::OffsetDateTime) -> serde_json::Value {
    t.format(&time::format_description::well_known::Rfc3339)
        .map_or(serde_json::Value::Null, serde_json::Value::String)
}

#[cfg(test)]
mod tests {
    fn at() -> time::OffsetDateTime {
        time::macros::datetime!(2026-09-30 12:34:56 UTC)
    }

    #[test]
    fn a_reply_undelivered_payload_carries_channel_peer_reason_and_when_only() {
        let v = super::reply_undelivered_payload(
            &super::ChannelId("matrix".into()),
            &super::PeerId("@me:srv".into()),
            super::UndeliveredReason::GaveUp,
            at(),
        );
        assert_eq!(
            v,
            serde_json::json!({
                "channel": "matrix",
                "peer": "@me:srv",
                "reason": "gave_up",
                "observed_at": "2026-09-30T12:34:56Z",
            })
        );
    }

    #[test]
    fn an_event_time_is_rfc3339_and_a_year_it_cannot_spell_is_null() {
        assert_eq!(super::observed_at_json(at()), serde_json::json!("2026-09-30T12:34:56Z"));
        let far = time::macros::datetime!(-0001-01-01 0:00 UTC);
        assert_eq!(super::observed_at_json(far), serde_json::Value::Null);
    }

    /// #790: the audit hook's view of a dropped reply cannot carry its body.
    /// Debug renders every field the view has, so a body field added later
    /// fails here.
    #[test]
    fn the_undelivered_view_leaves_the_body_behind() {
        let out = super::OutgoingMessage {
            channel: super::ChannelId("matrix".into()),
            peer: super::PeerId("@me:srv".into()),
            conversation: super::ConversationId("!room:srv".into()),
            body: "SECRET-BODY".into(),
        };
        let view = super::UndeliveredReply::of(&out, super::UndeliveredReason::GaveUp, at());
        assert!(format!("{view:?}").contains("!room:srv"), "POSITIVE CONTROL: Debug renders the fields");
        assert!(!format!("{view:?}").contains("SECRET-BODY"), "{view:?}");
        assert_eq!(
            view.payload(),
            serde_json::json!({
                "channel": "matrix",
                "peer": "@me:srv",
                "reason": "gave_up",
                "observed_at": "2026-09-30T12:34:56Z",
            })
        );
    }

    /// The `reason` labels are a durable operator interface, like the ask
    /// reasons in `super::super`'s tests: pinned literally, so a renamed label
    /// fails here.
    #[test]
    fn the_undelivered_reasons_are_pinned_literally() {
        use super::UndeliveredReason::*;
        let labels: Vec<_> = [SendFailed, GaveUp, QueueFull, DriverExit].map(|r| r.as_str()).into();
        assert_eq!(labels, ["send_failed", "gave_up", "queue_full", "driver_exit"]);
    }
}
