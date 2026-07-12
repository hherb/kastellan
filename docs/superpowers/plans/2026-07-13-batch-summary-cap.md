# Query-count-scaled planner-summary cap for `web.search_batch` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a `web.search_batch` plan step a query-count-scaled planner-summary cap so the planner sees ~5–8 of a batch's queries instead of ~2, without changing the shared 32 KiB summary budget.

**Architecture:** The plan-summary renderer (`core/src/scheduler/inner_loop/summary.rs`) currently caps every successful step's surfaced head at a flat `STEP_OK_SUMMARY_MAX = 4 KiB`. Thread the step's `method` into `render_step_outcome` and select the cap via a new pure `ok_summary_cap(method, value)`: `web.search_batch` scales the cap with its query count (one result element per query), clamped to `[4 KiB, 24 KiB]`; every other method keeps the flat 4 KiB. The batch method string is consolidated into one `WEB_SEARCH_BATCH_METHOD` const in the web-search manifest module.

**Tech Stack:** Rust (`kastellan-core`), `serde_json`, existing `cargo test`/`clippy`.

## Global Constraints

- AGPL-3.0 project; AGPL-compatible deps only. **This change adds no dependency.**
- Cross-platform (Linux + macOS): the change is pure host-side summary logic — platform-neutral. Verification is Mac-only (no PG/DGX/sandbox/seccomp surface); the DGX baseline carries forward, no DGX gate owed.
- Keep functions pure where feasible; `ok_summary_cap` is pure (no I/O).
- Files stay under 500 LOC: `summary.rs` is 333 LOC → ~375 after; `web_search.rs` gains one const.
- TDD: failing test first, minimal impl, green, commit. Never `git add -A` — add named files only.
- Run cargo in the FOREGROUND (no backgrounded build/test waits).
- Spec: `docs/superpowers/specs/2026-07-13-batch-summary-cap-design.md`.
- Branch `feat/batch-summary-cap` already exists (off `main`@`e7928e8`) with the spec committed.

---

### Task 1: Consolidate the batch method string into a const

**Files:**
- Modify: `core/src/workers/web_search.rs` (add const near line 37; reference it at line 218)

**Interfaces:**
- Produces: `pub(crate) const WEB_SEARCH_BATCH_METHOD: &str = "web.search_batch"` — consumed by Task 2's `ok_summary_cap` and by this file's `tool_docs()` `ToolDoc`.

This is a behaviour-preserving refactor (the advertised method string is byte-identical), so its "test" is a clean build + the existing manifest tests staying green.

- [ ] **Step 1: Add the const**

In `core/src/workers/web_search.rs`, immediately after the `MAX_BATCH_QUERIES_ENV` const (currently line 37), add:

```rust
/// JSON-RPC method the web-search worker exposes for batched search
/// (`web.search_batch`). One source of truth for the string: the `tool_docs()`
/// advertisement below and the planner-summary cap
/// (`scheduler::inner_loop::summary::ok_summary_cap`) both reference it, so a
/// rename can't silently desync the advertised method from the cap that keys on
/// it. `pub(crate)` because `summary.rs` (same crate) consumes it.
pub(crate) const WEB_SEARCH_BATCH_METHOD: &str = "web.search_batch";
```

- [ ] **Step 2: Reference the const in the ToolDoc**

In the same file, in `tool_docs()`, change the batch `ToolDoc`'s method line (currently line 218) from:

```rust
            method: "web.search_batch",
```

to:

```rust
            method: WEB_SEARCH_BATCH_METHOD,
```

- [ ] **Step 3: Build and run the web-search manifest + tools tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib 'workers::web_search' && cargo test -p kastellan-core --lib worker_manifest registry_build`
Expected: PASS (byte-identical advertised string; no assertion changes).

- [ ] **Step 4: Clippy the crate**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib -- -D warnings`
Expected: clean (no warnings).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/web_search.rs
git commit -m "refactor(web-search): hoist web.search_batch method into WEB_SEARCH_BATCH_METHOD const"
```

---

### Task 2: Query-count-scaled summary cap in the renderer

**Files:**
- Modify: `core/src/scheduler/inner_loop/summary.rs` (consts + `ok_summary_cap` + `render_step_outcome` signature + `PlanRecord::new` call site + one existing test call site + new tests)

**Interfaces:**
- Consumes: `crate::workers::web_search::WEB_SEARCH_BATCH_METHOD` (Task 1).
- Produces: `fn ok_summary_cap(method: &str, value: &serde_json::Value) -> usize` (private, unit-tested); `fn render_step_outcome(tool: &str, method: &str, o: &StepOutcome) -> RenderedStep` (private, now takes `method`).

- [ ] **Step 1: Write the failing tests**

In `core/src/scheduler/inner_loop/summary.rs`, inside `mod tests` (after the existing `render_step_outcome_marks_ok_elidable_and_err_not` test), add:

```rust
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

        let batch = render_step_outcome("web-search", WEB_SEARCH_BATCH_METHOD, &StepOutcome::Ok(val.clone()));
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib scheduler::inner_loop::summary 2>&1 | tail -20`
Expected: FAIL to **compile** — `ok_summary_cap`, `BATCH_PER_QUERY_SUMMARY_BYTES`, `STEP_OK_BATCH_SUMMARY_MAX` are undefined, and `render_step_outcome` takes 2 args not 3. (A compile failure is the expected "red" here.)

- [ ] **Step 3: Add the cap consts and the pure `ok_summary_cap`**

In `summary.rs`, immediately after the existing `STEP_OK_SUMMARY_MAX` const (currently line 24), add:

```rust
/// Per-query byte budget contributed by each element of a `web.search_batch`
/// head. A batch is ONE step but carries N independent queries; scaling the head
/// cap by the query count (below) lets the planner see more than the single
/// query that a flat [`STEP_OK_SUMMARY_MAX`] head would surface.
const BATCH_PER_QUERY_SUMMARY_BYTES: usize = 3 * 1024;

/// Hard ceiling on a `web.search_batch` step's head, so a large batch cannot
/// claim the entire [`PLANS_SUMMARY_BUDGET`]. 24 KiB = 8 (the default
/// `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`) × [`BATCH_PER_QUERY_SUMMARY_BYTES`],
/// i.e. 3/4 of the 32 KiB total — leaving headroom for other steps, which the
/// oldest-first [`apply_summary_budget`] elides if the accumulated total is
/// exceeded.
const STEP_OK_BATCH_SUMMARY_MAX: usize = 24 * 1024;

/// Byte cap for a successful step's surfaced head, given its `method` and the
/// result `value`. A `web.search_batch` result is
/// `{results:[{query,results,count}|{query,error}]}` — one element per query — so
/// its cap scales with the element count, clamped to
/// `[STEP_OK_SUMMARY_MAX, STEP_OK_BATCH_SUMMARY_MAX]`; every other method keeps
/// the flat single-step cap. A malformed/absent `results` array counts as zero
/// elements and clamps up to the flat floor (never larger than a real batch
/// would earn). Pure — no I/O, deterministic in `(method, value)`.
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
```

- [ ] **Step 4: Thread `method` through `render_step_outcome`**

In `summary.rs`, change the `render_step_outcome` signature and its `Ok` arm. Replace the current function header + `Ok` extraction (currently lines 154–158):

```rust
fn render_step_outcome(tool: &str, o: &StepOutcome) -> RenderedStep {
    match o {
        StepOutcome::Ok(v) => {
            let (head, truncated) =
                crate::cassandra::injection_guard::extract_scannable_text(v, STEP_OK_SUMMARY_MAX);
```

with:

```rust
fn render_step_outcome(tool: &str, method: &str, o: &StepOutcome) -> RenderedStep {
    match o {
        StepOutcome::Ok(v) => {
            let cap = ok_summary_cap(method, v);
            let (head, truncated) =
                crate::cassandra::injection_guard::extract_scannable_text(v, cap);
```

(The `sink_screen_blocks(tool, &head)` screen, the `…` truncation suffix, the `elidable` flag, and the whole `Err` arm are unchanged. `tool` still selects the guard profile; `method` selects only the cap.)

Also update the function's doc comment (currently line 148–153) to mention the method-selected cap. Replace the doc line reading `/// exact text about to enter the prompt with `tool`'s guard profile. An `Ok`` region with a note that the `Ok` head length is `ok_summary_cap(method, …)` — e.g. append to that rustdoc block:

```rust
/// The `Ok` head is bounded by [`ok_summary_cap`] (`method`-selected: a
/// `web.search_batch` step earns a larger, query-count-scaled head).
```

- [ ] **Step 5: Update the `PlanRecord::new` call site**

In `summary.rs`, in `PlanRecord::new` (currently lines 91–98), replace:

```rust
            .map(|(i, o)| {
                let tool = plan.steps.get(i).map(|s| s.tool.as_str()).unwrap_or("");
                render_step_outcome(tool, o)
            })
```

with:

```rust
            .map(|(i, o)| {
                let (tool, method) = plan
                    .steps
                    .get(i)
                    .map(|s| (s.tool.as_str(), s.method.as_str()))
                    .unwrap_or(("", ""));
                render_step_outcome(tool, method, o)
            })
```

- [ ] **Step 6: Update the existing `render_step_outcome` test call site**

In `summary.rs` `mod tests`, in `render_step_outcome_marks_ok_elidable_and_err_not` (currently lines 320–330), the two `render_step_outcome` calls now need a `method` arg. Replace:

```rust
        let ok_step = render_step_outcome("shell-exec", &StepOutcome::Ok(serde_json::json!("hello")));
```

with:

```rust
        let ok_step = render_step_outcome("shell-exec", "shell.exec", &StepOutcome::Ok(serde_json::json!("hello")));
```

and replace:

```rust
        let err_step = render_step_outcome(
            "shell-exec",
            &StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "no".into() },
        );
```

with:

```rust
        let err_step = render_step_outcome(
            "shell-exec",
            "shell.exec",
            &StepOutcome::Err { code: "POLICY_DENIED".into(), detail: "no".into() },
        );
```

- [ ] **Step 7: Run the summary tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib scheduler::inner_loop::summary 2>&1 | tail -20`
Expected: PASS — all existing summary tests + the 5 new tests green.

- [ ] **Step 8: Run the full core lib + clippy (regression)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib 2>&1 | tail -15`
Expected: PASS (the `inner_loop::tests` single-search truncation test at `tests.rs:630` still holds — single `web.search`/shell heads are untouched).

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 9: Commit**

```bash
git add core/src/scheduler/inner_loop/summary.rs
git commit -m "feat(planner): query-count-scaled summary cap for web.search_batch

A whole batch is one Ok step, so the flat 4 KiB render head surfaced only
~2 of 8 queries to the planner. render_step_outcome now takes the step
method and, for web.search_batch, scales the head cap with the query count
(3 KiB/query, clamped [4,24] KiB) so ~5-8 queries reach the planner. Total
32 KiB summary budget unchanged; single web.search untouched."
```

---

### Task 3: Workspace verification

**Files:** none (verification only).

- [ ] **Step 1: Workspace build**

Run: `source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -5`
Expected: exit 0 (Task 1's `pub(crate)` const is reachable from `summary.rs`; nothing else in the workspace references these private items).

- [ ] **Step 2: Workspace clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5`
Expected: clean.

(No commit — this task only confirms the two prior commits build clean workspace-wide.)

---

## Self-Review

**Spec coverage:**
- §Design.1 (distinguish batch by method) → Task 1 (const) + Task 2 Steps 4–6 (thread method).
- §Design.2 (pure cap function) → Task 2 Step 3.
- §Design.3 (total budget unchanged) → no code change; verified by Task 2 Step 8 regression.
- §Design.4 (front-loading accepted) → documented in spec; no code.
- §Testing → Task 2 Step 1 (5 tests: 4 `ok_summary_cap` cases + 1 render).
- §Verification → Task 2 Steps 7–8 + Task 3.
- §Files touched → Tasks 1–2 exactly.

**Placeholder scan:** none — every step carries exact code/commands.

**Type consistency:** `ok_summary_cap(method: &str, value: &serde_json::Value) -> usize`, `render_step_outcome(tool, method, o)`, and `WEB_SEARCH_BATCH_METHOD: &str` are used identically across Tasks 1–2. Const names (`BATCH_PER_QUERY_SUMMARY_BYTES`, `STEP_OK_BATCH_SUMMARY_MAX`, `STEP_OK_SUMMARY_MAX`) match between the impl (Step 3) and tests (Step 1).
