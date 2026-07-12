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
//! * [`pg_builder::PgSystemPromptBuilder`] — async impl of
//!   [`SystemPromptBuilder`] that holds a [`PgPool`] and calls
//!   the two loaders before invoking the pure assembler.
//! * [`pg_builder::StaticSystemPromptBuilder`] — test impl that
//!   returns a fixed string with `(l0_count, l1_count) = (0, 0)`.
//!
//! ## Why a trait instead of a free function
//!
//! Parallel to the existing
//! [`PlanFormulator`](crate::scheduler::agent::PlanFormulator) seam.
//! Tests swap in the static impl; production wires the PG impl
//! through `RouterAgent::new`. A future recall-aware impl is a new
//! type implementing the same trait, not a rewrite.

use async_trait::async_trait;
use kastellan_db::DbError;
use thiserror::Error;

pub mod assemble;
mod now;
pub mod pg_builder;

pub use assemble::assemble_system_prompt;
pub use now::{resolve_timezone, TzSource};
pub use pg_builder::{PgSystemPromptBuilder, StaticSystemPromptBuilder};

/// Error returned by [`SystemPromptBuilder::build`] when the underlying
/// memory loaders fail.
///
/// The variant exists primarily so callers (specifically
/// [`crate::scheduler::agent::RouterAgent::formulate_plan`]) can
/// fail-closed on memory-load errors. Running with a degraded prompt
/// (missing L0 → missing constitutional posture) is more dangerous than
/// failing the plan iteration and letting the scheduler retry.
#[derive(Debug, Error)]
pub enum PromptAssemblyError {
    /// One of the layer loaders returned an error from `db::memories`.
    #[error("memory load failed: {0}")]
    MemoryLoad(#[from] DbError),
}

/// Result of a [`SystemPromptBuilder::build`] call.
///
/// Carries the assembled `system_prompt` plus the per-layer row counts.
/// The counts come straight from the loader output at the moment of
/// assembly — they cannot drift away from what the model actually saw.
/// `RouterAgent` writes them into the `plan.formulate` audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledPrompt {
    /// The full system message text the model will see.
    pub system_prompt: String,
    /// Number of L0 (meta-rule) rows that fed into the assembly.
    pub l0_count: usize,
    /// Number of L1 (insight-index) rows that fed into the assembly.
    pub l1_count: usize,
    /// Number of L3 skill rows the assembler folded into the `<skills>`
    /// block. Stays 0 until an operator approves a crystallised skill.
    /// RouterAgent records this into `FormulationMeta::skill_count`.
    pub skill_count: usize,
    /// Number of recalled-memory rows that fed into the assembly.
    /// `0` for callers that don't run recall (e.g. tests using
    /// `StaticSystemPromptBuilder::empty()` without calling
    /// `build_with_recalled`). RouterAgent writes this into the
    /// `recall_count` audit-row key.
    pub recalled_count: usize,
}

/// Async seam between `RouterAgent` and the L0/L1 loaders.
///
/// Production: [`PgSystemPromptBuilder`] (runs the DB loaders).
/// Tests: [`StaticSystemPromptBuilder`] (fixed string + zero counts).
///
/// **Fail-closed contract:** any error from the underlying memory
/// loaders propagates as [`PromptAssemblyError`]. The caller must
/// surface it (don't fall back to base-only — see
/// [`PromptAssemblyError`] docstring for why).
#[async_trait]
pub trait SystemPromptBuilder: Send + Sync {
    /// Assemble a system prompt by combining the loaded L0/L1 rows
    /// with the supplied `base`. Equivalent to
    /// [`Self::build_with_recalled`] with an empty
    /// [`crate::recall_assembly::RecalledContext`].
    ///
    /// Retained as a convenience for call sites that pre-date the
    /// recall-lane wiring slice (mostly tests).
    async fn build(&self, base: &str) -> Result<AssembledPrompt, PromptAssemblyError> {
        self.build_with_recalled(base, &crate::recall_assembly::RecalledContext::empty()).await
    }

    /// Assemble a system prompt by combining the loaded L0/L1 rows,
    /// the supplied `recalled` context, and `base`.
    ///
    /// Production use site: `RouterAgent::formulate_plan` calls
    /// `RecallBuilder::build(query)` first, then passes the result here.
    async fn build_with_recalled(
        &self,
        base: &str,
        recalled: &crate::recall_assembly::RecalledContext,
    ) -> Result<AssembledPrompt, PromptAssemblyError>;
}
