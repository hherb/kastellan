//! What a failed call says about the worker that answered it (#769) — **the
//! one classifier.** The supervisor reads it to decide whether to keep,
//! replace or respawn the worker; the polled channel driver reads it to decide
//! how to pace and report the failure. One copy, so the two cannot disagree
//! (the #737 lesson: two lists that must agree will drift).
//!
//! ## Why a refusal is not a death
//!
//! Until #769 every call error respawned the worker, a structured refusal from
//! a live worker included. For the email channel with an expired credential
//! that meant, on every poll, a `[worker-death]` report for a worker that had
//! not died, a respawn, and within minutes the respawn-rate alarm — a crash
//! loop, as far as the log could tell.
//!
//! ## What the respawn used to do, and where each job went
//!
//! - **Pacing the retry.** Its ~1 s delay was the only thing slowing a caller
//!   that retries a refused call. The polled driver now backs off on refusals
//!   itself (`channel::polled_driver::refusal`).
//! - **Reading a renewed credential.** A worker reads its credential once, at
//!   spawn (email-in's token file), and bwrap binds that file by inode, so even
//!   a re-read would miss a rotation by rename. A credential refusal therefore
//!   still replaces the worker — quietly: [`CallFailure::CredentialRefused`].
//! - **Rebuilding a dead egress sidecar.** A worker whose sidecar died is alive
//!   but can only refuse: every upstream request fails, and it says so. The
//!   supervisor asks the transport on every refusal
//!   (`PersistentTransport::sidecar_exited`) and retires the pair if so.
//! - **Recovering a wedged Matrix session.** Not this path's job: the Matrix
//!   worker's sync task exits the process after sustained sync failures
//!   (`workers/matrix/src/sdk_live.rs`, policy in `sync_retry.rs`, #348), and
//!   that exit IS a death here. A refused *send* with healthy sync is a
//!   room-level answer (unknown room, forbidden), which a restored session
//!   cannot change. The worker tells the channel-wide answers apart (#782):
//!   no response, a 5xx, a 429 **and a 401** are [`CallFailure::Unavailable`]
//!   — the 401 deliberately not [`CallFailure::CredentialRefused`], whose
//!   replacement would run two clients on one Matrix store (see the worker's
//!   `handler::send_error`).

use kastellan_protocol::{codes, RpcError};

/// See [`classify_call_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallFailure {
    /// The worker is gone — a closed pipe, an early exit, undecodable bytes, a
    /// desynced stream, a respawn already in progress — or the failure reached
    /// the caller flattened to text, so nothing says otherwise. Retire and
    /// respawn it.
    Gone,
    /// A live worker read the call and said no. Keep it: a fresh process would
    /// restore the same session and store, and be refused again.
    Refused,
    /// A live worker's upstream refused its **credential**
    /// ([`codes::UPSTREAM_AUTH_FAILED`]). Answered like [`Self::Refused`], but
    /// the supervisor also replaces the worker — quietly, with no death report
    /// and no alarm tick — so that a renewed credential is read.
    CredentialRefused,
    /// A live worker's upstream failed for the whole service
    /// ([`codes::UPSTREAM_UNAVAILABLE`]: no response, a timeout, a 5xx, a
    /// 429, or a refused credential a restart cannot fix). Answered like
    /// [`Self::Refused`] — the worker is kept, never replaced — but it is
    /// about the **channel**, not the call's target, so the polled driver
    /// holds every conversation on it instead of charging one conversation's
    /// give-up (#782).
    Unavailable,
}

/// Pure: classify a call error as it reaches a [`super::PersistentHandle::call`]
/// caller.
///
/// A structured [`RpcError`] is the one answer a worker can only give by being
/// alive and listening. It survives to here because `client_error_to_anyhow`
/// keeps its type and every other `ClientError` is flattened, so anything
/// that is not a typed `RpcError` is [`CallFailure::Gone`]. That includes the
/// supervisor's own "is restarting" / "driver gone" errors, and a refusal the
/// supervisor flattened on purpose because it retired the worker anyway (a
/// dead sidecar). `anyhow`'s downcast sees through `.context(..)`.
///
/// Must agree with the idle-timeout lifecycle's census,
/// [`crate::worker_lifecycle::idle_timeout::WorkerRetirementCause::from_client_error`],
/// on which `ClientError`s are fatal; a test pins that over every variant.
pub(crate) fn classify_call_error(e: &anyhow::Error) -> CallFailure {
    match e.downcast_ref::<RpcError>() {
        None => CallFailure::Gone,
        Some(rpc) if rpc.code == codes::UPSTREAM_AUTH_FAILED => CallFailure::CredentialRefused,
        Some(rpc) if rpc.code == codes::UPSTREAM_UNAVAILABLE => CallFailure::Unavailable,
        Some(_) => CallFailure::Refused,
    }
}
