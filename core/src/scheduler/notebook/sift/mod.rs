//! `sift` (#922): one planner step that goes through every item of a
//! candidate list — reading each one if asked — keeps the items a cheap
//! per-item decision (the "sorter") says match, with the fields it
//! extracted, in the task's scrapbook, and tells the planner only the counts
//! and a small sample.
//!
//! * [`params`] — the call's parameters, scrapbook refs, the read allowlist.
//! * [`template`] — the candidate list, and each item's reads.
//! * [`config`] — the two operator knobs, checked at boot.
//! * [`sorter`] — the per-item decision, and what it is shown.
//! * [`table`] — the answer's table, rendered by code.
//!
//! Design: `docs/superpowers/specs/2026-11-02-sift-and-scrapbook-design.md`.

pub mod config;
pub(crate) mod params;
pub(crate) mod sorter;
pub(crate) mod table;
pub(crate) mod template;

/// The reserved built-in tool name (K1): no worker manifest may claim it.
pub const SIFT_TOOL: &str = "sift";
/// The one method on [`SIFT_TOOL`].
pub const SIFT_METHOD: &str = "sift.run";
