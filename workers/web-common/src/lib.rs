//! Shared building blocks for net-egress tool workers.
//!
//! - [`allowlist`] — host allowlist matcher (exact + `.domain` wildcard).
//! - [`http`] — the `HttpGet` transport seam + the real `ReqwestGet`.
//! - [`embed_rows`] — shared reorder/count/contiguity check for embedding responses.
//! - [`testing`] (feature `testing`) — a fake transport + builders for unit tests.
//! - [`search`] / [`parse`] (feature `search`) — pure SearxNG query logic.
//! - [`fetch`] (feature `fetch`) — redirect-following drive loop.
//! - [`extract`] (feature `extract`) — HTML/PDF/text readable-text extraction.

pub mod allowlist;
pub mod embed_rows;
pub mod http;

/// Env-var name that overrides the `web.search_batch` size cap. A cross-crate
/// contract between `kastellan-core` (which injects it into the web-search jail
/// when the operator sets it) and the web-search worker (which reads it at
/// startup). Defined here — always compiled, unlike the feature-gated `search`
/// module — so both sides share one definition rather than two literals that
/// must be "kept in sync".
pub const WEB_SEARCH_MAX_BATCH_QUERIES_ENV: &str = "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES";
#[cfg(feature = "search")]
pub mod parse;
#[cfg(feature = "search")]
pub mod search;
#[cfg(feature = "fetch")]
pub mod fetch;
#[cfg(feature = "extract")]
pub mod extract;
pub(crate) mod proxy_connect;

#[cfg(feature = "testing")]
pub mod testing;
