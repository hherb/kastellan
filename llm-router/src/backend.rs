//! Backend selector — local vs frontier — and the URL it resolves to.
//!
//! The router's job is funnelled through one decision: **which HTTP
//! base URL do we POST `/chat/completions` to?** Phase 0 has two:
//!
//! * [`Backend::Local`] — a self-hosted OpenAI-compatible server
//!   (vLLM/SGLang on Linux, llama.cpp/Ollama on macOS). Always
//!   reachable; never authenticated; no egress beyond the host.
//! * [`Backend::Frontier`] — a hosted commercial endpoint (Anthropic,
//!   OpenAI, etc.). **Wired here, but the router refuses to dispatch
//!   to it until the Phase-5 policy gate exists.** This slice just
//!   carries the URL through so the type surface is stable; see
//!   [`crate::policy::DefaultLocalPolicy`].
//!
//! ## Why a closed enum and not a `Box<dyn Backend>` trait
//! Phase 0 has exactly two backends and the Phase-5 sketch (policy
//! gate decides between two named pools) preserves that. A trait
//! would force every consumer to either hold a `&dyn Backend` or
//! pin it behind an `Arc`, which buys nothing today and has a
//! refactor cost when we want to pattern-match on the choice (the
//! audit log writer wants to record *which* backend served the
//! call). When a third backend appears (say, an on-prem
//! inference cluster as a third tier) a trait is cheap to refactor
//! into; today it would be premature.

use serde::{Deserialize, Serialize};

/// Which logical backend a request is routed to.
///
/// Wire-encoded as `"local"` / `"frontier"` for audit-log payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Local,
    Frontier,
}

impl Backend {
    /// Stable string tag used in tracing spans and audit-log payloads.
    /// Keep in sync with the serde rename — round-trips assume
    /// `as_tag()` and the `Serialize` impl agree.
    pub fn as_tag(&self) -> &'static str {
        match self {
            Backend::Local => "local",
            Backend::Frontier => "frontier",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_serializes_as_lowercase_tag() {
        // Wire-shape pin: the audit-log payload writer records this
        // string; a future operator-facing query like
        // `WHERE actor='llm:router' AND payload->>'backend'='frontier'`
        // depends on it.
        assert_eq!(serde_json::to_string(&Backend::Local).unwrap(), "\"local\"");
        assert_eq!(serde_json::to_string(&Backend::Frontier).unwrap(), "\"frontier\"");
    }

    #[test]
    fn backend_as_tag_matches_serde_rename() {
        // Round-trip-style pin so a future contributor adding a third
        // variant updates both sites.
        for b in [Backend::Local, Backend::Frontier] {
            let serde_form = serde_json::to_string(&b).unwrap();
            // Strip the surrounding quotes from the JSON-encoded string.
            let unquoted = serde_form.trim_matches('"').to_string();
            assert_eq!(unquoted, b.as_tag());
        }
    }

    #[test]
    fn backend_round_trips() {
        for b in [Backend::Local, Backend::Frontier] {
            let s = serde_json::to_string(&b).unwrap();
            let r: Backend = serde_json::from_str(&s).unwrap();
            assert_eq!(b, r);
        }
    }
}
