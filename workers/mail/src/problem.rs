//! Pulling the actionable sentence out of a localmail problem+json body.
//!
//! # Why this exists
//!
//! localmail reports a caller error as RFC 7807 problem+json:
//!
//! ```json
//! {"type": "/problems/validation-failed", "title": "Validation failed",
//!  "status": 400, "detail": "cursor: this cursor continues a date-sorted
//!  search; pass sort='date' or omit sort (got 'rank')"}
//! ```
//!
//! Only `detail` is written for the caller. `type`, `title` and `status` are
//! fixed strings that say nothing a planner can act on — and forwarding the
//! whole envelope spends **91 characters** of a 200-character budget saying
//! them, before the useful sentence even starts.
//!
//! # The budget, measured rather than assumed
//!
//! An error reaches the planner as `{"status": "err", "code": <CODE>, "detail": <detail>}` with `detail`
//! clamped to [`kastellan_protocol::STEP_ERR_DETAIL_MAX`] = 200 chars, and this
//! worker's own prefix (`localmail 400: `) is 15 of those. Measured against the
//! live service on 2026-08-15, the whole envelope for the sort/cursor
//! contradiction is **207 chars — 7 over**, so the planner was already being
//! shown a sentence truncated mid-word:
//!
//! ```text
//! … pass sort='date' or omit sort (got 'r…
//! ```
//!
//! It loses the value it was *diagnosing*. Extracting `detail` brings the same
//! error to 109 chars with ~90 to spare.
//!
//! **The margin was never real.** An earlier estimate of this put the envelope
//! at exactly 200 — surviving with zero bytes to spare — by assuming compact
//! JSON separators. The service emits `", "` and `": "`, which is 7 bytes more.
//! A guarantee that depends on another service's whitespace is not a guarantee,
//! which is the argument for not carrying the envelope at all rather than for
//! trimming it: the same class as #536, where repair advice was silently
//! clipped and every test on both sides stayed green.

use serde_json::Value;

/// The `detail` of an RFC 7807 problem+json body, if it has a usable one.
///
/// Pure. Returns `None` — meaning "show the caller the raw body instead" — for
/// anything that is not an object carrying a non-empty string `detail`. That
/// covers a non-JSON body (an HTML error page from a proxy, say), a JSON body
/// that is not problem+json, and a `detail` that is present but empty or of the
/// wrong type. **Falling back to the raw body rather than to a synthesised
/// message is deliberate:** if the shape is not what we expect, the operator
/// and the planner are both better served by whatever the service actually
/// said than by this worker's guess about it.
///
/// Deliberately does *not* truncate. The clamp is `core`'s and applies to the
/// whole rendered error; truncating here as well would cut a second time at a
/// different boundary and could drop a trailing `…` the reader relies on.
pub fn problem_detail(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let detail = parsed.get("detail")?.as_str()?;
    if detail.is_empty() {
        return None;
    }
    Some(detail.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact bytes the live service returned on 2026-08-15 for the
    /// sort/cursor contradiction — copied from the wire, not reconstructed, so
    /// the separator spacing that broke the earlier estimate is preserved.
    const LIVE_400_BODY: &str = r#"{"type": "/problems/validation-failed", "title": "Validation failed", "status": 400, "detail": "cursor: this cursor continues a date-sorted search; pass sort='date' or omit sort (got 'rank')"}"#;

    /// What `handler::mail_err_to_rpc` builds, and therefore what the clamp
    /// measures. The const is imported, never mirrored — the same reasoning as
    /// [`crate::ids`]'s clamp test: lowering it in `core` must fail here rather
    /// than leave production silently eating the advice.
    fn as_the_planner_sees_it(shown: &str) -> String {
        let rendered = format!("localmail 400: {shown}");
        rendered.chars().take(kastellan_protocol::STEP_ERR_DETAIL_MAX).collect()
    }

    #[test]
    fn extracts_the_detail_from_a_real_localmail_problem_body() {
        let d = problem_detail(LIVE_400_BODY).expect("detail present");
        assert!(d.starts_with("cursor: this cursor continues"), "{d}");
        assert!(!d.contains("validation-failed"), "envelope must not survive: {d}");
    }

    /// The guarantee this module exists for: the *whole* actionable sentence
    /// reaches the planner, including the value it is diagnosing.
    #[test]
    fn the_extracted_detail_survives_the_core_side_planner_clamp() {
        let d = problem_detail(LIVE_400_BODY).unwrap();
        let seen = as_the_planner_sees_it(&d);
        assert!(seen.contains("pass sort='date'"), "clamped to: {seen:?}");
        assert!(seen.contains("omit sort"), "clamped to: {seen:?}");
        // The tail is the part the envelope form lost mid-word.
        assert!(seen.contains("(got 'rank')"), "clamped to: {seen:?}");
    }

    /// The negative control, and the reason the test above is not vacuous:
    /// forwarding the envelope — what this worker did before — **does not** fit,
    /// and loses exactly the value being diagnosed. If this ever starts passing,
    /// either the clamp grew or the body shrank, and the guarantee above is no
    /// longer testing what it claims.
    #[test]
    fn the_raw_envelope_does_not_fit_which_is_why_extraction_is_needed() {
        let seen = as_the_planner_sees_it(LIVE_400_BODY);
        assert!(
            !seen.contains("(got 'rank')"),
            "the envelope now fits; this test no longer proves extraction is needed: {seen:?}"
        );
        assert!(seen.contains("pass sort='date'"), "sanity: {seen:?}");
    }

    #[test]
    fn a_non_json_body_falls_back_to_the_raw_text() {
        assert_eq!(problem_detail("<html>502 Bad Gateway</html>"), None);
        assert_eq!(problem_detail(""), None);
    }

    #[test]
    fn json_without_a_usable_detail_falls_back() {
        assert_eq!(problem_detail(r#"{"error": "nope"}"#), None);
        assert_eq!(problem_detail(r#"{"detail": ""}"#), None, "empty detail is not usable");
        assert_eq!(problem_detail(r#"{"detail": 42}"#), None, "non-string detail is not usable");
        assert_eq!(problem_detail(r#"["detail"]"#), None, "not an object");
    }
}
