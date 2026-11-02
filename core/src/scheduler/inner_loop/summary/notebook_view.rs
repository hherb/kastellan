//! What the notebook loop (#922) borrows from the plan summary: the screen
//! for model-authored text, and the screened view of a record's last step.
//!
//! Kept here, beside the code it reuses, so the notebook shows tool output
//! and model text through exactly the screens the classic summary uses —
//! never a second copy that could drift (#669).

use super::call::MODEL_AUTHORED_PROFILE;
use super::sink::{sink_screen_with, SinkBlock};
use super::{with_call, PlanRecord};

/// Screen text the model wrote from tool output (a notebook note, finding or
/// summary) with the planner-text profile. `Some` = blocked: show
/// [`super::WITHHELD_MARKER`] instead.
pub(crate) fn screen_model_text(text: &str) -> Option<SinkBlock> {
    sink_screen_with(MODEL_AUTHORED_PROFILE, text)
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
