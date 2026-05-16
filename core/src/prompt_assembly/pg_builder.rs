//! Production + test implementations of [`SystemPromptBuilder`].
//!
//! * [`PgSystemPromptBuilder`] — async DB-backed builder used by
//!   `RouterAgent` in production.
//! * [`StaticSystemPromptBuilder`] — fixed-string builder for tests
//!   that don't care about the assembled shape. Always reports
//!   `(l0_count, l1_count) = (0, 0)` — tests that need non-zero
//!   counts use the prod builder against a per-test PG cluster.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::memory::l0_seed::load_l0_active_default;
use crate::memory::layers::load_l1_default;

use super::{
    assemble::assemble_system_prompt, AssembledPrompt, PromptAssemblyError, SystemPromptBuilder,
};

/// Production builder: loads L0 + L1 from Postgres on every call.
///
/// Each `build` invocation re-runs both loaders so operator edits to
/// the seed file (after restart) and DB-level changes take effect on
/// the next plan iteration. The cost is two small SELECTs; cheap
/// relative to the LLM call that follows.
///
/// Holds [`PgPool`] by value (not `Arc<PgPool>`) to match the
/// codebase convention — `sqlx::PgPool` already wraps its connection
/// pool in an internal `Arc`, so cloning is cheap and ordinary
/// `pool.clone()` at call sites is the established idiom (see e.g.
/// `core::scheduler::tool_dispatch::ToolHostStepDispatcher::new`).
pub struct PgSystemPromptBuilder {
    pool: PgPool,
}

impl PgSystemPromptBuilder {
    /// Construct a builder pinned to the supplied pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SystemPromptBuilder for PgSystemPromptBuilder {
    async fn build(&self, base: &str) -> Result<AssembledPrompt, PromptAssemblyError> {
        // TODO(token-cap, issue #78): both loaders are uncapped at
        // the I/O layer beyond `load_l1_default`'s internal row-count
        // + byte caps. Safe today (L1 is empty in prod until a
        // promotion writer lands), but the day an L1 writer arrives
        // the assembled prompt can balloon. When the deferred "global
        // token cap with priority drop" follow-up lands, plumb a
        // budget through here so the assembler can priority-drop rows
        // rather than relying solely on per-layer caps inside the
        // loaders. See https://github.com/hherb/hhagent/issues/78.
        let l0 = load_l0_active_default(&self.pool).await?;
        let l1 = load_l1_default(&self.pool).await?;
        let system_prompt = assemble_system_prompt(&l0, &l1, base);
        Ok(AssembledPrompt {
            system_prompt,
            l0_count: l0.len(),
            l1_count: l1.len(),
        })
    }
}

/// Test-only fixed-string builder.
///
/// Always returns the same `system_prompt` regardless of the `base`
/// argument. Both counts are `0` (tests requiring real counts use
/// [`PgSystemPromptBuilder`] against a per-test PG cluster). `pub`
/// (not `cfg(test)`) so cross-crate integration tests in
/// `core/tests/*.rs` can use it without a separate dev-dep export.
pub struct StaticSystemPromptBuilder {
    fixed: String,
}

impl StaticSystemPromptBuilder {
    /// Empty-string builder. Most tests use this — the assembled
    /// prompt is empty and the model never sees L0/L1 framing.
    pub fn empty() -> Self {
        Self { fixed: String::new() }
    }

    /// Fixed-string builder. Used by the one test (in this module)
    /// that needs to assert a specific output flowed through.
    pub fn new(fixed: impl Into<String>) -> Self {
        Self { fixed: fixed.into() }
    }
}

#[async_trait]
impl SystemPromptBuilder for StaticSystemPromptBuilder {
    async fn build(&self, _base: &str) -> Result<AssembledPrompt, PromptAssemblyError> {
        Ok(AssembledPrompt {
            system_prompt: self.fixed.clone(),
            l0_count: 0,
            l1_count: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_builder_returns_fixed_string_ignoring_base() {
        let b = StaticSystemPromptBuilder::new("FIXED-OUTPUT");
        // The base is ignored — same return regardless of input.
        let r1 = b.build("base one").await.expect("static build never fails");
        let r2 = b.build("base two").await.expect("static build never fails");
        assert_eq!(r1.system_prompt, "FIXED-OUTPUT");
        assert_eq!(r2.system_prompt, "FIXED-OUTPUT");
        assert_eq!(r1.l0_count, 0, "static builder always reports 0 l0 rows");
        assert_eq!(r1.l1_count, 0, "static builder always reports 0 l1 rows");
        assert_eq!(r2.l0_count, 0, "second call must also report 0 l0 rows");
        assert_eq!(r2.l1_count, 0, "second call must also report 0 l1 rows");
    }

    #[tokio::test]
    async fn static_builder_empty_constructor_yields_empty_string() {
        let b = StaticSystemPromptBuilder::empty();
        let r = b.build("ignored").await.expect("static build never fails");
        assert_eq!(r.system_prompt, "", "empty constructor yields empty system_prompt");
        assert_eq!(r.l0_count, 0);
        assert_eq!(r.l1_count, 0);
    }
}
