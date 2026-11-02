//! The notebook loop's door to the model (#922).
//!
//! One method: send one notebook turn and return the raw reply. Parsing and
//! checking it is the loop's job ([`super::reply`]), so a scripted test
//! double only has to return strings.

use async_trait::async_trait;
use kastellan_llm_router::messages::ChatMessage;
use kastellan_llm_router::ThinkingPolicy;

use crate::scheduler::agent::{AgentError, FormulationMeta, RouterAgent};
use crate::scheduler::inner_loop::TaskContext;

/// The prompt file (`prompts/agent_notebook.md`) the notebook loop runs on.
pub const NOTEBOOK_PROMPT: &str = "agent_notebook";

/// Sent after a reply that was not a usable notebook reply (#945's re-ask,
/// for this loop's shape): the reason it was refused, then a request for the
/// reply again.
pub(crate) fn reformat_directive(reason: &str) -> String {
    format!(
        "Your previous reply could not be used: {reason}. Reply again with ONLY one JSON \
object in the shape from the system prompt (`tick`, `steps`, and exactly one of `call`, \
`answer` or `infeasible`): no prose, no markdown, no code fence. If that reply was your \
final answer, put it in `answer.body`."
    )
}

/// Which loop the daemon plans with (`KASTELLAN_PLANNER_MODE`, C4): one
/// setting per daemon, every lane and channel task alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerMode {
    /// The classic planner: one growing plan history per task.
    Classic,
    /// The notebook loop (#922).
    Notebook,
}

/// Read `KASTELLAN_PLANNER_MODE`. Unset, empty or `classic` → classic;
/// `notebook` → notebook; anything else is an error naming the key, so the
/// daemon refuses to start rather than guess which loop was meant.
pub fn planner_mode(value: Option<&str>) -> Result<PlannerMode, String> {
    match value.map(str::trim) {
        None | Some("") | Some("classic") => Ok(PlannerMode::Classic),
        Some("notebook") => Ok(PlannerMode::Notebook),
        Some(other) => Err(format!(
            "KASTELLAN_PLANNER_MODE={other:?} is not a planner mode (expected `classic` or `notebook`)"
        )),
    }
}

/// Renders a turn's user message given `now`.
pub type UserMessage<'a> = dyn Fn(Option<&str>) -> String + Send + Sync + 'a;

#[async_trait]
pub trait NotebookFormulator: Send + Sync {
    /// One notebook turn: the raw reply and its cost. `user_msg(now)`
    /// renders the turn's message once `now` (the operator's clock, from the
    /// prompt assembly) is known. `prior` is the #945 re-ask: the reply that
    /// could not be used, and why.
    async fn notebook_turn(
        &self,
        ctx: &TaskContext,
        user_msg: &UserMessage<'_>,
        prior: Option<(&str, &str)>,
        thinking: ThinkingPolicy,
    ) -> Result<(String, FormulationMeta), AgentError>;

    /// The full doc of the advertised tool named `tool`, for the current
    /// step; `None` when it is not advertised.
    fn tool_doc(&self, _tool: &str) -> Option<String> {
        None
    }
}

#[async_trait]
impl NotebookFormulator for RouterAgent {
    async fn notebook_turn(
        &self,
        ctx: &TaskContext,
        user_msg: &UserMessage<'_>,
        prior: Option<(&str, &str)>,
        thinking: ThinkingPolicy,
    ) -> Result<(String, FormulationMeta), AgentError> {
        let prior = prior.map(|(reply, reason)| {
            [ChatMessage::assistant(reply), ChatMessage::user(reformat_directive(reason))]
        });
        self.send_turn(NOTEBOOK_PROMPT, ctx, |now| user_msg(now), prior, thinking).await
    }

    fn tool_doc(&self, tool: &str) -> Option<String> {
        self.notebook_tool_doc(tool)
    }
}

#[cfg(test)]
mod tests;
