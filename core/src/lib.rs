//! hhagent-core: the agent core.
//!
//! Modules will be filled in across phases:
//!   - scheduler        — agent loop and task queue
//!   - context          — context-window manager and reset triggers
//!   - memory           — Postgres-backed hybrid recall (pgvector + BM25 + graph)
//!   - policy           — per-tool capability gate
//!   - llm_router       — sole egress for LLM calls
//!   - tool_host        — spawn/supervise sandboxed tool workers over JSON-RPC stdio
//!   - audit            — append-only audit log
//!   - channel_bus      — fan-in/out for messaging-channel adapters

pub mod audit_mirror;
pub mod audit_tail;
pub mod cassandra;
pub mod cli_audit;
pub mod memory;
pub mod observation;
pub mod scheduler;
pub mod tool_host;
pub mod workspace;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Log line emitted by the `hhagent` daemon's `main` after the database
/// bring-up probe completes. Tests and supervisors poll the daemon's
/// redirected stdout for this string to decide that the daemon is ready.
///
/// Exposing it as a `const` (rather than a free-form `info!` literal)
/// turns a future rename into a compile-time break for every external
/// consumer, instead of a silent timeout in the `supervisor_e2e`
/// integration test.
///
/// Replace with a real readiness protocol (e.g. a JSON-RPC notification
/// over a side channel) when the daemon grows other heartbeat signals;
/// see issue #14.
pub const STARTUP_READY_MSG: &str = "database probe succeeded";
