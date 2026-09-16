//! The planner's view of earlier turns (#701).
//!
//! Pure and deterministic in `(turns, budget)`. Three jobs, in this order:
//!
//! 1. **Render** each turn as `{at, user, calls?, answer}`.
//! 2. **Screen** every rendered turn — keys and string leaves, through the
//!    same [`result_view::screen_text`] the #702 step views use — with the
//!    `Strict` profile `channel::ingest` applies to an inbound body. A blocked
//!    turn becomes `{at, status: "withheld"}` and reports a
//!    [`ConversationBlock`] for the forensic row. **Withheld, not dropped:**
//!    the planner must know a turn existed, or it will re-derive it.
//! 3. **Fit the budget**, dropping whole turns oldest first and marking how
//!    many, then clamping the newest turn if it alone is still too large.
//!
//! Screening happens here, at load time, over the stored inputs — never over a
//! stored render. A render written by one version of the screen and read by
//! another would have been screened by neither. Same rule as `PlanRecord`.
//!
//! Every candidate is **measured** before it is returned, so the size bound
//! does not rest on an argument about how the caps interact (#702's rule).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::Turn;
use crate::cassandra::injection_guard::{screen_with_profile, GuardProfile, InjectionDecision};
use crate::scheduler::inner_loop::result_view;

/// Most turns of a conversation the planner is shown.
pub(crate) const MAX_TURNS: i64 = 3;

/// How far back a turn may have finished and still count as part of this
/// conversation. Five hours: a follow-up is nearly always minutes later, and a
/// window this size keeps yesterday's topic from steering today's question
/// without needing an explicit reset command.
pub(crate) const WINDOW_HOURS: i64 = 5;

/// Total serialised bytes the conversation block may occupy, beside the
/// existing 96 KiB `PLANS_SUMMARY_BUDGET`. Affordable because DGX plan latency
/// is bound by generation, not context.
pub(crate) const CONVERSATION_BUDGET: usize = 16 * 1024;

/// Most bytes of one earlier user message.
const USER_TEXT_CAP: usize = 2 * 1024;

/// Most bytes of one earlier answer.
const ANSWER_CAP: usize = 4 * 1024;

/// Marker element carrying how many older turns the budget dropped.
pub(crate) const OMITTED_TURNS_KEY: &str = "_omitted_turns";

/// The `tier` of the `policy / injection.blocked` row written for a block this
/// screen made, beside `catalogue`, `guard_model` and `sink`.
pub(crate) const TIER_CONVERSATION: &str = "conversation";

/// What replaces a turn the screen blocked.
const STATUS_WITHHELD: &str = "withheld";

/// What replaces a turn that cannot be made to fit the budget at all.
const STATUS_TOO_LARGE: &str = "too large for the conversation budget";

const _: () = {
    // `result_view::render` guarantees its size bound only from this value up,
    // and the clamping candidates below divide the budget by 4. Module level,
    // not `#[cfg(test)]`, which release builds strip.
    assert!(CONVERSATION_BUDGET / 4 >= result_view::MIN_VIEW_TOTAL);
};

/// What the conversation screen recorded about a turn it blocked: enough for
/// the forensic row, and never the screened text.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConversationBlock {
    pub(crate) task_id: i64,
    pub(crate) score: f32,
    pub(crate) reason_codes: Vec<&'static str>,
    pub(crate) body_sha256: String,
    pub(crate) body_byte_len: usize,
}

/// The rendered conversation plus whatever the screen blocked.
#[derive(Clone, Debug, Default)]
pub(crate) struct RenderedConversation {
    pub(crate) turns: Vec<Value>,
    pub(crate) blocks: Vec<ConversationBlock>,
}

/// Render `turns` (oldest first) within `budget`.
pub(crate) fn render(turns: &[Turn], budget: usize) -> RenderedConversation {
    let mut out = RenderedConversation::default();
    if turns.is_empty() {
        return out;
    }

    let mut rendered: Vec<(usize, Value)> = Vec::with_capacity(turns.len());
    for (i, turn) in turns.iter().enumerate() {
        let full = render_turn(turn, USER_TEXT_CAP, ANSWER_CAP, true);
        rendered.push((i, admit(turn, full, &mut out.blocks)));
    }

    // Drop whole turns, oldest first, until the array fits. Never the last
    // one: a conversation that renders as nothing tells the planner less than
    // a clamped turn does.
    let mut omitted = 0usize;
    while rendered.len() > 1 && total_len(&rendered) > budget {
        rendered.remove(0);
        omitted += 1;
    }

    // One turn left and still over budget: clamp it, cheapest loss first.
    if let Some((i, value)) = rendered.first_mut() {
        if result_view::serialised_len(value) > budget {
            let turn = &turns[*i];
            for candidate in [
                render_turn(turn, USER_TEXT_CAP, ANSWER_CAP, false),
                render_turn(turn, budget / 4, budget / 4, false),
                json!({"at": at(turn), "status": STATUS_TOO_LARGE}),
            ] {
                let candidate = admit(turn, candidate, &mut out.blocks);
                let fits = result_view::serialised_len(&candidate) <= budget;
                *value = candidate;
                if fits {
                    break;
                }
            }
        }
    }

    out.turns = rendered.into_iter().map(|(_, v)| v).collect();
    if omitted > 0 {
        out.turns.insert(0, json!({ OMITTED_TURNS_KEY: omitted }));
    }
    out
}

/// `value` if the screen passes it, else the withheld shape, recording the
/// block. The single point every rendered turn passes through, so no candidate
/// can reach the prompt unscreened by taking a different path.
fn admit(turn: &Turn, value: Value, blocks: &mut Vec<ConversationBlock>) -> Value {
    match screen(turn, &value) {
        None => value,
        Some(block) => {
            let withheld = json!({"at": at(turn), "status": STATUS_WITHHELD});
            blocks.push(block);
            withheld
        }
    }
}

/// One turn as `{at, user, calls?, answer}`, its text clamped through
/// [`result_view::render`] — which keeps identifiers whole and marks what it
/// cut — rather than a sixth hand-written truncation walk (#591).
fn render_turn(turn: &Turn, user_cap: usize, answer_cap: usize, with_calls: bool) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("at".into(), Value::String(at(turn)));
    obj.insert("user".into(), clamp_text(&turn.user, user_cap));
    if with_calls {
        if let Some(record) = turn.record.as_ref() {
            if let Ok(calls) = serde_json::to_value(&record.calls) {
                obj.insert("calls".into(), calls);
            }
        }
    }
    obj.insert("answer".into(), clamp_text(&turn.answer, answer_cap));
    Value::Object(obj)
}

/// `s` as a JSON value cut to `cap` serialised bytes.
fn clamp_text(s: &str, cap: usize) -> Value {
    result_view::render(
        &Value::String(s.to_string()),
        cap.max(result_view::MIN_VIEW_TOTAL),
    )
}

/// RFC 3339 timestamp of the turn, to the second.
fn at(turn: &Turn) -> String {
    turn.finished_at
        .replace_millisecond(0)
        .unwrap_or(turn.finished_at)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::from("unknown"))
}

/// `Some(block)` when this turn must be withheld, `None` when it may be shown.
fn screen(turn: &Turn, value: &Value) -> Option<ConversationBlock> {
    let text = result_view::screen_text(value);
    let verdict = screen_with_profile(&text, GuardProfile::Strict);
    (verdict.decision == InjectionDecision::Block).then(|| ConversationBlock {
        task_id: turn.task_id,
        score: verdict.score,
        reason_codes: verdict.reason_codes,
        body_sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
        body_byte_len: text.len(),
    })
}

fn total_len(rendered: &[(usize, Value)]) -> usize {
    rendered
        .iter()
        .fold(0usize, |t, (_, v)| t.saturating_add(result_view::serialised_len(v)))
}

#[cfg(test)]
mod tests;
