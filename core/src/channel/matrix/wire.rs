//! Matrix wire codecs + the polled-driver spec.
//!
//! The matrix-specific glue the channel-generic [`PolledWorkerDriver`] needs:
//! the [`MATRIX_POLLED_SPEC`] (method names + poll timeout), the drain decoder
//! [`parse_matrix_drain`] (#835), and the two pure
//! codecs that translate between the JSON-RPC wire ([`kastellan_matrix_wire`])
//! and the driver's [`PolledBatch`] / [`PolledEvent`] / [`OutgoingMessage`]
//! value types.
//!
//! Split out of the parent `matrix.rs` (2026-07-07 prod-split, Item 9b); every
//! `matrix::…` path is byte-identical via the parent's `pub use` re-exports.
//!
//! [`PolledWorkerDriver`]: crate::channel::polled_driver::PolledWorkerDriver

use kastellan_matrix_wire::{DrainResult, Event};

use crate::channel::polled_driver::{
    PolledBatch, PolledEvent, PolledWorkerSpec, UndecodableEvents, WorkerDrainSpec, WorkerHeld,
    REFUSAL_BACKOFF, REPLY_GIVE_UP,
};
use crate::channel::OutgoingMessage;

/// How long the driver waits in one `matrix.poll` before looping to check the
/// outbound queue. Outbound latency is bounded by this; a few seconds is fine for
/// a single-user assistant.
pub const POLL_MS: u64 = 2000;

/// The matrix instantiation of the channel-generic polled driver.
pub const MATRIX_POLLED_SPEC: PolledWorkerSpec = PolledWorkerSpec {
    label: "matrix",
    init_method: "matrix.init",
    poll_method: "matrix.poll",
    send_method: "matrix.send",
    // Matrix has no server-side polling cursor to advance — no ack RPC.
    ack_method: None,
    poll_timeout_ms: POLL_MS,
    refusal_backoff: REFUSAL_BACKOFF,
    reply_give_up: REPLY_GIVE_UP,
    // The live worker syncs continuously and queues what arrives between
    // polls, behind a sync token it has already saved: whatever it still holds
    // when the driver exits is lost, so the driver asks how many (#835).
    drain: Some(WorkerDrainSpec { method: "matrix.drain", parse: parse_matrix_drain }),
};

/// Decode a `matrix.drain` result (wire [`DrainResult`]): how many inbound
/// events the worker still held, and whether its stop cut a sync response
/// short (#835).
pub fn parse_matrix_drain(v: serde_json::Value) -> anyhow::Result<WorkerHeld> {
    let d: DrainResult =
        serde_json::from_value(v).map_err(|e| anyhow::anyhow!("decode drain result: {e}"))?;
    Ok(WorkerHeld { discarded: d.discarded, interrupted: d.interrupted })
}

/// A `matrix.poll` result with its events left undecoded: the wire
/// [`kastellan_matrix_wire::PollResult`]'s envelope, so that each event is
/// decoded on its own (#852). Its one field must follow `PollResult`'s;
/// `matrix/tests.rs` decodes a serialized `PollResult` through it.
#[derive(serde::Deserialize)]
struct PollEnvelope {
    events: Vec<serde_json::Value>,
}

/// Decode a `matrix.poll` result (wire `PollResult`) into driver events, one
/// event at a time (#852). Matrix does not redeliver, so a result decoded
/// whole would lose every good message in it to one event the core cannot
/// read; instead the good ones are returned and the bad ones counted. Only a
/// result whose envelope does not decode — no `events` array — is an `Err`.
pub fn parse_matrix_poll(v: serde_json::Value) -> anyhow::Result<PolledBatch> {
    let envelope: PollEnvelope =
        serde_json::from_value(v).map_err(|e| anyhow::anyhow!("decode poll result: {e}"))?;
    let mut events = Vec::with_capacity(envelope.events.len());
    let mut undecodable = None;
    for (i, raw) in envelope.events.into_iter().enumerate() {
        match serde_json::from_value::<Event>(raw) {
            Ok(e) => events.push(PolledEvent {
                peer: e.peer,
                conversation: e.conversation,
                body: e.body,
                // Matrix authenticates its own peers (E2E + homeserver auth);
                // the bus applies no extra evidence check for this transport.
                evidence: None,
                ack_token: None,
            }),
            // The index is the core's own count, not the worker's text, so
            // the line can place the event in the result.
            Err(e) => UndecodableEvents::note(&mut undecodable, || format!("decode poll event at index {i}: {e}")),
        }
    }
    Ok(PolledBatch { events, undecodable })
}

/// Encode an outbound message as `matrix.send` params.
pub fn encode_matrix_send(msg: &OutgoingMessage) -> serde_json::Value {
    serde_json::json!({ "conversation": msg.conversation.0, "body": msg.body })
}
