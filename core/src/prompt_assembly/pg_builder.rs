//! Production + test implementations of [`SystemPromptBuilder`].
//!
//! * [`PgSystemPromptBuilder`] — async DB-backed builder used by
//!   `RouterAgent` in production.
//! * [`StaticSystemPromptBuilder`] — fixed-string builder for tests
//!   that don't care about the assembled shape. Always reports
//!   `(l0_count, l1_count) = (0, 0)` — tests that need non-zero
//!   counts use the prod builder against a per-test PG cluster.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::memory::l0_seed::load_l0_active_default;
use crate::memory::layers::load_l1_default;
use crate::worker_manifest::ToolDoc;

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
    /// Advertised tool set rendered into the `<tools>` block. Empty until set
    /// via [`with_tool_docs`](Self::with_tool_docs); the daemon builds it once
    /// from the live registry at startup.
    tool_docs: Arc<[ToolDoc]>,
    /// Configured planner timezone. `None` (the `new()` default) → no `<now>`
    /// block, keeping output byte-identical to the pre-`<now>` builder. Set by
    /// the daemon via [`with_timezone`](Self::with_timezone).
    timezone: Option<jiff::tz::TimeZone>,
}

impl PgSystemPromptBuilder {
    /// Construct a builder pinned to the supplied pool, advertising no tools
    /// and injecting no `<now>` block.
    pub fn new(pool: PgPool) -> Self {
        Self { pool, tool_docs: Arc::from(Vec::new()), timezone: None }
    }

    /// Attach the advertised tool set (rendered into the `<tools>` block).
    /// Threaded from `build_tool_registry` so only registered tools appear.
    pub fn with_tool_docs(mut self, tool_docs: Arc<[ToolDoc]>) -> Self {
        self.tool_docs = tool_docs;
        self
    }

    /// Attach the planner timezone (enables the `<now>` block). Threaded from
    /// `resolve_timezone(KASTELLAN_TIMEZONE)` at daemon startup; the instant is
    /// captured fresh on every [`build_with_recalled`](Self::build_with_recalled).
    pub fn with_timezone(mut self, tz: jiff::tz::TimeZone) -> Self {
        self.timezone = Some(tz);
        self
    }

    #[cfg(test)]
    fn tool_docs_for_test(&self) -> &[ToolDoc] {
        &self.tool_docs
    }

    #[cfg(test)]
    fn timezone_for_test(&self) -> Option<&jiff::tz::TimeZone> {
        self.timezone.as_ref()
    }
}

#[async_trait]
impl SystemPromptBuilder for PgSystemPromptBuilder {
    async fn build_with_recalled(
        &self,
        base: &str,
        recalled: &crate::recall_assembly::RecalledContext,
    ) -> Result<AssembledPrompt, PromptAssemblyError> {
        // TODO(token-cap, issue #78): all three loaders (L0, L1,
        // recalled) are uncapped at the I/O layer beyond their
        // internal per-layer caps. Safe today because both L1 and the
        // recalled-bodies cap are bounded; the deferred "global token
        // cap with priority drop" follow-up will plumb a budget
        // through here. See https://github.com/hherb/kastellan/issues/78.
        let l0 = load_l0_active_default(&self.pool).await?;
        let l1 = load_l1_default(&self.pool).await?;
        let skills = crate::memory::l3_surface::load_l3_skills_default(&self.pool).await?;
        let now_block = self.timezone.as_ref().map(super::now::current_now_block);
        let system_prompt = assemble_system_prompt(
            &l0,
            &l1,
            &skills,
            recalled,
            base,
            &self.tool_docs,
            now_block.as_deref(),
        );
        Ok(AssembledPrompt {
            system_prompt,
            l0_count: l0.len(),
            l1_count: l1.len(),
            skill_count: skills.len(),
            // Source from RecalledContext::len() (bodies.len()) — what
            // the assembler actually rendered — rather than ids.len(),
            // so any future divergence fails towards the rendered truth.
            // The new() constructor invariant makes the two equal today.
            recalled_count: recalled.len(),
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
    async fn build_with_recalled(
        &self,
        _base: &str,
        recalled: &crate::recall_assembly::RecalledContext,
    ) -> Result<AssembledPrompt, PromptAssemblyError> {
        Ok(AssembledPrompt {
            system_prompt: self.fixed.clone(),
            l0_count: 0,
            l1_count: 0,
            skill_count: 0,
            recalled_count: recalled.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pg_builder_retains_tool_docs() {
        // A lazily-connected pool that is never queried in this unit test.
        // `connect_lazy` needs a Tokio context, hence `#[tokio::test]`.
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let docs: Arc<[ToolDoc]> = Arc::from(vec![ToolDoc {
            name: "web-search",
            method: "web.search",
            summary: "s",
            params: &[],
        }]);
        let b = PgSystemPromptBuilder::new(pool).with_tool_docs(docs);
        assert_eq!(b.tool_docs_for_test().len(), 1);
        assert_eq!(b.tool_docs_for_test()[0].name, "web-search");
    }

    #[tokio::test]
    async fn builder_defaults_to_no_timezone() {
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let b = PgSystemPromptBuilder::new(pool);
        assert!(b.timezone_for_test().is_none(), "new() must not inject <now>");
    }

    #[tokio::test]
    async fn with_timezone_sets_the_zone() {
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let (tz, _src) = crate::prompt_assembly::resolve_timezone(Some("Australia/Sydney"));
        let b = PgSystemPromptBuilder::new(pool).with_timezone(tz);
        // The block the builder would inject is well-formed and current-year.
        let block = super::super::now::current_now_block(b.timezone_for_test().unwrap());
        assert!(block.starts_with("<now>\n") && block.trim_end().ends_with("</now>"), "got: {block}");
        assert!(block.contains("202"), "renders a plausible year; got: {block}");
    }

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
        assert_eq!(r1.recalled_count, 0);
        assert_eq!(r2.l0_count, 0, "second call must also report 0 l0 rows");
        assert_eq!(r2.l1_count, 0, "second call must also report 0 l1 rows");
        assert_eq!(r2.recalled_count, 0);
    }

    #[tokio::test]
    async fn static_builder_empty_constructor_yields_empty_string() {
        let b = StaticSystemPromptBuilder::empty();
        let r = b.build("ignored").await.expect("static build never fails");
        assert_eq!(r.system_prompt, "", "empty constructor yields empty system_prompt");
        assert_eq!(r.l0_count, 0);
        assert_eq!(r.l1_count, 0);
        assert_eq!(r.recalled_count, 0);
    }

    #[tokio::test]
    async fn static_builder_build_with_recalled_passes_recalled_count_through() {
        use crate::recall_assembly::RecalledContext;
        let b = StaticSystemPromptBuilder::new("FIXED");
        let recalled = RecalledContext::new(
            vec![1, 2],
            vec!["body one".into(), "body two".into()],
            "a".repeat(64),
        );
        let r = b.build_with_recalled("base", &recalled).await.unwrap();
        // StaticSystemPromptBuilder ignores base + recalled in the
        // assembled string (it's fixed), but the recalled_count field
        // must report the supplied recalled.len() so RouterAgent
        // can write the audit row with the right number.
        assert_eq!(r.system_prompt, "FIXED");
        assert_eq!(r.l0_count, 0);
        assert_eq!(r.l1_count, 0);
        assert_eq!(r.recalled_count, 2, "recalled_count must reflect the supplied context");
    }
}
