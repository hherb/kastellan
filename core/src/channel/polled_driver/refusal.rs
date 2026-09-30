//! How the polled driver paces and reports a worker's **refusal** (#769).
//!
//! A refusal is a structured JSON-RPC error *answer* ([`RpcError`]): the worker
//! is alive and listening, and it said no. Examples: localmail rejecting the
//! email channel's credential (`UPSTREAM_AUTH_FAILED`), a Matrix homeserver
//! rejecting a send (`OPERATION_FAILED`, "send failed: …"), or a worker whose
//! own upstream request failed (email-in's `OPERATION_FAILED`, "transport: …").
//!
//! The supervisor keeps a refusing worker; why, and where each job its respawn
//! used to do went, is in `worker_lifecycle::persistent::call_failure`. One of
//! those jobs lands here: the respawn's delay was the only pacing of a retry.
//! Without it the driver would retry a refused poll every [`super::RETRY_SLICE`]
//! (200 ms) — five requests a second at an upstream that is saying no. So each
//! run of consecutive refusals ([`RefusalRun`]: poll, ack, each conversation's
//! sends, and the channel-wide send hold) backs off exponentially (the
//! channel's [`super::PolledWorkerSpec::refusal_backoff`]).
//!
//! Everything here is pure except [`refused`], [`accepted`] and
//! [`report_refusal`], which only emit the lines the pure parts chose. The
//! refusal lines go out through the marked `[worker-refusal]` emitter (#783),
//! so they reach a binary with no `tracing` subscriber too — and so does the
//! line that says a refusal ended (#788), so a reader of that stream can tell
//! a conversation that is still stuck from one that recovered.

use std::time::{Duration, Instant};

use kastellan_protocol::RpcError;

use crate::worker_lifecycle::persistent::{classify_call_error, CallFailure};
use crate::worker_lifecycle::RestartBackoff;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

use super::outage::{note_answer, OutageLog};
use super::PolledWorkerSpec;

/// How often a refusal that is still happening is logged again. One line per
/// run is not enough: a refusal can last for days (an expired credential), and
/// a single line from the start of it scrolls away. Four lines an hour stay
/// findable without becoming a flood. The log lines say "every 15 min"; a test
/// pins the two together.
pub(super) const REFUSAL_REPEAT: Duration = Duration::from_secs(15 * 60);

/// The longest refusal backoff a spec may ask for (see
/// [`check_refusal_backoff`]).
pub(super) const MAX_REFUSAL_DELAY: Duration = Duration::from_secs(60 * 60);

/// True when a worker call failed because the worker **answered with a
/// structured refusal** — it is alive — rather than because it died or is
/// being respawned. Pure; the supervisor's own classifier, so the two cannot
/// disagree.
pub(super) fn is_refusal(e: &anyhow::Error) -> bool {
    classify_call_error(e) != CallFailure::Gone
}

/// True when the refusal is the worker's **upstream refusing its credential**
/// — the one refusal whose log line names an operator action. Pure.
#[cfg(test)]
pub(super) fn is_upstream_auth_refusal(e: &anyhow::Error) -> bool {
    classify_call_error(e) == CallFailure::CredentialRefused
}

/// Pure: reject a refusal backoff that would not pace anything. Checked once,
/// by [`super::PolledWorkerDriver::spawn`]. A zero base, or a factor below 1,
/// decays to retrying on every loop — the five-a-second regime this module
/// exists to prevent — and a cap past [`MAX_REFUSAL_DELAY`] is no longer a
/// backoff but an outage.
pub(super) fn check_refusal_backoff(b: &RestartBackoff) -> anyhow::Result<()> {
    anyhow::ensure!(!b.base.is_zero(), "refusal backoff: base must be above zero");
    // `next_delay` reads a zero numerator or denominator as 1.
    anyhow::ensure!(
        b.factor_num.max(1) >= b.factor_den.max(1),
        "refusal backoff: factor must be at least 1 ({}/{})",
        b.factor_num,
        b.factor_den
    );
    anyhow::ensure!(b.cap >= b.base, "refusal backoff: cap {:?} is below base {:?}", b.cap, b.base);
    anyhow::ensure!(
        b.cap <= MAX_REFUSAL_DELAY,
        "refusal backoff: cap {:?} is above {MAX_REFUSAL_DELAY:?}",
        b.cap
    );
    Ok(())
}

/// What one more refusal means for the caller.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "carries whether this refusal is logged; pass it to report_refusal"]
pub(super) struct Refused {
    /// How long before this method is called again.
    pub(super) delay: Duration,
    /// Whether this refusal gets a log line.
    pub(super) report: bool,
}

/// One run of consecutive refusals: when the call may be made again, and when
/// the run was last reported. Pure — the clock is passed in.
///
/// The driver keeps one for poll and one for ack; sends keep one per
/// conversation plus one channel-wide hold (`super::replies`, #782). They are
/// refused independently: a Matrix homeserver can reject a send to one room
/// while sync, and so every poll, is healthy.
#[derive(Debug, Default)]
pub(super) struct RefusalRun {
    /// Refusals since this method was last accepted.
    consecutive: u32,
    /// Not before this instant. `None` once accepted, and before any refusal.
    retry_at: Option<Instant>,
    /// When this run was last logged.
    reported_at: Option<Instant>,
    /// The code of the refusal last logged.
    reported_code: Option<i32>,
    /// When this run's first refusal happened. `None` outside a run. Read by
    /// the reply give-up bound (#782, `super::replies`).
    since: Option<Instant>,
}

impl RefusalRun {
    /// May this method be called at `now`?
    pub(super) fn ready(&self, now: Instant) -> bool {
        self.retry_at.map_or(true, |at| now >= at)
    }

    /// Record a refusal with `code` at `now`: schedule the retry and say
    /// whether to log it.
    ///
    /// The first refusal waits `backoff.base`; each further one multiplies, up
    /// to `backoff.cap` ([`RestartBackoff::next_delay`], which counts from zero
    /// — hence `consecutive - 1`).
    ///
    /// Logged: the first refusal of a run, then one every [`REFUSAL_REPEAT`] —
    /// and at once whenever the code differs from the last one logged, so a run
    /// that starts as a transient refusal and turns into a credential refusal
    /// gets its operator-action line without waiting out the repeat.
    pub(super) fn on_refusal(&mut self, now: Instant, backoff: &RestartBackoff, code: i32) -> Refused {
        self.consecutive = self.consecutive.saturating_add(1);
        self.since.get_or_insert(now);
        let delay = backoff.next_delay(self.consecutive - 1);
        // `check_refusal_backoff` bounds `delay`, so the add cannot overflow in
        // practice; if it ever did, the longest allowed wait is the safe side.
        self.retry_at = now.checked_add(delay).or_else(|| now.checked_add(MAX_REFUSAL_DELAY));
        let report = self.reported_code != Some(code)
            || self.reported_at.map_or(true, |at| now.saturating_duration_since(at) >= REFUSAL_REPEAT);
        if report {
            self.reported_at = Some(now);
            self.reported_code = Some(code);
        }
        Refused { delay, report }
    }

    /// When this run's first refusal happened; `None` when there is no run.
    pub(super) fn since(&self) -> Option<Instant> {
        self.since
    }

    /// Forget when the run began, keeping its backoff and log cadence: the
    /// next refusal starts the clock again. For a channel-wide failure, which
    /// says nothing about this run's target (#782, `super::replies`).
    pub(super) fn restart_clock(&mut self) {
        self.since = None;
    }

    /// Log the next refusal whatever the cadence says. For a refusal that ends
    /// an outage: the "back up" line just said the driver resumed, and a
    /// silent refusal would leave that the last word.
    pub(super) fn rearm_report(&mut self) {
        self.reported_at = None;
    }

    /// Record that the method was accepted: the run ends, and every field
    /// resets. Returns how many refusals that ended (0 when there was no run),
    /// so the caller can say "accepted again" once.
    #[must_use]
    pub(super) fn on_accepted(&mut self) -> u32 {
        std::mem::take(self).consecutive
    }
}

/// Which line a refusal gets. Pure.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RefusalLine {
    /// Not due ([`Refused::report`] is false).
    Silent,
    /// A refusal from a live worker, retried with backoff.
    Warn,
    /// The upstream refused the channel's credential: an operator action (#674).
    CredentialError,
}

/// Pure: the line [`report_refusal`] emits for this refusal.
pub(super) fn refusal_line(e: &anyhow::Error, r: &Refused) -> RefusalLine {
    if !r.report {
        return RefusalLine::Silent;
    }
    match classify_call_error(e) {
        CallFailure::CredentialRefused => RefusalLine::CredentialError,
        CallFailure::Refused | CallFailure::Unavailable | CallFailure::Gone => RefusalLine::Warn,
    }
}

/// The code of a refusal, for [`RefusalRun::on_refusal`]. `0` when the error
/// is not a typed `RpcError` (the classifier would not call it a refusal).
pub(super) fn refusal_code(e: &anyhow::Error) -> i32 {
    e.downcast_ref::<RpcError>().map_or(0, |rpc| rpc.code)
}

/// A call of `method` was refused: the worker is up (log "back up" if that
/// ends an outage), the method waits out its backoff, and the refusal is
/// logged if due. `conversation` names the conversation whose reply was
/// refused, for a send (#782); `None` for poll, ack, and a send refused for
/// the whole channel.
pub(super) fn refused(
    run: &mut RefusalRun,
    outage: &mut OutageLog,
    spec: &PolledWorkerSpec,
    method: &str,
    conversation: Option<&str>,
    e: &anyhow::Error,
) {
    if note_answer(outage, spec.label) {
        run.rearm_report();
    }
    let r = run.on_refusal(Instant::now(), &spec.refusal_backoff, refusal_code(e));
    report_refusal(spec.label, method, conversation, e, &r);
}

/// A call of `method` was accepted: the worker is up, and a refusal run of
/// that method (for `conversation`, when it is a send) ends — said once, at
/// INFO, on the same marked `[worker-refusal]` emitter as the refusals it
/// closes (#788).
pub(super) fn accepted(
    run: &mut RefusalRun,
    outage: &mut OutageLog,
    label: &str,
    method: &str,
    conversation: Option<&str>,
) {
    note_answer(outage, label);
    if let Some(report) = format_accepted_report(method, conversation, run.on_accepted()) {
        emit_worker_refusal_report(label, &report, RefusalSeverity::Recovered);
    }
}

/// Pure: the line that closes a refusal run of `refusals` refusals, before
/// [`emit_worker_refusal_report`] folds the channel label in. `None` when no
/// run ended (`refusals == 0`): an ordinary accepted call says nothing.
///
/// Names the method and the conversation the same way the refusal lines do,
/// so a reader can pair the two.
pub(super) fn format_accepted_report(
    method: &str,
    conversation: Option<&str>,
    refusals: u32,
) -> Option<String> {
    if refusals == 0 {
        return None;
    }
    let what = match conversation {
        Some(c) => format!("{method} for conversation {c}"),
        None => method.to_string(),
    };
    let plural = if refusals == 1 { "" } else { "s" };
    Some(format!(
        "the worker accepted {what} again after {refusals} refusal{plural}; that refusal has ended"
    ))
}

/// Pure: the text of a refusal line, before [`emit_worker_refusal_report`]
/// folds the channel label in. `None` for [`RefusalLine::Silent`].
///
/// Everything a reader needs is in the text, not in `tracing` fields, because
/// the stderr fallback (#783) carries no fields.
pub(super) fn format_refusal_report(
    line: &RefusalLine,
    method: &str,
    conversation: Option<&str>,
    error: &str,
    retry_in: std::time::Duration,
) -> Option<String> {
    let retry_in_ms = retry_in.as_millis();
    let what = match conversation {
        Some(c) => format!("{method} for conversation {c}"),
        None => method.to_string(),
    };
    match line {
        RefusalLine::Silent => None,
        RefusalLine::CredentialError => Some(format!(
            "operator action needed: the channel's upstream refused its credential \
             (invalid, expired, revoked, or missing a grant); nothing arrives until it is \
             renewed. The worker is restarted at each retry, so it reads a renewed credential \
             by the next one. {what} was refused: {error}; retrying in {retry_in_ms} ms. \
             Repeated every 15 min while it lasts"
        )),
        RefusalLine::Warn => Some(format!(
            "the worker refused {what} (it is alive and was kept): {error}; retrying in \
             {retry_in_ms} ms with backoff. Repeated every 15 min while it lasts"
        )),
    }
}

/// Pure: the text and severity of the line [`refusal_line`] chose, or `None`
/// when it chose none.
pub(super) fn refusal_report(
    method: &str,
    conversation: Option<&str>,
    e: &anyhow::Error,
    r: &Refused,
) -> Option<(String, RefusalSeverity)> {
    let line = refusal_line(e, r);
    let report = format_refusal_report(&line, method, conversation, &e.to_string(), r.delay)?;
    let severity = match line {
        RefusalLine::CredentialError => RefusalSeverity::Error,
        RefusalLine::Warn | RefusalLine::Silent => RefusalSeverity::Warn,
    };
    Some((report, severity))
}

/// Emit the line [`refusal_report`] chose, if any, through the marked
/// `[worker-refusal]` emitter (#783), which neutralises the worker-written
/// error text and the conversation id.
pub(super) fn report_refusal(
    label: &str,
    method: &str,
    conversation: Option<&str>,
    e: &anyhow::Error,
    r: &Refused,
) {
    if let Some((report, severity)) = refusal_report(method, conversation, e, r) {
        emit_worker_refusal_report(label, &report, severity);
    }
}
