//! Agent LLM adapter — produces a `Plan` from a `TaskContext` via
//! the existing `kastellan_llm_router::Router`. Strict JSON parsing on
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
use kastellan_llm_router::messages::{ChatMessage, ChatRequest};
use kastellan_llm_router::{Router, RouterError};

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

    /// Forced-synthesis variant of [`formulate_plan`]: the same prompt
    /// assembly, but the agent is instructed to STOP gathering and emit a
    /// terminal `task_complete` answer synthesized from the observations
    /// already in `ctx.plans_so_far`. The inner loop calls this for the
    /// single fallback turn it spends when the plan-iteration cap is reached
    /// with observations in hand (see [`super::inner_loop::run_to_terminal`]).
    ///
    /// The default delegates to `formulate_plan` so scripted test doubles —
    /// which return plans by call order and ignore `ctx` — need no special
    /// handling; the production [`RouterAgent`] overrides it to inject the
    /// synthesis directive into the user message.
    async fn formulate_synthesis(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        self.formulate_plan(ctx).await
    }
}

/// Appended to the agent's user message on the forced-synthesis turn. Tells
/// the model to answer from what it already gathered rather than issue
/// another tool call — the last chance to produce a best-effort answer
/// before the inner loop fails at the plan-iteration cap.
const SYNTHESIS_DIRECTIVE: &str = "You have reached your tool-step budget for \
this task. Do NOT plan any more tool steps. Using ONLY the observations \
already gathered in `plans_so_far`, emit a terminal plan now: `decision` \
exactly \"task_complete\", `steps` [], and `result.body` = your best-effort \
final answer synthesized from what you already have. If the gathered \
information is incomplete, still emit task_complete and give the best answer \
you can, briefly noting what remains uncertain. Do not issue another search \
or tool call.";

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
    /// Number of L3 skill rows surfaced into the `<skills>` block. Stays
    /// 0 in production until an operator approves a crystallised skill.
    pub skill_count: usize,
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
    entity_extractor: std::sync::Arc<dyn crate::entity_extraction::EntityExtractor>,
}

impl RouterAgent {
    pub fn new(
        router: std::sync::Arc<Router>,
        prompts: std::sync::Arc<PromptCache>,
        prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
        recall_builder: std::sync::Arc<dyn crate::recall_assembly::RecallBuilder>,
        entity_extractor: std::sync::Arc<dyn crate::entity_extraction::EntityExtractor>,
    ) -> Self {
        Self { router, prompts, prompt_builder, recall_builder, entity_extractor }
    }
}

#[async_trait]
impl PlanFormulator for RouterAgent {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        self.formulate_inner(ctx, false).await
    }

    async fn formulate_synthesis(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        self.formulate_inner(ctx, true).await
    }
}

impl RouterAgent {
    /// Shared assembly for both [`PlanFormulator::formulate_plan`] and
    /// [`PlanFormulator::formulate_synthesis`]. `synthesize` selects whether
    /// the [`SYNTHESIS_DIRECTIVE`] is appended to the user message — the only
    /// difference between a normal planning turn and the forced-synthesis
    /// turn (identical system prompt, recall, entity seeds, and audit meta).
    async fn formulate_inner(
        &self,
        ctx: &TaskContext,
        synthesize: bool,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let entry = self.prompts.get("agent_planner")
            .ok_or(AgentError::PromptMissing)?;

        let base = entry.content.clone();

        // Entity extraction. Degrade-and-warn on failure.
        let seeds = match self.entity_extractor.extract(&ctx.instruction).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "kastellan::scheduler::agent",
                    error = %e,
                    "entity extraction failed; continuing with empty seeds",
                );
                crate::entity_extraction::EntitySeeds::empty()
            }
        };

        // Per-iteration recall, now seeded. Asymmetric posture vs the
        // prompt assembler below: recall failure DEGRADES (we still want
        // the model to plan with L0/L1/base even if retrieval is
        // broken), while prompt-assembly failure is FAIL-CLOSED (a
        // degraded safety prompt would have the agent flying blind on
        // operator rules). See spec "Failure-mode matrix".
        let recalled = match self.recall_builder
            .build_with_seeds(&ctx.instruction, &seeds.ids).await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "kastellan::scheduler::agent",
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

        let user_msg = serialise_context_for_agent(ctx, synthesize);
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

        // seeds.ids needs to be moved/cloned into the struct; capture
        // count + source first so we can read them after the move.
        let graph_seed_count = seeds.ids.len() as u32;
        let graph_seed_source = seeds.source;

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
            skill_count: assembled.skill_count,
            recalled_memory_ids: recalled.ids,
            recall_count,
            recall_query_sha256: recalled.query_sha256,
            graph_seed_entity_ids: seeds.ids,
            graph_seed_count,
            graph_seed_source,
        };
        Ok((plan, meta))
    }
}

fn serialise_context_for_agent(ctx: &TaskContext, synthesize: bool) -> String {
    // Compact, deterministic shape. The agent reads this each
    // iteration and must produce the next Plan.
    let mut obj = serde_json::json!({
        "instruction": ctx.instruction,
        "classification_floor": ctx.classification_floor,
        "plans_so_far": ctx.plans_so_far_summary(),
        "advisories": ctx.advisories,
        "blocks":     ctx.blocks,
    });
    // Forced-synthesis turn: append the directive telling the agent to stop
    // gathering and answer from what it already has. Only ever set on the
    // single fallback turn the inner loop spends at the plan cap.
    if synthesize {
        if let Some(map) = obj.as_object_mut() {
            map.insert(
                "directive".to_string(),
                serde_json::Value::String(SYNTHESIS_DIRECTIVE.to_string()),
            );
        }
    }
    obj.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::DataClass;
    use super::super::inner_loop::ClassificationFloorSource;

    fn ctx() -> TaskContext {
        TaskContext {
            task_id: 1,
            lane: kastellan_db::tasks::Lane::Fast,
            instruction: "what happened in Russia today?".into(),
            classification_floor: DataClass::Public,
            classification_floor_source: ClassificationFloorSource::Default,
            classification_floor_signals: vec![],
            plans: vec![],
            advisories: vec![],
            blocks: vec![],
            plan_count: 0,
            max_plans: 5,
        }
    }

    #[test]
    fn serialise_context_includes_instruction() {
        let s = serialise_context_for_agent(&ctx(), false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["instruction"], "what happened in Russia today?");
    }

    #[test]
    fn serialise_context_omits_directive_on_a_normal_turn() {
        let s = serialise_context_for_agent(&ctx(), false);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v.get("directive").is_none(), "normal turn must carry no directive");
    }

    #[test]
    fn serialise_context_appends_synthesis_directive_when_flagged() {
        let s = serialise_context_for_agent(&ctx(), true);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let directive = v["directive"].as_str().expect("directive present on synth turn");
        assert!(directive.contains("task_complete"), "directive must steer to task_complete");
        assert!(
            directive.contains("Do not issue another search"),
            "directive must forbid another tool call",
        );
        // The instruction + gathered context still ride alongside the directive.
        assert_eq!(v["instruction"], "what happened in Russia today?");
    }
}
