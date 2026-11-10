//! Agent LLM adapter — sends one planner turn for a `TaskContext` through
//! the existing `kastellan_llm_router::Router` and returns the model's raw
//! reply with its cost ([`RouterAgent::send_turn`]). Parsing the reply, and
//! the recoveries when it is unusable (an empty completion, #923; a reply
//! that is not the expected shape, #945), are the notebook loop's
//! (`super::notebook::ask`).
//!
//! The trait `super::notebook::formulator::NotebookFormulator` is the door
//! the loop calls, so its tests swap in a scripted stub without an LLM.

use thiserror::Error;

use kastellan_llm_router::messages::{ChatMessage, ChatRequest};
use kastellan_llm_router::{Router, RouterError, ThinkingPolicy};

use super::inner_loop::{ceiling, TaskContext};
use super::prompts::PromptCache;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("router: {0}")]
    Router(#[from] RouterError),
    /// The model answered, but not with a plan. `usage` and `latency_ms`
    /// are that answer's cost, kept so the `plan.formulate_failed` row can
    /// say whether the tokens went to reasoning (an empty `raw` with
    /// `reasoning_chars > 0`) or to no reasoning the backend returned.
    #[error("plan decode failed: {detail}")]
    Decode {
        detail: String,
        raw: String,
        usage: kastellan_llm_router::CompletionStats,
        latency_ms: u64,
    },
    #[error("agent prompt (agent_notebook) not found in cache")]
    PromptMissing,
    /// L0/L1 load failed under the [`SystemPromptBuilder`]; the scheduler's
    /// retry policy decides whether to retry or fail permanently.
    #[error("prompt assembly: {0}")]
    PromptAssembly(#[from] crate::prompt_assembly::PromptAssemblyError),
}

impl AgentError {
    /// The model was reached but did not finish within the request budget
    /// (`KASTELLAN_LLM_TIMEOUT_MS`) — the one failure a cheaper retry can
    /// fix. A connect timeout is excluded: see
    /// [`RouterError::is_request_timeout`].
    pub fn is_request_timeout(&self) -> bool {
        matches!(self, AgentError::Router(e) if e.is_request_timeout())
    }

    /// The model returned no content, and not because it ran out of room
    /// (#923 saw `stop` with thinking off). Unlike a non-JSON answer this
    /// may be transient, so it is worth one plain retry. A blank `length`
    /// answer is excluded: the same request at temperature 0 runs out the
    /// same way.
    pub fn is_empty_completion(&self) -> bool {
        matches!(self, AgentError::Decode { raw, usage, .. }
            if raw.trim().is_empty() && usage.finish_reason.as_deref() != Some("length"))
    }

    /// The backend cut the reply off at `max_tokens` (`finish_reason:
    /// length`, #941), blank or mid-reply: it ran out of room the way a
    /// timeout runs out of time, and the same request runs out the same way.
    pub fn ran_out_of_tokens(&self) -> bool {
        matches!(self, AgentError::Decode { usage, .. } if usage.finish_reason.as_deref() == Some("length"))
    }

    /// The model's reply, when it is one the #945 re-ask can use: something
    /// was said (a blank answer is [`Self::is_empty_completion`]'s), and it
    /// was not cut off by `length` (asked again, it is cut off again).
    pub fn non_plan_answer(&self) -> Option<&str> {
        match self {
            AgentError::Decode { raw, usage, .. }
                if !raw.trim().is_empty() && usage.finish_reason.as_deref() != Some("length") =>
            {
                Some(raw)
            }
            _ => None,
        }
    }

    /// The cost and text of the completion behind this error, when one came
    /// back. Only [`AgentError::Decode`] has one: a router error (a timeout, an
    /// HTTP status, a body that is not a `ChatResponse`) carries none, not even
    /// its latency. Read by the failed-call audit row.
    pub fn completion(&self) -> Option<(&kastellan_llm_router::CompletionStats, u64, &str)> {
        match self {
            AgentError::Decode { usage, latency_ms, raw, .. } => Some((usage, *latency_ms, raw.as_str())),
            _ => None,
        }
    }
}

/// Production adapter: calls the real `Router::send`.
pub struct RouterAgent {
    pub(crate) router: std::sync::Arc<Router>,
    pub(crate) prompts: std::sync::Arc<PromptCache>,
    prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
    recall_builder: std::sync::Arc<dyn crate::recall_assembly::RecallBuilder>,
    entity_extractor: std::sync::Arc<dyn crate::entity_extraction::EntityExtractor>,
    /// The advertised tools, for the current step's tool doc (#922), set by
    /// `with_notebook`. `None` (a bare `new`, as tests build it): no tool docs.
    pub(crate) notebook_tools: Option<std::sync::Arc<[crate::prompt_assembly::AdvertisedTool]>>,
    /// The operator's sift knobs (#922), set by `with_notebook`.
    pub(crate) sift_cfg: super::notebook::sift::config::SiftConfig,
}

impl RouterAgent {
    pub fn new(
        router: std::sync::Arc<Router>,
        prompts: std::sync::Arc<PromptCache>,
        prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
        recall_builder: std::sync::Arc<dyn crate::recall_assembly::RecallBuilder>,
        entity_extractor: std::sync::Arc<dyn crate::entity_extraction::EntityExtractor>,
    ) -> Self {
        Self {
            router,
            prompts,
            prompt_builder,
            recall_builder,
            entity_extractor,
            notebook_tools: None,
            sift_cfg: Default::default(),
        }
    }
}

impl RouterAgent {
    /// One planner request: entity seeds, recall, the assembled system
    /// prompt for `prompt_name`, the user message `user_msg(now)` builds,
    /// then `prior` (a re-ask's two messages), sent with both #941 bounds.
    /// Returns the raw reply and its cost; the caller (the notebook loop's
    /// [`super::notebook::formulator::NotebookFormulator`] door) parses it.
    pub(crate) async fn send_turn(
        &self,
        prompt_name: &str,
        ctx: &TaskContext,
        user_msg: impl FnOnce(Option<&str>) -> String + Send,
        prior: Option<[ChatMessage; 2]>,
        thinking: ThinkingPolicy,
    ) -> Result<(String, FormulationMeta), AgentError> {
        let entry = self.prompts.get(prompt_name)
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

        let user_msg = user_msg(assembled.now.as_deref());
        let local_model = self.router.config().local_model.clone();

        let mut messages = vec![ChatMessage::system(assembled.system_prompt), ChatMessage::user(user_msg)];
        if let Some(prior) = prior {
            messages.extend(prior);
        }
        let req = ChatRequest {
            model: local_model.clone(),
            messages,
            // Both bounds are #941: one reply cannot generate without end,
            // and one call cannot outlive the fast lane's ceiling by more
            // than the floor.
            max_tokens: Some(ceiling::PLANNER_MAX_TOKENS),
            temperature: Some(0.0),
            // Left None so the router writes the thinking switch in its
            // configured dialect (#773). Whether to suppress is `thinking`:
            // the router's `disable_thinking` default on every turn except
            // the timed-out synthesis retry (#774) and the re-ask (#945).
            chat_template_kwargs: None,
            reasoning_effort: None,
            thinking,
            // The planner is read from its *text*, so it asks for no
            // distribution. Logprobs are for classifier calls that are
            // read from the token distribution instead (see
            // `llm_router::logprob_score`); requesting them here would
            // enlarge every planner response for nothing.
            logprobs: None,
            top_logprobs: None,
            timeout: Some(ceiling::call_timeout(self.router.timeout(), ctx.deadline, std::time::Instant::now())),
        };
        if req.timeout.is_some_and(|t| t < self.router.timeout()) {
            tracing::info!(
                task_id = ctx.task_id,
                timeout_ms = req.timeout.map_or(0, |t| t.as_millis() as u64),
                "planner call bounded by the task's time ceiling, not KASTELLAN_LLM_TIMEOUT_MS"
            );
        }

        let start = std::time::Instant::now();
        let resp = self.router.send(&req).await?;
        let latency_ms = start.elapsed().as_millis() as u64;

        let raw = resp.choices.first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();
        let usage = kastellan_llm_router::CompletionStats::from_response(&resp);

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
            prompt_name: prompt_name.into(),
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
            usage,
        };
        Ok((raw, meta))
    }
}

mod meta;
pub use meta::FormulationMeta;

#[cfg(test)]
mod tests;
