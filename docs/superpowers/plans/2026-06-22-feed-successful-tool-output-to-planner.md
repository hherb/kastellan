# Feed Successful Tool Output Back to the Planner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render a successful tool step's already-injection-screened output (bounded to 4 KiB) into the planner prompt instead of discarding it as `"ok"`, so tool-using tasks stop looping to the plan cap (#338).

**Architecture:** A single render-function change in `core/src/scheduler/inner_loop.rs` plus a two-line planner-prompt update. The `StepOutcome::Ok(serde_json::Value)` arriving at `render_step_outcome` is *already* injection-screened (at the `tool_host` chokepoint) and ≤64 KiB (handoff stash), so the fix only extracts a bounded readable head via the existing `injection_guard::extract_scannable_text` helper and formats it `ok: <head>`. No new screening call, no handoff/dispatch change.

**Tech Stack:** Rust (`kastellan-core`), `serde_json`, the in-crate `cassandra::injection_guard` module. Tests are pure (no PG / no I/O), in the external test module `core/src/scheduler/inner_loop/tests.rs`.

## Global Constraints

- AGPL-3.0 project; AGPL-compatible deps only. No new dependencies in this change.
- Cross-platform Linux + macOS; this change is pure Rust, OS-agnostic — DGX not required for the unit tests (live acceptance is a separate operator step).
- TDD: failing test first, minimal impl, green, commit.
- Keep files under 500 LOC where feasible; `inner_loop.rs` is at 508 (within the documented ≤27-over deferral) — this adds ~10 LOC of production code, tests stay external.
- Build/test env: `source "$HOME/.cargo/env"` first (cargo not on non-interactive PATH).
- Branch: `feat/338-feed-tool-output-to-planner` (already created; spec committed as `d4afb99`).

---

### Task 1: Render successful step output into the plan summary

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (the `render_step_outcome` fn ~L67-80, add a const near `STEP_ERR_DETAIL_MAX` L59, update the `render_step_outcome` doc comment L61-66)
- Test: `core/src/scheduler/inner_loop/tests.rs` (update `task_context_plans_so_far_summary_is_compact` ~L204-235; add new tests)

**Interfaces:**
- Consumes: `StepOutcome::Ok(serde_json::Value)` / `StepOutcome::Err { code, detail }` (defined in `inner_loop.rs` L98-102); `crate::cassandra::injection_guard::extract_scannable_text(&Value, usize) -> (String, bool)` (L331).
- Produces: `pub(crate) const STEP_OK_SUMMARY_MAX: usize = 4 * 1024;` and the updated `fn render_step_outcome(o: &StepOutcome) -> String` whose `Ok` arm yields `"ok: <head>"` (or `"ok: <head>…"` when truncated). `plans_so_far_summary` shape is unchanged.

- [ ] **Step 1: Update the existing compact-summary test to expect the rendered output**

In `core/src/scheduler/inner_loop/tests.rs`, the Ok step in `task_context_plans_so_far_summary_is_compact` holds `StepOutcome::Ok(serde_json::json!("x"))`. `extract_scannable_text` emits string leaves, so `json!("x")` → `"x"` → renders `"ok: x"`. Replace the assertion + comment (currently L228-234):

```rust
    // An Ok step now surfaces its (already-screened, bounded) output
    // head so the agent can answer from it instead of re-running the
    // step; an Err step surfaces its code + detail (#337).
    assert_eq!(
        s[0]["step_outcomes"],
        serde_json::json!(["ok: x", "err: POLICY_DENIED: no"])
    );
```

- [ ] **Step 2: Add the new behavior tests (failing)**

Append to `core/src/scheduler/inner_loop/tests.rs`. These reference `STEP_OK_SUMMARY_MAX`, which does not exist yet, so they fail to compile (the TDD red). Add to the existing `use` of `super::*` items if needed — `STEP_OK_SUMMARY_MAX` is `pub(crate)` in the parent, reachable as `super::STEP_OK_SUMMARY_MAX` (the test module already uses `super::*`-style access for `STEP_ERR_DETAIL_MAX`; mirror it).

```rust
#[test]
fn plans_so_far_summary_surfaces_ok_output_head() {
    let mut c = ctx();
    c.plans.push((
        plan_with_decision("act"),
        vec![StepOutcome::Ok(serde_json::json!({
            "exit_code": 0,
            "stdout": "file1\nfile2\nfile3\n",
            "stderr": "",
        }))],
    ));
    let s = c.plans_so_far_summary();
    let surfaced = s[0]["step_outcomes"][0].as_str().unwrap();
    // The textual stdout is visible to the planner; it is no longer the
    // bare "ok" scalar.
    assert!(surfaced.starts_with("ok: "), "got: {surfaced}");
    assert!(surfaced.contains("file1"), "stdout not surfaced: {surfaced}");
    assert_ne!(surfaced, "ok");
}

#[test]
fn plans_so_far_summary_truncates_long_ok_output() {
    let mut c = ctx();
    let long_stdout = "y".repeat(STEP_OK_SUMMARY_MAX + 500);
    c.plans.push((
        plan_with_decision("act"),
        vec![StepOutcome::Ok(serde_json::json!({ "stdout": long_stdout }))],
    ));
    let s = c.plans_so_far_summary();
    let surfaced = s[0]["step_outcomes"][0].as_str().unwrap();
    assert!(surfaced.starts_with("ok: "), "got prefix: {surfaced}");
    // Bounded so a single chatty success can't blow up the always-in-context
    // prompt: "ok: " (4 chars) + at most STEP_OK_SUMMARY_MAX chars of head + the
    // trailing "…" marker.
    assert!(
        surfaced.chars().count() <= 4 + STEP_OK_SUMMARY_MAX + 1,
        "ok output not truncated: {} chars",
        surfaced.chars().count()
    );
    assert!(surfaced.ends_with('…'), "missing truncation marker: {surfaced}");
}

#[test]
fn plans_so_far_summary_ok_handoff_placeholder_surfaces_ref() {
    // An oversized result is stashed upstream and replaced with a small
    // handoff placeholder; rendering its head surfaces the summary_head +
    // handoff_ref so the planner can decide to fetch_handoff.
    let mut c = ctx();
    c.plans.push((
        plan_with_decision("act"),
        vec![StepOutcome::Ok(serde_json::json!({
            "handoff_ref": "h:abc123",
            "byte_len": 200000,
            "summary_head": "the first kilobyte of the big result",
            "truncated": true,
        }))],
    ));
    let s = c.plans_so_far_summary();
    let surfaced = s[0]["step_outcomes"][0].as_str().unwrap();
    assert!(surfaced.starts_with("ok: "), "got: {surfaced}");
    assert!(surfaced.contains("h:abc123"), "handoff_ref not surfaced: {surfaced}");
    assert!(surfaced.contains("the first kilobyte"), "summary_head not surfaced: {surfaced}");
}

#[test]
fn plans_so_far_summary_ok_injection_blocked_placeholder_surfaces_marker() {
    // Blocked content is replaced upstream (tool_host) with a tiny
    // placeholder; rendering must surface the marker and never raw blocked
    // text (proves the upstream screen carries through to the prompt).
    let mut c = ctx();
    c.plans.push((
        plan_with_decision("act"),
        vec![StepOutcome::Ok(serde_json::json!({
            "injection_blocked": true,
            "score": 0.91,
            "reason_codes": ["override"],
        }))],
    ));
    let s = c.plans_so_far_summary();
    let surfaced = s[0]["step_outcomes"][0].as_str().unwrap();
    assert!(surfaced.starts_with("ok: "), "got: {surfaced}");
    assert!(
        surfaced.contains("injection_blocked") || surfaced.contains("override"),
        "blocked marker not surfaced: {surfaced}"
    );
}
```

These tests use a `plan_with_decision(&str) -> Plan` helper to avoid repeating the 12-field `Plan` literal. The existing tests build the `Plan` inline; add the helper once near the top of the test module (after the `ctx()` helper):

```rust
fn plan_with_decision(decision: &str) -> crate::cassandra::types::Plan {
    crate::cassandra::types::Plan {
        context: "c".into(),
        decision: decision.into(),
        rationale: "r".into(),
        steps: vec![],
        result: None,
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        invoke_skill: None,
        python_skill: None,
    }
}
```

(If a `DataClass` import is not already in scope in the test module, it is — the existing inline `Plan` literals reference `DataClass::Public` at L213/L248, so the import is present.)

- [ ] **Step 3: Run the new tests to verify they fail (compile error: unknown `STEP_OK_SUMMARY_MAX`)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib scheduler::inner_loop::tests 2>&1 | tail -20`
Expected: compile error `cannot find value STEP_OK_SUMMARY_MAX in this scope` (the red).

- [ ] **Step 4: Add the const + new render arm + updated doc comment**

In `core/src/scheduler/inner_loop.rs`, add the const immediately after `STEP_ERR_DETAIL_MAX` (after L59):

```rust
/// Max chars of a *successful* step's output head surfaced back to the
/// planner in `plans_so_far_summary`. The value is already
/// injection-screened at the `tool_host` chokepoint (blocked content is
/// a tiny placeholder) and bounded to <=64 KiB by the handoff stash
/// before it reaches here; this cap only keeps the always-in-context
/// planner prompt small as successful outputs accumulate across up to
/// `max_plans` iterations. A truncated head gets a trailing `…`.
pub(crate) const STEP_OK_SUMMARY_MAX: usize = 4 * 1024;
```

Update the `render_step_outcome` doc comment (L61-66) so it no longer says the Ok step is the bare `"ok"` scalar:

```rust
/// Render one [`StepOutcome`] for the agent's plan summary. An `Ok`
/// step surfaces a bounded, already-screened head of its output as
/// `"ok: <head>"` so the agent can answer from the result instead of
/// re-running the step (the success-half of #338); an `Err` surfaces
/// its `code` and (length-clamped) `detail` as `"err: <CODE>: <detail>"`
/// (#337). Both prevent the `plan_iteration_cap_exceeded` loop.
```

Replace the `Ok` arm of `render_step_outcome` (L69):

```rust
        StepOutcome::Ok(v) => {
            // SAFETY (injection): `v` was injection-screened at the
            // tool_host chokepoint (blocked content is already a tiny
            // placeholder) and size-bounded to <=64 KiB by the handoff
            // stash before reaching here, so no re-screen is needed.
            // `extract_scannable_text` is the same char-boundary-safe
            // extractor `build_handoff_placeholder` uses; we only bound
            // further here for prompt-context size.
            let (head, truncated) =
                crate::cassandra::injection_guard::extract_scannable_text(
                    v,
                    STEP_OK_SUMMARY_MAX,
                );
            if truncated {
                format!("ok: {head}…")
            } else {
                format!("ok: {head}")
            }
        }
```

- [ ] **Step 5: Run the full inner_loop test module to verify green**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib scheduler::inner_loop 2>&1 | tail -25`
Expected: all `scheduler::inner_loop::tests::*` pass, including the updated `task_context_plans_so_far_summary_is_compact`, the 4 new tests, and the unchanged `plans_so_far_summary_truncates_long_error_detail` / `worker_rpc_error_surfaces_verbatim_in_plan_summary`.

- [ ] **Step 6: Clippy clean on the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -15`
Expected: no warnings.

- [ ] **Step 7: Commit**

```bash
git add core/src/scheduler/inner_loop.rs core/src/scheduler/inner_loop/tests.rs
git commit -m "feat(agent): surface successful step output to the planner (#338)

render_step_outcome's Ok arm now renders a bounded (STEP_OK_SUMMARY_MAX=4 KiB)
head of the already-injection-screened result as 'ok: <head>' instead of the
bare 'ok' scalar, so tool-using tasks see their output and stop re-running the
same step until plan_iteration_cap_exceeded. The value is screened at the
tool_host chokepoint and <=64 KiB via the handoff stash before reaching here, so
no new screening call is added.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Teach the planner that a successful step now carries its output

**Files:**
- Modify: `prompts/agent_planner.md` (L24 the `step_outcomes` description; the bullet block ~L172-177)

**Interfaces:**
- Consumes: nothing (prose). Produces: planner guidance that a successful `step_outcomes[j]` is `"ok: <output head>"` and should be read, not re-run.

- [ ] **Step 1: Update the `step_outcomes` value description (L24)**

Replace:

```
`plans_so_far[i].step_outcomes[j]` is `"ok"` or `"err"`; consult `blocks`
```

with:

```
`plans_so_far[i].step_outcomes[j]` is `"ok: <output head>"` (a bounded head
of the step's result) or `"err: <CODE>: <detail>"`; consult `blocks`
```

- [ ] **Step 2: Add a successful-output bullet beside the failure bullet (after L177)**

After the existing `- **A step that fails reports back a `code` and `detail`** …` bullet (ends "…unavailable." at L177), add a sibling bullet:

```
  - **A step that succeeds reports back a head of its output** in
    `plans_so_far[i].step_outcomes` as `"ok: <output head>"` (e.g. a
    command's stdout). Read it and answer the user's instruction from
    that output — do NOT re-issue the same successful step expecting to
    "see" the result again; you already have it. If the head was
    truncated (trailing `…`) and you need more, use the `handoff` /
    `fetch_handoff` mechanism rather than re-running the step.
```

- [ ] **Step 3: Sanity-check the prompt renders (no broken fences) and review wording**

Run: `sed -n '22,28p;170,185p' prompts/agent_planner.md`
Expected: the two edits read cleanly, code fences balanced.

- [ ] **Step 4: Commit**

```bash
git add prompts/agent_planner.md
git commit -m "docs(prompt): tell the planner a successful step carries its output head (#338)

Pairs with the render change: step_outcomes[j] is now 'ok: <output head>', so
the planner should answer from the output instead of re-running the step.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Workspace verification + handover

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md` (session wrap-up)

**Interfaces:** none (verification + docs).

- [ ] **Step 1: Targeted regression — the cap-pin tests that bounded plan iterations**

The #337 session noted `cli_ask_e2e` / `observation_capture` pin plan-iteration behavior. They do not assert the `"ok"` scalar (verified: only `inner_loop/tests.rs` did), but run them to confirm no behavioral pin broke.

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test cli_ask_e2e 2>&1 | tail -20`
Expected: pass (or the known pre-existing `ask_subprocess_fails_after_plan_iteration_cap` flake under heavy parallel load — re-run in isolation if it trips; this change cannot affect it).

- [ ] **Step 2: Full workspace build + clippy gate**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -D warnings 2>&1 | tail -15`
Expected: clean.

- [ ] **Step 3: Update HANDOVER.md + ROADMAP.md**

- Correct the stale "PR #337 (open)" → merged as `ff3e2f5`.
- Move #338 from "Next TODO ★ LEADING PICK" into "Recently completed", with: the root cause (`render_step_outcome` discarded the Ok value), the key finding (injection guard + 64 KiB stash already applied upstream, so no new screen needed), `STEP_OK_SUMMARY_MAX=4 KiB`, the planner-prompt update, the test-count delta, and the remaining live-DGX acceptance.
- Write a fresh "Next TODO (pick one)".
- Prune to stay concise.

- [ ] **Step 4: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: handover + roadmap — #338 success-output feedback shipped

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open PR linked to #338**

```bash
git push -u origin feat/338-feed-tool-output-to-planner
gh pr create --base main --title "feat(agent): feed successful tool output back to the planner (#338)" \
  --body "Closes #338. Success-half symmetric to PR #337's error-half. See docs/superpowers/specs/2026-06-22-feed-successful-tool-output-to-planner-design.md.

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

(If `git push` from the Mac is firewalled, relay via the DGX: `git format-patch origin/main..HEAD --stdout | ssh dgx 'cd ~/src/kastellan && git am' && ssh dgx 'cd ~/src/kastellan && git push'`, then `gh pr create` from the Mac.)

- [ ] **Step 6: Live acceptance (operator / DGX — the real gate)**

On the deployed DGX daemon, send a Matrix task: *"run /usr/bin/ls /tmp and tell me how many entries you saw."* Expected: the agent runs the step once, reads the listing from `step_outcomes`, and answers with a count — **without** looping to `plan_iteration_cap_exceeded`. Requires the new build deployed + daemon restart (the prompt is read at startup / per task; the binary carries the render change). Flag to the user if a hands-on deploy is needed — this is verification, not part of the merge gate.

## Self-Review

**Spec coverage:**
- Render the screened Ok value, bounded → Task 1 (const + Ok arm). ✓
- 4096 B cap, trust upstream screen (no re-screen) → Task 1 const = `4 * 1024`, SAFETY comment, no screen call. ✓
- Tests 1-6 from the spec → Task 1 Steps 1-2 (compact-summary update = small Ok; truncation; handoff placeholder; injection_blocked placeholder; Err regression pin already exists unchanged; `plans_so_far_summary` end-to-end = `surfaces_ok_output_head`). ✓
- Planner guidance line → Task 2. ✓
- Cap-pin updates → verified only `inner_loop/tests.rs` pinned `"ok"` (Task 1 Step 1); Task 3 Step 1 runs the e2e to confirm. ✓
- Verification (clippy, live DGX) → Task 3. ✓
- File-size note → covered by Global Constraints. ✓

**Placeholder scan:** No TBD/TODO; every code step shows full code. ✓

**Type consistency:** `STEP_OK_SUMMARY_MAX` (Task 1) used identically in tests + impl; `extract_scannable_text(&Value, usize) -> (String, bool)` matches the L331 signature; `plan_with_decision` helper defined once and reused; `render_step_outcome` Ok arm returns `String` matching the fn signature. ✓
