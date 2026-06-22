# Global budget for `plans_so_far_summary` (#339)

**Date:** 2026-06-23
**Issue:** [#339](https://github.com/hherb/kastellan/issues/339)
**Follow-up from:** #338 (feed successful tool output back to the planner)

## Problem

`render_step_outcome` (`core/src/scheduler/inner_loop.rs`) surfaces up to
`STEP_OK_SUMMARY_MAX` (4 KiB) of each successful step's output head into
`plans_so_far_summary`. `plans_so_far_summary` re-renders **every** plan's
**every** step on **every** planner iteration, and the result is part of the
always-in-context planner prompt.

The per-step head is bounded, but the *accumulated* total is not:
worst case ≈ `max_plans × steps_per_plan × 4 KiB`. `max_plans` is
operator-overridable to an arbitrary `u32` and there is no per-plan step cap,
so a long or pathological task can grow the planner prompt substantially. The
#338 change raised the per-step term ~2000× (from the old bare `"ok"`), so the
tail risk is new.

The common case (fast = 5 plans, few short outputs) is well within budget
today; this is hardening for the tail.

## Approach

**Oldest Ok-output heads elided first, against a 32 KiB total budget.**

Render the summary as today, then — only if the total exceeds the budget —
replace the **oldest** plans' successful-step output heads with a short marker,
walking oldest→newest until back within budget. This preserves the content the
planner reasons from next: each plan's `decision`, every error outcome (already
bounded at `STEP_ERR_DETAIL_MAX` and load-bearing for avoiding repeated denied
steps), and the **most-recent** plan's outputs (the ones it answers from).

Rejected alternatives:

- **Per-plan step-output cap** — shrinks even the most-recent plan's outputs,
  which are exactly the ones the planner needs to answer from.
- **Cap number of plans shown** — coarsest; drops older error history the
  planner uses to avoid repeating denied steps.

## Components

All new/moved logic lives in a new focused module
`core/src/scheduler/inner_loop/summary.rs`. `inner_loop.rs` is already 574 LOC
(+74 over the 500-LOC cap, flagged in the handover); the rendering helpers are
tightly coupled pure functions, so moving them out is both a clean
single-purpose module (rule 1) and progress against the over-cap file (rule 4).

### `RenderedStep` (new type)

```rust
struct RenderedStep {
    /// The rendered step string, as it will enter the planner prompt
    /// (`"ok: <head>"`, `"ok: <withheld marker>"`, or `"err: <CODE>: <detail>"`).
    text: String,
    /// True only for a successfully-screened `Ok` step carrying real output
    /// (the budget may elide it). False for errors and for the already-tiny
    /// withheld/empty `Ok` markers — those carry load-bearing signal and are
    /// never elided.
    elidable: bool,
}
```

### `render_step_outcome` (changed return type)

Returns `RenderedStep` instead of `String`. Behaviour is otherwise unchanged:

- `Ok` value, screen Allow → `RenderedStep { "ok: <head>"[…], elidable: true }`
- `Ok` value, screen Block → `RenderedStep { "ok: <WITHHELD_MARKER>", elidable: false }`
- `Err { code, detail }` → `RenderedStep { "err: <code>: <detail|WITHHELD>", elidable: false }`

`sink_screen_blocks` moves with it (unchanged).

### `apply_summary_budget` (new pure function)

```rust
/// Elide oldest `Ok`-output heads until the total byte size of all step
/// texts is within `budget`. Walks `plans` oldest→newest (and steps in
/// order), replacing each `elidable` head whose text is longer than the
/// marker with `OK_ELIDED_MARKER`, decrementing a running total, and
/// stopping the instant the total is within budget. No-op when already
/// within budget. Returns the number of steps elided.
fn apply_summary_budget(plans: &mut [Vec<RenderedStep>], budget: usize) -> usize
```

Invariants:

- Only `elidable` steps are touched — errors, decisions, and withheld/empty
  markers are never elided.
- A step is elided only when `text.len() > OK_ELIDED_MARKER.len()`, so eliding
  can never *grow* a tiny `Ok` (e.g. `"ok: 9"`).
- Oldest-first: `self.plans[0]` is the oldest plan (pushed first by the inner
  loop), so in-order iteration elides oldest first; the most-recent plan is
  elided last.
- Idempotent: re-running on an already-budgeted summary is a no-op. An elided
  step's text is exactly `OK_ELIDED_MARKER`, so the `text.len() > marker.len()`
  guard is false on the second pass — the marker is never re-elided.

### Constants

```rust
pub(crate) const STEP_ERR_DETAIL_MAX: usize = 200;   // moved, unchanged
pub(crate) const STEP_OK_SUMMARY_MAX: usize = 4 * 1024; // moved, unchanged
const WITHHELD_MARKER: &str = "[withheld: failed injection screen]"; // moved, unchanged

/// Total byte budget for the whole rendered `plans_so_far_summary`. Bounds
/// the always-in-context planner prompt as successful step outputs accumulate
/// across up to `max_plans` iterations (#339). Per-step heads stay
/// `STEP_OK_SUMMARY_MAX`; this caps the *accumulated* total over all plans and
/// steps. Covers the unbounded term (step-output text bytes), not the small
/// fixed per-plan JSON overhead (`decision`).
pub(crate) const PLANS_SUMMARY_BUDGET: usize = 32 * 1024;

/// Replacement for an elided `Ok`-output head (#339). A clear, structured
/// signal to the planner that an older step's output was dropped to keep the
/// prompt bounded — distinct from the injection `WITHHELD_MARKER`.
const OK_ELIDED_MARKER: &str = "ok: [output elided: summary budget]";
```

`STEP_OK_SUMMARY_MAX` / `STEP_ERR_DETAIL_MAX` stay `pub(crate)` and are
re-exported from `inner_loop` so the existing tests in `inner_loop/tests.rs`
(which reference them via `super::*`) are untouched.

## Data flow

`TaskContext::plans_so_far_summary`:

1. Render each plan's outcomes to `Vec<RenderedStep>` (via `render_step_outcome`,
   using `p.steps[i].tool` for the per-tool sink screen, as today).
2. `apply_summary_budget(&mut rendered, PLANS_SUMMARY_BUDGET)`.
3. Build the JSON: `{ "decision": p.decision, "step_outcomes": [text, …] }` per
   plan, from the (possibly elided) `RenderedStep.text` values.

The public shape of `plans_so_far_summary` (`Vec<{decision, step_outcomes}>`) is
unchanged; only the *content* of an over-budget summary changes.

## Error handling

Pure functions; no I/O, no fallible paths. A missing step (outcomes longer than
steps — not expected) still falls back to the fail-closed Strict default
(`for_tool("")`), exactly as today.

## Testing (TDD)

Unit (in `inner_loop/tests.rs` or a `summary` test module):

1. **Under budget → no-op:** a summary below `PLANS_SUMMARY_BUDGET` is rendered
   byte-identically to today (existing `plans_so_far_summary` tests already pin
   this; add a direct `apply_summary_budget` returns-0 assertion).
2. **Over budget → oldest elided first:** several plans each with a large Ok
   head; assert the oldest heads become `OK_ELIDED_MARKER` and the most-recent
   plan's head is intact, and the total is ≤ budget.
3. **Errors / decisions never elided:** an over-budget summary whose oldest
   steps are errors keeps the `err: …` strings; decisions are preserved.
4. **Withheld marker never elided:** an over-budget summary keeps the injection
   `WITHHELD_MARKER` signal (it is not `elidable`).
5. **Never grows a tiny Ok:** a small `"ok: 9"` head is left untouched even when
   the budget pass runs.
6. **Idempotent:** running the budget twice yields the same result.
7. **Worst case lands ≤ budget:** many plans × many large heads → the rendered
   total byte size is ≤ `PLANS_SUMMARY_BUDGET`.

Regression: all existing `inner_loop/tests.rs` `plans_so_far_summary` /
`render_sink_screen_*` tests stay green (they go through the public API).

## Verification

- `cargo test -p kastellan-core inner_loop` (unit + the moved render tests).
- `cargo clippy -p kastellan-core --all-targets -D warnings`.

Pure Rust, no migration, no OS-gated code → DGX not required for the unit gate.

## Out of scope (YAGNI)

- No change to `max_plans` or the per-step `STEP_OK_SUMMARY_MAX`.
- No operator-configurable budget env var — a compile-time constant like its
  siblings (`STEP_OK_SUMMARY_MAX`, `STEP_ERR_DETAIL_MAX`).
- #340 (human-readable `note` on the `tool_host` injection-blocked placeholder)
  is a separate sibling issue, not addressed here.
