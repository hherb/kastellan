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
//!   - `audit`          — pure helpers for scheduler-emitted audit rows (spec §7)
//!   - `crash_recovery` — startup sweep + `task.crashed` audit row emission (spec §7)

pub mod agent;
pub mod audit;
pub mod crash_recovery;
pub mod inner_loop;
pub mod prompts;
pub mod runner;
pub mod tool_dispatch;

pub use runner::{spawn_scheduler, SchedulerHandle};
pub use tool_dispatch::{shell_exec_entry, ToolEntry, ToolHostStepDispatcher, ToolRegistry};
