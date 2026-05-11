//! Errors returned by the LLM router.
//!
//! The variants split the failure space into causes the caller can
//! actually act on:
//!
//! * [`RouterError::Config`] — misconfiguration before the wire is even
//!   touched (bad URL, missing env var). The router refused to send.
//! * [`RouterError::Transport`] — the HTTP call itself failed (DNS,
//!   connection refused, TLS handshake, request/response timeout). The
//!   request never made it to a healthy backend, or the response was
//!   never fully read.
//! * [`RouterError::HttpStatus`] — the backend answered with a non-2xx
//!   status. Body text is captured (truncated) for operator triage,
//!   not parsed — different OpenAI-compatible backends shape error
//!   bodies differently and we don't want to fight them at this layer.
//! * [`RouterError::DecodeResponse`] — 2xx body did not match
//!   [`crate::messages::ChatResponse`]. Either the backend is not
//!   actually OpenAI-compatible or the schema drifted.
//! * [`RouterError::PolicyDeniedFrontier`] — the policy gate refused
//!   to escalate to the frontier backend. Phase 0's
//!   [`crate::policy::DefaultLocalPolicy`] never escalates, so this
//!   variant is reserved for Phase-5 policy implementations and the
//!   "frontier-disabled" stub path.
//! * [`RouterError::EmbeddingCountMismatch`] — the backend's
//!   `/embeddings` response carried a different number of vectors
//!   than the request asked for. The caller cannot safely zip
//!   results back to inputs; treat as a backend protocol bug.

use thiserror::Error;

/// Truncate an error-response body so a hostile or oversized backend
/// reply can't blow up our log lines / panic messages. Pure function;
/// kept here rather than in a `util` module because [`RouterError`]
/// is the only consumer.
pub(crate) fn truncate_for_error(body: &str, max: usize) -> String {
    if body.len() <= max {
        body.to_string()
    } else {
        let mut s = body[..max].to_string();
        s.push_str("…[truncated]");
        s
    }
}

/// Cap on the captured-body length inside [`RouterError::HttpStatus`]
/// and [`RouterError::DecodeResponse`]. 1 KiB is enough for a typical
/// `{"error": {...}}` envelope without becoming a denial-of-service
/// vector if a backend dumps megabytes of HTML.
pub(crate) const ERROR_BODY_CAP: usize = 1024;

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("router configuration error: {0}")]
    Config(String),

    #[error("HTTP transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("backend returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("failed to decode response body as ChatResponse: {source}; raw body: {body}")]
    DecodeResponse {
        #[source]
        source: serde_json::Error,
        body: String,
    },

    #[error("policy denied escalation to frontier backend: {0}")]
    PolicyDeniedFrontier(String),

    /// Backend returned a different number of embedding vectors
    /// than the request asked for. Fires inside `Router::embed`
    /// when `response.data.len() != request.input.len()`. The
    /// caller cannot safely zip the returned vectors back to their
    /// input texts; treat this as a backend protocol violation.
    #[error("embedding count mismatch: requested {requested}, got {returned}")]
    EmbeddingCountMismatch { requested: usize, returned: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_for_error_passes_through_short_strings() {
        assert_eq!(truncate_for_error("ok", 10), "ok");
        // Exact-cap boundary is also pass-through (`<=`, not `<`).
        let exact = "a".repeat(10);
        assert_eq!(truncate_for_error(&exact, 10), exact);
    }

    #[test]
    fn truncate_for_error_appends_marker_when_oversized() {
        let big = "a".repeat(20);
        let out = truncate_for_error(&big, 10);
        assert_eq!(out, format!("{}…[truncated]", "a".repeat(10)));
        assert!(out.starts_with(&"a".repeat(10)));
        assert!(out.ends_with("…[truncated]"));
    }

    #[test]
    fn error_body_cap_is_one_kib() {
        // Regression pin: tightening this number should be a deliberate
        // choice (it shows up in operator-facing log lines and panic
        // messages). 1 KiB matches the "typical OpenAI error envelope
        // fits comfortably" reasoning in the module docstring.
        assert_eq!(ERROR_BODY_CAP, 1024);
    }

    #[test]
    fn embedding_count_mismatch_display_and_fields() {
        let err = RouterError::EmbeddingCountMismatch {
            requested: 3,
            returned: 2,
        };
        // Pin the exact operator-facing message. Once this lands in
        // production logs the wording is hard to change without
        // breaking downstream log-search queries.
        assert_eq!(
            err.to_string(),
            "embedding count mismatch: requested 3, got 2"
        );
        // Field-shape pin: matching by name proves the variant carries
        // the expected fields, not just positional placeholders.
        if let RouterError::EmbeddingCountMismatch { requested, returned } = err {
            assert_eq!(requested, 3);
            assert_eq!(returned, 2);
        } else {
            panic!("wrong variant");
        }
    }
}
