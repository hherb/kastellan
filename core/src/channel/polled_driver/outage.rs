//! How the polled driver logs a failed worker call (#674).
//!
//! Split out of [`super`] to keep it under the 500-LOC soft cap. Everything
//! here is pure except [`report_call_failure`], which only emits the line
//! [`OutageLog::on_failure`] chose.

use std::time::{Duration, Instant};

use kastellan_protocol::{codes, RpcError};

/// True when a worker call failed because the worker's **upstream refused its
/// credential** ([`codes::UPSTREAM_AUTH_FAILED`]) rather than because the
/// worker died. Pure.
///
/// Readable because [`crate::worker_lifecycle::persistent::PersistentHandle::call`] keeps a structured refusal's
/// [`RpcError`] type (see `client_error_to_anyhow` in
/// `worker_lifecycle::persistent`); any other error — a death, a respawn in
/// progress, a flattened transport failure — is not one.
pub(super) fn is_upstream_auth_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RpcError>().is_some_and(|rpc| rpc.code == codes::UPSTREAM_AUTH_FAILED)
}

/// How often a credential refusal that is still happening is logged again at
/// ERROR. One line per outage was not enough (#770 review): every refusal
/// also respawns the worker (#769), and each respawn logs a `[worker-death]`
/// line, so a single ERROR was buried within minutes under lines that blame a
/// crash. Four lines an hour stay findable without becoming the flood.
pub(super) const CREDENTIAL_REFUSAL_REPEAT: Duration = Duration::from_secs(15 * 60);

/// Which line, if any, a failed worker call gets.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FailureLine {
    /// Already said for this outage.
    Silent,
    /// The first failure of an outage that is not a credential refusal.
    Warn,
    /// A credential refusal: first seen, or not reported for
    /// [`CREDENTIAL_REFUSAL_REPEAT`].
    CredentialError,
}

/// The driver's per-outage log latch. Pure — the clock is passed in.
///
/// The credential refusal is tracked **apart** from the up/down latch. With a
/// single latch (#674 as first written) the first failure of an outage chose
/// the only line it got: an outage that began as a localmail restart and came
/// back as a 401 said "worker died or restarting" for as long as it lasted —
/// the exact symptom #674 exists to end.
#[derive(Debug, Default)]
pub(super) struct OutageLog {
    /// The last worker call failed.
    down: bool,
    /// When the current outage's credential refusal was last reported.
    credential_reported_at: Option<Instant>,
}

impl OutageLog {
    /// Record a failed call, and say which line it gets.
    pub(super) fn on_failure(&mut self, credential_refused: bool, now: Instant) -> FailureLine {
        let first_of_outage = !self.down;
        self.down = true;
        if credential_refused {
            let due = match self.credential_reported_at {
                None => true,
                Some(at) => now.saturating_duration_since(at) >= CREDENTIAL_REFUSAL_REPEAT,
            };
            if due {
                self.credential_reported_at = Some(now);
                return FailureLine::CredentialError;
            }
            return FailureLine::Silent;
        }
        if first_of_outage {
            FailureLine::Warn
        } else {
            FailureLine::Silent
        }
    }

    /// Record a successful call; true when it ends an outage (so the caller
    /// logs "back up" once).
    pub(super) fn on_success(&mut self) -> bool {
        std::mem::take(self).down
    }
}

/// Emit the line [`OutageLog::on_failure`] chose for a failed call.
///
/// #674: when the email channel's localmail credential expired, this said
/// "worker died or restarting" — true of the respawn that followed, false of
/// the cause — so an operator reading it looked for a crash. A credential
/// refusal now says what it is, at ERROR, and what to do; every other failure
/// keeps the caller's own wording at WARN.
///
/// The error text is worker-influenced (an `RpcError`'s message is written by
/// the worker, and a compromised worker is in scope), so it is logged through
/// [`crate::untrusted_text::neutralise_controls`], as every other worker-to-log
/// path is.
pub(super) fn report_call_failure(label: &str, e: &anyhow::Error, otherwise: &str, line: FailureLine) {
    let error = crate::untrusted_text::neutralise_controls(&e.to_string());
    match line {
        FailureLine::Silent => {}
        FailureLine::CredentialError => tracing::error!(
            label, error = %error,
            "operator action needed: the channel's upstream refused its credential \
             (invalid, expired, revoked, or missing a grant); nothing arrives until it is \
             renewed. Repeated every 15 min while it lasts"
        ),
        FailureLine::Warn => tracing::warn!(label, error = %error, "{otherwise}"),
    }
}
