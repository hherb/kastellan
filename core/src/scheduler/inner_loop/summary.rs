//! Rendering of the per-task plan summary that feeds the planner prompt.
//!
//! Pure, I/O-free helpers lifted out of `inner_loop.rs` (which sat over the
//! 500-LOC cap) into a focused, separately-testable module. The single entry
//! point is [`render_plans_summary`], which `TaskContext::plans_so_far_summary`
//! delegates to.
//!
//! # The shape the planner reads (#677)
//!
//! Each plan renders as `{"decision", "step_outcomes"}`, and each step outcome
//! is an object in one of four shapes:
//!
//! | Shape | When |
//! | --- | --- |
//! | `{"status": "ok", "output": <view>}` | success; `<view>` is [`result_view::render`]'s pruned, labelled copy of the result |
//! | `{"status": "ok", "withheld": "failed injection screen"}` | success, but the sink screen blocked the view |
//! | `{"status": "ok", "elided": "summary budget"}` | an older success dropped by [`apply_summary_budget`] |
//! | `{"status": "err", "code": <CODE>, "detail": <clamped detail>}` | failure |
//!
//! Until #677 an outcome was the string `"ok: <head>"`, where the head was
//! `extract_scannable_text`'s flattening with every object key, number and
//! boolean discarded, so the planner could not see which search hit had an
//! attachment or which bare number was the message id.
//! `prompts/agent_planner.md` documents these shapes, and
//! `the_planner_prompt_documents_every_outcome_shape` fails if the two drift.

use serde_json::json;

use super::result_view;
use super::StepOutcome;
use crate::cassandra::types::Plan;

/// Max chars of a step error `detail` surfaced back to the agent in
/// `plans_so_far_summary`. Long worker stderr / RPC messages are clamped so a
/// single chatty failure can't blow up the always-in-context planner prompt;
/// the `code` (always short) is never truncated. A truncated detail gets a
/// trailing `…`, so the rendered detail is at most `STEP_ERR_DETAIL_MAX + 1`
/// chars.
///
/// Defined in `kastellan-protocol` rather than here because it binds *both*
/// sides of the wire: a worker that writes an error meant to repair a planner
/// mistake has to fit its advice in this budget, and until #536 `workers/mail`
/// carried a hand-synced `#[cfg(test)]` copy of the number. Lowering it here
/// would have left that copy stale and silently truncated the repair advice
/// with every test on both sides still green.
pub(crate) use kastellan_protocol::STEP_ERR_DETAIL_MAX;

/// Max bytes of a *successful* step's view surfaced back to the planner: the
/// serialised length of [`result_view::render`]'s output. 16 KiB since #677
/// (4 KiB before), which is what it takes for a ten-hit `mail.search` result
/// to reach the planner whole and labelled. Affordable because DGX plan
/// latency is bound by generation, not context: cutting `num_ctx` from 262144
/// to 65536 bought only about 10 %.
pub(crate) const STEP_OK_SUMMARY_MAX: usize = 16 * 1024;

/// Per-query byte budget contributed by each element of a `web.search_batch`
/// result. A batch is ONE step but carries N independent queries; scaling the
/// view's budget by the query count (see [`ok_summary_cap`]) lets the planner
/// see more than a flat [`STEP_OK_SUMMARY_MAX`] would.
const BATCH_PER_QUERY_SUMMARY_BYTES: usize = 3 * 1024;

/// Hard ceiling on a `web.search_batch` step's view, so a large batch cannot
/// claim the entire [`PLANS_SUMMARY_BUDGET`]. 24 KiB = 8 (the default
/// `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`) × [`BATCH_PER_QUERY_SUMMARY_BYTES`],
/// a quarter of the 96 KiB total.
///
/// The ceiling is deliberately **independent** of the operator's batch-query cap
/// (`KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`, tunable up to 32): it bounds the
/// planner-prompt cost regardless of how large a batch the operator allows.
const STEP_OK_BATCH_SUMMARY_MAX: usize = 24 * 1024;

/// Total byte budget for the whole rendered `plans_so_far_summary`, counted in
/// serialised step-outcome bytes. 96 KiB since #677: six times the per-step
/// ceiling, so a full fast-lane task (`DEFAULT_MAX_PLANS_FAST` = 5, plus the
/// forced-synthesis turn) is not pushed into elision by the ceiling alone. A
/// long-lane task can still exceed it and elide, oldest first. The planner's
/// own `decision` strings are not counted, so the serialised prompt is
/// modestly larger than this value but still bounded.
const PLANS_SUMMARY_BUDGET: usize = 96 * 1024;

const _: () = {
    // `usize::clamp` panics when its lower bound exceeds its upper bound, and
    // `ok_summary_cap` clamps between these two.
    assert!(STEP_OK_SUMMARY_MAX <= STEP_OK_BATCH_SUMMARY_MAX);
    // `result_view::render` guarantees its size bound only from MIN_VIEW_TOTAL up.
    // Module level, not `#[cfg(test)]`, which release builds strip.
    assert!(STEP_OK_SUMMARY_MAX >= result_view::MIN_VIEW_TOTAL);
};

/// Byte budget for a successful step's view, given its `method` and result
/// `value`. A `web.search_batch` result is
/// `{results:[{query,results,count}|{query,error}]}` — one element per query — so
/// its budget scales with the element count, clamped to
/// `[STEP_OK_SUMMARY_MAX, STEP_OK_BATCH_SUMMARY_MAX]`; every other method keeps
/// the flat single-step budget. A malformed/absent `results` array counts as zero
/// elements and clamps up to the flat floor. Pure — deterministic in `(method, value)`.
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

// Keys and fixed values of a step outcome object; see the module docs.
const STATUS_KEY: &str = "status";
const STATUS_OK: &str = "ok";
const STATUS_ERR: &str = "err";
const OUTPUT_KEY: &str = "output";
const WITHHELD_KEY: &str = "withheld";
const ELIDED_KEY: &str = "elided";
const CODE_KEY: &str = "code";
const DETAIL_KEY: &str = "detail";
/// Why a successful step shows no output: the sink screen blocked it.
const WITHHELD_REASON: &str = "failed injection screen";
/// Why an older successful step shows no output: the summary budget dropped it.
const ELIDED_REASON: &str = "summary budget";

/// Replaces a failed step's `detail` when the sink screen blocks it. The
/// `code` is an internal constant and is always kept, so the planner still
/// learns why the step failed.
const WITHHELD_MARKER: &str = "[withheld: failed injection screen]";

/// The outcome that replaces an older successful step dropped by
/// [`apply_summary_budget`] (#339). A clear signal to the planner that output
/// was dropped for size — distinct from the injection-screen `withheld` shape.
fn elided_outcome() -> serde_json::Value {
    json!({STATUS_KEY: STATUS_OK, ELIDED_KEY: ELIDED_REASON})
}

/// One rendered step outcome, its serialised size, and whether the budget may
/// replace it with [`elided_outcome`].
///
/// `elidable` is `true` only for a successful step carrying real output; errors
/// and the withheld shape carry load-bearing signal and are never elided.
/// `bytes` is computed once, in [`RenderedStep::new`], because it is the unit
/// [`apply_summary_budget`] counts.
///
/// `Clone` so [`render_plans_summary`] can copy the memoized renders before the
/// per-call budget elision mutates them (see [`PlanRecord`]); `Debug` so
/// [`PlanRecord`] (a field of the `Debug`-deriving `TaskContext`) can derive it.
#[derive(Clone, Debug)]
struct RenderedStep {
    value: serde_json::Value,
    bytes: usize,
    elidable: bool,
}

impl RenderedStep {
    fn new(value: serde_json::Value, elidable: bool) -> Self {
        let bytes = result_view::serialised_len(&value);
        Self { value, bytes, elidable }
    }
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
    /// The raw, unscreened outcomes this record was built from. Kept so a
    /// suspended run can be persisted and rebuilt (#564 slice 1b, D11):
    /// `rendered` is a pure function of `(plan, outcomes)`, so persisting
    /// the INPUTS and calling [`PlanRecord::new`] again on restore re-applies
    /// the screen through the same code path — rather than trusting a
    /// serialized render that arrived from storage.
    ///
    /// Private, with [`PlanRecord::outcomes`] as the only way out, for the
    /// same reason `rendered` is private: a `pub` field could be pushed to
    /// after construction, and `rendered` would then no longer describe
    /// `outcomes`.
    outcomes: Vec<StepOutcome>,
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
    ///
    /// Deterministic in `(plan, outcomes)` and free of I/O, which is what
    /// makes it safe to call again when a suspended run is restored.
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
        Self { plan, outcomes, rendered }
    }

    /// The raw outcomes this record was built from, in step order.
    ///
    /// The one caller is the suspend path
    /// (`scheduler::asks::resume_state_from`), which serialises `plan` and
    /// these outcomes so the resumed run can rebuild the record with
    /// [`PlanRecord::new`]. **Not** for building a planner prompt: these are
    /// unscreened, and [`render_plans_summary`] is the only thing that turns
    /// an outcome into planner-bound text.
    pub fn outcomes(&self) -> &[StepOutcome] {
        &self.outcomes
    }
}

/// Elide the oldest successful-step outputs until the serialised total of all
/// step outcomes is within `budget`. Walks `plans` oldest→newest (`plans[0]` is
/// the oldest, pushed first by the inner loop) and steps in order, replacing
/// each `elidable` step **larger than the marker** with [`elided_outcome`],
/// decrementing a running total, and stopping the instant it is within budget.
/// No-op when already within budget. The `bytes > marker.bytes` guard means
/// eliding can never *grow* a tiny output and makes the pass idempotent (an
/// elided step is exactly the marker's size, so it is never re-elided).
/// Returns the number of steps elided.
fn apply_summary_budget(plans: &mut [Vec<RenderedStep>], budget: usize) -> usize {
    let mut total: usize = plans.iter().flatten().map(|s| s.bytes).sum();
    if total <= budget {
        return 0;
    }
    let marker = RenderedStep::new(elided_outcome(), false);
    let mut elided = 0;
    for plan in plans.iter_mut() {
        for step in plan.iter_mut() {
            if total <= budget {
                return elided;
            }
            if step.elidable && step.bytes > marker.bytes {
                total -= step.bytes - marker.bytes;
                *step = marker.clone(); // now minimal and non-elidable
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
/// exact content about to enter the prompt with `tool`'s guard profile.
///
/// A successful step becomes `{"status":"ok","output":<view>}`, where `<view>`
/// is [`result_view::render`]'s pruned copy of the result, bounded by
/// [`ok_summary_cap`] (`method` selects the budget; a `web.search_batch` step
/// earns more). The screen checks [`result_view::screen_text`] — every key and
/// string of that view — and on a Block the step becomes the withheld shape.
///
/// A failed step becomes `{"status":"err","code":…,"detail":…}` with `detail`
/// clamped to [`STEP_ERR_DETAIL_MAX`] chars (#337); on a Block the
/// worker-influenced `detail` is replaced by [`WITHHELD_MARKER`] and the
/// internal `code` is kept.
fn render_step_outcome(tool: &str, method: &str, o: &StepOutcome) -> RenderedStep {
    match o {
        StepOutcome::Ok(v) => {
            let (view, _bytes) = result_view::render(v, ok_summary_cap(method, v));
            if sink_screen_blocks(tool, &result_view::screen_text(&view)) {
                // Already tiny and load-bearing: never elide.
                RenderedStep::new(json!({STATUS_KEY: STATUS_OK, WITHHELD_KEY: WITHHELD_REASON}), false)
            } else {
                RenderedStep::new(json!({STATUS_KEY: STATUS_OK, OUTPUT_KEY: view}), true)
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
            RenderedStep::new(json!({STATUS_KEY: STATUS_ERR, CODE_KEY: code, DETAIL_KEY: shown}), false)
        }
    }
}

/// Build the compact per-plan summary for the planner prompt: one
/// `{ "decision", "step_outcomes": [..] }` object per completed plan, each
/// outcome in one of the shapes described in the module docs.
///
/// The step outcomes were **already screened once** when each [`PlanRecord`]
/// was constructed at push time (issue #344), so this function performs *zero*
/// injection screening — it clones the memoized renders and runs only the
/// per-call size budget over them. The clone is required because
/// [`apply_summary_budget`] elides in place and must not mutate the stored,
/// immutable record.
pub(super) fn render_plans_summary(plans: &[PlanRecord]) -> Vec<serde_json::Value> {
    let mut rendered: Vec<Vec<RenderedStep>> =
        plans.iter().map(|r| r.rendered.clone()).collect();

    // Bound the accumulated size of the always-in-context summary, eliding the
    // oldest successful-step outputs first (#339).
    apply_summary_budget(&mut rendered, PLANS_SUMMARY_BUDGET);

    plans
        .iter()
        .zip(rendered)
        .map(|(r, steps)| {
            let step_outcomes: Vec<serde_json::Value> = steps.into_iter().map(|s| s.value).collect();
            json!({
                "decision":      r.plan.decision,
                "step_outcomes": step_outcomes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
