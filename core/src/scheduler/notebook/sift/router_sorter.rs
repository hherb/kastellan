//! The sorter on the local LLM (#922, S3, K5): a fixed system prompt
//! (`prompts/sift_sorter.md`), the item as the user message, thinking off,
//! a few output tokens. No recall and no prompt assembly: the sorter sees
//! the criterion and the item, nothing else.

use async_trait::async_trait;
use kastellan_llm_router::messages::{ChatMessage, ChatRequest};
use kastellan_llm_router::ThinkingPolicy;

use super::sorter::Sorter;
use crate::scheduler::agent::{AgentError, RouterAgent};
use crate::scheduler::inner_loop::{ceiling, TaskContext};

/// The sorter's prompt file (`prompts/sift_sorter.md`).
pub const SORTER_PROMPT: &str = "sift_sorter";

/// A reply is `{"keep": false}` or up to [`super::params::MAX_FIELDS`]
/// short fields: room for all of them, since a reply cut off mid-object
/// does not parse and its item is lost as undecided.
pub(crate) const SORTER_MAX_TOKENS: u32 = 1024;

#[async_trait]
impl Sorter for RouterAgent {
    async fn sort(&self, ctx: &TaskContext, user_msg: String) -> Result<String, AgentError> {
        let prompt = self.prompts.get(SORTER_PROMPT).ok_or_else(|| AgentError::PromptMissing(SORTER_PROMPT.into()))?;
        let req = ChatRequest {
            model: self.router.config().local_model.clone(),
            messages: vec![ChatMessage::system(prompt.content.clone()), ChatMessage::user(user_msg)],
            max_tokens: Some(SORTER_MAX_TOKENS),
            temperature: Some(0.0),
            // The router writes the switch in its configured dialect (#773).
            chat_template_kwargs: None,
            reasoning_effort: None,
            thinking: ThinkingPolicy::Suppress,
            logprobs: None,
            top_logprobs: None,
            timeout: Some(ceiling::call_timeout(self.router.timeout(), ctx.deadline, std::time::Instant::now())),
        };
        let resp = self.router.send(&req).await?;
        let Some(choice) = resp.choices.first() else {
            tracing::warn!(task_id = ctx.task_id, "sift: the sorter returned no choice; undecided");
            return Ok(String::new());
        };
        // A cut-off reply does not parse and becomes undecided: say why.
        if choice.finish_reason.as_deref() == Some("length") {
            tracing::warn!(task_id = ctx.task_id, max_tokens = SORTER_MAX_TOKENS, "sift: a sorter reply hit max_tokens");
        }
        Ok(choice.message.content.clone())
    }
}
