//! Agent LLM adapter — produces a `Plan` from a `TaskContext` via
//! the existing `hhagent_llm_router::Router`. Strict JSON parsing on
//! the way out: a model that emits a malformed plan is treated as a
//! decode-error, surfaced as `RouterError::DecodeResponse`, and the
//! scheduler's retry policy applies (transient → backoff; decode →
//! permanent fail).
//!
//! The trait `PlanFormulator` lets the inner-loop integration tests
//! swap in a scripted stub without spinning up an LLM.

use async_trait::async_trait;
use thiserror::Error;

use crate::cassandra::types::Plan;
use hhagent_llm_router::messages::{ChatMessage, ChatRequest};
use hhagent_llm_router::{Router, RouterError};

use super::inner_loop::TaskContext;
use super::plan_parser::parse_plan_lenient;
use super::prompts::PromptCache;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("router: {0}")]
    Router(#[from] RouterError),
    #[error("plan decode failed: {detail}")]
    Decode { detail: String, raw: String },
    #[error("agent prompt 'agent_planner' not found in cache")]
    PromptMissing,
    /// L0/L1 load failed under the [`SystemPromptBuilder`]; the scheduler's
    /// retry policy decides whether to retry or fail permanently.
    #[error("prompt assembly: {0}")]
    PromptAssembly(#[from] crate::prompt_assembly::PromptAssemblyError),
}

#[async_trait]
pub trait PlanFormulator: Send + Sync {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError>;
}

/// Returned alongside the decoded `Plan`. The inner loop writes
/// these fields into the `plan.formulate` audit-log row payload.
#[derive(Clone, Debug)]
pub struct FormulationMeta {
    pub prompt_name: String,
    pub prompt_sha256: String,
    pub llm_model: String,
    pub llm_backend: String,
    pub latency_ms: u64,
    pub retry_count: u32,
    /// SHA-256 (hex) of the *assembled* system prompt the model
    /// actually saw — distinct from `prompt_sha256`, which is the
    /// base agent_planner.md hash only.
    pub assembled_prompt_sha256: String,
    /// Number of L0 rows the assembler folded in. Operator triage:
    /// 0 here on a clinical task means the L0 seeder didn't run.
    pub l0_count: usize,
    /// Number of L1 rows the assembler folded in. Stays 0 in
    /// production until an L1 promotion writer lands.
    pub l1_count: usize,
    /// Memory ids the recall lane surfaced for this iteration's
    /// instruction (RRF-fused order, capped at `L_RECALL_CAP_BYTES`).
    /// Empty when recall returned nothing or degraded due to error.
    /// Written verbatim to the `recalled_memory_ids` audit-row key.
    pub recalled_memory_ids: Vec<i64>,
    /// `recalled_memory_ids.len() as u32`. Redundant but cheap to
    /// query — observation-phase SQL avoids `jsonb_array_length` for
    /// the common "did recall fire at all?" question.
    pub recall_count: u32,
    /// Hex SHA-256 of the query text (the task instruction). Lets
    /// observation phase detect when paraphrased prompts produce the
    /// same recalled-id set vs. genuine drift.
    pub recall_query_sha256: String,
    /// Slice F (entity-extraction v2, 2026-05-19): the entity ids the
    /// gliner-relex extractor (or NoOp) resolved for this query.
    /// Empty when extraction degraded or no entities matched.
    pub graph_seed_entity_ids: Vec<i64>,
    /// `graph_seed_entity_ids.len() as u32`. Cheap-to-query duplicate
    /// for observation-phase SQL.
    pub graph_seed_count: u32,
    /// Which extraction path produced the seeds. v2 production is
    /// always `SeedSource::GlinerRelex` or `SeedSource::None`.
    pub graph_seed_source: crate::entity_extraction::SeedSource,
}

/// Production adapter: calls the real `Router::send`.
pub struct RouterAgent {
    router: std::sync::Arc<Router>,
    prompts: std::sync::Arc<PromptCache>,
    prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
    recall_builder: std::sync::Arc<dyn crate::recall_assembly::RecallBuilder>,
}

impl RouterAgent {
    pub fn new(
        router: std::sync::Arc<Router>,
        prompts: std::sync::Arc<PromptCache>,
        prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
        recall_builder: std::sync::Arc<dyn crate::recall_assembly::RecallBuilder>,
    ) -> Self {
        Self { router, prompts, prompt_builder, recall_builder }
    }
}

#[async_trait]
impl PlanFormulator for RouterAgent {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let entry = self.prompts.get("agent_planner")
            .ok_or(AgentError::PromptMissing)?;

        let base = entry.content.clone();

        // Per-iteration recall. Asymmetric posture vs the prompt
        // assembler below: recall failure DEGRADES (we still want the
        // model to plan with L0/L1/base even if retrieval is broken),
        // while prompt-assembly failure is FAIL-CLOSED (a degraded
        // safety prompt would have the agent flying blind on operator
        // rules). See spec "Failure-mode matrix".
        let recalled = match self.recall_builder.build(&ctx.instruction).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "hhagent::scheduler::agent",
                    error = %e,
                    "recall failed; continuing with empty recall context",
                );
                crate::recall_assembly::RecalledContext::empty()
            }
        };

        let assembled = self.prompt_builder
            .build_with_recalled(&base, &recalled)
            .await
            .map_err(AgentError::PromptAssembly)?;
        let assembled_prompt_sha256 = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(assembled.system_prompt.as_bytes());
            format!("{:x}", h.finalize())
        };

        let user_msg = serialise_context_for_agent(ctx);
        let local_model = self.router.config().local_model.clone();

        let req = ChatRequest {
            model: local_model.clone(),
            messages: vec![
                ChatMessage::system(assembled.system_prompt),
                ChatMessage::user(user_msg),
            ],
            max_tokens: None,
            temperature: Some(0.0),
        };

        let start = std::time::Instant::now();
        let resp = self.router.send(&req).await?;
        let latency_ms = start.elapsed().as_millis() as u64;

        let raw = resp.choices.first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        // Tolerant of markdown-fenced JSON (```json … ```) and short
        // model preambles before the JSON body. See
        // `super::plan_parser::parse_plan_lenient` for the contract.
        let plan: Plan = parse_plan_lenient(&raw).map_err(|e| AgentError::Decode {
            detail: e.to_string(),
            raw: raw.clone(),
        })?;

        // recall_count is `usize` → `u32` via `as`; the cap_and_split
        // helper bounds the row count to L_RECALL_CAP_BYTES/min-body
        // size = at most ~4096 rows in the pathological 1-byte case,
        // so a u32 has 6 orders of magnitude of headroom. Sourced from
        // RecalledContext::len() (bodies.len()) — the same field the
        // assembler renders and the prompt-builder records as
        // `recalled_count` — so the audit row, the assembled bytes, and
        // the AssembledPrompt all agree.
        let recall_count = recalled.len() as u32;

        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: entry.sha256.clone(),
            llm_model: local_model,
            llm_backend: "local".to_string(),
            latency_ms,
            retry_count: 0,
            assembled_prompt_sha256,
            l0_count: assembled.l0_count,
            l1_count: assembled.l1_count,
            recalled_memory_ids: recalled.ids,
            recall_count,
            recall_query_sha256: recalled.query_sha256,
            // Slice F (entity-extraction v2, 2026-05-19): defaults
            // here; Task 14 wires the real extractor output in.
            graph_seed_entity_ids: Vec::new(),
            graph_seed_count: 0,
            graph_seed_source: crate::entity_extraction::SeedSource::None,
        };
        Ok((plan, meta))
    }
}

fn serialise_context_for_agent(ctx: &TaskContext) -> String {
    // Compact, deterministic shape. The agent reads this each
    // iteration and must produce the next Plan.
    serde_json::json!({
        "instruction": ctx.instruction,
        "classification_floor": ctx.classification_floor,
        "plans_so_far": ctx.plans_so_far_summary(),
        "advisories": ctx.advisories,
        "blocks":     ctx.blocks,
    }).to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn serialise_context_includes_instruction() {
        // Deferred until inner_loop::TaskContext is concrete (Task 2.4).
        // The pure-function test lives there; this module's only
        // surface is the trait + RouterAgent integration which is
        // exercised by scheduler_inner_loop_e2e.
    }
}
