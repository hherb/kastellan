//! Is this worker still alive, and if not, *why* — the one census.
//!
//! Two questions used to be answered in two places. [`dispatch_indicates_worker_dead`]
//! decided whether to retire a worker, and `tool_host` decided separately whether to
//! say anything about it — and it said something for exactly one of the variants the
//! census calls dead ([#737](https://github.com/hherb/kastellan/issues/737)). Four
//! others were retired with their captured stderr discarded and nothing written on any
//! channel, in the daemon as well as in test binaries.
//!
//! The fix is not a second list that agrees with the first: it is
//! [`WorkerRetirementCause::from_client_error`] being **the** classifier, with
//! `dispatch_indicates_worker_dead` delegating to it. A variant cannot be dead-but-
//! unreportable, because there is only one match to add it to.

use kastellan_protocol::client::ClientError;

use crate::tool_host::ToolHostError;

/// Pure: *why* a tool worker must be retired after a failed call.
///
/// One variant per `ClientError` that means the worker cannot serve another call.
/// The distinction is not decorative — it is where a reader looks next, and the
/// five point at five different places (see [`Self::from_client_error`]).
///
/// Deliberately `Copy` and field-free: it is a classification, and everything
/// situational (which worker, which method, what it said) is supplied by the
/// reporter rather than carried here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRetirementCause {
    /// `ClientError::EarlyExit` — the worker's stdout closed before it answered.
    ExitedBeforeResponding,
    /// `ClientError::Io` — the stdio pipe failed mid-call (a kill, an OOM, a
    /// jail teardown).
    PipeBroke,
    /// `ClientError::Decode` — the worker wrote bytes that are not a JSON-RPC
    /// response. Classically a worker printing to stdout instead of stderr.
    Undecodable,
    /// `ClientError::IdMismatch` — the answer carried the wrong request id, so
    /// the request/response stream is out of step.
    IdMismatch,
    /// `ClientError::ResponseTooLarge` — the worker flooded the pipe past the
    /// record cap. The unread remainder leaves the read stream desynced.
    ResponseTooLarge,
}

impl WorkerRetirementCause {
    /// Pure: classify a protocol-client error, or `None` when the worker is
    /// still alive and listening.
    ///
    /// **This is the census** — the single place a `ClientError` variant is
    /// called fatal. [`dispatch_indicates_worker_dead`] delegates here rather
    /// than keeping a parallel match, so "we retired it" and "we said why"
    /// cannot disagree (#737).
    ///
    /// ⚠️ **Exhaustive on purpose, with no `_` arm.** A new variant added to
    /// `kastellan-protocol` must break this build and force a deliberate
    /// decision, rather than inherit a default. That property was already load-
    /// bearing where this match used to live; it moved with it.
    pub fn from_client_error(err: &ClientError) -> Option<Self> {
        match err {
            // The worker answered with a structured RPC error. It rejected the
            // call and is still listening — the one non-fatal variant.
            ClientError::Rpc(_) => None,
            ClientError::EarlyExit => Some(Self::ExitedBeforeResponding),
            ClientError::Io(_) => Some(Self::PipeBroke),
            ClientError::Decode(_) => Some(Self::Undecodable),
            ClientError::IdMismatch { .. } => Some(Self::IdMismatch),
            ClientError::ResponseTooLarge { .. } => Some(Self::ResponseTooLarge),
        }
    }

}

/// Pure: classify a dispatch error as "worker died" or "worker still alive".
///
/// The spec's "Cap-check semantics" §"Mid-flight termination" §2 says a worker process
/// reported dead by the OS triggers restart; v1 slice 2 detects death *passively* on
/// the next dispatch attempt.
///
/// | Variant                                          | Classification |
/// | ------------------------------------------------ | -------------- |
/// | `Ok(_)`                                          | alive          |
/// | `Err(Sandbox(_))`                                | n/a (no worker exists; pre-spawn) |
/// | `Err(Io(_))`                                     | n/a (no worker exists; pre-spawn — see below) |
/// | `Err(SecretRedemptionFailed(_))`                 | n/a (fires before the worker is called) |
/// | `Err(EgressProvisionFailed(_))`                  | n/a (fires before the worker is called) |
/// | `Err(Protocol(_))`                               | delegated to [`WorkerRetirementCause::from_client_error`] |
///
/// ⚠️ **`ToolHostError::Io` is a PRE-SPAWN bucket, and used to be classified
/// dead** ([#737](https://github.com/hherb/kastellan/issues/737)). Every one of
/// its producers in this tree fires before a worker is serving: broker sidecar
/// provisioning (`broker::spawn`, 5 sites), egress sidecar provisioning
/// (`egress::net_worker`, 3), scratch-dir creation (`tool_host::scratch`), and
/// the `Lifecycle::SingleUse` wiring-bug guard below. A worker killed mid-response
/// does **not** land here — that is `Protocol(ClientError::Io)`, which this
/// still calls dead.
///
/// So the old `true` had exactly the failure mode the `SecretRedemptionFailed`
/// arm's own comment warns about: a provisioning error on the *planner's* side
/// ticking the restart-backoff counter and degrading the warm-worker hit rate.
///
/// ⚠️ **What makes `false` safe rather than fail-open is a separate change:**
/// `ToolHostError::Io` no longer carries `#[from] std::io::Error`. While it did,
/// any future `?` on an `io::Error` in a function returning `ToolHostError`
/// joined this bucket silently, and a post-spawn one would have left a dead
/// worker in the warm cache. Construction is now explicit everywhere, so the
/// census above is complete by construction instead of by a grep that rots.
pub fn dispatch_indicates_worker_dead<T>(result: &Result<T, ToolHostError>) -> bool {
    match result {
        Ok(_) => false,
        Err(ToolHostError::Sandbox(_)) => false, // pre-spawn; no worker to be dead
        Err(ToolHostError::Io(_)) => false,      // pre-spawn provisioning; see above
        // The one census, not a copy of it.
        Err(ToolHostError::Protocol(e)) => WorkerRetirementCause::from_client_error(e).is_some(),
        // SecretRedemptionFailed fires before the worker is called —
        // the worker process was never contacted, so it is not dead.
        Err(ToolHostError::SecretRedemptionFailed(_)) => false,
        // EgressProvisionFailed (slice #3b, #268) also fires before the worker
        // is called — worker process was never contacted, not dead.
        Err(ToolHostError::EgressProvisionFailed(_)) => false,
    }
}

#[cfg(test)]
#[path = "liveness_tests.rs"]
mod tests;
