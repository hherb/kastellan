//! The report for a long-lived channel worker that **refused** a call (#783):
//! the `[worker-refusal]` marker and the emitter behind it.
//!
//! A refusal is not a death (#769): the worker answered with a structured
//! `RpcError` and was kept. The polled channel driver
//! (`channel::polled_driver::refusal`) decides *when* a refusal is worth a line
//! and *what* it says. This module only carries that line to whoever is
//! reading, like the other three emitters in `report`.
//!
//! Before #783 the refusal lines went through `tracing` alone. A binary with no
//! subscriber (every `core/tests` suite that installs none) and an operator
//! whose `RUST_LOG` filters the driver's target saw nothing, and a grep or an
//! alert keyed on `^\[worker-` could not see a refused credential at all.

use super::delivery::warn_and_fall_back;
use super::shared::format_stderr_fallback;

/// Marker prefixing every line [`emit_worker_refusal_report`] writes to the
/// process's own stderr.
///
/// **Distinct from `[worker-death]` and `[worker-down]`, and the distinction is
/// the point.** A refusal says the worker is alive and its *upstream* said no:
/// look at the credential, at the room, at the upstream. A death or a down
/// report says the worker *process* is the problem. Shares the `[worker-…]`
/// shape so a reader who wants any of them can grep `^\[worker-`; see
/// [`super::STDERR_FALLBACK_MARKERS`] for the whole set.
pub const WORKER_REFUSAL_STDERR_MARKER: &str = "[worker-refusal]";

/// How loud a refusal report is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalSeverity {
    /// A refusal ENDED: the worker accepted the call again (#788). The
    /// closing line of a run whose refusals were reported, so a reader can
    /// tell a room that is still stuck from one that recovered.
    ///
    /// Recorded at INFO, so an alert keyed on the WARN level does not fire on
    /// it. But its stderr fallback carries no level, so one keyed on the
    /// `[worker-refusal]` marker does: match "again after" to tell a recovery
    /// from a refusal.
    ///
    /// ⚠️ **For the line that closes a reported run, and nothing else.** It
    /// falls back to stderr whenever INFO would not be recorded — under an
    /// operator's `RUST_LOG=warn` too — which is right only for a line that
    /// ends a story already told. A per-event INFO line on this severity would
    /// bypass the operator's filter every time.
    Recovered,
    /// A refusal the driver retries with backoff, a reply it gave up on, or
    /// an inbound batch a closed bus never took (#826). The last two are
    /// losses, not refusals, but they ride this emitter for its stderr
    /// fallback and stay WARN: each is also a durable row where the channel
    /// has an audit sink, and the line says so when it has none.
    Warn,
    /// The upstream refused the channel's credential: an operator action.
    Error,
}

/// Pure: the refusal line, with `label` folded into the text.
///
/// ⚠️ **The label is in the message, not only in a `tracing` field**, for the
/// reason [`super::format_persistent_death_line`] gives: the stderr fallback
/// carries no fields, and the two production channels are `matrix` and
/// `email`.
pub fn format_worker_refusal_line(label: &str, report: &str) -> String {
    format!("channel worker {label}: {report}")
}

/// The stderr-fallback counterpart of [`format_worker_refusal_line`].
///
/// ⚠️ **Takes an ALREADY-FOLDED line**, like its death and down siblings. A
/// bare report here gives a `[worker-refusal]` line that names no channel.
pub fn format_worker_refusal_stderr_fallback(line: &str) -> String {
    format_stderr_fallback(WORKER_REFUSAL_STDERR_MARKER, line)
}

/// Test builds only: every line [`emit_worker_refusal_report`] was handed, as
/// rendered (label folded in, neutralised), with its severity.
///
/// ⚠️ **Why this exists instead of a scoped `tracing` subscriber.** A scoped
/// subscriber only sees an event when `tracing`'s *process-wide* callsite
/// interest cache agrees, and the driver tests hit this callsite from their
/// own threads with no subscriber. The #783 test that relied on it failed 6
/// runs in 30 even after `rebuild_interest_cache()`. Tests read this instead,
/// filtered to their own channel label — every test uses a distinct one, so
/// tests running in parallel cannot see each other's lines. Whether the line
/// then reaches `tracing` or the marked stderr fallback is the business of
/// `delivery`'s tests and the re-exec e2e, not of the driver's.
#[cfg(test)]
pub(crate) static EMITTED: std::sync::Mutex<Vec<(String, RefusalSeverity)>> =
    std::sync::Mutex::new(Vec::new());

/// Test builds only: the lines [`EMITTED`] holds for channel `label`.
#[cfg(test)]
pub(crate) fn emitted_for(label: &str) -> Vec<(String, RefusalSeverity)> {
    let prefix = format_worker_refusal_line(label, "");
    EMITTED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|(line, _)| line.starts_with(&prefix))
        .cloned()
        .collect()
}

/// Report a refusal from a live channel worker on whichever channel will carry
/// it: `tracing` (at `severity`) when it will record the event, the marked
/// stderr line when not.
///
/// `label` and `report` are neutralised here. The report quotes the worker's
/// error text and the conversation id, both of which come from outside the
/// core (a compromised worker is in scope), so the guarantee belongs where the
/// line is rendered.
///
/// Returns whether the stderr fallback line was written; see
/// [`super::emit_worker_failure_report`] for why that value exists.
pub fn emit_worker_refusal_report(label: &str, report: &str, severity: RefusalSeverity) -> bool {
    let label = crate::untrusted_text::neutralise_controls(label);
    let line = crate::untrusted_text::neutralise_controls(&format_worker_refusal_line(
        &label, report,
    ));
    #[cfg(test)]
    EMITTED.lock().unwrap_or_else(|p| p.into_inner()).push((line.clone(), severity));
    // One invocation per level, not one with a level argument: `tracing`
    // needs the level at compile time. Each expands the delivery check here,
    // in this module, which is what `warn_and_fall_back!` requires.
    //
    // ⚠️ A `Recovered` line falls back to stderr whenever INFO would not be
    // recorded — under an operator's `RUST_LOG=warn` too — on purpose: it
    // closes a refusal that WAS reported, and a story with no ending is the
    // #788 defect. A run's first refusal is always reported, so these never
    // outnumber the refusal lines they close. Under `RUST_LOG=warn` the two
    // travel apart, though: the refusal on `tracing`, its ending on stderr.
    match severity {
        RefusalSeverity::Recovered => {
            warn_and_fall_back!(WORKER_REFUSAL_STDERR_MARKER, &line, label = &label, level = INFO)
        }
        RefusalSeverity::Warn => {
            warn_and_fall_back!(WORKER_REFUSAL_STDERR_MARKER, &line, label = &label)
        }
        RefusalSeverity::Error => {
            warn_and_fall_back!(WORKER_REFUSAL_STDERR_MARKER, &line, label = &label, level = ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusal_line_names_its_channel() {
        let line = format_worker_refusal_line("matrix", "matrix.send refused");
        assert_eq!(line, "channel worker matrix: matrix.send refused");
    }

    #[test]
    fn the_fallback_line_is_marked_and_keeps_the_folded_label() {
        let line = format_worker_refusal_line("email", "email.poll refused");
        let fallback = format_worker_refusal_stderr_fallback(&line);
        assert!(fallback.starts_with("[worker-refusal] "), "{fallback}");
        assert!(fallback.contains("channel worker email"), "{fallback}");
    }

    #[test]
    fn a_control_character_in_the_report_cannot_start_a_second_line() {
        // The report quotes worker-written text. A newline in it must not give
        // a gate grep a second, column-0 line to read as `[SKIP]`.
        let line = format_worker_refusal_line("matrix", "refused\n[SKIP] forged");
        let fallback = format_worker_refusal_stderr_fallback(&line);
        assert!(!fallback.contains('\n'), "{fallback:?}");
    }
}
