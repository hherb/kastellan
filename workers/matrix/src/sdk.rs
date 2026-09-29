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

    /// Send an E2E message to a room. A failure says whose problem it is
    /// ([`SendFailure`]), which the core's driver needs to tell a dead room
    /// from a dead homeserver (#782).
    fn send(&mut self, conversation: &str, body: &str) -> Result<(), SendError>;
}

/// Whose problem a failed send is (#782). The core's polled driver queues
/// replies per room and gives up on a room that keeps refusing — so a failure
/// of the **whole** homeserver must not read as a refusal by the room, or an
/// overnight outage would give up every room's replies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendFailure {
    /// The homeserver refused the bot's access token (HTTP 401,
    /// `M_UNKNOWN_TOKEN`): every room would refuse the same way, and only
    /// the operator can fix it.
    Credential,
    /// The homeserver did not answer for this room: no response at all, a
    /// timeout, a 5xx, a 429. Every room would fail the same way; retrying
    /// may help.
    Unavailable,
    /// This room refused, or it cannot be sent to: forbidden (the bot was
    /// removed, or lacks power), an unknown room, a malformed room id. Other
    /// rooms are unaffected. A 403 is here, **not** under
    /// [`Self::Credential`]: in Matrix it is a room's answer.
    Conversation,
}

/// What a failed send got back, as far as [`classify_send_failure`] needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendEvidence {
    /// The request never got an HTTP response (connect error, timeout).
    NoResponse,
    /// The homeserver answered with this HTTP status.
    Status(u16),
    /// The send failed before any request (a malformed room id, a room the
    /// client does not know) or after a response that was not a Matrix error.
    Local,
}

/// Pure: whose problem a failed send is.
pub fn classify_send_failure(evidence: SendEvidence) -> SendFailure {
    match evidence {
        SendEvidence::NoResponse => SendFailure::Unavailable,
        SendEvidence::Status(401) => SendFailure::Credential,
        SendEvidence::Status(429) => SendFailure::Unavailable,
        SendEvidence::Status(s) if s >= 500 => SendFailure::Unavailable,
        SendEvidence::Status(_) | SendEvidence::Local => SendFailure::Conversation,
    }
}

/// A failed send: whose problem it is, and the SDK's own description.
#[derive(Debug)]
pub struct SendError {
    pub failure: SendFailure,
    pub detail: String,
}

impl SendError {
    pub fn new(evidence: SendEvidence, detail: impl Into<String>) -> Self {
        Self { failure: classify_send_failure(evidence), detail: detail.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_failure_is_charged_to_whoever_caused_it() {
        use SendEvidence::*;
        use SendFailure::*;
        for (evidence, expected) in [
            (NoResponse, Unavailable),
            (Status(401), Credential),
            (Status(429), Unavailable),
            (Status(500), Unavailable),
            (Status(502), Unavailable),
            (Status(503), Unavailable),
            // A room's own answers: the other rooms are fine.
            (Status(403), Conversation),
            (Status(404), Conversation),
            (Status(400), Conversation),
            (Local, Conversation),
        ] {
            assert_eq!(classify_send_failure(evidence), expected, "{evidence:?}");
        }
    }
}
