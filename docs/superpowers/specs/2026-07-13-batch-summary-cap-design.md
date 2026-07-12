# Query-count-scaled planner-summary cap for `web.search_batch`

**Date:** 2026-07-13
**Status:** design approved, pre-implementation
**Area:** `core/src/scheduler/inner_loop/summary.rs`, `core/src/workers/web_search.rs`
**Related:** batch web-search PR #443 (the feature this hardens); planner-feedback
arc #338/#441 (same "what reaches the planner" area)

## Problem

`web.search_batch{queries:[…]}` (PR #443) lets the planner run up to
`KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` (default 8) independent searches in **one**
plan step, so a multi-search task fits inside the 5-plan fast-lane cap
(`DEFAULT_MAX_PLANS_FAST`) instead of exhausting it.

But a whole batch is **one** `StepOutcome::Ok(value)`, and the planner-summary
renderer caps every Ok step's surfaced head at a flat
`STEP_OK_SUMMARY_MAX = 4 KiB` via `extract_scannable_text`
(`core/src/scheduler/inner_loop/summary.rs`). Measured against a default
8-query × 10-hit batch (faithful simulation of `extract_scannable_text`):

| snippet size | serialized batch | 4 KiB head | queries visible | hits visible |
|---|---|---|---|---|
| 80 B | 21.5 KiB | 4033 B | **2 / 8** | 17 / 80 |
| 150 B | 26.9 KiB | 4050 B | **2 / 8** | 13 / 80 |
| 250 B | 34.7 KiB | 4014 B | **2 / 8** | 10 / 80 |

So the planner synthesizes its answer (`plan.result.body`) from ~2 of the 8
queries. There is **no recovery path**: a typical batch (~21–35 KiB) is under the
64 KiB handoff-stash threshold (`handoff::DEFAULT_RESULT_BYTE_CAP`), so no
`handoff_ref` is minted and the dropped queries are simply invisible. The batch
beats the iteration cap but under-delivers the multi-search synthesis it exists
for.

**Security posture is unaffected** — nothing unscanned reaches the LLM (the
per-step head is still screened at the mandatory `sink_screen_blocks` sink); the
gap is purely *how much* of the batch the planner can see.

## Goal

Surface more of a batch's queries to the planner by giving a `web.search_batch`
step a **larger, query-count-scaled** summary cap, while keeping the shared
32 KiB `PLANS_SUMMARY_BUDGET` (the accumulated-across-plans total) unchanged.

Non-goals: no worker change, no handoff/dispatch change, no total-budget change,
no per-query structured render (see "Future option"). Scope is
`web.search_batch` only.

## Design

### 1. Distinguish batch from single search by method

`render_step_outcome` currently receives only the step's `tool` string, but
`web.search` and `web.search_batch` share `tool = "web-search"` — the
distinguishing signal is the **method**. `PlanRecord::new` already reads
`plan.steps.get(i)` (a `PlannedStep` carrying both `tool` and `method`), so thread
the method through:

```rust
// PlanRecord::new
let (tool, method) = plan.steps.get(i)
    .map(|s| (s.tool.as_str(), s.method.as_str()))
    .unwrap_or(("", ""));           // outcomes-longer-than-steps → fail-closed defaults
render_step_outcome(tool, method, o)
```

The batch method string is consolidated into one source of truth. Today
`core/src/workers/web_search.rs` carries a bare `"web.search_batch"` literal in
its `tool_docs()` `ToolDoc`; replace it with

```rust
/// JSON-RPC method the web-search worker exposes for batched search.
pub(crate) const WEB_SEARCH_BATCH_METHOD: &str = "web.search_batch";
```

referenced by both the `ToolDoc` and the new cap function. (Mirrors the earlier
consolidation of the batch-cap env var into a single const.)

### 2. Pure cap function

In `summary.rs`:

```rust
/// Max bytes of a *successful* single step's output head (unchanged default).
pub(crate) const STEP_OK_SUMMARY_MAX: usize = 4 * 1024;

/// Per-query byte budget contributed by each element of a web.search_batch head.
const BATCH_PER_QUERY_SUMMARY_BYTES: usize = 3 * 1024;

/// Hard ceiling on a web.search_batch step's head, so a large batch cannot claim
/// the entire PLANS_SUMMARY_BUDGET. 24 KiB = 8 (default max_batch) × 3 KiB.
const STEP_OK_BATCH_SUMMARY_MAX: usize = 24 * 1024;

/// Byte cap for an Ok step's surfaced head. A `web.search_batch` result carries
/// one element per query (`{results:[{query,results,count}|{query,error}]}`), so
/// its cap scales with the query count, clamped to
/// `[STEP_OK_SUMMARY_MAX, STEP_OK_BATCH_SUMMARY_MAX]`; every other method keeps
/// the flat single-step cap. Pure. A malformed/absent `results` array degrades to
/// the flat cap (never larger than a real batch would earn).
fn ok_summary_cap(method: &str, value: &serde_json::Value) -> usize {
    if method == crate::workers::web_search::WEB_SEARCH_BATCH_METHOD {
        let n = value.get("results").and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        (n * BATCH_PER_QUERY_SUMMARY_BYTES)
            .clamp(STEP_OK_SUMMARY_MAX, STEP_OK_BATCH_SUMMARY_MAX)
    } else {
        STEP_OK_SUMMARY_MAX
    }
}
```

The clamp lower bound (`STEP_OK_SUMMARY_MAX`) means a 0- or 1-query batch never
gets *less* than a single `web.search`; the upper bound caps an 8+-query batch at
24 KiB (3/4 of the total budget), leaving headroom for other steps.

`render_step_outcome`'s `Ok` arm uses `ok_summary_cap(method, v)` in place of the
flat constant; the screen (`sink_screen_blocks(tool, &head)`), the truncation `…`
suffix, and the `elidable: true` flag are unchanged.

### 3. Total budget unchanged

`PLANS_SUMMARY_BUDGET` stays 32 KiB. `apply_summary_budget` already elides the
**oldest** elidable Ok heads first to keep the accumulated total within budget. A
batch head (≤ 24 KiB, `elidable = true`) therefore survives while it is recent
(the common case — the planner just ran the batch) and elides only when it is the
oldest step *and* the total is over budget. No change to that pass.

### 4. Front-loading (accepted behavior)

`extract_scannable_text` is a single **global** byte cap over an ordered walk, not
a per-query allocation, so extraction is **front-loaded**: early queries fill the
cap first. For short/medium snippets `N × 3 KiB` exceeds the whole batch's
scannable size, so all N queries fit. For *verbose* snippets (~250 B) later
queries can still truncate once 24 KiB is reached (≈ 5–6 of 8 visible). This is a
strict improvement over today's ~2 and is consistent with how the single-search
cap already front-loads. Documented as-is; not special-cased.

### Future option (not in scope)

A true per-query render — iterate the `results` array, `extract_scannable_text`
each element under a `BATCH_PER_QUERY_SUMMARY_BYTES` sub-cap, join, then screen the
join once — would guarantee every query visibility regardless of snippet verbosity.
Deferred unless verbose-snippet batches prove to still truncate meaningfully in
practice. If pursued, file as a follow-up issue.

## Testing (TDD)

Pure unit tests for `ok_summary_cap` (no I/O):

- non-batch method → `STEP_OK_SUMMARY_MAX` (4 KiB)
- `web.search_batch` with 0 elements / missing `results` / non-array `results`
  → clamped up to `STEP_OK_SUMMARY_MAX`
- 1 query → `STEP_OK_SUMMARY_MAX` (clamp low: 3 KiB < 4 KiB)
- 2 queries → `6 KiB`
- 8 queries → `24 KiB`
- 16 queries → `24 KiB` (clamp high)

Render-level test:

- an 8-element `web.search_batch` `StepOutcome::Ok` surfaces a head **> 4 KiB**,
  while a `web.search` `StepOutcome::Ok` with the same underlying text stays
  **≤ 4 KiB + framing** (regression pin that the flat default is untouched for
  single search).

Existing `summary.rs` tests (budget elision, withheld/error non-elision) are
unaffected and must stay green.

## Verification

- `cargo test -p kastellan-core --lib scheduler::inner_loop::summary`
- `cargo test -p kastellan-core --lib` (regression)
- `cargo build --workspace` + `cargo clippy --workspace --all-targets -- -D warnings`

No PG / DGX / sandbox / seccomp surface is touched (pure in-crate summary logic +
a const relocation), so the DGX test baseline carries forward unchanged; no DGX
gate is owed. Dev-box (macOS Seatbelt) verification suffices.

## Files touched

- `core/src/scheduler/inner_loop/summary.rs` — cap consts + `ok_summary_cap` +
  `render_step_outcome` signature (`+method`) + `PlanRecord::new` call site +
  tests. Stays under 500 LOC (333 → ~375).
- `core/src/workers/web_search.rs` — `WEB_SEARCH_BATCH_METHOD` const; `ToolDoc`
  references it (byte-identical advertised string).
