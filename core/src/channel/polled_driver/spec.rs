//! The driver's wire shape: what a channel-shaped worker looks like to it
//! ([`PolledWorkerSpec`]), the events it yields ([`PolledEvent`]), the codec
//! fns a channel supplies, and the [`WorkerCalls`] seam the driver calls it
//! through. Split out of [`super`] to keep it under the 500-LOC soft cap
//! before #826 grew it (the items moved verbatim; their docs now point back at
//! [`super`]); re-exported there, so every path is unchanged.

use super::ReplyGiveUp;
use crate::channel::{OutgoingMessage, PeerEvidence};
use crate::worker_lifecycle::persistent::PersistentHandle;
use crate::worker_lifecycle::RestartBackoff;

/// What a channel-shaped worker looks like to the driver: three JSON-RPC
/// methods plus the worker-side long-poll wait.
#[derive(Clone, Copy, Debug)]
pub struct PolledWorkerSpec {
    /// Log label (also a good supervisor label), e.g. `"matrix"`.
    pub label: &'static str,
    /// Identity/login-proof method, called once at spawn (e.g. `matrix.init`).
    pub init_method: &'static str,
    /// Long-poll method; params are `{"timeout_ms": <poll_timeout_ms>}`.
    pub poll_method: &'static str,
    /// Outbound-delivery method; params come from the `EncodeSend` fn.
    pub send_method: &'static str,
    /// Optional cursor-advance method, called once per inbound event right
    /// after the driver hands that event to the bus (see `super::run`, step
    /// 3). `None` for a worker with no server-side polling cursor to advance —
    /// Matrix sets this to `None`, so it never gets the extra RPC.
    ///
    /// ⚠️ Since #826 this also tells the driver whether the channel
    /// **redelivers**: with `None`, a batch a closed bus never took is treated
    /// as lost — counted, said, and handed to
    /// [`InboundDroppedAudit`](super::InboundDroppedAudit). A channel whose
    /// upstream redelivers without an ack must not leave this `None` without
    /// revisiting `inbound_drop`, or it writes false `channel.inbound_dropped`
    /// rows.
    pub ack_method: Option<&'static str>,
    /// Worker-side long-poll wait. Outbound latency is bounded by this (the
    /// single JSON-RPC pipe serializes poll and send).
    pub poll_timeout_ms: u64,
    /// How long to wait before calling a method again after the worker
    /// refused it, per consecutive refusal of that method (#769) — for a send,
    /// per conversation (#782). Production specs use [`REFUSAL_BACKOFF`](super::REFUSAL_BACKOFF).
    pub refusal_backoff: RestartBackoff,
    /// When to give up on a reply the worker keeps refusing (#782).
    /// Production specs use [`REPLY_GIVE_UP`](super::REPLY_GIVE_UP).
    pub reply_give_up: ReplyGiveUp,
    /// How to ask the worker, as the driver exits, how many inbound messages
    /// it still held (#835). `Some` for a worker that receives on its own
    /// between polls and holds what it received in memory (Matrix's live
    /// sync); `None` for one that holds nothing between polls (email: the
    /// messages stay in localmail until acked).
    ///
    /// ⚠️ Only on a channel that does **not** redeliver (`ack_method`
    /// `None`): the worker stage's line says "this channel does not
    /// redeliver" and its count becomes a `channel.inbound_dropped` row, so on
    /// an ack channel both would be false. [`PolledWorkerDriver::spawn`]
    /// refuses a spec with both.
    ///
    /// [`PolledWorkerDriver::spawn`]: super::PolledWorkerDriver::spawn
    pub drain: Option<WorkerDrainSpec>,
}

/// A worker's drain method (#835): called once, with `{}`, on every exit of
/// the driver. The worker stops receiving, discards what it still held, and
/// answers with how many — decoded by `parse`.
#[derive(Clone, Copy, Debug)]
pub struct WorkerDrainSpec {
    /// The method, e.g. `matrix.drain`.
    pub method: &'static str,
    /// Decodes its result.
    pub parse: ParseDrain,
}

/// What a worker's drain answered: a count, never anything a peer wrote.
/// Deliberately no `Default`: its value would be the all-clear answer, and an
/// unknown count must never fall back to it — that is an `Err` from
/// [`ParseDrain`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerHeld {
    /// Inbound messages the worker still held, now discarded.
    pub discarded: usize,
    /// The worker's stop may have cut an upstream batch short, or could not
    /// be confirmed: an unknown number of messages, possibly none, are lost
    /// and NOT in `discarded`.
    pub interrupted: bool,
}

/// Decode a drain method's RESULT. An error means the count is unknown, and
/// the driver says so.
pub type ParseDrain = fn(serde_json::Value) -> anyhow::Result<WorkerHeld>;

/// One inbound event as the channel layer sees it, before the driver stamps
/// its [`ChannelId`](crate::channel::ChannelId) on. Produced by a [`ParsePoll`] fn from the poll result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolledEvent {
    pub peer: String,
    pub conversation: String,
    pub body: String,
    /// Transport-supplied authenticity evidence, carried straight through to
    /// the [`IncomingMessage`](crate::channel::IncomingMessage) the driver builds. `None` for transports that
    /// authenticate their own peers (Matrix — see `matrix::wire::parse_matrix_poll`).
    pub evidence: Option<PeerEvidence>,
    /// A per-message cursor token, passed to the spec's `ack_method` (via
    /// [`EncodeAck`]) once the driver has handed this event to the bus (e.g.
    /// the email fallback worker's cursor). Unused by Matrix.
    pub ack_token: Option<String>,
}

/// What a [`ParsePoll`] fn read from one poll result: the events it decoded,
/// and how many it could not (#852).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolledBatch {
    /// The events that decoded, in the worker's order.
    pub events: Vec<PolledEvent>,
    /// The events that did not, if any. Only a decoder that reads events one
    /// at a time reports these; one that reads the result whole fails the
    /// whole result instead (an `Err` from [`ParsePoll`]).
    ///
    /// ⚠️ On a channel with an ack method the driver treats a batch with any
    /// of these as wholly undecodable and acks none of it: its cursor is one
    /// monotonic high-water mark, so acking the events after a bad one would
    /// move it past the bad one for good (see [`super`]'s module docs).
    pub undecodable: Option<UndecodableEvents>,
}

impl PolledBatch {
    /// A batch whose every event decoded.
    pub fn whole(events: Vec<PolledEvent>) -> Self {
        Self { events, undecodable: None }
    }
}

/// The events of a poll result that did not decode (#852): how many, and why
/// the first did not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndecodableEvents {
    /// How many. Never 0 — a batch whose every event decoded has no
    /// [`PolledBatch::undecodable`] — and the type says so.
    pub count: std::num::NonZeroUsize,
    /// The first one's decode error. For the driver's line only, never a row:
    /// a decode error can quote a field of the message, and so a peer's words.
    pub first_error: String,
}

impl UndecodableEvents {
    /// One more event that did not decode, folded into `slot`: the first sets
    /// the count to 1 and keeps `error()`, each after it only counts. One
    /// value, so a decoder cannot keep a count with no error to say.
    pub fn note(slot: &mut Option<Self>, error: impl FnOnce() -> String) {
        match slot {
            Some(u) => u.count = u.count.saturating_add(1),
            None => *slot = Some(Self { count: std::num::NonZeroUsize::MIN, first_error: error() }),
        }
    }
}

/// Decode one poll RESULT. An `Err` means the result as a whole could not be
/// read — a worker bug (said, held, and on a channel that does not redeliver
/// recorded), NOT a worker death; events that could not be read one at a time
/// are an `Ok` with [`PolledBatch::undecodable`] set.
pub type ParsePoll = fn(serde_json::Value) -> anyhow::Result<PolledBatch>;

/// Encode one outbound message into the send method's params.
pub type EncodeSend = fn(&OutgoingMessage) -> serde_json::Value;

/// Encode one event's [`PolledEvent::ack_token`] into the ack method's params
/// (e.g. `{"cursor": tok}`). Only called when both `PolledWorkerSpec::ack_method`
/// and the event's own `ack_token` are present — see `super::run`.
pub type EncodeAck = fn(&str) -> serde_json::Value;

/// Extract `(id, reason)` pairs to acknowledge that never became a
/// [`PolledEvent`] at all — see [`super`]'s module docs, "Acking ids that
/// never become an event". Run against the raw poll [`serde_json::Value`], in
/// addition to (and before) `parse_poll` consumes it. Extraction itself is
/// unconditional, but `super::run` only actually *acks* the resulting ids when
/// the same batch decoded whole: `parse_poll` succeeded **and** reported no
/// undecodable events (`whole_batch_on_ack_channel`, #852) — see the monotonic-cursor
/// note in [`super`]'s module docs. Only ever invoked when
/// `PolledWorkerSpec::ack_method` and an `EncodeAck` are both present too.
/// `reason` is a short, static-ish diagnostic (never message content) — it is
/// only ever used for a log line and, when supplied, an
/// [`AckOnlyAudit`](super::AckOnlyAudit) call.
pub type ParseAckOnly = fn(&serde_json::Value) -> Vec<(String, String)>;

/// Seam over "something that can call the worker" so the driver is unit-tested
/// without a supervisor or a process. Production is [`PersistentHandle`].
pub trait WorkerCalls: Send + 'static {
    fn call(&self, method: &str, params: serde_json::Value)
        -> anyhow::Result<serde_json::Value>;
}

impl WorkerCalls for PersistentHandle {
    fn call(&self, method: &str, params: serde_json::Value)
        -> anyhow::Result<serde_json::Value> {
        PersistentHandle::call(self, method, params)
    }
}
