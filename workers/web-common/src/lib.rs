//! Shared building blocks for net-egress tool workers.
//!
//! - [`allowlist`] — host allowlist matcher (exact + `.domain` wildcard).
//! - [`http`] — the `HttpGet` transport seam + the real `ReqwestGet`.
//! - [`embed_rows`] — shared reorder/count/contiguity check for embedding responses.
//! - [`testing`] (feature `testing`) — a fake transport + builders for unit tests.
//! - [`search`] / [`parse`] (feature `search`) — pure SearxNG query logic.
//! - [`search_provider`] (feature `search`) — the direct/brokered `SearchProvider`
//!   seam + the shared `SearchError → RpcError` mapper.
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

/// JSON-RPC method the web-search worker dispatches on for batched search. A
/// cross-crate contract: `kastellan-core` advertises this exact string (as its
/// own `WEB_SEARCH_BATCH_METHOD`) in `tool_docs()`, the planner emits it as the
/// step `method`, and the worker matches on it here. Core can't import
/// `web-common` (dev-dependency only), so the two live as separate literals
/// pinned equal by the `web_search_batch_method_matches_worker_contract`
/// integration test — a rename on either side then fails CI instead of silently
/// routing every batch call to `METHOD_NOT_FOUND`.
pub const WEB_SEARCH_BATCH_METHOD: &str = "web.search_batch";
#[cfg(feature = "search")]
pub mod parse;
#[cfg(feature = "search")]
pub mod search;
#[cfg(feature = "search")]
pub mod search_provider;
#[cfg(feature = "fetch")]
pub mod fetch;
#[cfg(feature = "extract")]
pub mod extract;
pub(crate) mod proxy_connect;

#[cfg(feature = "testing")]
pub mod testing;
