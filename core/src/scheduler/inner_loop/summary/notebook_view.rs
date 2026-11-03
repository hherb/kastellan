//! What the notebook loop (#922) borrows from the plan summary: the screen
//! for model-authored text, and the screened view of a record's last step.
//!
//! Kept here, beside the code it reuses, so the notebook shows tool output
//! and model text through exactly the screens the classic summary uses —
//! never a second copy that could drift (#669).

use super::call::MODEL_AUTHORED_PROFILE;
use super::sink::{sink_screen_with, SinkBlock};
use super::{with_call, PlanRecord};

/// The `tier` of a notebook text's `policy / injection.blocked` row.
pub(crate) const TIER_NOTEBOOK: &str = "notebook";

/// The `policy / injection.blocked` payload for model text the screen blocked
/// when a step was ticked: which step, which `part` (`note`, `summary`,
/// `finding`), and the block's fixed-catalogue reason codes and hash — the
/// sink row's shape. The text itself is never written.
pub(crate) fn notebook_block_audit_payload(task_id: i64, step: u32, part: &str, block: &SinkBlock) -> serde_json::Value {
    serde_json::json!({
        "task_id":       task_id,
        "step":          step,
        "part":          part,
        "score":         block.score,
        "decision":      "block",
        "tier":          TIER_NOTEBOOK,
        "reason_codes":  block.reason_codes,
        "body_sha256":   block.body_sha256,
        "body_byte_len": block.body_byte_len,
    })
}

/// Screen text the model wrote from tool output (a notebook note, finding or
/// summary) with the planner-text profile. `Some` = blocked: show
/// [`super::WITHHELD_MARKER`] instead.
pub(crate) fn screen_model_text(text: &str) -> Option<SinkBlock> {
    sink_screen_with(MODEL_AUTHORED_PROFILE, text)
}

/// One value as the planner would see it — the summary's own pruned view
/// ([`super::render_ok`]), capped at `cap` bytes — screened under `tool`'s
/// sink profile. What the sift's sorter is shown of an item (#922, K4).
/// `Err` = the screen blocked it: the item is withheld, never sorted.
pub(crate) fn sorter_view(tool: &str, value: &serde_json::Value, cap: usize) -> Result<serde_json::Value, SinkBlock> {
    let rendered = super::render_ok(tool, value, |_| cap);
    match rendered.sink_block {
        Some(block) => Err(block),
        None => Ok(rendered.value[super::OUTPUT_KEY].clone()),
    }
}

impl PlanRecord {
    /// The last step's outcome as the planner sees it in the summary — the
    /// stored, already-screened render, with its screened `call` — or `None`
    /// for a record with no outcomes. The notebook's current step shows this
    /// for the call made for it last turn.
    pub(crate) fn last_step_view(&self) -> Option<serde_json::Value> {
        let i = self.rendered.len().checked_sub(1)?;
        let call = self.calls.get(i).and_then(|c| c.as_ref()).map(|c| c.value.clone());
        Some(with_call(self.rendered[i].value.clone(), call))
    }
}
