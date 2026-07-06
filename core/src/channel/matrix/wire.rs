//! Matrix wire codecs + the polled-driver spec.
//!
//! The matrix-specific glue the channel-generic [`PolledWorkerDriver`] needs:
//! the [`MATRIX_POLLED_SPEC`] (method names + poll timeout) and the two pure
//! codecs that translate between the JSON-RPC wire ([`kastellan_matrix_wire`])
//! and the driver's [`PolledEvent`] / [`OutgoingMessage`] value types.
//!
//! Split out of the parent `matrix.rs` (2026-07-07 prod-split, Item 9b); every
//! `matrix::…` path is byte-identical via the parent's `pub use` re-exports.
//!
//! [`PolledWorkerDriver`]: crate::channel::polled_driver::PolledWorkerDriver

use kastellan_matrix_wire::PollResult;

use crate::channel::polled_driver::{PolledEvent, PolledWorkerSpec};
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
    poll_timeout_ms: POLL_MS,
};

/// Decode a `matrix.poll` result (wire [`PollResult`]) into driver events.
pub fn parse_matrix_poll(v: serde_json::Value) -> anyhow::Result<Vec<PolledEvent>> {
    let pr: PollResult =
        serde_json::from_value(v).map_err(|e| anyhow::anyhow!("decode poll result: {e}"))?;
    Ok(pr
        .events
        .into_iter()
        .map(|e| PolledEvent { peer: e.peer, conversation: e.conversation, body: e.body })
        .collect())
}

/// Encode an outbound message as `matrix.send` params.
pub fn encode_matrix_send(msg: &OutgoingMessage) -> serde_json::Value {
    serde_json::json!({ "conversation": msg.conversation.0, "body": msg.body })
}
