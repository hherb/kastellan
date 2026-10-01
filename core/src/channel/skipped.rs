//! What the writer of an [`actions::SKIPPED_ACK_ONLY`] row is told about an
//! id a polled driver acked without it ever becoming an event (#793), and the
//! row's one definition. Mirrors [`super::UndeliveredReply`] for replies.

#[allow(unused_imports)] // referenced by the doc comments' intra-doc links
use super::actions;
use super::audit_text::cap_chars;
use super::boot_supervisor::AUDIT_CAUSE_CAP_CHARS;
use super::undelivered::observed_at_json;
use super::ChannelId;

/// Cap on `reason`'s length before it becomes a durable `audit_log` payload
/// value. `reason` originates from the worker's `skipped[].reason`
/// (`workers/email-in/src/handler.rs::describe_email_error`), which already
/// truncates an upstream (localmail) HTTP error body to 200 chars — but the
/// polled driver that hands it on is DB-free by design and applies no cap of
/// its own. Defence in depth: the row must not trust a future worker change
/// (or a compromised worker) to keep bounding it before it lands permanently
/// in `audit_log`. Comfortably above the worker's own 200-char cap so today's
/// values pass through untouched.
///
/// Aliased to the boot supervisor's cap rather than a second `256` typed here:
/// both bound an unbounded, externally-originated string on its way into the
/// same column, so one number and one set of edge cases is the honest
/// arrangement.
pub const SKIPPED_REASON_CAP_CHARS: usize = AUDIT_CAUSE_CAP_CHARS;

/// An id a polled driver is about to ack although it never became an event
/// (email's `skipped` list: an unattributable `From`, an unfetchable detail
/// fetch), as the driver's `AckOnlyAudit` hook is handed it.
///
/// Named fields rather than the positional `(message_id, reason)` it replaced
/// (#793): two `&str`s side by side are easy to pass swapped, and nothing
/// would notice until an operator read a row whose message id was a sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkippedId<'a> {
    /// The channel whose driver skipped it.
    pub channel: &'a ChannelId,
    /// The upstream's id for the message: worker-written, so untrusted.
    pub message_id: &'a str,
    /// Why the worker skipped it: worker-written, so untrusted and unbounded.
    pub reason: &'a str,
    /// When the driver skipped it. The row is written after the fact
    /// (#789), so `audit_log.ts` is the insert's time, not this.
    pub observed_at: time::OffsetDateTime,
}

impl SkippedId<'_> {
    /// Pure: this id's row payload — the channel, the message id, the reason
    /// capped at [`SKIPPED_REASON_CAP_CHARS`] on a `char` boundary, and when.
    /// Never a body, never headers.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "channel": self.channel.0,
            "message_id": self.message_id,
            "reason": cap_chars(self.reason, SKIPPED_REASON_CAP_CHARS),
            "observed_at": observed_at_json(self.observed_at),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> time::OffsetDateTime {
        time::macros::datetime!(2026-09-30 12:34:56 UTC)
    }

    /// The row: the message id and the reason ONLY, plus the channel and when.
    #[test]
    fn a_skipped_id_payload_is_the_id_the_reason_and_when() {
        let channel = ChannelId("email".into());
        let id = SkippedId {
            channel: &channel,
            message_id: "<id@host>",
            reason: "no usable From address",
            observed_at: at(),
        };
        assert_eq!(
            id.payload(),
            serde_json::json!({
                "channel": "email",
                "message_id": "<id@host>",
                "reason": "no usable From address",
                "observed_at": "2026-09-30T12:34:56Z",
            })
        );
    }

    /// The reason is capped: it is worker-written and nothing upstream of the
    /// row bounds it.
    #[test]
    fn a_skipped_id_s_reason_is_capped() {
        let channel = ChannelId("email".into());
        let long = "r".repeat(SKIPPED_REASON_CAP_CHARS + 10);
        let id = SkippedId { channel: &channel, message_id: "<id@host>", reason: &long, observed_at: at() };
        let payload = id.payload();
        assert_eq!(payload["reason"], serde_json::json!(cap_chars(&long, SKIPPED_REASON_CAP_CHARS)));
        assert_ne!(payload["reason"], serde_json::json!(long), "POSITIVE CONTROL: the cap must bite");
    }
}
