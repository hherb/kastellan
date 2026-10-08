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
}

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

/// Decode one poll RESULT into events. A decode error marks the batch as a
/// worker bug (logged + skipped), NOT a worker death.
pub type ParsePoll = fn(serde_json::Value) -> anyhow::Result<Vec<PolledEvent>>;

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
/// the same batch's `parse_poll` call also succeeded — see the monotonic-cursor
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
