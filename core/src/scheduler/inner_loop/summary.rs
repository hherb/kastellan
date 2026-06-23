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

/// Total byte budget for the whole rendered `plans_so_far_summary`. Bounds the
/// always-in-context planner prompt as successful step outputs accumulate
/// across up to `max_plans` iterations (#339). Per-step heads stay
/// `STEP_OK_SUMMARY_MAX`; this caps the *accumulated* total over all plans and
/// steps. Covers the unbounded term (step-output text bytes), not the small
/// fixed per-plan JSON overhead (`decision`).
const PLANS_SUMMARY_BUDGET: usize = 32 * 1024;

/// Replacement for an elided `Ok`-output head (#339). A clear, structured
/// signal to the planner that an older step's output was dropped to keep the
/// prompt bounded — distinct from the injection [`WITHHELD_MARKER`].
const OK_ELIDED_MARKER: &str = "ok: [output elided: summary budget]";

/// One rendered step plus whether its text is an elidable `Ok`-output head.
/// `elidable` is `true` only for a successfully-screened `Ok` step carrying
/// real output (the budget may drop it); `false` for errors and for the
/// already-tiny withheld/empty `Ok` markers, which carry load-bearing signal
/// and are never elided.
struct RenderedStep {
    text: String,
    elidable: bool,
}

/// Elide oldest `Ok`-output heads until the total byte size of all step texts
/// is within `budget`. Walks `plans` oldest→newest (`plans[0]` is the oldest,
/// pushed first by the inner loop) and steps in order, replacing each
/// `elidable` head **longer than the marker** with [`OK_ELIDED_MARKER`],
/// decrementing a running total, and stopping the instant it is within budget.
/// No-op when already within budget. The `> marker.len()` guard means eliding
/// can never *grow* a tiny `Ok` (e.g. `"ok: 9"`) and makes the pass idempotent
/// (an elided step is exactly the marker length, so it is never re-elided).
/// Returns the number of steps elided.
fn apply_summary_budget(plans: &mut [Vec<RenderedStep>], budget: usize) -> usize {
    let mut total: usize = plans.iter().flatten().map(|s| s.text.len()).sum();
    if total <= budget {
        return 0;
    }
    let marker_len = OK_ELIDED_MARKER.len();
    let mut elided = 0;
    for plan in plans.iter_mut() {
        for step in plan.iter_mut() {
            if total <= budget {
                return elided;
            }
            if step.elidable && step.text.len() > marker_len {
                total -= step.text.len() - marker_len;
                step.text = OK_ELIDED_MARKER.to_string();
                step.elidable = false; // now minimal; never touch again
                elided += 1;
            }
        }
    }
    elided
}

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
fn render_step_outcome(tool: &str, o: &StepOutcome) -> RenderedStep {
    match o {
        StepOutcome::Ok(v) => {
            let (head, truncated) =
                crate::cassandra::injection_guard::extract_scannable_text(v, STEP_OK_SUMMARY_MAX);
            if sink_screen_blocks(tool, &head) {
                // Already-tiny withheld signal: load-bearing, never elide.
                RenderedStep { text: format!("ok: {WITHHELD_MARKER}"), elidable: false }
            } else if truncated {
                RenderedStep { text: format!("ok: {head}…"), elidable: true }
            } else {
                RenderedStep { text: format!("ok: {head}"), elidable: true }
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
            RenderedStep { text: format!("err: {code}: {shown}"), elidable: false }
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
    let mut rendered: Vec<Vec<RenderedStep>> = plans
        .iter()
        .map(|(p, outcomes)| {
            outcomes
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    let tool = p.steps.get(i).map(|s| s.tool.as_str()).unwrap_or("");
                    render_step_outcome(tool, o)
                })
                .collect()
        })
        .collect();

    // Bound the accumulated size of the always-in-context summary, eliding the
    // oldest successful-step output heads first (#339).
    apply_summary_budget(&mut rendered, PLANS_SUMMARY_BUDGET);

    plans
        .iter()
        .zip(rendered)
        .map(|((p, _), steps)| {
            let step_outcomes: Vec<String> = steps.into_iter().map(|s| s.text).collect();
            serde_json::json!({
                "decision":      p.decision,
                "step_outcomes": step_outcomes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An elidable `Ok` step carrying `text` of the given size.
    fn ok(text: &str) -> RenderedStep {
        RenderedStep { text: format!("ok: {text}"), elidable: true }
    }
    /// A (non-elidable) error step.
    fn err(detail: &str) -> RenderedStep {
        RenderedStep { text: format!("err: CODE: {detail}"), elidable: false }
    }
    fn total(plans: &[Vec<RenderedStep>]) -> usize {
        plans.iter().flatten().map(|s| s.text.len()).sum()
    }

    #[test]
    fn budget_is_a_no_op_when_under() {
        let mut plans = vec![vec![ok("small"), err("nope")]];
        let elided = apply_summary_budget(&mut plans, 1024);
        assert_eq!(elided, 0);
        assert_eq!(plans[0][0].text, "ok: small");
        assert_eq!(plans[0][1].text, "err: CODE: nope");
    }

    #[test]
    fn budget_elides_oldest_ok_heads_first() {
        let big = "y".repeat(1000);
        // plans[0] is the oldest, plans[2] the most recent.
        let mut plans = vec![vec![ok(&big)], vec![ok(&big)], vec![ok(&big)]];
        let elided = apply_summary_budget(&mut plans, 1100);
        assert!(elided >= 1, "expected at least one elision");
        // Oldest elided, most-recent intact.
        assert_eq!(plans[0][0].text, OK_ELIDED_MARKER);
        assert_eq!(plans[2][0].text, format!("ok: {big}"));
        assert!(total(&plans) <= 1100, "total {} over budget", total(&plans));
    }

    #[test]
    fn budget_never_elides_errors_or_decisions() {
        let big = "z".repeat(1000);
        // Oldest step is an error (non-elidable); the elidable Ok is newest.
        let mut plans = vec![vec![err("kept")], vec![ok(&big)]];
        apply_summary_budget(&mut plans, 50);
        assert_eq!(plans[0][0].text, "err: CODE: kept", "error must be preserved");
        assert_eq!(plans[1][0].text, OK_ELIDED_MARKER, "the only elidable head is elided");
    }

    #[test]
    fn budget_never_elides_withheld_marker() {
        let withheld = RenderedStep {
            text: "ok: [withheld: failed injection screen]".to_string(),
            elidable: false,
        };
        let big = "q".repeat(1000);
        let mut plans = vec![vec![withheld], vec![ok(&big)]];
        apply_summary_budget(&mut plans, 50);
        assert_eq!(plans[0][0].text, "ok: [withheld: failed injection screen]");
    }

    #[test]
    fn budget_never_grows_a_tiny_ok() {
        // "ok: 9" is shorter than the elided marker; eliding would *increase*
        // size, so the tiny head is left untouched even under a 0 budget.
        let mut plans = vec![vec![ok("9")]];
        let elided = apply_summary_budget(&mut plans, 0);
        assert_eq!(elided, 0);
        assert_eq!(plans[0][0].text, "ok: 9");
    }

    #[test]
    fn budget_is_idempotent() {
        let big = "y".repeat(1000);
        let mut plans = vec![vec![ok(&big)], vec![ok(&big)], vec![ok(&big)]];
        apply_summary_budget(&mut plans, 1100);
        let snapshot: Vec<Vec<String>> =
            plans.iter().map(|p| p.iter().map(|s| s.text.clone()).collect()).collect();
        let elided_again = apply_summary_budget(&mut plans, 1100);
        assert_eq!(elided_again, 0, "second pass should be a no-op");
        let after: Vec<Vec<String>> =
            plans.iter().map(|p| p.iter().map(|s| s.text.clone()).collect()).collect();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn budget_worst_case_lands_within_budget() {
        let big = "y".repeat(STEP_OK_SUMMARY_MAX);
        // 40 plans × one full-size head each — far over a 32 KiB budget.
        let mut plans: Vec<Vec<RenderedStep>> =
            (0..40).map(|_| vec![ok(&big)]).collect();
        apply_summary_budget(&mut plans, PLANS_SUMMARY_BUDGET);
        assert!(
            total(&plans) <= PLANS_SUMMARY_BUDGET,
            "total {} exceeds budget {}",
            total(&plans),
            PLANS_SUMMARY_BUDGET
        );
    }

    #[test]
    fn render_step_outcome_marks_ok_elidable_and_err_not() {
        let ok_step = render_step_outcome("shell-exec", &StepOutcome::Ok(serde_json::json!("hello")));
        assert_eq!(ok_step.text, "ok: hello");
        assert!(ok_step.elidable);

        let err_step = render_step_outcome(
            "shell-exec",
            &StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "no".into() },
        );
        assert_eq!(err_step.text, "err: POLICY_DENIED: no");
        assert!(!err_step.elidable);
    }
}
