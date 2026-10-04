//! The channel bus's inbound NUL boundary (#818).
//!
//! ## Why this exists
//!
//! Postgres cannot store U+0000 in `text` or `jsonb`, and every field of an
//! [`IncomingMessage`] is written to it: the peer and conversation ids into
//! the task payload (and the pairing tables), the body as the task's
//! `instruction`. A peer controls all three. Before #818 a NUL in any of them
//! failed a write somewhere downstream, each in its own misleading way:
//!
//! * in the **peer**, the pairing lookup failed in Postgres, was logged as
//!   "pairing lookup failed", and the message was audited as an ordinary
//!   `channel.rejected_unpaired`;
//! * in the **conversation id or body** of a *paired* peer, the task insert
//!   failed and the message was dropped with a `warn!` and **no audit row** —
//!   on email, after the driver had already acked it, so it was gone.
//!
//! ## What it does
//!
//! It applies the rule `kastellan_db::nul` states — escape records, refuse
//! identities — at the one place every inbound message passes, the head of
//! [`super::bus::handle_inbound`], before authorization:
//!
//! * a NUL in the **channel, peer or conversation id** refuses the message
//!   with one [`actions::REJECTED_MALFORMED`] row naming the field. These are
//!   identities: a reply is routed by them and a pairing matched on them, so
//!   rewriting one would be an identity decision. No ack is sent, as for an
//!   unpaired peer.
//! * a NUL in the **body** is escaped to `␀` and the message carries on. The
//!   body is the peer's words, a record; keeping the message with one glyph
//!   rewritten beats dropping it.
//!
//! Identity is checked first, so a message with NULs in both is refused —
//! escaping the body never launders an id.
//!
//! Two consequences of escaping the body, both deliberate:
//!
//! * `channel.injection_blocked`'s `sha256` is computed over the **escaped**
//!   body, so for a NUL-bearing message it will not match a hash of the
//!   transport's raw bytes.
//! * a body the task insert used to refuse now reaches the queue. One such
//!   shape is a verb glued to a live token by a NUL (`/approve\0TOKEN`): it
//!   was "contained" only by the insert failing, and is now enqueued with
//!   the token in it — the containment arm's accepted open risk (no
//!   whitespace-separated verb, no gate), not a new class. A peer could
//!   already send the same text with a literal `␀`.
//!
//! ⚠️ Running before authorization means an **unpaired** peer can produce a
//! `rejected_malformed` row, exactly as it can produce a `rejected_unpaired`
//! one today: one row per message, no more. The body escape writes no row and
//! no `warn!` of its own, for the same reason — the `␀` in the stored
//! `instruction` is its record.

use std::borrow::Cow;

use kastellan_db::nul::escape_str;

use super::bus::ChannelEvents;
use super::{actions, IncomingMessage};

/// The fixed `reason` label on a [`actions::REJECTED_MALFORMED`] row for a
/// NUL. A label rather than prose so the row is safe to persist and to
/// group by; a later malformation gets its own.
pub const REASON_NUL: &str = "nul";

/// Which identity field of an [`IncomingMessage`] was malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MalformedField {
    Channel,
    Peer,
    Conversation,
}

impl MalformedField {
    /// The audit label: a fixed `[a-z_]` string, never message text.
    pub fn as_str(self) -> &'static str {
        match self {
            MalformedField::Channel => "channel",
            MalformedField::Peer => "peer",
            MalformedField::Conversation => "conversation",
        }
    }
}

/// What [`screen`] decided.
#[derive(Debug, PartialEq, Eq)]
pub enum NulScreen {
    /// No NUL anywhere; use the message as it is.
    Clean,
    /// The body held `nuls` NULs, now `␀`; every other field is unchanged.
    BodyEscaped { msg: IncomingMessage, nuls: u64 },
    /// An identity field holds a NUL; the message must be refused.
    Malformed { field: MalformedField },
}

/// Decide what a NUL in `msg` means. Pure; see the module doc for the rule.
pub fn screen(msg: &IncomingMessage) -> NulScreen {
    let ids = [
        (MalformedField::Channel, &msg.channel.0),
        (MalformedField::Peer, &msg.peer.0),
        (MalformedField::Conversation, &msg.conversation.0),
    ];
    if let Some((field, _)) = ids.iter().find(|(_, id)| id.contains('\0')) {
        return NulScreen::Malformed { field: *field };
    }
    match escape_str(&msg.body) {
        (Cow::Borrowed(_), _) => NulScreen::Clean,
        (Cow::Owned(body), nuls) => NulScreen::BodyEscaped {
            msg: IncomingMessage { body, ..msg.clone() },
            nuls,
        },
    }
}

/// Apply [`screen`] at the head of the bus: the message to carry on with,
/// or `None` after writing the refusal's audit row.
///
/// The row carries the channel, the peer, the field and [`REASON_NUL`] —
/// never the body. The peer is passed raw: the audit layer escapes it on
/// insert (#816), and it is the one fact that says who sent it.
pub async fn admit<'m>(
    events: &dyn ChannelEvents,
    msg: &'m IncomingMessage,
) -> Option<Cow<'m, IncomingMessage>> {
    match screen(msg) {
        NulScreen::Clean => Some(Cow::Borrowed(msg)),
        NulScreen::BodyEscaped { msg, .. } => Some(Cow::Owned(msg)),
        NulScreen::Malformed { field } => {
            events
                .audit(
                    actions::REJECTED_MALFORMED,
                    serde_json::json!({
                        "channel": msg.channel.0,
                        "peer": msg.peer.0,
                        "field": field.as_str(),
                        "reason": REASON_NUL,
                    }),
                )
                .await;
            None
        }
    }
}

#[cfg(test)]
mod tests;
