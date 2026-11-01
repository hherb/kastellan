//! The user message of a planning turn: the task's state as one compact JSON
//! object. Pure; moved verbatim out of `agent.rs` (over the 500-line cap).

use super::SYNTHESIS_DIRECTIVE;
use crate::scheduler::inner_loop::TaskContext;

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
