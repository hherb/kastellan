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

/// Total serialised bytes the conversation block may occupy, beside the
/// existing 96 KiB `PLANS_SUMMARY_BUDGET`. Affordable because DGX plan latency
/// is bound by generation, not context.
pub(crate) const CONVERSATION_BUDGET: usize = 16 * 1024;

/// Most bytes of one earlier user message.
const USER_TEXT_CAP: usize = 2 * 1024;

/// Most bytes of one earlier answer.
const ANSWER_CAP: usize = 4 * 1024;

/// What the enclosing JSON array costs beyond its elements: two brackets, plus
/// room for the omission marker and its comma. Subtracted from the budget when
/// measuring a single clamped turn, so the returned array is within bound.
const ARRAY_FRAMING_BYTES: usize = 32;

/// Marker element carrying how many older turns the budget dropped.
pub(crate) const OMITTED_TURNS_KEY: &str = "_omitted_turns";

/// The `tier` of the `policy / injection.blocked` row written for a block this
/// screen made, beside `catalogue`, `guard_model` and `sink`.
pub(crate) const TIER_CONVERSATION: &str = "conversation";

/// What replaces a turn the screen blocked.
pub(crate) const STATUS_WITHHELD: &str = "withheld";

/// What replaces a turn that cannot be made to fit the budget at all.
pub(crate) const STATUS_TOO_LARGE: &str = "too large for the conversation budget";

/// What replaces a turn whose stored class could not be read.
///
/// Such a turn is shown as having happened and nothing more. Rendering its
/// text while its classification is unknown would let a clinical answer reach
/// a follow-up running at `Public` — a turn present in the prompt with no
/// class is strictly worse than a turn left out. Reachable for every turn that
/// finished before migration 0026, so it is the shape a live deployment sees
/// on its first follow-up in each room.
pub(crate) const STATUS_UNCLASSIFIED: &str = "unclassified";

/// The `calls` a turn dropped to fit its own record cap, surfaced to the
/// planner so it knows the list it sees is partial.
pub(crate) const OMITTED_CALLS_KEY: &str = "_omitted_calls";

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

/// The screening chokepoint, sealed so that "every candidate is screened" is a
/// property of the type system rather than of this file's good manners.
///
/// [`Admitted`] wraps a value that has been through the screen, and it is the
/// only thing [`render`] will put in its output. Its field is private **to this
/// inner module**, so no code outside `admitted` — including the rest of
/// `view.rs` — can build one without calling [`admitted::admit`], which
/// screens. A review mutant that routed the budget-clamped candidates straight
/// to the output survived the entire suite while that rule was only a comment;
/// now it does not compile.
mod admitted {
    use super::{at, screen, ConversationBlock, Value, STATUS_WITHHELD};
    use serde_json::json;

    /// A value the screen has passed (or replaced with the withheld shape).
    pub(super) struct Admitted(Value);

    impl Admitted {
        /// The screened value. Read-only: there is no way back to a mutable
        /// handle, so nothing can edit a value after it was screened.
        pub(super) fn value(&self) -> &Value {
            &self.0
        }

        /// Consume the wrapper for the rendered output.
        pub(super) fn into_value(self) -> Value {
            self.0
        }
    }

    /// `value` if the screen passes it, else the withheld shape, recording the
    /// block. **The only constructor of [`Admitted`].**
    pub(super) fn admit(
        turn: &super::Turn,
        value: Value,
        blocks: &mut Vec<ConversationBlock>,
    ) -> Admitted {
        match screen(turn, &value) {
            None => Admitted(value),
            Some(block) => {
                // `{at, status}` only: the score and the reason codes go to the
                // audit row and never to the planner, which would otherwise
                // hold an oracle telling an attacker how close a phrasing came.
                let withheld = json!({"at": at(turn), "status": STATUS_WITHHELD});
                blocks.push(block);
                Admitted(withheld)
            }
        }
    }
}

use admitted::{admit, Admitted};

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

    let mut rendered: Vec<(usize, Admitted)> = Vec::with_capacity(turns.len());
    for (i, turn) in turns.iter().enumerate() {
        let candidate = if turn.data_class.is_none() {
            // Shown as having happened, and nothing more: see
            // [`STATUS_UNCLASSIFIED`].
            json!({"at": at(turn), "status": STATUS_UNCLASSIFIED})
        } else {
            render_turn(turn, USER_TEXT_CAP, ANSWER_CAP, true)
        };
        rendered.push((i, admit(turn, candidate, &mut out.blocks)));
    }

    // Drop whole turns, oldest first, until the array fits. Never the last
    // one: a conversation that renders as nothing tells the planner less than
    // a clamped turn does.
    //
    // The measure is the ARRAY as it will serialise — its brackets, its commas
    // and the omission marker it will carry — not the sum of its elements. The
    // element sum undercounts by a couple of dozen bytes, which does not matter
    // at 16 KiB but does make the doc's bound untrue.
    let mut omitted = 0usize;
    while rendered.len() > 1 && array_len(&rendered, omitted + 1) > budget {
        rendered.remove(0);
        omitted += 1;
    }

    // One turn left and still over budget: clamp it, cheapest loss first.
    if let Some((i, value)) = rendered.first_mut() {
        if result_view::serialised_len(value.value()) > budget.saturating_sub(ARRAY_FRAMING_BYTES) {
            let turn = &turns[*i];
            for candidate in [
                render_turn(turn, USER_TEXT_CAP, ANSWER_CAP, false),
                render_turn(turn, budget / 4, budget / 4, false),
                json!({"at": at(turn), "status": STATUS_TOO_LARGE}),
            ] {
                let candidate = admit(turn, candidate, &mut out.blocks);
                let fits = result_view::serialised_len(candidate.value())
                    <= budget.saturating_sub(ARRAY_FRAMING_BYTES);
                *value = candidate;
                if fits {
                    break;
                }
            }
        }
    }

    out.turns = rendered.into_iter().map(|(_, v)| v.into_value()).collect();
    if omitted > 0 {
        out.turns.insert(0, json!({ OMITTED_TURNS_KEY: omitted }));
    }
    out
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
            // The counter `record` stores exists for this reader and no other:
            // without it a turn that made 40 calls shows 16 with no sign the
            // rest happened — `_omitted_turns`' failure mode, one level down.
            if record.omitted_calls > 0 {
                obj.insert(OMITTED_CALLS_KEY.into(), Value::from(record.omitted_calls));
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

/// Serialised size of the array `render` would return from `rendered`, if
/// `omitted` turns had been dropped (the marker element is counted when
/// `omitted > 0`).
fn array_len(rendered: &[(usize, Admitted)], omitted: usize) -> usize {
    let mut values: Vec<Value> = rendered.iter().map(|(_, v)| v.value().clone()).collect();
    if omitted > 0 {
        values.insert(0, json!({ OMITTED_TURNS_KEY: omitted }));
    }
    result_view::serialised_len(&Value::Array(values))
}

#[cfg(test)]
mod tests;
