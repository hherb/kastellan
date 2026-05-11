//! Scheduler — agent loop with two concurrent lanes.
//!
//! See `docs/superpowers/specs/2026-05-10-scheduler-design.md` for
//! the full design contract.
//!
//! Module split:
//!   - `prompts`        — version-tracked agent prompts (PromptCache + ledger)
//!   - `agent`          — formulate_plan LLM adapter
//!   - `inner_loop`     — per-task iterative replanning (TaskContext + run_to_terminal)
//!   - `runner`         — per-lane runner loop
//!   - `tool_dispatch`  — production `StepDispatcher` wiring to `tool_host::dispatch`

pub mod agent;
pub mod inner_loop;
pub mod prompts;
pub mod runner;
pub mod tool_dispatch;

pub use runner::{spawn_scheduler, SchedulerHandle};
pub use tool_dispatch::{
    map_dispatch_result, rpc_code_name, shell_exec_entry, ToolEntry, ToolHostStepDispatcher,
    ToolRegistry,
};
