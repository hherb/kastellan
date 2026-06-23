//! Rendering of the per-task plan summary that feeds the planner prompt.
//!
//! Pure, I/O-free helpers lifted out of `inner_loop.rs` (which sat over the
//! 500-LOC cap) into a focused, separately-testable module. The single entry
//! point is [`render_plans_summary`], which `TaskContext::plans_so_far_summary`
//! delegates to.

use super::StepOutcome;
use crate::cassandra::types::Plan;

/// Max chars of a step error `detail` surfaced back to the agent in
/// `plans_so_far_summary`. Long worker stderr / RPC messages are clamped so a
/// single chatty failure can't blow up the always-in-context planner prompt;
/// the `code` (always short) is never truncated. A truncated detail gets a
/// trailing `…`, so the rendered detail is at most `STEP_ERR_DETAIL_MAX + 1`
/// chars.
pub(crate) const STEP_ERR_DETAIL_MAX: usize = 200;

/// Max bytes of a *successful* step's output head surfaced back to the planner
/// in `plans_so_far_summary`. The head is screened at the sink (see
/// [`render_step_outcome`]/[`sink_screen_blocks`]) and bounded here to keep the
/// always-in-context planner prompt small as successful outputs accumulate
/// across iterations. A truncated head gets a trailing `…`.
pub(crate) const STEP_OK_SUMMARY_MAX: usize = 4 * 1024;

/// Marker rendered in place of step text that the sink screen blocked. A clear,
/// structured signal to the planner that content was withheld — never raw
/// blocked content.
const WITHHELD_MARKER: &str = "[withheld: failed injection screen]";

/// Screen `text` with `tool`'s own guard profile; `true` if it must be
/// withheld. The **single, mandatory sink screen**: every string this module
/// places into the planner prompt passes through here, so the
/// "nothing-unscreened-reaches-the-planner" invariant is *enforced* at one
/// point rather than *relied upon* across the source chokepoints (`tool_host`,
/// `tool_dispatch::fetch_screen`). For legitimately-allowed content this
/// re-screen is idempotent (same per-tool profile → Allow) and cannot
/// over-block a Relaxed-profile doc-fetch worker (issue #142).
fn sink_screen_blocks(tool: &str, text: &str) -> bool {
    use crate::cassandra::injection_guard::{screen_with_profile, GuardProfile, InjectionDecision};
    screen_with_profile(text, GuardProfile::for_tool(tool)).decision == InjectionDecision::Block
}

/// Render one [`StepOutcome`] for the planner's plan summary, screening the
/// exact text about to enter the prompt with `tool`'s guard profile. An `Ok`
/// step surfaces a bounded head of its output as `"ok: <head>"` (#338); an
/// `Err` surfaces `"err: <CODE>: <detail>"` (#337). On a Block the
/// worker-influenced text (the `Ok` head, or the `Err` `detail` — the `code`
/// is an internal constant, always kept) is replaced by [`WITHHELD_MARKER`].
fn render_step_outcome(tool: &str, o: &StepOutcome) -> String {
    match o {
        StepOutcome::Ok(v) => {
            let (head, truncated) =
                crate::cassandra::injection_guard::extract_scannable_text(v, STEP_OK_SUMMARY_MAX);
            if sink_screen_blocks(tool, &head) {
                format!("ok: {WITHHELD_MARKER}")
            } else if truncated {
                format!("ok: {head}…")
            } else {
                format!("ok: {head}")
            }
        }
        StepOutcome::Err { code, detail } => {
            let shown = if detail.chars().count() > STEP_ERR_DETAIL_MAX {
                let truncated: String = detail.chars().take(STEP_ERR_DETAIL_MAX).collect();
                format!("{truncated}…")
            } else {
                detail.clone()
            };
            let shown = if sink_screen_blocks(tool, &shown) {
                WITHHELD_MARKER.to_string()
            } else {
                shown
            };
            format!("err: {code}: {shown}")
        }
    }
}

/// Build the compact per-plan summary for the planner prompt: one
/// `{ "decision", "step_outcomes": [..] }` object per completed plan. Each
/// outcome is the result of `p.steps[i]`, so the tool whose guard profile
/// screens it is `p.steps[i].tool`; a missing step (outcomes longer than
/// steps — not expected) falls back to the fail-closed Strict default
/// (`for_tool("")`).
pub(super) fn render_plans_summary(plans: &[(Plan, Vec<StepOutcome>)]) -> Vec<serde_json::Value> {
    plans
        .iter()
        .map(|(p, outcomes)| {
            let step_outcomes: Vec<String> = outcomes
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    let tool = p.steps.get(i).map(|s| s.tool.as_str()).unwrap_or("");
                    render_step_outcome(tool, o)
                })
                .collect();
            serde_json::json!({
                "decision":      p.decision,
                "step_outcomes": step_outcomes,
            })
        })
        .collect()
}
