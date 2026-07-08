//! Shared building blocks for net-egress tool workers.
//!
//! - [`allowlist`] — host allowlist matcher (exact + `.domain` wildcard).
//! - [`http`] — the `HttpGet` transport seam + the real `ReqwestGet`.
//! - [`testing`] (feature `testing`) — a fake transport + builders for unit tests.
//! - [`search`] / [`parse`] (feature `search`) — pure SearxNG query logic.
//! - [`fetch`] (feature `fetch`) — redirect-following drive loop.
//! - [`extract`] (feature `extract`) — HTML/PDF/text readable-text extraction.

pub mod allowlist;
pub mod http;
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
