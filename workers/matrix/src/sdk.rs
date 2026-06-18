//! The SDK seam: the handler talks to Matrix only through this trait, so the
//! JSON-RPC dispatch + buffering is unit-tested with a fake (no homeserver). The
//! real `matrix-rust-sdk`-backed implementation will live in `sdk_live.rs`
//! behind the `live-matrix` feature (Phase D, next slice). The egress transport
//! that impl relies on is already proven — see `bridge.rs` + the `egress_spike`
//! test.

use kastellan_matrix_wire::{Event, InitResult};

/// Synchronous facade over the (internally async) matrix client. The real impl
/// holds a tokio runtime and `block_on`s the SDK calls behind these methods; the
/// sync loop runs as a background task that fills the inbound buffer `poll`
/// drains.
pub trait MatrixSdk: Send {
    /// Login + first sync already happened at construction; report identity.
    fn identity(&self) -> InitResult;

    /// Drain currently-buffered inbound events. If the buffer is empty, wait up
    /// to `timeout_ms` for the first event (then return whatever arrived, possibly
    /// empty).
    fn poll(&mut self, timeout_ms: u64) -> Vec<Event>;

    /// Send an E2E message to a room.
    fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()>;
}
