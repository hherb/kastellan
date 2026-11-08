//! A scripted notebook planner for in-process scheduler e2es (#993).
//!
//! The daemon e2es script the model over HTTP ([`crate::scripted_llm`]).
//! The in-process suites (`scheduler_ask_path_e2e`, `scheduler_lanes_e2e`,
//! …) hand `spawn_scheduler` a formulator instead. [`ScriptedNotebook`] is
//! that formulator for the notebook loop: each turn it asks a closure for the
//! reply, given the task's context, and returns it as the model's text.
//!
//! Choosing the reply from the context (usually `ctx.plans.len()`, the number
//! of calls the task has run) rather than from a call counter is what lets a
//! resumed task walk forward from where it stopped: a resume restores
//! `ctx.plans`, so the turn after an approval sees the same index the
//! suspended turn did and re-derives the same call (the same plan digest).
//!
//! The reply builders below spell the notebook's reply keys
//! (`prompts/agent_notebook.md`); the loop's parser refuses unknown keys.

use std::sync::Mutex;

use async_trait::async_trait;
use kastellan_core::cassandra::types::{Plan, PlannedStep};
use kastellan_core::scheduler::agent::{AgentError, FormulationMeta, PlanFormulator};
use kastellan_core::scheduler::inner_loop::TaskContext;
use kastellan_core::scheduler::notebook::formulator::{NotebookFormulator, UserMessage};
use kastellan_llm_router::ThinkingPolicy;
use serde_json::{json, Value};

type Pick = dyn Fn(&TaskContext) -> String + Send + Sync;

/// A notebook formulator whose every reply comes from `pick(ctx)`.
///
/// It thinks by default (`thinks_by_default` is `true`), so an answer is the
/// task's answer as written — no thinking rewrite turn to script around.
/// [`Self::not_thinking`] is the router's default backend instead: an answer
/// on a routine turn is discarded and written again on a thinking turn.
pub struct ScriptedNotebook {
    pick: Box<Pick>,
    thinks: bool,
    /// The rendered user message of every turn, oldest first.
    pub messages: Mutex<Vec<String>>,
}

impl ScriptedNotebook {
    /// Each turn replies `pick(ctx)`.
    pub fn new(pick: impl Fn(&TaskContext) -> String + Send + Sync + 'static) -> Self {
        Self { pick: Box::new(pick), thinks: true, messages: Mutex::new(Vec::new()) }
    }

    /// As a backend that does not think by default (`disable_thinking`).
    pub fn not_thinking(mut self) -> Self {
        self.thinks = false;
        self
    }

    /// The rendered user message of every turn so far.
    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

#[async_trait]
impl NotebookFormulator for ScriptedNotebook {
    async fn notebook_turn(
        &self,
        ctx: &TaskContext,
        user_msg: &UserMessage<'_>,
        _prior: Option<(&str, &str)>,
        _thinking: ThinkingPolicy,
    ) -> Result<(String, FormulationMeta), AgentError> {
        self.messages.lock().unwrap().push(user_msg(None));
        Ok(((self.pick)(ctx), notebook_meta()))
    }

    fn thinks_by_default(&self) -> bool {
        self.thinks
    }
}

/// The scheduler takes a [`PlanFormulator`]; the notebook is behind its
/// `notebook()` door, and the runner never calls the classic doors on a
/// formulator that has one.
#[async_trait]
impl PlanFormulator for ScriptedNotebook {
    async fn formulate_plan(&self, _ctx: &TaskContext) -> Result<(Plan, FormulationMeta), AgentError> {
        unreachable!("a notebook formulator is never asked for a classic plan")
    }

    fn notebook(&self) -> Option<&dyn NotebookFormulator> {
        Some(self)
    }
}

/// A well-formed `FormulationMeta` for the `plan.formulate` audit row; nothing
/// under test reads its values.
pub fn notebook_meta() -> FormulationMeta {
    FormulationMeta {
        prompt_name: "agent_notebook".into(),
        prompt_sha256: "test".into(),
        llm_model: "test-model".into(),
        llm_backend: "local".into(),
        latency_ms: 1,
        retry_count: 0,
        assembled_prompt_sha256: "0".repeat(64),
        l0_count: 0,
        l1_count: 0,
        skill_count: 0,
        recalled_memory_ids: Vec::new(),
        recall_count: 0,
        recall_query_sha256: "0".repeat(64),
        graph_seed_entity_ids: Vec::new(),
        graph_seed_count: 0,
        graph_seed_source: kastellan_core::entity_extraction::SeedSource::None,
        usage: Default::default(),
    }
}

/// A reply that lists step `id` alone and calls `step`'s tool and method for
/// it with `step`'s parameters, classified as `step` is.
///
/// Listing the step on every call is a replan that replaces whatever step is
/// still pending. A call reply carries no tick, so its step stays pending and
/// the next call's list replaces it: a script never has to tick. The one rule
/// is the loop's: a call on a step that was ticked (done) is refused.
pub fn call_reply(id: u32, step: &PlannedStep) -> String {
    json!({
        "steps": [{"id": id, "purpose": "test step", "tool": step.tool, "done_when": step.done_when}],
        "call": {
            "step": id,
            "tool": step.tool,
            "method": step.method,
            "parameters": step.parameters,
            "returns": step.returns,
            "done_when": step.done_when,
            "classification": step.classification,
        },
    })
    .to_string()
}

/// A reply that ends the task with `body` as its answer.
///
/// It lists a step too, because the notebook refuses a first reply with no
/// `steps`, and a closure answering on the first turn would otherwise fail
/// the task in words. On a later turn the list is harmless: a done step stays
/// done, and a pending one is replaced by a step the answer then leaves open.
pub fn answer_reply(body: &str) -> String {
    json!({"steps": [answer_step()], "answer": {"body": body}}).to_string()
}

/// The step [`answer_reply`] lists. Its id is past any [`call_reply`] in these
/// suites, so it never re-lists (and so reopens) a failed call's step.
fn answer_step() -> Value {
    json!({"id": 999, "purpose": "answer", "tool": "none", "done_when": "answered"})
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_core::cassandra::types::DataClass;

    #[test]
    fn a_call_reply_lists_its_step_and_calls_it() {
        let step = PlannedStep {
            tool: "sleep".into(),
            method: "doit".into(),
            parameters: json!({"n": 2}),
            returns: "x".into(),
            done_when: "y".into(),
            classification: DataClass::Personal,
        };
        let v: Value = serde_json::from_str(&call_reply(7, &step)).unwrap();
        assert_eq!(v["steps"][0]["id"], 7);
        assert_eq!(v["call"]["step"], 7);
        assert_eq!(v["call"]["parameters"], json!({"n": 2}));
        assert_eq!(v["call"]["classification"], "Personal");
    }

    #[test]
    fn an_answer_reply_lists_a_step_so_it_can_be_the_first_reply() {
        let v: Value = serde_json::from_str(&answer_reply("b")).unwrap();
        assert_eq!(v["answer"], json!({"body": "b"}));
        assert_eq!(v["steps"].as_array().map(Vec::len), Some(1));
    }
}
