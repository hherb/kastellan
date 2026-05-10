//! Scheduler — agent loop with two concurrent lanes.
//!
//! See `docs/superpowers/specs/2026-05-10-scheduler-design.md` for
//! the full design contract.
//!
//! Module split:
//!   - `prompts`   — version-tracked agent prompts (PromptCache + ledger)
//!   - `agent`     — formulate_plan LLM adapter
//!   - `inner_loop` — per-task iterative replanning (TaskContext + run_to_terminal)
//!   - `runner`    — per-lane runner loop (this lands in Phase 3)

pub mod prompts;

// Added by Task 2.3:
// pub mod agent;
// Added by Task 2.4:
// pub mod inner_loop;
// Added by Task 3.1:
// pub mod runner;
// pub use runner::{spawn_scheduler, SchedulerHandle};
