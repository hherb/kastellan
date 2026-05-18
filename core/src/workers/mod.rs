//! Worker manifests + wire-shape types.
//!
//! Each submodule owns one worker's [`crate::scheduler::ToolEntry`]
//! constructor plus the request/response serde types that pin its
//! JSON-RPC wire contract from the Rust side. Manifests stay as Rust
//! functions (matches the [`crate::scheduler::shell_exec_entry`]
//! precedent); the TOML-manifest-on-disk option is deferred to a
//! later slice per the worker-lifecycle spec's open question 1.

pub mod gliner_relex;
