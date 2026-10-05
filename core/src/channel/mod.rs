//! The channel bus: the transport-agnostic boundary between an external
//! messaging channel (Matrix primary, email fallback — see
//! `docs/superpowers/specs/2026-06-12-primary-communication-channel-design.md`)
//! and the core conversation queue (the Postgres `tasks` table).
//!
//! Security model (three separable layers — see the spec + `docs/threat-model.md`
//! "Communication channel"):
//!   1. **Peer authentication** ([`auth`]) — fail-closed: an unrecognised peer's
//!      message never becomes a task (dropped + audited). Pairing (TOTP/WebAuthn)
//!      that makes a peer *recognised* is comms slice #3; this slice ships the seam.
//!   2. **Untrusted-input screening** ([`ingest`]) — every inbound body runs
//!      through `cassandra::injection_guard` exactly like worker output. A channel
//!      peer is no more trusted than a fetched web page.
//!   3. **Audit** — every received / rejected / enqueued / replied message lands
//!      in `audit_log`.
//!
//! All security decisions are **pure** (`auth`/`ingest`/`route`); [`bus`] is a thin
//! async pump over the [`Channel`] transport seam + the DB seams, so the whole
//! inbound→enqueue→complete→reply loop is testable with fakes (no network, no PG).

pub mod ask_message;
pub mod audit_text;
pub mod auth;
pub mod boot_supervisor;
pub mod bus;
mod bus_inbound;
pub mod email;
pub mod inbound_nul;
pub mod ingest;
pub mod matrix;
pub mod outbox;
pub mod pairing;
mod pg_events;
pub mod polled_driver;
pub mod pump_liveness;
pub mod respawn_alarm;
pub mod route;
mod skipped;
mod undelivered;

pub use bus::ChannelBus;
pub use skipped::{SkippedId, SKIPPED_REASON_CAP_CHARS};
pub use undelivered::{UndeliveredReason, UndeliveredReply};

use serde::{Deserialize, Serialize};

/// Stable identifier of a configured channel (e.g. `"matrix"`, `"email"`). The
/// outbound router uses it to find the `Channel` to reply through.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub String);

/// Channel-native identity of the *sender* (e.g. a Matrix `@user:server`, an
/// email `From`). Opaque to the bus; the [`auth::PeerAuthorizer`] interprets it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerId(pub String);

/// Channel-native conversation/thread the message belongs to (a Matrix room id,
/// an email thread). Carried through so the reply lands in the same place.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub String);

/// A normalized inbound message handed up by a [`Channel`] transport. The
/// transport is responsible for decrypting (E2E) and flattening to this shape;
/// the bus never sees ciphertext or protocol frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncomingMessage {
    pub channel: ChannelId,
    pub peer: PeerId,
    pub conversation: ConversationId,
    /// The plaintext user message body. Treated as fully untrusted input.
    pub body: String,
    /// Transport-supplied authenticity evidence, or `None` when the transport
    /// already authenticates its own peers (Matrix). See [`PeerEvidence`].
    pub evidence: Option<PeerEvidence>,
}

/// Transport-supplied evidence that an inbound message really came from the
/// claimed peer.
///
/// `IncomingMessage.evidence` is `None` when the transport authenticates its
/// own peers (Matrix: E2E + homeserver auth) — the bus then applies no extra
/// check, which is what keeps Matrix behaviour byte-identical. `Some` means the
/// transport cannot vouch for the sender and the bus must decide.
///
/// [`Debug`] is hand-written to **redact `presented_token`**: it is the
/// plaintext per-pairing shared secret, and this struct is reachable from
/// `IncomingMessage`/`PolledEvent`, which both derive `Debug`. Nothing
/// debug-formats them today, but the whole point of a `Debug` impl is that
/// someone eventually will — a `?msg` in a `tracing` call anywhere on the
/// inbound path would otherwise write the secret to the daemon log, which is
/// exactly the leak `gate::extract_token` works to prevent one layer up.
/// Derive-by-default is the wrong side of that trade for a secret-bearing type.
#[derive(Clone, PartialEq, Eq)]
pub struct PeerEvidence {
    /// Our own MX reported `dmarc=pass` (see `email::gate::trusted_dmarc_pass`).
    pub dmarc_pass: bool,
    /// The per-pairing token the sender presented, already stripped from the body.
    pub presented_token: Option<String>,
}

impl std::fmt::Debug for PeerEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Presence is diagnostically useful ("did they send one at all?"); the
        // value never is, and must never reach a log.
        let token = if self.presented_token.is_some() { "Some(<redacted>)" } else { "None" };
        f.debug_struct("PeerEvidence")
            .field("dmarc_pass", &self.dmarc_pass)
            .field("presented_token", &token)
            .finish()
    }
}

/// A reply the bus asks a [`Channel`] to deliver back to the originating peer +
/// conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub channel: ChannelId,
    pub peer: PeerId,
    pub conversation: ConversationId,
    pub body: String,
}

/// The transport seam. One implementation per channel (slice #2: `MatrixChannel`;
/// slice #5: `EmailChannel`). Dyn-safe (no generic methods) so the bus drives a
/// `Vec<Box<dyn Channel>>`. Network I/O + E2E live behind this; the bus is pure
/// orchestration above it.
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// This channel's stable id (matched against `OutgoingMessage.channel`).
    fn id(&self) -> ChannelId;

    /// Block for the next inbound message. `None` means the channel closed (the
    /// bus then drops this channel's inbound pump). Cancellation-safe: the bus
    /// `select!`s this against shutdown.
    async fn recv(&mut self) -> Option<IncomingMessage>;

    /// Deliver a reply. Errors are logged + audited by the bus, never panic.
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()>;
}

/// Canonical audit action strings for the channel bus. Centralised so the
/// negative-test e2e and the mirror consumers key off one source of truth.
pub mod actions {
    /// A message arrived from a recognised peer and was screened.
    pub const RECEIVED: &str = "channel.received";
    /// A recognised peer's message passed screening but could not be
    /// enqueued (#815), so the agent never saw it. Written **instead of**
    /// [`RECEIVED`], which would claim a task id there is none of. Carries
    /// the channel, the peer and the conversation only: never the body, and
    /// never the error (driver text, not a fixed label; it goes to the
    /// daemon log). No `nul_escaped_body` count either: the escaped body is
    /// never stored on this path.
    ///
    /// Not the row a *full* Postgres outage leaves: authorization reads the
    /// pairing table first, fails closed, and the message is recorded as
    /// [`REJECTED_UNPAIRED`] (#827). This one is for a lookup that succeeded
    /// and an insert that then failed. If this row's own insert fails too,
    /// the bus's writer says so (#808), naming the channel and peer — except
    /// for an insert still awaited when the bus is stopped (#813).
    pub const ENQUEUE_FAILED: &str = "channel.enqueue_failed";
    /// A message from an unrecognised/unpaired peer was dropped (fail-closed).
    pub const REJECTED_UNPAIRED: &str = "channel.rejected_unpaired";
    /// A message was refused before authorization because an identity field
    /// (channel, peer or conversation id) is malformed — today, holds a NUL,
    /// which Postgres cannot store (#818). Carries the channel, the peer, a
    /// fixed `field` label and a fixed `reason` label
    /// (`super::inbound_nul::REASON_NUL`), never the body. A NUL in the
    /// *body* is escaped instead, writes no row of its own, and is counted on
    /// the message's received / injection-blocked row
    /// (`super::inbound_nul::NUL_ESCAPED_BODY_KEY`).
    pub const REJECTED_MALFORMED: &str = "channel.rejected_malformed";
    /// An unpaired peer presented a valid pairing code and was bound (slice #3).
    pub const PAIRED: &str = "channel.paired";
    /// A recognised peer's message was blocked by the injection guard.
    pub const INJECTION_BLOCKED: &str = "channel.injection_blocked";
    /// A reply was routed to its channel for delivery.
    ///
    /// Precisely: `handle_completed` resolved a completed task to an
    /// [`super::OutgoingMessage`] and handed it to the owning channel's
    /// outbound queue. It is **not** proof the transport delivered it — the
    /// `send` happens afterwards, in the per-channel pump, and can still fail
    /// (in slice 1 `EmailChannel::send` fails *unconditionally*, since there is
    /// no outbound worker yet). A failure emits [`REPLY_UNDELIVERED`] for the
    /// same reply. So a `channel.replied` with no matching
    /// `channel.reply_undelivered` is a delivered reply — or, for a polled
    /// channel (Matrix), one still queued in its driver, which writes the row
    /// when it gives the reply up, drops it or exits with it (#782). The name is kept
    /// because these strings are a committed operator-facing interface (see
    /// `auth::UnauthenticReason::as_str`); the doc is what was wrong, and it
    /// claimed delivery.
    ///
    /// **The converse does not hold** — see [`REPLY_UNDELIVERED`].
    pub const REPLIED: &str = "channel.replied";
    /// A message for a channel was not delivered: a reply, an ask, or an
    /// inbound ack — routed to its channel's queue or, for `queue_closed`,
    /// not even that. Carries the channel, the peer, a fixed
    /// [`super::UndeliveredReason`] label and the event's time
    /// (`observed_at`, #792) only — never the reply body and never the error
    /// string (which is transport text, not a fixed label) — built by
    /// [`super::UndeliveredReply::payload`], its one definition (#800).
    ///
    /// **Two writers.** The bus: its per-channel pump, when `Channel::send`
    /// fails (`send_failed`) — for a reply, an ask, or (since #824) the ack
    /// `handle_inbound` sends back — and, since #815, `handle_completed`, when
    /// the channel's queue is closed because its pump has ended
    /// (`queue_closed`; that reply has **no** [`REPLIED`] row, since it was
    /// never routed). And, since #782, a polled channel's driver
    /// (`polled_driver::replies`), whose `send` only queues: it writes the row
    /// when it **gives up** on a reply its worker kept refusing (`gave_up`),
    /// drops one past a full conversation queue (`queue_full`), or exits with
    /// replies still queued (`driver_exit`) — when the channel supplies an
    /// audit sink, which the daemon's Matrix channel does. Until #782 a polled
    /// channel never wrote this row, so its `channel.replied` read as
    /// delivered even when the reply was stuck for ever.
    ///
    /// Usually the compensating row for a [`REPLIED`] that did not land —
    /// but **not always, and an anti-join on that pairing will report false
    /// positives.** Since #564 slice 2 the per-channel pump also drains
    /// core-initiated messages from the `ChannelOutbox`, so a *raised ask*
    /// whose transport refused it lands here too, with `ask.delivered`
    /// behind it rather than `channel.replied`. Because the payload is
    /// channel + peer only, such a row names neither the ask nor the task
    /// and is correlatable only by timestamp; carrying `ask_id` into the
    /// pump is tracked as its own issue. Two more orphan sources: an inbound
    /// ack the transport refused (it answers a message, not a task), and a
    /// `queue_closed` row, which by definition has no `channel.replied`.
    pub const REPLY_UNDELIVERED: &str = "channel.reply_undelivered";
    /// A message failed transport authenticity (DMARC and/or token) — dropped
    /// before authorization, so it never reaches the pairing carve-out.
    pub const REJECTED_UNAUTHENTIC: &str = "channel.rejected_unauthentic";
    /// A raw id was acked without ever becoming a [`super::IncomingMessage`]
    /// at all — e.g. email's `skipped` list (unattributable `From`, an
    /// unfetchable detail fetch). These are messages the agent silently
    /// never saw, so they must stay traceable even though the driver that
    /// acks them (`polled_driver::run`) is DB-free by design. See
    /// [`super::polled_driver::AckOnlyAudit`]; the payload is
    /// [`super::SkippedId::payload`]'s.
    pub const SKIPPED_ACK_ONLY: &str = "channel.skipped_ack_only";
    /// A channel bus came up. Payload carries the channel and how many
    /// bring-up attempts it took, so "did it have to retry?" is answerable
    /// after the fact — `attempts: 1` is the healthy shape (#514).
    pub const BOOT_STARTED: &str = "channel.started";
    /// A channel bring-up attempt failed. Payload carries the channel, the
    /// attempt number, the delay before the next attempt (absent when the
    /// failure is fatal and there will be none), and the capped cause.
    ///
    /// This is the durable record of a deaf window. Before #514 the only
    /// trace was one `channel not started` line in a daemon log that rotates,
    /// and the failure was not noticed until someone messaged the bot and got
    /// silence — 12 hours later, in the case that prompted this.
    pub const BOOT_FAILED: &str = "channel.boot_failed";
    /// A channel that had come up stopped working on its own and is being
    /// restarted (#517). Payload carries the channel, how long it ran
    /// (`ran_ms`) and the delay before the restart attempt.
    ///
    /// Separate from [`BOOT_FAILED`] on purpose: that row means the channel
    /// never came up, this one means it did and then went deaf — a pump ended,
    /// which until #517 was permanent and produced no row at all. `ran_ms` is
    /// what tells a sustained outage apart from a flapping channel.
    pub const CHANNEL_DIED: &str = "channel.died";
    /// A paired peer's attempt to answer an ask did not stand (#564 slice
    /// 2). Carries the channel + peer only — never the token, never the
    /// body.
    ///
    /// **Four distinct producers write this one action** — one per arm of
    /// `handle_inbound`'s split (#582, spec D4), each naming itself in the
    /// `reason` field:
    /// 1. a well-formed `/approve`/`/deny` whose token resolved nothing, or
    ///    whose resolver returned `Err` — e.g. a DB outage. The two are
    ///    deliberately collapsed into one arm, because an error path that
    ///    looks different to the peer is the existence oracle the refusal
    ///    path refuses to be. [`ASK_REASON_UNRESOLVABLE`];
    /// 2. **containment** — a candidate in the body hashes to a live nonce
    ///    of this peer's own ask, so the body is kept out of the enqueue
    ///    path and the live token never lands in `tasks.payload`. This is
    ///    the arm that carries that security property.
    ///    [`ASK_REASON_CARRIES_LIVE_TOKEN`];
    /// 3. the containment question could not be *answered* at all — no
    ///    wiring, over the candidate caps, or a resolver `Err`. Refused,
    ///    because an unanswered question must never enqueue.
    ///    [`ASK_REASON_UNSCANNABLE`];
    /// 4. a *malformed* attempt — the body's first token is one of the two
    ///    verbs but the rest does not parse (`/deny` alone), which gets
    ///    [`super::ask_message::ACK_MALFORMED_COMMAND`]. By construction
    ///    this arm carries **no** live token: arm 2 runs first.
    ///    [`ASK_REASON_MALFORMED`].
    ///
    /// **Which arm refused IS now recorded**, in the payload's `reason`
    /// (#584) — see [`ASK_REASON_UNRESOLVABLE`] and its three siblings.
    /// Before that field existed every producer wrote an identical
    /// payload, and the row could not answer the one question anyone
    /// asked of it: diagnosing #583 needed `strings` on the deployed
    /// binary plus a second hand-run experiment in Element.
    ///
    /// **What stays collapsed, deliberately.** Within
    /// [`ASK_REASON_UNRESOLVABLE`], a wrong token, an already-answered
    /// ask, one past its deadline, "not this peer's ask", and a resolver
    /// `Err` are ONE outcome by construction
    /// (`db::asks::resolve_with_nonce`). Splitting them hands a
    /// token-guessing peer an existence oracle over ask ids, and an error
    /// path that looks different to the peer is that same oracle by
    /// another door. **Do not add a fifth value to separate them.**
    ///
    /// What the row is for is still counting: repeated rejections from a
    /// paired peer are a signal even when no single one is — and the
    /// `reason` is what keeps that signal from being diluted by ordinary
    /// syntax errors.
    pub const ASK_ANSWER_REJECTED: &str = "channel.ask_answer_rejected";

    /// Why an answer was refused: a **closed** four-value vocabulary on
    /// [`ASK_ANSWER_REJECTED`] (#584).
    ///
    /// **One action with a field, not four actions** — observation SQL
    /// grouping on `action` must keep seeing one population by default.
    ///
    /// **The field leaks nothing.** The row lands in `audit_log`, which is
    /// role-gated and operator-queried; the peer sees only the ack body,
    /// and the containment and malformed arms deliberately share one ack
    /// so the peer cannot tell them apart.
    ///
    /// This value: the body parsed as a command, and the resolver returned
    /// `Ok(None)` or `Err`. The two are collapsed on purpose — see above.
    pub const ASK_REASON_UNRESOLVABLE: &str = "unresolvable";

    /// A token in the body is a live nonce of one of this peer's own asks,
    /// so the body was kept out of the enqueue path.
    ///
    /// **The sole evidence that a live capability was about to be written
    /// into `tasks.payload` and was not**, which makes it the most
    /// operationally valuable of the four. (`unscannable` also comes from
    /// the containment arm, but records that the question could not be
    /// answered — not that a token was caught.)
    pub const ASK_REASON_CARRIES_LIVE_TOKEN: &str = "carries_live_token";

    /// The containment question could **not be answered** — the body was
    /// larger than `ask_message::CANDIDATE_BYTE_CAP`, carried more distinct
    /// candidates than `ask_message::CANDIDATE_TOKEN_CAP`, the resolver
    /// errored, or no resolver was wired — so the body was refused.
    ///
    /// Fail-closed, and one honest cause with four triggers. The peer must
    /// not learn which, so the row does not say; the daemon log carries it,
    /// and every trigger writes one (two of them did not, which left a bus
    /// that had lost its wiring indistinguishable from a large paste).
    /// Reporting any of them as
    /// [`ASK_REASON_MALFORMED`] would be a lie, since none of the four
    /// requires the body to be verb-first and no syntax judgement was made.
    pub const ASK_REASON_UNSCANNABLE: &str = "unscannable";

    /// The body's first token is one of the two verbs but it did not
    /// parse. A syntax error by a human, carrying no live token.
    pub const ASK_REASON_MALFORMED: &str = "malformed";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four `reason` values are a durable operator interface: observation
    /// SQL groups on them and they are already in `audit_log` rows on the
    /// deployed host. Every other test compares against the CONSTANT, so a
    /// value change — `ASK_REASON_MALFORMED = "unscannable"` is the plausible
    /// copy-paste when adding a fifth — keeps the whole suite green while
    /// silently collapsing the vocabulary. Pinned literally here, the way
    /// `ask_message::the_wire_verbs_are_the_stored_choices` pins the verbs.
    #[test]
    fn the_ask_rejection_reasons_are_a_closed_four_value_vocabulary() {
        use actions::*;
        assert_eq!(ASK_REASON_UNRESOLVABLE, "unresolvable");
        assert_eq!(ASK_REASON_CARRIES_LIVE_TOKEN, "carries_live_token");
        assert_eq!(ASK_REASON_UNSCANNABLE, "unscannable");
        assert_eq!(ASK_REASON_MALFORMED, "malformed");

        let all = [
            ASK_REASON_UNRESOLVABLE,
            ASK_REASON_CARRIES_LIVE_TOKEN,
            ASK_REASON_UNSCANNABLE,
            ASK_REASON_MALFORMED,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "the four reasons must stay distinct: {all:?}");

        // A reason is a payload FIELD, never an action name. Nothing in the
        // type system says so — every `actions::*` constant is `&'static str`
        // — so it is asserted instead.
        assert!(
            all.iter().all(|r| !r.contains('.')),
            "an action name is dotted (`channel.received`); a reason is not: {all:?}",
        );
    }

    /// `PeerEvidence`'s `Debug` must never render the token. Asserted here
    /// rather than trusted, because the failure mode is silent: a `?evidence`
    /// or `?msg` added to any `tracing` call on the inbound path would start
    /// writing the per-pairing secret into the daemon log, and nothing else
    /// would complain.
    #[test]
    fn peer_evidence_debug_redacts_the_presented_token() {
        let ev = PeerEvidence {
            dmarc_pass: true,
            presented_token: Some("S3CRET-TOKEN-VALUE".to_string()),
        };
        let rendered = format!("{ev:?}");
        assert!(!rendered.contains("S3CRET-TOKEN-VALUE"), "token leaked into Debug: {rendered}");
        assert!(rendered.contains("redacted"), "must say it was redacted: {rendered}");
        // The DMARC verdict is not a secret and stays legible for diagnosis.
        assert!(rendered.contains("true"), "dmarc_pass must stay visible: {rendered}");
    }

    /// An absent token must still be distinguishable from a redacted one, or
    /// the redaction destroys the only diagnostically useful bit.
    #[test]
    fn peer_evidence_debug_distinguishes_absent_from_redacted() {
        let none = format!("{:?}", PeerEvidence { dmarc_pass: false, presented_token: None });
        let some = format!(
            "{:?}",
            PeerEvidence { dmarc_pass: false, presented_token: Some("x".into()) }
        );
        assert_ne!(none, some);
        assert!(none.contains("None"), "{none}");
    }

    /// Redacting `Debug` must not have cost the derived traits the rest of the
    /// code relies on (`PolledEvent`/`IncomingMessage` compare evidence in
    /// tests; the driver clones it).
    #[test]
    fn peer_evidence_still_clones_and_compares() {
        let a = PeerEvidence { dmarc_pass: true, presented_token: Some("t".into()) };
        assert_eq!(a.clone(), a);
        assert_ne!(a, PeerEvidence { dmarc_pass: false, presented_token: Some("t".into()) });
    }
}
