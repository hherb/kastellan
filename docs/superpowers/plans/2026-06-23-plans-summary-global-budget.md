# Global budget for `plans_so_far_summary` (#339) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the accumulated size of `TaskContext::plans_so_far_summary` so the always-in-context planner prompt can't grow without limit as successful step outputs pile up across iterations (#339).

**Architecture:** Extract the plan-summary rendering helpers out of the over-cap `inner_loop.rs` into a focused, pure `inner_loop/summary.rs` module, then add a global byte budget that elides the *oldest* successful-step output heads first when the total exceeds 32 KiB. Decisions, all error outcomes, and the most-recent plan's outputs are preserved.

**Tech Stack:** Rust (workspace crate `kastellan-core`), `serde_json`, existing `cassandra::injection_guard`.

## Global Constraints

- AGPL-3.0; AGPL-compatible deps only. No new dependency is needed.
- Pure functions in a focused module; I/O-free (rule 1).
- TDD: failing test first, then minimal implementation (rule 2).
- Junior-readable inline docs mandatory (rule 3).
- Keep files under 500 LOC where feasible; this work *reduces* `inner_loop.rs` (currently 574, +74 over cap) by moving helpers out (rule 4).
- All tests pass before committing (rule 6).
- Source the cargo env first in every shell: `source "$HOME/.cargo/env"`.
- Verification is macOS-local; pure Rust, no migration, no OS-gated code → DGX not required.

---

## File Structure

- **Create** `core/src/scheduler/inner_loop/summary.rs` — pure plan-summary rendering: the moved constants + `sink_screen_blocks` + `render_step_outcome`, plus the new `RenderedStep`, `apply_summary_budget`, and the `render_plans_summary` entry point. Owns its own `#[cfg(test)] mod tests` for the budget units.
- **Modify** `core/src/scheduler/inner_loop.rs` — declare `mod summary;`, re-export the two `pub(crate)` constants, delete the moved code, and make `plans_so_far_summary` a thin delegate.
- **Regression** `core/src/scheduler/inner_loop/tests.rs` — unchanged; its existing `plans_so_far_summary` / `render_sink_screen_*` tests are the behavior-preservation gate.

---

## Task 1: Move rendering helpers into `inner_loop/summary.rs` (pure refactor, no behavior change)

**Files:**
- Create: `core/src/scheduler/inner_loop/summary.rs`
- Modify: `core/src/scheduler/inner_loop.rs` (mod decl ~line 28; constants/helpers lines 53–136; `plans_so_far_summary` lines 144–161; `mod tests;` line 574)

**Interfaces:**
- Consumes: `super::StepOutcome` (the `pub enum` in `inner_loop.rs`), `crate::cassandra::types::Plan`, `crate::cassandra::injection_guard::{screen_with_profile, GuardProfile, InjectionDecision, extract_scannable_text}`.
- Produces (used by `inner_loop.rs`): `pub(super) fn render_plans_summary(plans: &[(Plan, Vec<StepOutcome>)]) -> Vec<serde_json::Value>`; re-exported `pub(crate) const STEP_ERR_DETAIL_MAX: usize`, `pub(crate) const STEP_OK_SUMMARY_MAX: usize`.

- [ ] **Step 1: Create `summary.rs` with the moved helpers (String-returning, behavior identical to today)**

Create `core/src/scheduler/inner_loop/summary.rs`:

```rust
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
```

- [ ] **Step 2: Wire `summary.rs` into `inner_loop.rs` and delete the moved code**

In `core/src/scheduler/inner_loop.rs`, add the module declaration next to the existing ones (after line 28 `mod invoke_expand;`):

```rust
mod summary;
```

Add the use/re-export near the other `self::` uses (after line 21):

```rust
use self::summary::render_plans_summary;
pub(crate) use self::summary::{STEP_ERR_DETAIL_MAX, STEP_OK_SUMMARY_MAX};
```

Delete the now-moved code: the entire block from the `STEP_ERR_DETAIL_MAX` doc comment (line 53) through the end of `render_step_outcome` (line 136) — i.e. both constants, `WITHHELD_MARKER`, `sink_screen_blocks`, and `render_step_outcome`.

Replace the body of `plans_so_far_summary` (lines 144–161) so the method becomes a thin delegate (keep the existing doc comment, but update the cross-reference):

```rust
    /// Compact summary of completed plans, for inclusion in the agent's
    /// input. Avoids dumping unbounded `serde_json::Value` blobs into the
    /// prompt; gives just enough for the agent to reflect — including each
    /// failed step's `code` + clamped `detail`. Rendering, screening, and the
    /// global size budget all live in [`summary::render_plans_summary`].
    pub fn plans_so_far_summary(&self) -> Vec<serde_json::Value> {
        render_plans_summary(&self.plans)
    }
```

- [ ] **Step 3: Build and run the existing tests as the regression gate**

Run:
```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core inner_loop 2>&1 | tail -20
```
Expected: PASS — all existing `inner_loop` tests green, including `task_context_plans_so_far_summary_is_compact`, `plans_so_far_summary_surfaces_ok_output_head`, `plans_so_far_summary_truncates_long_ok_output`, `plans_so_far_summary_truncates_long_error_detail`, and the `render_sink_screen_*` set. (`use super::*` in `inner_loop/tests.rs` still resolves `STEP_OK_SUMMARY_MAX` / `STEP_ERR_DETAIL_MAX` via the re-export.)

- [ ] **Step 4: Clippy clean**

Run:
```bash
cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -5
```
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/inner_loop.rs core/src/scheduler/inner_loop/summary.rs
git commit -m "refactor(scheduler): lift plan-summary rendering into inner_loop/summary.rs (#339)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Add the global summary budget (oldest Ok-heads elided first)

**Files:**
- Modify: `core/src/scheduler/inner_loop/summary.rs`

**Interfaces:**
- Consumes: the Task 1 module (`render_step_outcome`, `render_plans_summary`, `STEP_OK_SUMMARY_MAX`).
- Produces: `struct RenderedStep { text: String, elidable: bool }` (private to `summary`); `fn apply_summary_budget(plans: &mut [Vec<RenderedStep>], budget: usize) -> usize`; `const PLANS_SUMMARY_BUDGET: usize = 32 * 1024`; `const OK_ELIDED_MARKER: &str`. `render_step_outcome` changes return type `String` → `RenderedStep`. `render_plans_summary`'s public signature is unchanged.

- [ ] **Step 1: Write the failing budget unit tests**

Append a test module at the end of `core/src/scheduler/inner_loop/summary.rs`:

```rust
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
```

- [ ] **Step 2: Run the new tests to verify they fail to compile / fail**

Run:
```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core summary 2>&1 | tail -20
```
Expected: FAIL — `RenderedStep`, `apply_summary_budget`, `PLANS_SUMMARY_BUDGET`, `OK_ELIDED_MARKER` do not exist yet, and `render_step_outcome` still returns `String` (the `.text`/`.elidable` field accesses won't compile).

- [ ] **Step 3: Implement the budget — add the type, constants, and function; change `render_step_outcome` and `render_plans_summary`**

In `core/src/scheduler/inner_loop/summary.rs`:

Add below `WITHHELD_MARKER`:

```rust
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
```

Change `render_step_outcome` to return `RenderedStep` (set `elidable` per arm):

```rust
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
```

Change `render_plans_summary` to render → budget → build JSON:

```rust
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
```

- [ ] **Step 4: Run the new + existing tests**

Run:
```bash
cargo test -p kastellan-core summary 2>&1 | tail -20
cargo test -p kastellan-core inner_loop 2>&1 | tail -20
```
Expected: PASS — all 8 new `summary::tests` pass and every existing `inner_loop` test stays green (budget is a no-op for the small fixtures they use, so rendered output is byte-identical to today).

- [ ] **Step 5: Clippy clean + file-size check**

Run:
```bash
cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -5
wc -l core/src/scheduler/inner_loop.rs core/src/scheduler/inner_loop/summary.rs
```
Expected: no clippy warnings; `inner_loop.rs` dropped well below its previous 574 (the moved block was ~85 lines); `summary.rs` is a focused module under cap.

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/inner_loop/summary.rs
git commit -m "feat(scheduler): global byte budget for plans_so_far_summary (#339)

Elide oldest Ok-output heads first when the rendered summary exceeds
32 KiB, bounding the always-in-context planner prompt. Errors,
decisions, withheld markers, and the most-recent plan's outputs are
preserved.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Verification (whole change)

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core inner_loop 2>&1 | tail -20      # render + budget units green
cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -5
```

Pure Rust, no migration, no OS-gated code → the DGX native-Linux gate is not required for this change; the macOS unit run is the gate.

## Session-end (per project rules 7–9, outside this plan)

Update `docs/devel/handovers/HANDOVER.md` (move #339 to Recently completed, re-census the `inner_loop.rs` over-cap line, write a fresh Next TODO) and `docs/devel/ROADMAP.md`, then open a PR linking #339.

---

## Self-Review

- **Spec coverage:** module `summary.rs` (spec §Components) → Task 1. `RenderedStep` + `render_step_outcome` return-type change → Task 2 Step 3. `apply_summary_budget` + invariants (oldest-first, never-elide-errors/decisions/withheld, never-grow-tiny, idempotent) → Task 2 tests Steps 1 + impl Step 3. `PLANS_SUMMARY_BUDGET = 32 KiB` + `OK_ELIDED_MARKER` → Task 2 Step 3. Data flow (render → budget → JSON) → `render_plans_summary` Task 2 Step 3. Testing list (7 cases) → 8 `summary::tests` (the spec's 7 + a render-flag pin). All covered.
- **Placeholder scan:** no TBD/TODO; every code step shows full code; every command states expected output.
- **Type consistency:** `RenderedStep { text, elidable }`, `apply_summary_budget(&mut [Vec<RenderedStep>], usize) -> usize`, `render_plans_summary(&[(Plan, Vec<StepOutcome>)]) -> Vec<serde_json::Value>`, `OK_ELIDED_MARKER` / `PLANS_SUMMARY_BUDGET` / `STEP_OK_SUMMARY_MAX` / `STEP_ERR_DETAIL_MAX` used identically across Task 1 and Task 2. Re-export in Task 1 Step 2 keeps `inner_loop/tests.rs::use super::*` resolving the two `pub(crate)` constants.
