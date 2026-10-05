//! The channel bus's inbound NUL boundary (#818).
//!
//! ## Why this exists
//!
//! Postgres cannot store U+0000 in `text` or `jsonb`, and an
//! [`IncomingMessage`] is written to it: the channel and peer ids into the
//! task payload and the pairing tables, the conversation id into the task
//! payload, the body as the task's `instruction`. A peer controls all of
//! them but the channel. Before #818 a NUL in any of them failed a write
//! somewhere downstream, each in its own misleading way:
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
//!   rewritten beats dropping it. How many NULs were rewritten rides on the
//!   message's own audit row ([`NUL_ESCAPED_BODY_KEY`], on
//!   `channel.received` and `channel.injection_blocked`): a peer can also
//!   type a literal `␀`, so the glyph alone cannot say which ones were NULs
//!   — the same reason #816 gave the audit log its `_nul_escaped` count.
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
//!   the token in it — the containment arm's accepted open risk (spec Open
//!   risk 3: no whitespace-separated verb, so the gate never fires), not a
//!   new class. A peer could already send the same text with a literal `␀`.
//!
//! ⚠️ Running before authorization means an **unpaired** peer can produce a
//! `rejected_malformed` row, exactly as it can produce a `rejected_unpaired`
//! one: one row per message, no more. The body escape writes no row of its
//! own for the same reason; its count joins the row the message gets anyway.

use std::borrow::Cow;

use kastellan_db::nul::escape_str;

use super::bus::ChannelEvents;
use super::{actions, IncomingMessage};

/// The fixed `reason` label on a [`actions::REJECTED_MALFORMED`] row for a
/// NUL. A label rather than prose so the row is safe to persist and to
/// group by; a later malformation gets its own.
pub const REASON_NUL: &str = "nul";

/// The key on a message's `channel.received` / `channel.injection_blocked`
/// row holding how many NULs its body had rewritten to `␀`. Present only
/// when at least one was, like the audit log's `_nul_escaped`.
pub const NUL_ESCAPED_BODY_KEY: &str = "nul_escaped_body";

/// Which identity field of an [`IncomingMessage`] was malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MalformedField {
    Channel,
    Peer,
    Conversation,
}

impl MalformedField {
    /// The audit label: a fixed `[a-z_]` string, never message text.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MalformedField::Channel => "channel",
            MalformedField::Peer => "peer",
            MalformedField::Conversation => "conversation",
        }
    }
}

/// What [`screen`] decided.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NulScreen {
    /// No NUL anywhere; use the message as it is.
    Clean,
    /// The body held `nuls` NULs, now `␀`; every other field is unchanged.
    BodyEscaped { msg: IncomingMessage, nuls: u64 },
    /// An identity field holds a NUL; the message must be refused.
    Malformed { field: MalformedField },
}

/// Decide what a NUL in `msg` means. Pure; see the module doc for the rule.
pub(crate) fn screen(msg: &IncomingMessage) -> NulScreen {
    // Every field is named, so a field added to `IncomingMessage` does not
    // compile here until someone decides whether it is an identity or a
    // record. `evidence` is neither: it is never stored (the token is hashed
    // in Rust, DMARC is a bool).
    let IncomingMessage { channel, peer, conversation, body, evidence: _ } = msg;
    let ids = [
        (MalformedField::Channel, &channel.0),
        (MalformedField::Peer, &peer.0),
        (MalformedField::Conversation, &conversation.0),
    ];
    if let Some((field, _)) = ids.iter().find(|(_, id)| id.contains('\0')) {
        return NulScreen::Malformed { field: *field };
    }
    let (escaped, nuls) = escape_str(body);
    if nuls == 0 {
        return NulScreen::Clean;
    }
    NulScreen::BodyEscaped {
        msg: IncomingMessage { body: escaped.into_owned(), ..msg.clone() },
        nuls,
    }
}

/// A message [`admit`] let through: the one to carry on with, and how many
/// NULs its body had escaped.
pub(crate) struct Admitted<'m> {
    pub(crate) msg: Cow<'m, IncomingMessage>,
    pub(crate) body_nuls: u64,
}

impl Admitted<'_> {
    /// `payload` with [`NUL_ESCAPED_BODY_KEY`] set when the body had a NUL
    /// escaped; unchanged otherwise. For the rows that describe the body —
    /// `channel.received` (it is stored) and `channel.injection_blocked`
    /// (it is hashed).
    pub(crate) fn mark(&self, mut payload: serde_json::Value) -> serde_json::Value {
        if self.body_nuls > 0 {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert(NUL_ESCAPED_BODY_KEY.into(), self.body_nuls.into());
            }
        }
        payload
    }
}

/// Apply [`screen`] at the head of the bus: the message to carry on with,
/// or `None` after writing the refusal's audit row.
///
/// The row carries the channel, the peer, the field and [`REASON_NUL`] —
/// never the body. The channel and peer are passed raw: the audit layer
/// escapes them on insert (#816), and they are what says who sent it.
pub(crate) async fn admit<'m>(
    events: &dyn ChannelEvents,
    msg: &'m IncomingMessage,
) -> Option<Admitted<'m>> {
    match screen(msg) {
        NulScreen::Clean => Some(Admitted { msg: Cow::Borrowed(msg), body_nuls: 0 }),
        NulScreen::BodyEscaped { msg, nuls } => {
            Some(Admitted { msg: Cow::Owned(msg), body_nuls: nuls })
        }
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
