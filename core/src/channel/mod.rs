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

pub mod auth;
pub mod bus;
pub mod ingest;
pub mod matrix;
pub mod pairing;
pub mod route;

pub use bus::ChannelBus;

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
    /// A message from an unrecognised/unpaired peer was dropped (fail-closed).
    pub const REJECTED_UNPAIRED: &str = "channel.rejected_unpaired";
    /// An unpaired peer presented a valid pairing code and was bound (slice #3).
    pub const PAIRED: &str = "channel.paired";
    /// A recognised peer's message was blocked by the injection guard.
    pub const INJECTION_BLOCKED: &str = "channel.injection_blocked";
    /// A reply was delivered back to a peer.
    pub const REPLIED: &str = "channel.replied";
}
