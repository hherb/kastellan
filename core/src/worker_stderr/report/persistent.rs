//! What the system says about a **persistent** (long-lived) worker: the death
//! report body, the `[worker-death]` marker, and the emitter behind it.
//!
//! Split out of [`super`] by *event*, not by layer: everything here answers
//! "a long-lived worker that had been serving stopped". Its sibling
//! [`super::tool_worker`] answers "a tool worker did not answer this call",
//! and [`super::shared`] holds the one renderer both go through.

use std::process::ExitStatus;

use super::delivery::warn_and_fall_back;
use crate::worker_stderr::{CapturedTail, TailState};
use super::shared::format_stderr_fallback;
#[allow(unused_imports)] // referenced by the marker doc's intra-doc link
use super::shared::STDERR_FALLBACK_MARKERS;
#[allow(unused_imports)] // referenced by this module's doc links
use super::tool_worker::{
    emit_worker_failure_report, format_worker_failure_stderr_fallback,
    WORKER_FAILED_STDERR_MARKER,
};

/// Human-readable one-line summary of a worker's death for the daemon log: the
/// exit status (which distinguishes a clean `exit status: 1` — a deliberate
/// fail-loud exit — from a `signal: 6 (SIGABRT)` — a crash) plus the recent
/// stderr lines, joined for a single log record.
///
/// ⚠️ **Takes a [`CapturedTail`], not a slice, and its state is load-bearing**
/// ([#732](https://github.com/hherb/kastellan/issues/732),
/// [#747](https://github.com/hherb/kastellan/issues/747)). "no stderr
/// captured" is a claim about the *worker*; an empty tail whose drain timed
/// out — or whose drain *failed* — only supports a claim about *us*. Rendering
/// them identically is the contentless-line defect #730 exists to remove,
/// wearing a confident sentence.
///
/// ⚠️ **Matches `CapturedTail::state` exhaustively**, so a state added to
/// the system cannot quietly inherit whichever arm happens to be last. See
/// `TailState` for why that replaced an order-dependent guard chain.
/// (Named, not linked: both are `pub(crate)`.)
///
/// `status` is `try_wait`'s own result, kept whole: an `Err` is our `waitpid`
/// failing, which must not read as the child not having exited yet (#754).
pub fn format_death_report(
    status: std::io::Result<Option<ExitStatus>>,
    stderr_tail: &CapturedTail,
) -> String {
    let status_str = match status {
        Ok(Some(s)) => s.to_string(),
        Ok(None) => "exit status unknown (not yet reaped)".to_string(),
        Err(e) => format!("exit status unknown: our waitpid failed ({e})"),
    };
    let ms = crate::worker_stderr::TAIL_DRAIN_WAIT.as_millis();
    match stderr_tail.state() {
        // KNOWN silent: the drain reached EOF and there was nothing in it.
        TailState::KnownSilent => format!("worker exited ({status_str}); no stderr captured"),
        // Nothing arrived, but we stopped waiting. "No stderr captured" would
        // be a claim about the WORKER; this is a fact about US (#732).
        TailState::NothingCapturedYet => format!(
            "worker exited ({status_str}); the stderr drain did not reach EOF within {ms} ms, \
             so nothing was captured YET — a PARTIAL capture, not evidence that the worker \
             was silent"
        ),
        // Our read of the pipe failed at byte 0 (#747). Also a fact about us,
        // but a different one: waiting longer would not have helped.
        TailState::DrainFailedSilent(fault) => format!(
            "worker exited ({status_str}); our read of its stderr FAILED ({fault}) before \
             anything arrived, so nothing was captured — our pipe, not evidence that the \
             worker was silent{}",
            fault.suspect_clause()
        ),
        // The ring evicts oldest first, so an incomplete tail is the FIRST
        // lines, not the most recent ones. "recent stderr" would mislabel them.
        TailState::FirstWords(lines) => format!(
            "worker exited ({status_str}); stderr so far (drain incomplete after {ms} ms, so \
             these may be its FIRST lines rather than its most recent): {}",
            lines.join(" | ")
        ),
        TailState::DrainFailedWords(fault, lines) => format!(
            "worker exited ({status_str}); stderr up to the point our read of the pipe FAILED \
             ({fault}{}; so these may be its FIRST lines rather than its most recent, and there \
             may have been more we could never read): {}",
            fault.suspect_clause(),
            lines.join(" | ")
        ),
        TailState::LastWords(lines) => {
            format!("worker exited ({status_str}); recent stderr: {}", lines.join(" | "))
        }
    }
}

/// Marker prefixing every line [`emit_persistent_death_report`] writes to the
/// process's own stderr.
///
/// Deliberately **distinct from** [`WORKER_FAILED_STDERR_MARKER`], because the two
/// events point somewhere different. An early exit says a tool worker never
/// answered a *specific call* — look at that call's jail, its arguments, its
/// wall clock. A persistent-worker death says a *long-lived* worker that had
/// been serving fine stopped, and the supervisor is now respawning it — look at
/// the respawn-rate alarm and at what changed underneath it. Reusing one marker
/// for both would make a gate log unable to answer which of those happened.
///
/// Shares the `[worker-…]` shape so a reader who wants *either* can grep
/// `^\[worker-`; see [`STDERR_FALLBACK_MARKERS`] for the whole set.
pub const WORKER_DEATH_STDERR_MARKER: &str = "[worker-death]";

/// Pure: the persistent-worker death line, with `label` folded into the text.
///
/// ⚠️ **The label is in the message, not only in a `tracing` field.** The daemon's
/// subscriber is `tracing_subscriber::fmt().with_env_filter(…).json()`
/// (`core/src/main.rs`) — the filter half named explicitly because it is what
/// #734 below is about, and omitting it let two notes in this file disagree. So
/// `%label` is a queryable JSON field worth keeping — but the stderr fallback
/// carries no fields at all. A label that lived only in the field would leave the
/// fallback saying "persistent worker died" without naming *which*, and the two
/// production users are `matrix` and `email`. So it goes in both places: the
/// field for the daemon's log queries, the text so the line stands alone.
pub fn format_persistent_death_line(label: &str, report: &str) -> String {
    format!("persistent worker {label} died: {report}")
}

/// The [`format_worker_failure_stderr_fallback`] counterpart for a persistent
/// worker's death report.
///
/// ⚠️ **Takes an ALREADY-FOLDED line, not a raw `death_report()` string — the
/// two counterparts are not symmetric.** The early-exit formatter is handed a
/// self-contained report that already names its worker. This one is not: the
/// label is folded in by [`format_persistent_death_line`], and
/// [`emit_persistent_death_report`] passes *that* result here. A caller who
/// passes a bare `death_report()` string instead gets
/// `[worker-death] worker exited (…)` with **no label** — precisely the failure
/// `format_persistent_death_line`'s own doc argues must never happen, since the
/// fallback channel carries no `tracing` fields to recover it from.
pub fn format_persistent_death_stderr_fallback(report: &str) -> String {
    format_stderr_fallback(WORKER_DEATH_STDERR_MARKER, report)
}

/// Report a persistent worker's death on whichever channel will carry it:
/// `tracing` when it will record the event, the marked stderr line when not.
///
/// The one producer for that event (#730), mirroring [`emit_worker_failure_report`]
/// for the early-exit one. Before this, `worker_lifecycle::persistent`'s driver
/// called `tracing::warn!` directly — a hand-rolled copy of the `tracing`-only
/// half that inherited none of #725's work. The persistent path is how the
/// **Matrix** and **email** channel workers run, so in a test binary those died
/// in silence: the tail was captured, the report was rendered, and then it was
/// discarded because nothing was listening.
///
/// ⚠️ **Neutralisation here is defence-in-depth at the trait boundary, not a
/// live hole — say so rather than imply otherwise.** Unlike the early-exit
/// report, whose `method` is genuinely model-authored, every input reaching this
/// today is already safe: `ClientTransport::death_report` renders an
/// `ExitStatus` plus a tail that `push_trimmed` neutralised on the way into the
/// ring, and every `label` in the tree is a literal. What makes it worth doing
/// anyway is that [`PersistentTransport::death_report`] is a **public trait
/// method** — any implementor is a producer of this string, and
/// `egress::persistent_net` already delegates through it — so the guarantee
/// belongs at the point the line is rendered rather than in an audit of today's
/// implementors.
///
/// [`PersistentTransport::death_report`]: crate::worker_lifecycle::PersistentTransport::death_report
///
/// Returns whether the stderr fallback line was written; see
/// [`emit_worker_failure_report`] for why that value exists.
pub fn emit_persistent_death_report(label: &str, report: &str) -> bool {
    let label = crate::untrusted_text::neutralise_controls(label);
    let line = crate::untrusted_text::neutralise_controls(&format_persistent_death_line(
        &label, report,
    ));
    warn_and_fall_back!(WORKER_DEATH_STDERR_MARKER, &line, label = &label)
}

/// Marker prefixing every line [`emit_persistent_down_report`] writes to the
/// process's own stderr.
///
/// **Distinct from [`WORKER_DEATH_STDERR_MARKER`], and the distinction is the
/// point** ([#738](https://github.com/hherb/kastellan/issues/738)). A death says
/// a worker that had been serving stopped and the supervisor is respawning it —
/// an event that is often benign and always transient. This one says the
/// supervisor is **not getting it back**: the respawn failed, it is crash-
/// looping, or the driver thread itself is gone. "Died once and came back" and
/// "the Matrix channel has been down for an hour" are different pages, and a
/// reader who can only grep for one of them gets the wrong one.
///
/// Shares the `[worker-…]` shape so a reader who wants *any* of them can grep
/// `^\[worker-`; see [`STDERR_FALLBACK_MARKERS`] for the whole set.
pub const WORKER_DOWN_STDERR_MARKER: &str = "[worker-down]";

/// Pure: the "this worker is not coming back" line, with `label` folded in.
///
/// Mirrors [`format_persistent_death_line`], and for the same reason: the
/// stderr fallback carries no `tracing` fields, so a label kept only in a field
/// leaves the reader of a fallback line unable to tell `matrix` from `email`.
pub fn format_persistent_down_line(label: &str, reason: &str) -> String {
    format!("persistent worker {label} is down: {reason}")
}

/// The [`format_persistent_death_stderr_fallback`] counterpart for the
/// not-coming-back event.
///
/// ⚠️ **Takes an ALREADY-FOLDED line**, exactly like its death sibling — see
/// that function's warning. A caller who passes a bare reason gets a
/// `[worker-down]` line naming no worker.
pub fn format_persistent_down_stderr_fallback(report: &str) -> String {
    format_stderr_fallback(WORKER_DOWN_STDERR_MARKER, report)
}

/// Report that a persistent worker is **not coming back**, on whichever
/// channel will actually carry it.
///
/// The one producer for that event (#738). Before this, the driver's
/// respawn-failure and rate-alarm lines were bare `tracing::warn!`s — the exact
/// hand-rolled-second-copy shape #730 removed one layer down, and with the same
/// consequence: after #730 a persistent worker whose factory can *never* succeed
/// produced exactly one `[worker-death]` line in a subscriber-less binary and
/// then looped forever in silence.
///
/// ⚠️ **Not a death report, and deliberately not marked as one.** See
/// [`WORKER_DOWN_STDERR_MARKER`].
///
/// Goes through the same `warn_and_fall_back!` as both other emitters, so the
/// delivery check and the control neutralisation still exist in exactly one
/// copy. `reason` is neutralised here on its own account,
/// because the three callers include one whose input is a **panic payload**
/// (`worker_lifecycle::persistent`'s join), and a panic message is arbitrary
/// text from arbitrary code.
///
/// Returns whether the stderr fallback line was written; see
/// [`emit_worker_failure_report`] for why that value exists.
pub fn emit_persistent_down_report(label: &str, reason: &str) -> bool {
    let label = crate::untrusted_text::neutralise_controls(label);
    let line = crate::untrusted_text::neutralise_controls(&format_persistent_down_line(
        &label, reason,
    ));
    warn_and_fall_back!(WORKER_DOWN_STDERR_MARKER, &line, label = &label)
}

#[cfg(test)]
mod tests;
