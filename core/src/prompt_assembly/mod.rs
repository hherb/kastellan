//! `prompt_assembly` — build the LLM system message from L0 meta-rules,
//! L1 insights, and the existing `agent_planner.md` base.
//!
//! ## Role in the system
//!
//! `RouterAgent::formulate_plan` ([crate::scheduler::agent]) previously
//! sent the bare base prompt as the system message. Now it sends an
//! assembled prompt that frames the L0 layer (hard agent constraints)
//! and L1 layer (insight routing pointers) ahead of the base. The
//! model sees safety + operational context every plan iteration, with
//! a fresh load on each call so operator-edited rules take effect
//! without a daemon restart.
//!
//! ## Module layout
//!
//! * [`assemble::assemble_system_prompt`] — pure: takes `&[Memory]`
//!   slices and a base `&str`, returns the assembled `String`. Empty
//!   layers are omitted entirely (no tag emitted).
//!
//! ## Why a trait instead of a free function
//!
//! Parallel to the existing [`PlanFormulator`](crate::scheduler::agent::PlanFormulator)
//! seam. Tests swap in the static impl; production wires the PG impl
//! through `RouterAgent::new`. A future recall-aware impl is a new
//! type implementing the same trait, not a rewrite.

pub mod assemble;

pub use assemble::assemble_system_prompt;
