//! Shared building blocks for net-egress tool workers.
//!
//! - [`allowlist`] — host allowlist matcher (exact + `.domain` wildcard).
//! - [`http`] — the `HttpGet` transport seam + the real `ReqwestGet`.
//! - [`testing`] (feature `testing`) — a fake transport + builders for unit tests.
//! - [`search`] / [`parse`] (feature `search`) — pure SearxNG query logic.

pub mod allowlist;
pub mod http;
#[cfg(feature = "search")]
pub mod parse;
#[cfg(feature = "search")]
pub mod search;
pub(crate) mod proxy_connect;

#[cfg(feature = "testing")]
pub mod testing;
