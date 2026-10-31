//! Unit tests for [`super`] — the planner adapter's context serialisation.
//!
//! Moved verbatim (de-indented one level) from the inline `mod tests`
//! block at the tail of `agent.rs`.

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
        resolved_asks: Vec::new(),
        origin: None,
        conversation: Vec::new(),
        conversation_task_ids: Some(Vec::new()),
    }
}

#[test]
fn serialise_context_includes_instruction() {
    let s = serialise_context_for_agent(&ctx(), false);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["instruction"], "what happened in Russia today?");
}

/// #922: the planner was never told its budget, so it spent one plan per
/// fetched message and learned of the cap only at forced synthesis.
#[test]
fn a_normal_turn_says_which_plan_this_is_and_how_many_there_are() {
    let mut c = ctx();
    c.plan_count = 2;
    let s = serialise_context_for_agent(&c, false);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["plan_budget"]["this_plan"], 3, "{s}");
    assert_eq!(v["plan_budget"]["max_plans"], 5, "{s}");
}

/// The prompt must tell the planner to spend that budget well (#922).
#[test]
fn the_planner_prompt_asks_for_batched_steps_attachments_and_coverage() {
    let prompt = include_str!("../../../../prompts/agent_planner.md");
    assert!(prompt.contains("`plan_budget`"), "must explain the budget key");
    assert!(prompt.contains("in ONE plan"), "must ask to batch independent steps");
    assert!(prompt.contains("mail.get_attachment_text"), "must say where figures are");
    assert!(prompt.contains("second search"), "must ask to check coverage before answering");
}

// ── #701: the conversation block ───────────────────────────

#[test]
fn the_conversation_key_is_absent_when_there_are_no_earlier_turns() {
    // A CLI task's serialised context must be byte-identical to what it
    // was before #701, so nothing about the non-channel path changes.
    let s = serialise_context_for_agent(&ctx(), false);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert!(v.get("conversation").is_none());
}

#[test]
fn the_conversation_key_carries_the_rendered_turns_in_order() {
    let mut c = ctx();
    c.conversation = vec![
        serde_json::json!({"at": "2026-09-14T14:02:11Z", "user": "older", "answer": "a1"}),
        serde_json::json!({"at": "2026-09-14T14:06:11Z", "user": "newer", "answer": "a2"}),
    ];
    let s = serialise_context_for_agent(&c, false);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["conversation"][0]["user"], "older");
    assert_eq!(v["conversation"][1]["user"], "newer");
}

#[test]
fn the_planner_prompt_documents_the_conversation_block() {
    // Drift guard, in the shape of #702's
    // `the_planner_prompt_documents_every_outcome_shape`: every key and
    // status the conversation renderer can emit must be named in the
    // prompt, or the planner is reading a shape nobody told it about.
    //
    // The statuses and `_omitted_*` markers come from the renderer's own
    // constants, NOT from literals copied here. A review found that the
    // hand-copied list had already gone stale: the renderer could emit a
    // "too large" status the prompt never mentioned, and the test could
    // not tell.
    //
    // The five KEY names below are still literals, because `view.rs`
    // spells them inline too and there is no constant to bind to — so this
    // guard's blind spot is exactly that set. Renaming a key in
    // `render_turn` without touching this list still passes. #711.
    use crate::scheduler::conversation::view::{
        OMITTED_CALLS_KEY, OMITTED_TURNS_KEY, STATUS_TOO_LARGE, STATUS_UNCLASSIFIED,
        STATUS_WITHHELD,
    };
    let prompt = include_str!("../../../../prompts/agent_planner.md");
    let mut expected: Vec<&str> = vec![
        "\"conversation\"", "\"at\"", "\"calls\"", "\"answer\"", "\"user\"",
    ];
    expected.extend([
        OMITTED_TURNS_KEY,
        OMITTED_CALLS_KEY,
        STATUS_WITHHELD,
        STATUS_TOO_LARGE,
        STATUS_UNCLASSIFIED,
    ]);
    for marker in expected {
        assert!(prompt.contains(marker), "agent_planner.md must document {marker}");
    }
}

/// Tool results carry UTC timestamps (`…+00:00`, localmail's wire form) while
/// the user lives in the `<now>` zone. DGX task 219 (2026-10-30) reported an
/// invoice that arrived at 09:58 Sydney time as "October 29" — the UTC date.
/// The prompt must tell the planner to convert before it states a date.
#[test]
fn the_planner_prompt_says_to_convert_utc_tool_times_to_the_now_zone() {
    let prompt = include_str!("../../../../prompts/agent_planner.md");
    assert!(prompt.contains("+00:00"), "must name the UTC form tool results use");
    assert!(
        prompt.contains("convert it to the `<now>` timezone"),
        "must say to convert tool times before stating a date"
    );
}

/// The failed-call row reads its cost through `completion()`. Pinned here
/// without Postgres: the row-level check in `timeout_tests` needs a cluster.
#[test]
fn only_a_decode_failure_carries_its_completion() {
    let usage = kastellan_llm_router::CompletionStats {
        completion_tokens: Some(48),
        ..Default::default()
    };
    let decode = AgentError::Decode { detail: "EOF".into(), raw: String::new(), usage, latency_ms: 18_976 };
    let (got, latency_ms) = decode.completion().expect("a decode failure has a completion");
    assert_eq!(got.completion_tokens, Some(48));
    assert_eq!(latency_ms, 18_976);
    assert!(AgentError::PromptMissing.completion().is_none());
}

/// #923: only an answer with nothing in it is worth a plain retry; a
/// non-JSON answer would most likely come back non-JSON again.
#[test]
fn only_a_blank_decode_failure_is_an_empty_completion() {
    let decode = |raw: &str| AgentError::Decode {
        detail: "d".into(),
        raw: raw.into(),
        usage: Default::default(),
        latency_ms: 0,
    };
    assert!(decode("").is_empty_completion());
    assert!(decode(" \n\t").is_empty_completion());
    assert!(!decode("Sure! Here is the plan").is_empty_completion());
    assert!(!decode("{").is_empty_completion());
    assert!(!AgentError::PromptMissing.is_empty_completion());
    // Out of room is not transient: the same request runs out again.
    let out_of_room = |finish_reason: Option<&str>| AgentError::Decode {
        detail: "d".into(),
        raw: String::new(),
        usage: kastellan_llm_router::CompletionStats {
            finish_reason: finish_reason.map(Into::into),
            ..Default::default()
        },
        latency_ms: 0,
    };
    assert!(!out_of_room(Some("length")).is_empty_completion());
    assert!(out_of_room(Some("stop")).is_empty_completion());
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
