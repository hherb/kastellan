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

/// Who lost the audit row(s): a channel's sink, the channel bus, or the
/// daemon's own shutdown.
///
/// A closed set, not a `&str` (#800): the writer sits beside the report text in
/// the signature, and two adjacent strings are two that can be swapped, or one
/// of which can be "shutdown", which is not a writer's name in the way
/// `matrix` is. Each label is a fixed word, so it needs no neutralising.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditLostWriter {
    /// The Matrix channel's `channel.reply_undelivered` sink.
    Matrix,
    /// The email channel's `channel.skipped_ack_only` sink.
    Email,
    /// The channel bus's own writer (`channel::pg_events`, #808): every
    /// `channel.*` row the bus writes itself — `channel.received`, a
    /// `channel.reply_undelivered` for a failed `send`, and the rest. The
    /// line names the channel, since this writer serves all of them.
    Bus,
    /// The daemon's shutdown drain: rows still pending when the pool closes.
    Shutdown,
}

impl AuditLostWriter {
    /// The word folded into the line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matrix => "matrix",
            Self::Email => "email",
            Self::Bus => "bus",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Pure: the audit-lost line, with `writer` folded into the text.
///
/// The writer is in the message, not only in a `tracing` field, because the
/// stderr fallback carries no fields (the reason
/// [`super::format_persistent_death_line`] gives).
///
/// `report` is **neutralised here** (#800), not only in the emitter: it can
/// quote a conversation or message id, which come from outside the core, and
/// a caller that formats a line without emitting it must not get a newline
/// that starts a second, column-0 line.
pub fn format_audit_lost_line(writer: AuditLostWriter, report: &str) -> String {
    crate::untrusted_text::neutralise_controls(&format!(
        "audit rows from {}: {report}",
        writer.as_str()
    ))
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
/// Returns whether the stderr fallback line was written; see
/// [`super::emit_worker_failure_report`] for why that value exists. The
/// daemon's reporters ignore it: nothing more can be done for a row that the
/// report itself could not reach anyone about (#792).
pub fn emit_audit_lost_report(writer: AuditLostWriter, report: &str) -> bool {
    let line = format_audit_lost_line(writer, report);
    warn_and_fall_back!(AUDIT_LOST_STDERR_MARKER, &line, label = writer.as_str(), level = ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audit_lost_line_names_its_writer() {
        let line = format_audit_lost_line(AuditLostWriter::Matrix, "1 row not written");
        assert_eq!(line, "audit rows from matrix: 1 row not written");
    }

    #[test]
    fn the_writers_are_pinned_literally() {
        use AuditLostWriter::*;
        assert_eq!(
            [Matrix, Email, Bus, Shutdown].map(AuditLostWriter::as_str),
            ["matrix", "email", "bus", "shutdown"]
        );
    }

    #[test]
    fn the_fallback_line_is_marked_and_keeps_the_folded_writer() {
        let line = format_audit_lost_line(AuditLostWriter::Shutdown, "2 rows pending");
        let fallback = format_audit_lost_stderr_fallback(&line);
        assert!(fallback.starts_with("[audit-lost] "), "{fallback}");
        assert!(fallback.contains("audit rows from shutdown"), "{fallback}");
    }

    #[test]
    fn a_control_character_in_the_report_cannot_start_a_second_line() {
        // A report quotes a conversation or message id. A newline in one must
        // not give a gate grep a second, column-0 line to read as `[SKIP]`.
        let line = format_audit_lost_line(AuditLostWriter::Email, "id <a>\n[SKIP] forged");
        assert!(!line.contains('\n'), "the folded line itself must be one line: {line:?}");
        let fallback = format_audit_lost_stderr_fallback(&line);
        assert!(!fallback.contains('\n'), "{fallback:?}");
    }
}
