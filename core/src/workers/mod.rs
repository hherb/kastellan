//! Worker manifests + wire-shape types.
//!
//! Each submodule owns one worker's host-side manifest — a
//! [`crate::worker_manifest::WorkerManifest`] impl plus its
//! [`crate::scheduler::ToolEntry`] constructor and the request/response serde
//! types that pin its JSON-RPC wire contract. Manifests are Rust (compiled in,
//! not on-disk TOML) per the 2026-06-05 worker-manifest-plumbing design.

pub mod browser_driver;
pub mod gliner_relex;
pub mod shell_exec;
pub mod web_fetch;
pub mod web_search;
