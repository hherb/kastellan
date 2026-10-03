//! Bounding text that is about to become a durable `audit_log` payload value.
//!
//! Audit payloads are capped as a whole by
//! [`kastellan_db::audit::truncate_payload`], which replaces the payload
//! with a hash, carrying through only
//! [`kastellan_db::audit::PRESERVED_KEYS`] — the right backstop, and the
//! wrong outcome for a row whose other fields (which channel, which
//! attempt) are the useful part. Those fields are **not** preserved keys
//! and would not qualify as any: they are per-row context, not the bounded
//! verdict of a control. So bounding the one unbounded field first is still
//! what keeps the row readable; the allowlist is not a substitute for it.
//!
//! Defence in depth, not belt-and-braces: the values that reach here originate
//! outside the core (an upstream HTTP error body, a transport error string),
//! and a sink must not trust the producer to keep bounding them.

/// Marker appended to a value this module shortened, so a reader can tell
/// truncation from a genuinely terse message.
pub const TRUNCATION_MARKER: &str = "...(truncated)";

/// Return `text` unchanged when it is at most `cap` **chars**, else its first
/// `cap` chars followed by [`TRUNCATION_MARKER`].
///
/// Counts and cuts by `char`, never by byte, so a multi-byte codepoint
/// straddling the cap can neither panic nor produce invalid UTF-8.
///
/// Pure: no I/O, no global state. Same input → same output, every call.
pub fn cap_chars(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let mut capped: String = text.chars().take(cap).collect();
    capped.push_str(TRUNCATION_MARKER);
    capped
}

/// How long an id quoted by [`quoted_id`] may be, before quoting: escaping
/// can lengthen it (a control character becomes `\u{1b}`). Worker-supplied
/// ids are uncapped (email's `skipped` list), and a report line can be
/// written on a channel driver's thread (#798).
pub const QUOTED_ID_CAP_CHARS: usize = 128;

/// Pure: `id`, shortened and **quoted** for an `[audit-lost]` line or an
/// audit-sink row label: in double quotes, with any `"`, `\` or control
/// character inside escaped (Rust's `{:?}` of a string).
///
/// Quoted because the ids come from outside the core and the daemon's
/// shutdown line joins row labels with `"; "` (#802): an unquoted id
/// `x; matrix reply to …` would read as two rows. Inside quotes whose own `"`
/// cannot appear unescaped, an id cannot end its entry early. Lives in the
/// lib since #808, so the bus's own lost-row line quotes ids the same way the
/// daemon's sinks do.
pub fn quoted_id(id: &str) -> String {
    format!("{:?}", cap_chars(id, QUOTED_ID_CAP_CHARS))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id is quoted and its own quote escaped; the cap bites inside the
    /// quotes. (The forgery cases live with the shutdown line that joins
    /// labels, in the daemon's `audit_sink_report_tests.rs`.)
    #[test]
    fn a_quoted_id_is_quoted_escaped_and_capped() {
        assert_eq!(quoted_id(r#"<a"b@h>"#), r#""<a\"b@h>""#);
        assert!(quoted_id(&"x".repeat(10_000)).len() < 300);
    }

    /// The cap the email channel's skipped-id sink has always used; kept here
    /// so these tests read as the same cases they were before the lift.
    const CAP: usize = 256;

    #[test]
    fn short_input_passes_through_unchanged() {
        assert_eq!(cap_chars("no usable From address", CAP), "no usable From address");
    }

    #[test]
    fn input_at_exactly_the_cap_passes_through_unchanged() {
        let at_cap = "a".repeat(CAP);
        assert_eq!(cap_chars(&at_cap, CAP), at_cap);
    }

    /// Simulates `describe_email_error`'s `localmail {status}: {body}` shape
    /// with a body well past its own 200-char worker-side cap — a sink must
    /// not rely on that cap holding.
    #[test]
    fn oversized_input_is_truncated_and_marked() {
        let huge = format!("localmail 500: {}", "x".repeat(5_000));
        let capped = cap_chars(&huge, CAP);

        assert!(capped.chars().count() <= CAP + TRUNCATION_MARKER.len());
        assert!(capped.ends_with(TRUNCATION_MARKER), "{capped}");
        assert!(huge.len() > capped.len(), "must actually shrink an oversized value");
    }

    /// Multi-byte chars (a non-ASCII upstream error message) straddling the
    /// cap must not panic or produce an invalid `String`.
    #[test]
    fn truncation_lands_on_a_char_boundary_not_mid_utf8_codepoint() {
        let multibyte = "€".repeat(CAP + 10);
        let capped = cap_chars(&multibyte, CAP); // would panic on a mid-codepoint byte slice

        assert!(capped.starts_with('€'));
    }

    /// The degenerate cap keeps only the marker — no panic, no empty string
    /// that would read as "there was no cause".
    #[test]
    fn a_zero_cap_keeps_only_the_marker() {
        assert_eq!(cap_chars("anything", 0), TRUNCATION_MARKER);
    }
}
