//! Shared building blocks for net-egress tool workers.
//!
//! - [`allowlist`] — host allowlist matcher (exact + `.domain` wildcard).
//! - [`http`] — the `HttpGet` transport seam + the real `ReqwestGet`.
//! - [`testing`] (feature `testing`) — a fake transport + builders for unit tests.

pub mod allowlist;
