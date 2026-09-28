//! How the polled driver paces and reports a worker's **refusal** (#769).
//!
//! A refusal is a structured JSON-RPC error *answer*
//! ([`kastellan_protocol::RpcError`]): the worker is alive and listening, and
//! it said no. Examples: localmail rejecting the email channel's credential
//! (`UPSTREAM_AUTH_FAILED`), or the Matrix homeserver rejecting a send
//! (`OPERATION_FAILED`, "send failed: …").
//!
//! Until #769 the supervisor respawned the worker on every refusal. That had
//! three effects, only one of them wanted:
//!
//! 1. a `[worker-death]` report and, within minutes, a respawn-rate alarm for
//!    a worker that never died — an expired credential read as a crash loop;
//! 2. a fresh process that restores the *same* session and store, so it cannot
//!    fix whatever the upstream refused;
//! 3. **a pause of about a second before the retry** — the one useful effect.
//!    Without it the driver would retry every [`super::RETRY_SLICE`] (200 ms),
//!    five requests a second against an upstream that is saying no.
//!
//! The supervisor now keeps a refusing worker (see
//! `worker_lifecycle::persistent::call_error_retires_worker`), so effect 3 is
//! done here, on purpose: each method's run of consecutive refusals backs off
//! exponentially (the channel's [`super::PolledWorkerSpec::refusal_backoff`]).
//!
//! Everything here is pure except [`report_refusal`] and
//! [`report_refusal_ended`], which only emit the lines the pure parts chose.

use std::time::{Duration, Instant};

use kastellan_protocol::RpcError;

use crate::worker_lifecycle::RestartBackoff;

use super::outage::is_upstream_auth_refusal;

/// How often a refusal that is still happening is logged again. One line per
/// run is not enough: a refusal can last for days (an expired credential), and
/// a single line from the start of it scrolls away. Four lines an hour stay
/// findable without becoming a flood. (Introduced for credential refusals by
/// the #770 review; it now covers every refusal.)
pub(super) const REFUSAL_REPEAT: Duration = Duration::from_secs(15 * 60);

/// True when a worker call failed because the worker **answered with a
/// structured refusal** — it is alive — rather than because it died or is
/// being respawned. Pure.
///
/// Readable because `PersistentHandle::call` keeps an [`RpcError`]'s type
/// (`client_error_to_anyhow` in `worker_lifecycle::persistent`); every other
/// failure reaches the driver flattened to text, so it is never a refusal.
pub(super) fn is_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RpcError>().is_some()
}

/// What one more refusal means for the caller.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Refused {
    /// How long before this method is called again.
    pub(super) delay: Duration,
    /// Whether this refusal gets a log line (the first of a run, then one
    /// every [`REFUSAL_REPEAT`]).
    pub(super) report: bool,
}

/// One method's run of consecutive refusals: when it may be called again, and
/// when the run was last reported. Pure — the clock is passed in.
///
/// The driver keeps one per method (send, poll), because the two are refused
/// independently: a Matrix homeserver can reject a send while sync, and so
/// every poll, is healthy.
#[derive(Debug, Default)]
pub(super) struct RefusalRun {
    /// Refusals since this method was last accepted.
    consecutive: u32,
    /// Not before this instant. `None` once accepted, and before any refusal.
    retry_at: Option<Instant>,
    /// When this run was last logged.
    reported_at: Option<Instant>,
}

impl RefusalRun {
    /// May this method be called at `now`?
    pub(super) fn ready(&self, now: Instant) -> bool {
        self.retry_at.map_or(true, |at| now >= at)
    }

    /// Record a refusal at `now`: schedule the retry and say whether to log.
    ///
    /// The first refusal waits `backoff.base`; each further one multiplies,
    /// up to `backoff.cap` ([`RestartBackoff::next_delay`], which counts from
    /// zero — hence `consecutive - 1`).
    pub(super) fn on_refusal(&mut self, now: Instant, backoff: &RestartBackoff) -> Refused {
        self.consecutive = self.consecutive.saturating_add(1);
        let delay = backoff.next_delay(self.consecutive - 1);
        self.retry_at = Some(now + delay);
        let report = self
            .reported_at
            .map_or(true, |at| now.saturating_duration_since(at) >= REFUSAL_REPEAT);
        if report {
            self.reported_at = Some(now);
        }
        Refused { delay, report }
    }

    /// Record that the method was accepted. Returns how many refusals that
    /// ended (0 when there was no run), so the caller can say "accepted again"
    /// once.
    pub(super) fn on_accepted(&mut self) -> u32 {
        std::mem::take(self).consecutive
    }
}

/// Emit the line [`RefusalRun::on_refusal`] chose, if any.
///
/// A credential refusal says what it is, at ERROR, and what to do (#674);
/// every other refusal is a WARN naming the method that was refused. The
/// error text is worker-written (and a compromised worker is in scope), so it
/// goes through [`crate::untrusted_text::neutralise_controls`] like every other
/// worker-to-log path.
pub(super) fn report_refusal(label: &str, method: &str, e: &anyhow::Error, r: &Refused) {
    if !r.report {
        return;
    }
    let error = crate::untrusted_text::neutralise_controls(&e.to_string());
    let retry_in_ms = r.delay.as_millis() as u64;
    if is_upstream_auth_refusal(e) {
        tracing::error!(
            label, method, error = %error, retry_in_ms,
            "operator action needed: the channel's upstream refused its credential \
             (invalid, expired, revoked, or missing a grant); nothing arrives until it is \
             renewed. Repeated every 15 min while it lasts"
        );
    } else {
        tracing::warn!(
            label, method, error = %error, retry_in_ms,
            "the worker refused the call (it is alive and was kept); retrying with backoff. \
             Repeated every 15 min while it lasts"
        );
    }
}

/// Emit the one line that ends a refusal run.
pub(super) fn report_refusal_ended(label: &str, method: &str, refusals: u32) {
    if refusals > 0 {
        tracing::info!(label, method, refusals, "the worker accepted the call again");
    }
}
