//! The messages of a planning turn: the user message carrying the task's
//! state as one compact JSON object (moved verbatim out of `agent.rs`, over
//! the 500-line cap), and the two the #945 re-ask appends. Pure.

use kastellan_llm_router::messages::ChatMessage;

use super::SYNTHESIS_DIRECTIVE;
use crate::scheduler::inner_loop::TaskContext;

/// Sent after a reply that was not a plan (#945). On the DGX the planner's
/// last turn was, two times in three on one question, a correct final answer
/// written as markdown instead of the JSON plan carrying it; this asks for
/// that answer again in the shape the loop can use.
pub(super) const REFORMAT_DIRECTIVE: &str = "Your previous reply was not a JSON \
plan, so it could not be used. Reply again with ONLY the JSON plan object in the \
schema from the system prompt: no prose, no markdown, no code fence. If that reply \
was your final answer, put it in a terminal plan: `decision` exactly \
\"task_complete\", `steps` [], and `result.body` = that answer. Otherwise emit the \
plan for your next step.";

/// What the re-ask appends to the failed turn's messages: the model's own
/// reply, then [`REFORMAT_DIRECTIVE`]. Appending (never rewriting) keeps the
/// request before them identical to the failed turn's, so a backend's prompt
/// cache serves it.
pub(super) fn reformat_messages(reply: &str) -> [ChatMessage; 2] {
    [ChatMessage::assistant(reply), ChatMessage::user(REFORMAT_DIRECTIVE)]
}

pub(super) fn serialise_context_for_agent(ctx: &TaskContext, synthesize: bool, now: Option<&str>) -> String {
    // Compact, deterministic shape. The agent reads this each iteration and
    // must produce the next Plan.
    //
    // **Key order is the prompt cache's order** (it follows the system prompt,
    // see `prompt_assembly::assemble`): what is fixed for the whole task first,
    // then the append-only history, then the small parts that change, the
    // per-turn budget, and the minute-precise `now` last — anywhere earlier, a
    // new minute would make the backend re-read the history after it. A
    // `serde_json::Map` would sort the keys
    // alphabetically (the workspace does not enable `preserve_order`), which put
    // the per-turn `plan_budget` in front of `plans_so_far` and made every turn
    // re-read the whole history.
    let mut fields: Vec<(&str, serde_json::Value)> = vec![
        ("instruction", serde_json::json!(ctx.instruction)),
        ("classification_floor", serde_json::json!(ctx.classification_floor)),
    ];
    // #701: the earlier turns of this conversation, oldest first — already
    // rendered, screened and budgeted by `scheduler::conversation::view` when
    // the task was claimed. Added only when non-empty, so a CLI task carries no
    // `conversation` key at all.
    if !ctx.conversation.is_empty() {
        fields.push(("conversation", serde_json::Value::Array(ctx.conversation.clone())));
    }
    fields.push(("plans_so_far", serde_json::json!(ctx.plans_so_far_summary())));
    fields.push(("advisories", serde_json::json!(ctx.advisories)));
    fields.push(("blocks", serde_json::json!(ctx.blocks)));
    if synthesize {
        // Forced-synthesis turn: the directive telling the agent to stop
        // gathering and answer from what it already has. Only ever set on the
        // single fallback turn the inner loop spends at the plan cap.
        fields.push(("directive", serde_json::Value::String(SYNTHESIS_DIRECTIVE.to_string())));
    } else {
        // #922: without this the planner learns of the cap only at forced
        // synthesis, and spends one plan per fetched message on the way there.
        let mut budget = serde_json::json!({
            "this_plan": ctx.plan_count.saturating_add(1),
            "max_plans": ctx.max_plans,
        });
        // The fast lane's ceiling (#922 part A); the long lane has none.
        if let Some(s) = crate::scheduler::inner_loop::ceiling::seconds_left(ctx.deadline, std::time::Instant::now()) {
            budget["seconds_left"] = serde_json::json!(s);
        }
        fields.push(("plan_budget", budget));
    }
    if let Some(now) = now {
        fields.push(("now", serde_json::json!(now)));
    }
    ordered_json_object(&fields)
}

/// A compact JSON object whose keys appear exactly in `fields`' order.
fn ordered_json_object(fields: &[(&str, serde_json::Value)]) -> String {
    let body: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{}:{}", serde_json::Value::from(*k), v))
        .collect();
    format!("{{{}}}", body.join(","))
}
