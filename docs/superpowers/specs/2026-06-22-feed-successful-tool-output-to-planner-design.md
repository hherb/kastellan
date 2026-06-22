# Feed successful tool output back to the planner (#338)

**Date:** 2026-06-22
**Issue:** [#338](https://github.com/hherb/kastellan/issues/338)
**Status:** design approved, pre-implementation

## Problem

After PR #337 fixed the *error* half of step-outcome feedback, tool-using tasks
still fail at the plan cap. The agent runs a step (e.g. `/usr/bin/ls /tmp`), it
**succeeds**, but the planner never sees the step's *output* — only the scalar
`"ok"` — so it re-issues the same step every iteration until
`plan_iteration_cap_exceeded`.

Live evidence (DGX, post-#337): a "run ls on /tmp and tell me how many entries"
task produced 5 identical plans, each `terminal_kind:ok`; the model's own
plan-2 prose said *"the output was not visible in the current context."*

## Root cause

`render_step_outcome` in `core/src/scheduler/inner_loop.rs` renders
`StepOutcome::Ok(serde_json::Value)` as the bare string `"ok"`, discarding the
worker's actual result `Value`. The symmetric *error* arm (`err: <CODE>:
<detail>`, capped at `STEP_ERR_DETAIL_MAX = 200`) was added in #337; the success
arm was left as the placeholder scalar.

## Key finding: the injection-guard requirement is already satisfied upstream

#338 flags that feeding worker stdout into the planner prompt is the
prompt-injection surface and asks that successful output be routed through the
injection guard and/or the handoff mechanism. Investigation shows **both are
already applied before the value reaches `render_step_outcome`:**

1. **`tool_host::dispatch`** runs `injection_guard::screen_with_profile` on every
   worker result. On `Block` it replaces the result with a tiny
   `{ "injection_blocked": true, ... }` placeholder. It screens the first
   `SCAN_BYTE_CAP` = 64 KiB.

2. **`tool_dispatch::dispatch_step`** stashes any `Ok(v)` larger than
   `DEFAULT_RESULT_BYTE_CAP` = 64 KiB into the per-task handoff cache, replacing
   it with a small `build_handoff_placeholder` value carrying a `summary_head`
   (`SUMMARY_HEAD_BYTES` = 1024 readable bytes) + a `handoff_ref` the planner can
   `fetch_handoff`.

Because `SCAN_BYTE_CAP == DEFAULT_RESULT_BYTE_CAP == 64 KiB`, **every `Ok(v)`
arriving at `render_step_outcome` is already fully injection-screened and
≤64 KiB** (≤64 KiB → screened whole; >64 KiB → stashed to a screened placeholder;
blocked → tiny placeholder). No new screening call is needed in the render layer;
the fix only renders the already-screened value, bounded for prompt-context size.

> **Post-review correction (2026-06-22):** the final whole-branch review found one
> hole in the reasoning above — the `fetch_handoff` branch
> (`tool_dispatch::dispatch_step`) returns a *slice* of a stashed body, and the
> `tool_host` screen only ever covered the body's first `SCAN_BYTE_CAP` = 64 KiB,
> so a fetch at `offset ≥ 64 KiB` could surface an **unscreened tail** into the
> prompt once the render change landed. The fix adds
> `tool_dispatch::fetch_screen::screen_fetched_data` (Strict / fail-closed), which
> screens each served slice at the dispatch chokepoint and replaces blocked
> `data` with a withheld-note placeholder. Known inherent limitation (parity
> with the existing `tool_host` screen, follow-up): injection text split across
> two fetch slices can evade single-slice screening.

> **Design revision (2026-06-23): single mandatory sink screen.** The "render layer
> stays screen-free, trust the source chokepoints" decision above was reconsidered
> and **reversed**. Relying on two source chokepoints (`tool_host` +
> `fetch_screen`) means the "nothing-unscreened-reaches-the-planner" invariant is
> *maintained by convention* — any future path producing a planner-bound
> `StepOutcome::Ok` must remember to screen. `render_step_outcome` is now the
> **single mandatory sink screen**: it re-screens the exact text it is about to
> place in the planner prompt — the `Ok` head AND the `Err` `detail` (the
> worker-influenced #337 surface; the `code` is an internal constant, always kept)
> — with the step's **own per-tool profile** (`GuardProfile::for_tool`, threaded
> from `plans_so_far[i].steps[j].tool`). Using the per-tool profile (not a blind
> Strict) makes the re-screen **idempotent** for legitimately-allowed content — so
> it cannot over-block a Relaxed-profile doc-fetch worker (issue #142) — while
> still guaranteeing the sink. On a Block the worker text is replaced with
> `WITHHELD_MARKER`. The source screens (`tool_host`, `fetch_screen`) **stay** —
> they protect non-planner consumers (audit, operator CLI) and do the heavy
> lifting — but the planner invariant is now *enforced at one point* rather than
> *relied upon* across many. Branch `feat/render-sink-injection-screen`.

## Design

Single change point: `render_step_outcome` (pure, sync, no I/O).

```rust
/// Max bytes of a successful step's output head surfaced back to the
/// planner in `plans_so_far_summary`. The value is already
/// injection-screened at the tool_host chokepoint and bounded to
/// <=64 KiB by the handoff stash before reaching here; this cap is
/// purely to keep the always-in-context planner prompt small as
/// successful outputs accumulate across up to `max_plans` iterations.
pub(crate) const STEP_OK_SUMMARY_MAX: usize = 4 * 1024;

fn render_step_outcome(o: &StepOutcome) -> String {
    match o {
        StepOutcome::Ok(v) => {
            // SAFETY (injection): `v` was screened at the tool_host
            // chokepoint (blocked content is already a tiny placeholder)
            // and size-bounded by the handoff stash. `extract_scannable_text`
            // is the same char-boundary-safe extractor
            // `build_handoff_placeholder` uses; we only bound further here.
            let (head, truncated) =
                crate::cassandra::injection_guard::extract_scannable_text(
                    v, STEP_OK_SUMMARY_MAX,
                );
            if truncated { format!("ok: {head}…") } else { format!("ok: {head}") }
        }
        StepOutcome::Err { code, detail } => { /* unchanged (#337) */ }
    }
}
```

`plans_so_far_summary` is unchanged in shape — each plan still maps to
`{ "decision", "step_outcomes": [<rendered strings>] }`; the `Ok` strings now
carry the output head instead of `"ok"`.

### Why no special-casing of placeholders

The handoff placeholder and the `injection_blocked` placeholder are both small
JSON objects. Rendering their head through `extract_scannable_text` surfaces
exactly what the planner needs:
- handoff placeholder → `summary_head` + `handoff_ref` (so the planner can
  decide to `fetch_handoff` for the full body), and
- `injection_blocked` placeholder → the `injection_blocked: true` marker (so the
  planner sees the content was withheld), **never raw blocked content**.

### Planner guidance

Add one line to `agent_planner.md`: a step's `step_outcomes` now carries
`ok: <output head>` for a successful step — read the output and answer from it
rather than re-running the step. This is the behavioral fix for the observed
looping.

## Testing (TDD, pure / no I/O)

New unit tests in the external test module:

1. `Ok` with a small value renders `ok: <json head>` (not `"ok"`).
2. `Ok` whose text head exceeds `STEP_OK_SUMMARY_MAX` gets the `…` truncation
   marker.
3. `Ok` holding a handoff placeholder renders its `summary_head` + `handoff_ref`.
4. `Ok` holding the `injection_blocked` placeholder renders that marker and no
   raw blocked content (proves the upstream screen carries through).
5. `Err` rendering unchanged (regression pin for #337).
6. `plans_so_far_summary` end-to-end contains the output head for an `Ok` step.

Cap-pin updates: `cli_ask_e2e` / `observation_capture` assertions that pin the
old `"ok"` scalar (touched by the #337 session) are updated to the new rendering.

## Verification

- Unit tests green; `cargo clippy --workspace --all-targets -D warnings` clean on
  touched crates.
- **Live acceptance (the real gate):** on the DGX, a "run `/usr/bin/ls /tmp` and
  tell me how many entries" task completes — the agent reads the listing and
  answers — without looping to the plan cap. Requires the deployed daemon; flag
  to the operator if a hands-on deploy/restart is needed.

## Non-goals / out of scope

- No change to the injection guard, the handoff cache, or the 64 KiB caps.
- No new screening call (the value is already screened).
- Model-side `num_ctx` / `OLLAMA_CONTEXT_LENGTH` perf tuning (separate, noted in
  HANDOVER).

## File-size note

`core/src/scheduler/inner_loop.rs` is 508 LOC (already +8 over the 500 cap,
within the documented ≤27-over deferral). This change adds ~10 LOC (const +
comment + render arm); tests live in the external test module. Stays within the
deferral band.
