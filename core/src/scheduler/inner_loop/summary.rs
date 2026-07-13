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

/// Per-query byte budget contributed by each element of a `web.search_batch`
/// head. A batch is ONE step but carries N independent queries; scaling the head
/// cap by the query count (see [`ok_summary_cap`]) lets the planner see more than
/// the single query that a flat [`STEP_OK_SUMMARY_MAX`] head would surface.
const BATCH_PER_QUERY_SUMMARY_BYTES: usize = 3 * 1024;

/// Hard ceiling on a `web.search_batch` step's head, so a large batch cannot
/// claim the entire [`PLANS_SUMMARY_BUDGET`]. 24 KiB = 8 (the default
/// `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`) × [`BATCH_PER_QUERY_SUMMARY_BYTES`],
/// i.e. 3/4 of the 32 KiB total — leaving headroom for other steps, which the
/// oldest-first [`apply_summary_budget`] elides if the accumulated total is
/// exceeded.
///
/// The ceiling is deliberately **independent** of the operator's batch-query cap
/// (`KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`, tunable up to 32): it bounds the
/// planner-prompt cost regardless of how large a batch the operator allows. So
/// raising the cap above 8 does not grow this head — instead per-query head
/// visibility scales *down* (the front-loading trade-off documented on
/// [`ok_summary_cap`]), which is the intended cost/coverage balance.
const STEP_OK_BATCH_SUMMARY_MAX: usize = 24 * 1024;

/// Byte cap for a successful step's surfaced head, given its `method` and the
/// result `value`. A `web.search_batch` result is
/// `{results:[{query,results,count}|{query,error}]}` — one element per query — so
/// its cap scales with the element count, clamped to
/// `[STEP_OK_SUMMARY_MAX, STEP_OK_BATCH_SUMMARY_MAX]`; every other method keeps
/// the flat single-step cap. A malformed/absent `results` array counts as zero
/// elements and clamps up to the flat floor (never larger than a real batch would
/// earn). Pure — no I/O, deterministic in `(method, value)`.
fn ok_summary_cap(method: &str, value: &serde_json::Value) -> usize {
    if method == crate::workers::web_search::WEB_SEARCH_BATCH_METHOD {
        let n = value
            .get("results")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        (n * BATCH_PER_QUERY_SUMMARY_BYTES).clamp(STEP_OK_SUMMARY_MAX, STEP_OK_BATCH_SUMMARY_MAX)
    } else {
        STEP_OK_SUMMARY_MAX
    }
}

/// Marker rendered in place of step text that the sink screen blocked. A clear,
/// structured signal to the planner that content was withheld — never raw
/// blocked content.
const WITHHELD_MARKER: &str = "[withheld: failed injection screen]";

/// Total byte budget for the whole rendered `plans_so_far_summary`. Bounds the
/// always-in-context planner prompt as successful step outputs accumulate
/// across up to `max_plans` iterations (#339). Per-step heads stay
/// `STEP_OK_SUMMARY_MAX`; this caps the *accumulated* total over all plans and
/// steps. It bounds the dominant, otherwise-unbounded term — the step-output
/// text bytes (`RenderedStep.text`). The small per-step framing (the `"ok: "` /
/// `"err: …"` prefixes and the JSON array/key punctuation) and the planner's own
/// `decision` field are not counted, so the fully-serialized prompt is modestly
/// larger than this value but still bounded.
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
///
/// `Clone` so [`render_plans_summary`] can copy the memoized renders before
/// the per-call budget elision mutates them (see [`PlanRecord`]); `Debug` so
/// [`PlanRecord`] (a field of the `Debug`-deriving `TaskContext`) can derive it.
#[derive(Clone, Debug)]
struct RenderedStep {
    text: String,
    elidable: bool,
}

/// A completed plan plus the screened, planner-bound render of its outcomes.
///
/// `rendered` is computed **once**, at the sole append point
/// ([`PlanRecord::new`], called from `inner_loop.rs`), rather than being
/// re-derived on every planner iteration. The sink screen
/// ([`render_step_outcome`] → [`sink_screen_blocks`]) is a pure, deterministic
/// function of `(tool, outcome)`, and both inputs are frozen the moment the
/// record is pushed onto the append-only `TaskContext::plans`. Re-screening
/// every accumulated outcome on every loop was latent-quadratic in
/// `max_plans` (which is operator-overridable and unbounded); memoizing at the
/// push is observationally identical and drops it to linear. Issue #344.
#[derive(Debug)]
pub struct PlanRecord {
    /// The completed plan; its `decision` labels the summary object.
    pub plan: Plan,
    /// Screened renders of `plan`'s step outcomes, one per outcome. Private:
    /// the only consumer is [`render_plans_summary`], and the screened-once
    /// invariant depends on nothing else being able to inject an unscreened
    /// `RenderedStep`.
    rendered: Vec<RenderedStep>,
}

impl PlanRecord {
    /// Screen each outcome under its step's own guard profile and store the
    /// result. The `i`-th outcome is produced by `plan.steps[i]`, so
    /// `plan.steps[i].tool` selects the profile; a missing step (outcomes
    /// longer than steps — not expected) falls back to the fail-closed Strict
    /// default (`for_tool("")`).
    pub fn new(plan: Plan, outcomes: Vec<StepOutcome>) -> Self {
        let rendered = outcomes
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let (tool, method) = plan
                    .steps
                    .get(i)
                    .map(|s| (s.tool.as_str(), s.method.as_str()))
                    .unwrap_or(("", ""));
                render_step_outcome(tool, method, o)
            })
            .collect();
        Self { plan, rendered }
    }
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
///
/// The `Ok` head length is bounded by [`ok_summary_cap`] (`method`-selected: a
/// `web.search_batch` step earns a larger, query-count-scaled head). `tool` still
/// selects the injection guard profile; `method` selects only the cap.
fn render_step_outcome(tool: &str, method: &str, o: &StepOutcome) -> RenderedStep {
    match o {
        StepOutcome::Ok(v) => {
            let cap = ok_summary_cap(method, v);
            let (head, truncated) =
                crate::cassandra::injection_guard::extract_scannable_text(v, cap);
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
/// `{ "decision", "step_outcomes": [..] }` object per completed plan.
///
/// The step outcomes were **already screened once** when each [`PlanRecord`]
/// was constructed at push time (issue #344), so this function performs *zero*
/// injection screening — it clones the memoized renders (cheap string copies,
/// no catalogue scans) and runs only the per-call size budget over them. The
/// clone is required because [`apply_summary_budget`] elides in place and must
/// not mutate the stored, immutable record.
pub(super) fn render_plans_summary(plans: &[PlanRecord]) -> Vec<serde_json::Value> {
    let mut rendered: Vec<Vec<RenderedStep>> =
        plans.iter().map(|r| r.rendered.clone()).collect();

    // Bound the accumulated size of the always-in-context summary, eliding the
    // oldest successful-step output heads first (#339).
    apply_summary_budget(&mut rendered, PLANS_SUMMARY_BUDGET);

    plans
        .iter()
        .zip(rendered)
        .map(|(r, steps)| {
            let step_outcomes: Vec<String> = steps.into_iter().map(|s| s.text).collect();
            serde_json::json!({
                "decision":      r.plan.decision,
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
    fn budget_lands_within_budget_when_all_heads_are_elidable() {
        // Worst case *for the elision pass*: every step is a full-size elidable
        // Ok head, so the budget can always be met. (A summary made entirely of
        // non-elidable errors can exceed the budget — by design, since errors
        // are load-bearing; that path is bounded instead by the per-error
        // `STEP_ERR_DETAIL_MAX` clamp, not by this pass.)
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
        let ok_step =
            render_step_outcome("shell-exec", "shell.exec", &StepOutcome::Ok(serde_json::json!("hello")));
        assert_eq!(ok_step.text, "ok: hello");
        assert!(ok_step.elidable);

        let err_step = render_step_outcome(
            "shell-exec",
            "shell.exec",
            &StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "no".into() },
        );
        assert_eq!(err_step.text, "err: POLICY_DENIED: no");
        assert!(!err_step.elidable);
    }

    use crate::workers::web_search::WEB_SEARCH_BATCH_METHOD;

    /// A `web.search_batch`-shaped Ok value with `n` per-query elements.
    fn batch_value(n: usize) -> serde_json::Value {
        let elements: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({ "query": format!("q{i}"), "results": [], "count": 0 }))
            .collect();
        serde_json::json!({ "results": elements })
    }

    #[test]
    fn ok_summary_cap_is_flat_for_non_batch_methods() {
        let v = batch_value(8); // shape is irrelevant for a non-batch method
        assert_eq!(ok_summary_cap("web.search", &v), STEP_OK_SUMMARY_MAX);
        assert_eq!(ok_summary_cap("shell.exec", &v), STEP_OK_SUMMARY_MAX);
        assert_eq!(ok_summary_cap("", &v), STEP_OK_SUMMARY_MAX);
    }

    #[test]
    fn ok_summary_cap_scales_with_query_count() {
        assert_eq!(
            ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(2)),
            2 * BATCH_PER_QUERY_SUMMARY_BYTES
        );
        assert_eq!(
            ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(8)),
            STEP_OK_BATCH_SUMMARY_MAX // 8 * 3 KiB == 24 KiB
        );
    }

    #[test]
    fn ok_summary_cap_clamps_low_and_high() {
        // 1 query → 3 KiB < 4 KiB → clamped up to the single-step floor.
        assert_eq!(ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(1)), STEP_OK_SUMMARY_MAX);
        // 16 queries → 48 KiB → clamped down to the hard ceiling.
        assert_eq!(ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &batch_value(16)), STEP_OK_BATCH_SUMMARY_MAX);
    }

    #[test]
    fn ok_summary_cap_degrades_to_flat_on_malformed_results() {
        // Missing `results` → 0 elements → floor.
        assert_eq!(ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &serde_json::json!({})), STEP_OK_SUMMARY_MAX);
        // Non-array `results` → 0 → floor.
        assert_eq!(
            ok_summary_cap(WEB_SEARCH_BATCH_METHOD, &serde_json::json!({ "results": "nope" })),
            STEP_OK_SUMMARY_MAX
        );
    }

    #[test]
    fn batch_step_surfaces_more_than_a_single_search_head() {
        // An 8-query × 10-hit batch whose scannable text far exceeds 4 KiB.
        let hit = |i: usize| {
            serde_json::json!({
                "title": format!("title number {i} about a topic"),
                "url": format!("https://example.com/results/{i}"),
                "snippet": "s".repeat(300),
                "engine": "google",
            })
        };
        let elements: Vec<serde_json::Value> = (0..8)
            .map(|q| {
                serde_json::json!({
                    "query": format!("query number {q}"),
                    "results": (0..10).map(hit).collect::<Vec<_>>(),
                    "count": 10,
                })
            })
            .collect();
        let val = serde_json::json!({ "results": elements });

        let batch =
            render_step_outcome("web-search", WEB_SEARCH_BATCH_METHOD, &StepOutcome::Ok(val.clone()));
        let single = render_step_outcome("web-search", "web.search", &StepOutcome::Ok(val));

        // Batch surfaces well over the flat 4 KiB; a single web.search of the
        // SAME value stays at the flat cap (regression pin: single search
        // untouched). Framing = "ok: " (4) + "…" (3 bytes) ≤ 8.
        assert!(
            batch.text.len() > STEP_OK_SUMMARY_MAX,
            "batch head {} should exceed 4 KiB",
            batch.text.len()
        );
        assert!(
            single.text.len() <= STEP_OK_SUMMARY_MAX + 8,
            "single-search head {} should stay ~4 KiB",
            single.text.len()
        );
        // Batch head is bounded by the hard ceiling (+ framing).
        assert!(batch.text.len() <= STEP_OK_BATCH_SUMMARY_MAX + 8);
    }
}
