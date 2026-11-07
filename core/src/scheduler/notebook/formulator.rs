//! The notebook loop's door to the model (#922).
//!
//! One method: send one notebook turn and return the raw reply. Parsing and
//! checking it is the loop's job ([`super::reply`]), so a scripted test
//! double only has to return strings.

use async_trait::async_trait;
use kastellan_llm_router::messages::ChatMessage;
use kastellan_llm_router::ThinkingPolicy;

use super::sift::config::SiftConfig;
use super::sift::sorter::Sorter;
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

/// Read `KASTELLAN_PLANNER_MODE`. Unset, empty or `notebook` → notebook (the
/// default since #922 slice 4); `classic` → classic, kept opt-in until the
/// classic loop retires; anything else is an error naming the key, so the
/// daemon refuses to start rather than guess which loop was meant.
pub fn planner_mode(value: Option<&str>) -> Result<PlannerMode, String> {
    match value.map(str::trim) {
        None | Some("") | Some("notebook") => Ok(PlannerMode::Notebook),
        Some("classic") => Ok(PlannerMode::Classic),
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

    /// Does a turn that asks nothing of thinking (`RouterDefault`) think?
    /// When it does not, an answer such a turn writes is rewritten by a
    /// thinking turn (`thinking::rewrite_answer`).
    fn thinks_by_default(&self) -> bool {
        true
    }

    /// The per-item decision a `sift` uses (#922); `None`: this formulator
    /// cannot sift, and a `sift` call is refused.
    fn sorter(&self) -> Option<&dyn Sorter> {
        None
    }

    /// The operator's sift knobs (`KASTELLAN_SIFT_*`), as read at boot.
    fn sift_config(&self) -> SiftConfig {
        SiftConfig::default()
    }
}

impl RouterAgent {
    /// Run every task on the notebook loop (#922), showing each step the doc
    /// of its tool from `tools`, and sifting with `sift` (the operator's
    /// `KASTELLAN_SIFT_*` knobs, [`super::sift::config::sift_config`]).
    pub fn with_notebook(mut self, tools: std::sync::Arc<[crate::prompt_assembly::AdvertisedTool]>, sift: SiftConfig) -> Self {
        self.notebook_tools = Some(tools);
        self.sift_cfg = sift;
        self
    }

    /// The doc of the advertised tool named `tool`, in notebook mode.
    fn notebook_tool_doc(&self, tool: &str) -> Option<String> {
        let tools = self.notebook_tools.as_ref()?;
        tools.iter().find(|t| t.doc.name == tool).map(crate::prompt_assembly::assemble::render_tool_doc)
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
        self.send_turn(NOTEBOOK_PROMPT, ctx, user_msg, prior, thinking).await
    }

    fn tool_doc(&self, tool: &str) -> Option<String> {
        self.notebook_tool_doc(tool)
    }

    fn thinks_by_default(&self) -> bool {
        !self.router.config().disable_thinking
    }

    fn sorter(&self) -> Option<&dyn Sorter> {
        Some(self)
    }

    fn sift_config(&self) -> SiftConfig {
        self.sift_cfg.clone()
    }
}

#[cfg(test)]
mod tests;
