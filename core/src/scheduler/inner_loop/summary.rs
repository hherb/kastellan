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
//! is an object in one of four shapes, plus — since #699 — a `"call"` key
//! holding `{"tool", "method", "parameters"}` of the step that produced it
//! (see [`call`]). The call is kept when the budget elides the output: what
//! the planner asked for is exactly what it needs to see to avoid asking
//! again. Its `parameters` go only if the calls alone overrun the budget.
//! `decision` and `call` are both screened (#700, #699) and become
//! `"[withheld: failed injection screen]"` on a block.
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
//! attachment or which bare line was the message id.
//! `prompts/agent_planner.md` documents these shapes, and
//! `the_planner_prompt_documents_every_outcome_shape` fails if the prompt stops
//! naming any key, reason or marker the renderer emits.

use serde_json::json;

use super::result_view;
use super::StepOutcome;
use crate::cassandra::types::Plan;

mod call;
mod sink;
use call::{apply_call_budget, call_cost, render_call, render_decision, Screened, CALL_KEY};
pub(crate) use sink::TIER_SINK;
use sink::{clamp_audit_label, sink_screen, without_screen_audit_fields, SinkBlock};
#[cfg(test)]
use sink::AUDIT_LABEL_MAX_CHARS;

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
/// (4 KiB before): enough for a ten-hit `mail.search` result to reach the
/// planner with every hit and every label, its snippets trimmed evenly.
/// Affordable because DGX plan latency is bound by generation, not context:
/// cutting `num_ctx` from 262144 to 65536 bought only about 10 %.
pub(crate) const STEP_OK_SUMMARY_MAX: usize = 16 * 1024;

/// Per-query byte budget contributed by each element of a `web.search_batch`
/// result. A batch is ONE step but carries N independent queries; scaling the
/// view's budget by the query count (see [`ok_summary_cap`]) lets the planner
/// see more than a flat [`STEP_OK_SUMMARY_MAX`] would — from six queries up,
/// since five earn 15 KiB, below the flat budget.
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
/// serialised step-outcome and step-call (#699) bytes. 96 KiB since #677: six
/// times the flat per-step budget, chosen so a fast-lane task
/// (`DEFAULT_MAX_PLANS_FAST` = 5, plus the forced-synthesis turn) of one
/// flat-budget step per plan is at most marginally pushed into elision by the
/// ceiling alone — marginally, not never, because each outcome's `{"status",
/// "output"}` wrapper and its call (up to ~1 KiB, since #699) are counted on
/// top of the 16 KiB view. Plans with several steps, `web.search_batch` steps
/// (up to 24 KiB each) or a long-lane task exceed it and elide, oldest first;
/// if the calls alone overrun it, the oldest calls then lose their
/// `parameters` (see `call::apply_call_budget`). In that case every elidable
/// output is already gone, the newest included: the second pass frees room but
/// does not bring one back, which keeps the two passes independent — a task
/// that fills the budget with calls alone is one repeating itself, and the
/// calls are what shows it that. What cannot be elided — error and withheld
/// outcomes, and each call's clamped `tool`/`method` — can still exceed the
/// budget on a pathological task. The planner's own `decision` strings are
/// neither clamped nor counted (#729), so the serialised prompt is modestly
/// larger than this value.
const PLANS_SUMMARY_BUDGET: usize = 96 * 1024;

const _: () = {
    // `usize::clamp` panics when its lower bound exceeds its upper bound, and
    // `ok_summary_cap` clamps between these two.
    assert!(STEP_OK_SUMMARY_MAX <= STEP_OK_BATCH_SUMMARY_MAX);
    // `result_view::render` guarantees its size bound only from MIN_VIEW_TOTAL up.
    // Module level, not `#[cfg(test)]`, which release builds strip.
    assert!(STEP_OK_SUMMARY_MAX >= result_view::MIN_VIEW_TOTAL);
    // Every string of a tool_host result's view lies inside the text its
    // source screen scanned (the catalogue, plus the guard model where a guard
    // tier is configured) only because a result larger than that scan window
    // never reaches this render whole: `tool_dispatch` stashes it behind a
    // handoff placeholder first, measuring serialised bytes. A string leaf's
    // flattened text is never longer than its serialised JSON, so a stash cap
    // within the scan cap keeps the whole view inside the scanned text. The
    // view trims strings evenly across the whole result rather than keeping a
    // prefix, so raising the stash cap past the scan cap would show the planner
    // a tail only this module's sink catalogue had checked (#702 review).
    // (`fetch_handoff` slices are screened by `fetch_screen`'s catalogue alone.)
    assert!(crate::handoff::DEFAULT_RESULT_BYTE_CAP <= crate::cassandra::injection_guard::SCAN_BYTE_CAP);
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

/// The `part` of a sink audit row: which piece of a plan record was blocked.
const PART_DECISION: &str = "decision";
const PART_CALL: &str = "call";
const PART_OUTCOME: &str = "outcome";

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

/// One rendered step outcome, its serialised size, whether the budget may
/// replace it with [`elided_outcome`], and what the sink screen blocked in it.
///
/// Built only through the four named constructors, each of which owns its
/// shape and its flag, so a withheld or failed step cannot be marked elidable
/// by a call site passing the wrong `bool`. `bytes` is computed once, at
/// construction, because it is the unit [`apply_summary_budget`] counts.
///
/// `Clone` so [`render_plans_summary`] can copy the memoized renders before the
/// per-call budget elision mutates them (see [`PlanRecord`]); `Debug` so
/// [`PlanRecord`] (a field of the `Debug`-deriving `TaskContext`) can derive it.
#[derive(Clone, Debug)]
struct RenderedStep {
    value: serde_json::Value,
    bytes: usize,
    elidable: bool,
    sink_block: Option<SinkBlock>,
}

impl RenderedStep {
    fn measured(value: serde_json::Value, elidable: bool, sink_block: Option<SinkBlock>) -> Self {
        let bytes = result_view::serialised_len(&value);
        Self { value, bytes, elidable, sink_block }
    }

    /// A successful step's view: the one shape the budget may elide.
    fn output(view: serde_json::Value) -> Self {
        Self::measured(json!({STATUS_KEY: STATUS_OK, OUTPUT_KEY: view}), true, None)
    }

    /// A successful step whose view the sink screen blocked. Tiny and
    /// load-bearing, so never elided.
    fn withheld(block: SinkBlock) -> Self {
        Self::measured(json!({STATUS_KEY: STATUS_OK, WITHHELD_KEY: WITHHELD_REASON}), false, Some(block))
    }

    /// A failed step, with `sink_block` set when the screen replaced its
    /// detail. Never elided: the error is the signal.
    fn err(code: &str, detail: &str, sink_block: Option<SinkBlock>) -> Self {
        Self::measured(json!({STATUS_KEY: STATUS_ERR, CODE_KEY: code, DETAIL_KEY: detail}), false, sink_block)
    }

    /// What [`apply_summary_budget`] puts in place of an elided output.
    fn elided() -> Self {
        Self::measured(elided_outcome(), false, None)
    }
}

/// A completed plan plus the screened, planner-bound render of its outcomes.
///
/// `rendered` is computed **once**, at the sole append point
/// ([`PlanRecord::new`], called from `inner_loop.rs`), rather than being
/// re-derived on every planner iteration. The sink screen
/// ([`render_step_outcome`] → [`sink_screen`]) is a pure, deterministic
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
    /// the only consumers are [`render_plans_summary`] and
    /// [`PlanRecord::sink_block_audit_payloads`], and the screened-once
    /// invariant depends on nothing else being able to inject an unscreened
    /// `RenderedStep`.
    rendered: Vec<RenderedStep>,
    /// The screened call that produced each outcome, index-aligned with
    /// `rendered` (#699). `None` only when outcomes outnumber steps (not
    /// expected), in which case the outcome is shown without a call.
    calls: Vec<Option<Screened>>,
    /// The screened `plan.decision` (#700). Private for the same reason.
    decision: Screened,
}

impl PlanRecord {
    /// Screen each outcome under its step's own guard profile and store the
    /// result. The `i`-th outcome is produced by `plan.steps[i]`, so
    /// `plan.steps[i].tool` selects the profile; a missing step (outcomes
    /// longer than steps — not expected) falls back to the fail-closed Strict
    /// default (`for_tool("")`) and is logged, because an outcome with no step
    /// is an invariant break that would otherwise show only as a missing
    /// `"call"` key in a prompt nobody is reading.
    ///
    /// Also renders the call each step made and the plan's own `decision`
    /// (#699, #700). Those two are planner-authored, so — unlike the outcomes —
    /// they are screened under a fixed `Strict` profile whatever tool the step
    /// names; see [`call`].
    ///
    /// Deterministic in `(plan, outcomes)` and free of I/O, which is what
    /// makes it safe to call again when a suspended run is restored.
    pub fn new(plan: Plan, outcomes: Vec<StepOutcome>) -> Self {
        if outcomes.len() > plan.steps.len() {
            tracing::warn!(
                outcomes = outcomes.len(),
                steps = plan.steps.len(),
                "plan record has more outcomes than steps; those outcomes reach the planner with no call"
            );
        }
        let rendered = outcomes
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let (tool, method) = step_tool_and_method(&plan, i);
                render_step_outcome(tool, method, o)
            })
            .collect();
        let calls = (0..outcomes.len()).map(|i| plan.steps.get(i).map(render_call)).collect();
        let decision = render_decision(&plan.decision);
        Self { plan, outcomes, rendered, calls, decision }
    }

    /// The `policy / injection.blocked` payloads for everything in this record
    /// the sink screen blocked, with `tier` [`TIER_SINK`]: the `decision` first
    /// (`"part": "decision"`, `step_index` null, `tool`/`method` empty), then
    /// each step in order, its `"call"` before its `"outcome"`.
    ///
    /// Since #677 the sink screens object keys, which no source screen sees, so
    /// it can block a result the source allowed; until #702's review nothing
    /// recorded that. Pure, so the one caller (the inner loop's push site) only
    /// writes rows. A resumed run rebuilds its records with [`PlanRecord::new`]
    /// and must **not** call this, or it would record the same blocks twice.
    ///
    /// Every field is bounded, as `post_process`'s row argues for its own, so the
    /// row keeps `tier` and `reason_codes` under the audit payload cap:
    /// `reason_codes` is drawn from the fixed catalogue, `body_sha256` is 64 hex
    /// characters, and `tool`/`method` — which the planner writes — are clamped
    /// to [`sink::AUDIT_LABEL_MAX_CHARS`]. The screened text is never written,
    /// **but** the row's `tool`/`method` can be part of it: on an `UNKNOWN_TOOL`
    /// outcome the detail echoes the invented tool name (the `step.unknown_tool`
    /// row already records that name in full), and a `"call"` row's screened
    /// text is `{tool, method, parameters}`, so its clamped labels are a head of
    /// what was blocked. A call's `parameters` are never written.
    pub fn sink_block_audit_payloads(&self, task_id: i64, plan_count: u32) -> Vec<serde_json::Value> {
        let row = |part: &str, step_index: Option<usize>, block: &SinkBlock| {
            let (tool, method) = step_index.map_or(("", ""), |i| step_tool_and_method(&self.plan, i));
            json!({
                "tool":          clamp_audit_label(tool),
                "method":        clamp_audit_label(method),
                "task_id":       task_id,
                "plan_count":    plan_count,
                "step_index":    step_index,
                "part":          part,
                "score":         block.score,
                "decision":      "block",
                "tier":          TIER_SINK,
                "reason_codes":  block.reason_codes,
                "body_sha256":   block.body_sha256,
                "body_byte_len": block.body_byte_len,
            })
        };
        let decision = self.decision.sink_block.as_ref().map(|b| row(PART_DECISION, None, b));
        let steps = self.rendered.iter().zip(&self.calls).enumerate().flat_map(|(i, (step, call))| {
            let call = call.as_ref().and_then(|c| c.sink_block.as_ref()).map(|b| row(PART_CALL, Some(i), b));
            let outcome = step.sink_block.as_ref().map(|b| row(PART_OUTCOME, Some(i), b));
            call.into_iter().chain(outcome)
        });
        decision.into_iter().chain(steps).collect()
    }

    /// The raw outcomes this record was built from, in step order.
    ///
    /// Callers read these to reconstruct or to classify, never to render. The
    /// suspend path (`scheduler::asks::resume_state_from`) serialises `plan`
    /// and these outcomes so the resumed run can rebuild the record with
    /// [`PlanRecord::new`]; `scheduler::conversation::record` (#701) reads them
    /// only to tell `Ok` from `Err`, never their text.
    /// **Not** for building a planner prompt: these are unscreened, and
    /// [`render_plans_summary`] is the only thing that turns an outcome into
    /// planner-bound text.
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
    // Saturating: `serialised_len` reports an unserialisable value as
    // `usize::MAX`, and a plain sum would panic in debug and wrap in release.
    let mut total = plans.iter().flatten().fold(0usize, |t, s| t.saturating_add(s.bytes));
    if total <= budget {
        return 0;
    }
    let marker = RenderedStep::elided();
    let mut elided = 0;
    for plan in plans.iter_mut() {
        for step in plan.iter_mut() {
            if total <= budget {
                return elided;
            }
            if step.elidable && step.bytes > marker.bytes {
                // Saturating for the same reason as the sum: once it has
                // saturated, the running total can be below a step's own size.
                total = total.saturating_sub(step.bytes - marker.bytes);
                *step = marker.clone(); // now minimal and non-elidable
                elided += 1;
            }
        }
    }
    elided
}

/// The tool and method of `plan`'s `i`-th step, or `("", "")` when outcomes
/// outnumber steps (not expected), which selects the fail-closed Strict profile.
fn step_tool_and_method(plan: &Plan, i: usize) -> (&str, &str) {
    plan.steps.get(i).map(|s| (s.tool.as_str(), s.method.as_str())).unwrap_or(("", ""))
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
            let v = without_screen_audit_fields(v);
            let view = result_view::render(&v, ok_summary_cap(method, &v));
            match sink_screen(tool, &result_view::screen_text(&view)) {
                Some(block) => RenderedStep::withheld(block),
                None => RenderedStep::output(view),
            }
        }
        StepOutcome::Err { code, detail } => {
            let shown = if detail.chars().count() > STEP_ERR_DETAIL_MAX {
                let truncated: String = detail.chars().take(STEP_ERR_DETAIL_MAX).collect();
                format!("{truncated}…")
            } else {
                detail.clone()
            };
            match sink_screen(tool, &shown) {
                Some(block) => RenderedStep::err(code, WITHHELD_MARKER, Some(block)),
                None => RenderedStep::err(code, &shown, None),
            }
        }
    }
}

/// Build the compact per-plan summary for the planner prompt: one
/// `{ "decision", "step_outcomes": [..] }` object per completed plan, each
/// outcome in one of the shapes described in the module docs.
///
/// The step outcomes, calls and decisions were **already screened once** when
/// each [`PlanRecord`] was constructed at push time (issue #344), so this
/// function performs *zero* injection screening — it clones the memoized
/// renders and calls, and runs the two size-budget passes over them (outputs
/// first, then `parameters`). The clone is required because both passes elide
/// in place and must not mutate the stored, immutable record.
pub(super) fn render_plans_summary(plans: &[PlanRecord]) -> Vec<serde_json::Value> {
    let mut rendered: Vec<Vec<RenderedStep>> =
        plans.iter().map(|r| r.rendered.clone()).collect();

    let mut calls: Vec<Vec<Option<serde_json::Value>>> = plans
        .iter()
        .map(|r| r.calls.iter().map(|c| c.as_ref().map(|c| c.value.clone())).collect())
        .collect();

    // Bound the accumulated size of the always-in-context summary (#339) in
    // two passes. The calls are what stop the planner repeating itself (#699),
    // so their bytes come off the budget first and the oldest successful-step
    // OUTPUTS are elided to fit what is left. Only if the calls alone still
    // overrun it do the oldest calls lose their `parameters` (a task of
    // several 64-step plans with near-cap parameters; found by review).
    let call_bytes = calls.iter().flatten().flatten().fold(0usize, |t, c| t.saturating_add(call_cost(c)));
    let elided = apply_summary_budget(&mut rendered, PLANS_SUMMARY_BUDGET.saturating_sub(call_bytes));
    let outcome_bytes = rendered.iter().flatten().fold(0usize, |t, s| t.saturating_add(s.bytes));
    let elided_calls = apply_call_budget(&mut calls, outcome_bytes.saturating_add(call_bytes), PLANS_SUMMARY_BUDGET);
    if elided + elided_calls > 0 {
        // Debug, not warn: this runs on every planner iteration once a long
        // task passes the budget, and the planner itself is told in-band.
        tracing::debug!(
            elided,
            elided_calls,
            budget = PLANS_SUMMARY_BUDGET,
            "plan summary elided older step outputs and/or call parameters"
        );
    }

    plans
        .iter()
        .zip(rendered)
        .zip(calls)
        .map(|((r, steps), calls)| {
            let step_outcomes: Vec<serde_json::Value> =
                steps.into_iter().zip(calls).map(|(s, call)| with_call(s.value, call)).collect();
            json!({
                "decision":      r.decision.value,
                "step_outcomes": step_outcomes,
            })
        })
        .collect()
}

/// `outcome` with the screened `call` that produced it under [`CALL_KEY`].
/// Every outcome shape is an object, so the insert always lands; a call of
/// `None` leaves the outcome as it was.
fn with_call(mut outcome: serde_json::Value, call: Option<serde_json::Value>) -> serde_json::Value {
    if let (Some(obj), Some(call)) = (outcome.as_object_mut(), call) {
        obj.insert(CALL_KEY.into(), call);
    }
    outcome
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod prior_call_tests;
