//! How the polled driver logs a worker that is **down** — dead, or being
//! respawned underneath it by the supervisor (#674, #769).
//!
//! A worker that *answered* with a refusal is not down; that has its own
//! pacing and its own lines in [`super::refusal`]. Split out of [`super`] to
//! keep it under the 500-LOC soft cap. Everything here is pure except
//! [`report_down`], which only emits a line.

use kastellan_protocol::{codes, RpcError};

/// True when a worker call failed because the worker's **upstream refused its
/// credential** ([`codes::UPSTREAM_AUTH_FAILED`]) — the one refusal whose log
/// line names an operator action. Pure.
///
/// Readable because [`crate::worker_lifecycle::persistent::PersistentHandle::call`] keeps a structured refusal's
/// [`RpcError`] type (see `client_error_to_anyhow` in
/// `worker_lifecycle::persistent`); any other error — a death, a respawn in
/// progress, a flattened transport failure, another refusal — is not one.
pub(super) fn is_upstream_auth_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RpcError>().is_some_and(|rpc| rpc.code == codes::UPSTREAM_AUTH_FAILED)
}

/// The driver's worker-down latch: says "down" once per outage and "back up"
/// once when it ends. Pure.
///
/// Until #769 this also carried the credential-refusal cadence, because every
/// refusal respawned the worker and so *was* an outage. A refusal now comes
/// from a worker that is kept, so it is not tracked here at all — which also
/// dissolves the #770 review's case (an outage that began as a restart and
/// came back as a 401 kept the restart's wording): the two can no longer
/// share a latch.
#[derive(Debug, Default)]
pub(super) struct OutageLog {
    /// The last worker call failed because the worker was down.
    down: bool,
}

impl OutageLog {
    /// Record a call that failed because the worker is down. True for the
    /// first failure of an outage, which is the one that gets a line.
    pub(super) fn on_down(&mut self) -> bool {
        !std::mem::replace(&mut self.down, true)
    }

    /// Record that the worker answered — accepted the call, or refused it.
    /// Either way it is up. True when that ends an outage, so the caller logs
    /// "back up" once.
    pub(super) fn on_answer(&mut self) -> bool {
        std::mem::take(&mut self.down)
    }
}

/// Emit the one line an outage gets ([`OutageLog::on_down`] said it is due).
///
/// The error text is worker-influenced, so it is logged through
/// [`crate::untrusted_text::neutralise_controls`], as every other
/// worker-to-log path is.
pub(super) fn report_down(label: &str, e: &anyhow::Error, what: &str) {
    let error = crate::untrusted_text::neutralise_controls(&e.to_string());
    tracing::warn!(label, error = %error, "{what}");
}
