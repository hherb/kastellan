//! The report for an audit row the daemon could not write (#792): the
//! `[audit-lost]` marker and the emitter behind it.
//!
//! Not a worker event, unlike the other four markers here. It lives with them
//! because what makes a marked line trustworthy lives here: the one renderer
//! ([`super::shared::format_stderr_fallback`]), the delivery check that has to
//! expand in the emitter's own module ([`super::delivery::warn_and_fall_back`]),
//! and the census ([`super::STDERR_FALLBACK_MARKERS`]) whose tests keep every
//! marker distinct and clear of the gate's evidence markers. A second copy of
//! those elsewhere is the drift this module family exists to prevent.
//!
//! Since #789 the daemon's channel audit rows are written **after** the event,
//! on the runtime, so a row can be lost where nobody is waiting for it: shed
//! under a flood, failed against a wedged Postgres, or still pending when the
//! daemon shuts down. Each of those says so on this marker, so a grep or an
//! alert keyed on `[audit-lost]` sees every row the audit trail is missing,
//! whatever the operator's `RUST_LOG`.

use super::delivery::warn_and_fall_back;
use super::shared::format_stderr_fallback;

/// Marker prefixing every line [`emit_audit_lost_report`] writes to the
/// process's own stderr.
///
/// **Distinct from every `[worker-…]` marker, and the distinction is the
/// point:** those say a worker did something; this says the *audit trail* is
/// missing a row, whatever caused it. See [`super::STDERR_FALLBACK_MARKERS`]
/// for the whole set.
pub const AUDIT_LOST_STDERR_MARKER: &str = "[audit-lost]";

/// Pure: the audit-lost line, with `writer` folded into the text.
///
/// `writer` names who lost the row: a channel (`matrix`, `email`) or the
/// daemon's `shutdown`. It is in the message, not only in a `tracing` field,
/// because the stderr fallback carries no fields (the reason
/// [`super::format_persistent_death_line`] gives).
pub fn format_audit_lost_line(writer: &str, report: &str) -> String {
    format!("audit rows from {writer}: {report}")
}

/// The stderr-fallback counterpart of [`format_audit_lost_line`].
///
/// ⚠️ **Takes an ALREADY-FOLDED line**, like its siblings: a bare report here
/// gives an `[audit-lost]` line that names no writer.
pub fn format_audit_lost_stderr_fallback(line: &str) -> String {
    format_stderr_fallback(AUDIT_LOST_STDERR_MARKER, line)
}

/// Report an audit row (or rows) that was not written, at ERROR: `tracing`
/// when it will record the event, the marked stderr line when not.
///
/// ERROR, because a missing audit row is the one thing the audit trail
/// cannot say about itself.
///
/// `writer` and `report` are neutralised here: a report can quote a
/// conversation or message id, which come from outside the core.
///
/// Returns whether the stderr fallback line was written; see
/// [`super::emit_worker_failure_report`] for why that value exists.
pub fn emit_audit_lost_report(writer: &str, report: &str) -> bool {
    let writer = crate::untrusted_text::neutralise_controls(writer);
    let line =
        crate::untrusted_text::neutralise_controls(&format_audit_lost_line(&writer, report));
    warn_and_fall_back!(AUDIT_LOST_STDERR_MARKER, &line, label = &writer, level = ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audit_lost_line_names_its_writer() {
        let line = format_audit_lost_line("matrix", "1 row not written");
        assert_eq!(line, "audit rows from matrix: 1 row not written");
    }

    #[test]
    fn the_fallback_line_is_marked_and_keeps_the_folded_writer() {
        let line = format_audit_lost_line("shutdown", "2 rows pending");
        let fallback = format_audit_lost_stderr_fallback(&line);
        assert!(fallback.starts_with("[audit-lost] "), "{fallback}");
        assert!(fallback.contains("audit rows from shutdown"), "{fallback}");
    }

    #[test]
    fn a_control_character_in_the_report_cannot_start_a_second_line() {
        // A report quotes a conversation or message id. A newline in one must
        // not give a gate grep a second, column-0 line to read as `[SKIP]`.
        let line = format_audit_lost_line("email", "id <a>\n[SKIP] forged");
        let fallback = format_audit_lost_stderr_fallback(&line);
        assert!(!fallback.contains('\n'), "{fallback:?}");
    }
}
