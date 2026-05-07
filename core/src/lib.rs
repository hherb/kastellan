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

pub mod tool_host;
pub mod workspace;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
