# Rule-iteration harness for CASSANDRA review pipeline

**Date:** 2026-05-15
**Status:** Draft (awaiting operator review)
**Audience:** future Claude session implementing this; operator iterating on `ConstitutionalGuard` + `DeterministicPolicy` rule sets

## Why

The CASSANDRA reviewer (`ChainReviewStage` over `ConstitutionalGuard` + `DeterministicPolicy`) currently ships as always-`Approve` stubs. Real rules cannot be designed against speculation — they need empirical baseline data. PR #60 (2026-05-14) shipped that data: 7 captured plan iterations against `gemma4:26b-a4b-it-q8_0` under `tests/observation/captures/<id>/`.

What is missing is the mechanism that converts a captured plan into a test for a candidate rule: replay the captured plan through `ChainReviewStage::new(vec![Arc::new(ConstitutionalGuard), Arc::new(DeterministicPolicy)])`, compare the new verdict against the recorded baseline, and report deltas. Without this, the operator-iteration loop for real rules is "edit `review.rs` → bring up Postgres → start daemon → re-run the LLM against all 7 fixtures → eyeball the audit log." That loop costs minutes per iteration and consumes LLM cycles for what is fundamentally an offline replay.

This spec covers two slices, shipped as separate PRs in one session:

- **Slice A** — audit-payload bump: `agent/plan.formulate` audit rows must carry the full `Plan` JSON and the task's `classification_floor` so the reviewer pipeline can be replayed from captures alone.
- **Slice B** — the harness itself: a pure-Rust library + a thin `kastellan-cli` subcommand that loads captures, replays them through the production chain, and prints a per-fixture verdict-delta report.

After this spec ships and Slice A's audit-payload change merges, the operator recaptures (one-time action against their local LLM) and the harness becomes the iteration loop for every future real rule.

## Slice A — Audit-payload bump

### Scope

Add two new keys to the `agent/plan.formulate` audit-row payload, both pure-additive:

| Key                    | Type            | Source                                                       |
| ---------------------- | --------------- | ------------------------------------------------------------ |
| `plan`                 | JSON object     | `serde_json::to_value(plan)` — full `Plan` JSON              |
| `classification_floor` | string          | Task-level classification floor (e.g. `"Public"`)            |

The current 11-key shape becomes 13 keys. Downstream JSONB consumers that did not request these keys are unaffected.

### Where the change lands

`core/src/scheduler/inner_loop.rs::write_audit_plan_formulate` (lines 332–372 at `7588b9e`). The function already has `ctx: &TaskContext` and `plan: &Plan` in scope. The new fields slot into the existing `serde_json::json!` macro call. `classification_floor` reads from `ctx.classification_floor: DataClass` (the field exists on `TaskContext`; `runner.rs` parses it from `task.payload.get("classification_floor")` at claim time, defaulting to `DataClass::Public` when the producer omits it). The serialization shape is the same PascalCase string the existing `DataClass` `serde_with` derive emits.

The matching `db::audit::truncate_payload` envelope already handles oversized payloads via SHA-256 truncation at 4 KiB. A plan with 20+ act-steps could push past that ceiling; truncation is the correct safety net (forensics still works via the SHA prefix) and no replay-side change is needed because the harness reads captures from disk, not from the live audit table.

### Test-pin updates

Existing tests pin specific key values inside the `agent/plan.formulate` payload (`decision_kind`, `refused`, `plan_step_count`), not the total key count, so adding two new keys is **fully additive** — no existing assertion changes. The Slice A test work is purely *new* assertions on the new keys:

1. `core/tests/scheduler_inner_loop_e2e.rs` — happy-path assertion block (around line 440) gains a `payload["plan"]` round-trip assertion: deserialise the payload's `plan` field back into a `Plan` and `assert_eq!` it against the planner-stub-emitted plan, plus a `payload["classification_floor"]` string assertion (matches the `ctx.classification_floor` PascalCase serialisation, e.g. `"Public"`).
2. `core/tests/scheduler_inner_loop_e2e.rs` — refusal-scenario assertion block (around line 730) gains the same two assertions for the refusal case (plan must round-trip including its `refused: {…}` field; classification_floor reflects the test's task fixture).
3. New unit test in `core/src/scheduler/inner_loop.rs::tests` — pure helper test (or wherever `write_audit_plan_formulate`'s unit-test layer lives) that calls the writer against a fake pool, fetches the inserted row, and pins the round-trip of `payload["plan"]` and `payload["classification_floor"]`. If no fake-pool layer exists today (the writer is async and DB-touching), this test instead asserts the *payload value* the helper would emit by extracting a pure `build_plan_formulate_payload(...)` helper from the writer first (small refactor — keeps the SQL insert separate from the payload shape). The refactor is optional in Slice A; preferred over duplicating shape pins between writer and reader.

The `observation_capture.rs::extract_plans_returns_*` tests already exercise `extract_plans_from_audit_rows` against synthetic rows that include or omit `plan` in the payload (see `core/src/observation/capture.rs:207`). No change needed there — the helper's current branch already handles both shapes.

### What does NOT change in Slice A

- Existing capture files on disk (`tests/observation/captures/<id>/2026-05-14_*.json`). They retain `plan_json: null`. Slice B handles this gracefully via the missing-plan-body skip path. Operator recaptures to get the new shape — that recapture is operator action, not part of either slice.
- The capture-format schema version (`SCHEMA_VERSION = 2`). The capture format already accommodates the richer payload; only the writer side was missing it.
- `extract_plans_from_audit_rows`. The helper already reads `payload.get("plan")` and falls back to `null`; with Slice A's payload bump it will start emitting non-null `plan_json` automatically.
- Any other audit-row family. `cassandra:chain/verdict`, `scheduler/task.*`, `tool:*/shell.exec`, `cli/task.*` are all unchanged.

### Acceptance criteria

- `cargo test --workspace` green on Linux. Zero failures, zero warnings, zero `[SKIP]` lines.
- Test pins on the `agent/plan.formulate` payload shape updated to assert the two new keys are present and well-typed.
- A new payload-shape unit test in `core/src/scheduler/inner_loop.rs` (or wherever the writer's unit tests live) pinning that the `plan` field round-trips bytewise through `serde_json::to_value` + `from_value`.
- No new migration. No new dependencies.

### Risk surface

Tiny. Two pure-additive payload keys behind one production code path; five existing test pins extended.

## Slice B — Rule-iteration harness

### Module: `core::observation::replay`

New file `core/src/observation/replay.rs` (~300–400 LOC including tests). Pure-functional replay logic.

#### Public surface

```rust
/// Result of replaying one capture file against a candidate chain.
pub struct ReplayResult {
    pub fixture_id: String,
    pub fixture_summary: String,
    pub captured_at: String,
    pub llm_model: String,
    pub plans_replayed: u32,
    pub plans_skipped_missing_body: u32,
    pub per_plan: Vec<ReplayedPlan>,
}

/// Result of replaying one plan iteration through the candidate chain.
pub struct ReplayedPlan {
    pub iter: u32,
    /// Verdict recorded in the capture (the cassandra:chain/verdict row).
    /// `None` if the capture has no verdict row for this iteration.
    pub baseline_verdict: Option<String>,
    /// Verdict from the candidate chain. `None` when the plan body is
    /// missing (capture pre-dates Slice A's payload bump) and replay
    /// is skipped.
    pub new_verdict: Option<VerdictSnapshot>,
    /// True iff new_verdict and baseline_verdict differ in kind
    /// (detail strings ignored). Always false when the plan is skipped.
    pub is_delta: bool,
    /// Populated iff the plan was skipped (plan body missing from
    /// capture). The replay output still emits the per-plan row so
    /// the operator sees which fixtures need recapture.
    pub skipped_reason: Option<String>,
}

/// JSON-serializable snapshot of a `Verdict`. Pure projection; carries
/// the verdict kind plus its detail (if any) as a JSON value. The
/// constitutional_block variant projects to {principle, reason}.
pub struct VerdictSnapshot {
    pub kind: String, // "approve" | "advisory" | "escalate" | "block" | "constitutional_block"
    pub detail: Option<serde_json::Value>,
}

/// Pure: feed one capture + a chain, produce a result. Async because
/// ChainReviewStage::review is async (trait contract). No I/O.
pub async fn replay_capture(
    capture: &CaptureJson,
    chain: &ChainReviewStage,
) -> ReplayResult;

/// I/O: walk a directory, deserialize every <fixture_id>/<date>_<slug>.json
/// file. Returns one entry per file, sorted by (fixture_id, captured_at).
/// Errors aggregate; one bad file does not abort the walk.
pub fn load_captures_from_dir(
    dir: &Path,
) -> std::io::Result<Vec<LoadedCapture>>;

pub struct LoadedCapture {
    pub path: PathBuf,
    pub capture: CaptureJson,
}

/// Pure: format a Vec<ReplayResult> as an ASCII table. Stable column
/// widths; no terminal escapes.
pub fn format_report_table(results: &[ReplayResult]) -> String;
```

#### Key behaviors

- **Missing plan body** (`capture.plans[i].plan_json` is JSON `null`): the per-plan record gets `skipped_reason = Some("plan body missing; recapture against current daemon (Slice A's audit-payload v2)")`, `new_verdict = None`, `is_delta = false`. The aggregate `plans_skipped_missing_body` increments. The harness never silently fabricates a synthetic `Plan` from derived fields — that would let the operator design rules against fake inputs.
- **Delta semantics**: `is_delta = baseline_kind != new_kind` where kind is one of the five strings above. Detail strings are ignored — a reviewer might emit `"physical harm"` vs `"weapons"` for the same `constitutional_block` and both are equally "different from baseline of approve."
- **`baseline_verdict` resolution**: read straight from `CapturedPlan.verdict_today: Option<String>`. The capture format already emits these as lowercase verdict-kind strings (`"approve"` etc.). The harness compares lowercase-to-lowercase.
- **`ReviewStageContext` reconstruction**: from `CaptureJson`:
  - `task_id` → `capture.task_id`.
  - `instruction` → `capture.prompt`.
  - `classification_floor` → parsed from `capture.plans[i].data_ceiling` if present, falling back to `DataClass::Public`. Real Slice-A-era captures will have this in the audit-row payload directly (parse from `agent/plan.formulate.classification_floor` instead of `data_ceiling`); Slice B should prefer the audit-row value when present.
  - `plan_count` → `capture.plans[i].iter`.
- **Async surface**: `replay_capture` is `async` because `ReviewStage::review` is async (trait contract). No actual async work happens for stubs; real rules might use async DB queries against a future side-table.

### CLI subcommand: `kastellan-cli observation replay`

New top-level subcommand `observation`, with a single sub-subcommand `replay`. Pattern follows the existing `tools allowlist add|remove|list` (hand-rolled dispatcher, no `clap`).

```
kastellan-cli observation replay [--captures-dir PATH] [--model SLUG]
```

- `--captures-dir PATH`: defaults to `tests/observation/captures` resolved relative to `CARGO_MANIFEST_DIR` (for `cargo run`-style invocation) with a fallback to the CWD-relative path for an installed binary. The fallback is the same posture as `KASTELLAN_PROMPTS_DIR` in the daemon (see open issue #24 — operator wires this explicitly in production unit files).
- `--model SLUG`: filter to one model's captures. When omitted, every `<fixture_id>/*.json` is replayed; when present, only files matching `*_<SLUG>.json` are replayed. No "newest per fixture" auto-selection — explicit choice or replay-all.
- **Exit codes**:
  - `0` — replay completed; one or more results emitted. Deltas being present is *not* an error; that is the harness's reason to exist.
  - `1` — at least one capture file failed to load (file IO or JSON parse). Per-file errors print to stderr; the table still emits for the captures that loaded.
  - `2` — CLI argument parse error.
- **Output**: ASCII table to stdout (format below). No `--json` flag in Slice B — YAGNI until a CI consumer exists.

#### Report format

```
fixture                                  iter  baseline    new                         d?
─────────────────────────────────────── ────  ─────────── ─────────────────────────── ──
safe-001-echo-marker                       1  approve     approve                      .
p1-001-physical-harm-instructions          1  approve     [skipped: plan body missing] -
p2-001-impersonation                       1  approve     [skipped: plan body missing] -

3 plans across 3 fixtures . 0 replayed . 3 skipped (plan body missing)
0 deltas
```

Columns:
- `fixture` — `fixture_id`, left-aligned, truncated to 40 chars.
- `iter` — plan iteration index (1-based).
- `baseline` — `baseline_verdict` lowercase string, or `[none]` if missing.
- `new` — `new_verdict.kind`, with one of:
  - `constitutional_block(p=<n>)` — detail's principle field.
  - `escalate(<severity>)` — detail's severity if structured.
  - `block`, `advisory`, `approve` — bare kind.
  - `[skipped: ...]` — skipped reason if `skipped_reason.is_some()`.
- `d?` — delta indicator: `.` (no delta), `*` (delta), `-` (skipped, no comparison).

ASCII-only. Pure unit-tested via `format_report_table`.

#### The chain it runs

```rust
let chain = ChainReviewStage::new(vec![
    Arc::new(ConstitutionalGuard),
    Arc::new(DeterministicPolicy),
]);
```

Hard-coded production composition. The operator iterates by editing those struct bodies (`core/src/cassandra/review.rs`) — same shape the live daemon uses. No `--skip-stage-minus-1` flag; the harness measures what production would do.

### Integration test: `core/tests/observation_replay_e2e.rs`

Hand-crafts two synthetic `CaptureJson` files under a per-test temp directory (no on-disk fixture files committed — the test owns its scratch space). Two cases:

1. **`t1-approve-baseline-with-plan-body`** — one terminal `Plan { decision: "task_complete", refused: None, steps: [], result: Some({...}), data_ceiling: Public }` captured with `agent/plan.formulate.plan = <serialized>` + `verdict_today: "approve"`. Expected: `replay_capture` returns `plans_replayed: 1`, `per_plan[0].is_delta: false`, `new_verdict.kind: "approve"`.
2. **`t2-missing-plan-body`** — one `agent/plan.formulate` row WITHOUT the `plan` key (simulates pre-Slice-A capture). Expected: `plans_skipped_missing_body: 1`, `per_plan[0].skipped_reason.is_some()`, `is_delta: false`.

Pinned via the library API (`replay_capture` direct call). No CLI subprocess at the e2e layer — keeps the test fast.

### CLI integration test: `core/tests/observation_replay_cli_e2e.rs`

Spawns `kastellan-cli observation replay --captures-dir <tempdir>` against a hand-crafted tempdir. Asserts:
- Exit code 0.
- Stdout contains the expected fixture row.
- Stderr empty on the happy path.
- Exit code 1 when one capture file is malformed JSON (other captures still report; malformed prints to stderr).

Per-test tempdir cleanup via the existing `tests-common::PathGuard` pattern.

### Unit tests in `core/src/observation/replay.rs::tests`

~8–10 cases covering pure helpers:

- `VerdictSnapshot` round-trip serialization for each of the five verdict kinds.
- `is_delta` true/false matrix across `(baseline, new)` pairs (approve/approve, approve/block, none/approve, none/constitutional_block, …).
- `format_report_table` against a synthetic `Vec<ReplayResult>` — happy path, multi-iter fixture, skipped plan, mixed deltas.
- Edge: empty `Vec<ReplayResult>` formats to a header-only table with the trailing summary line.

### What this slice deliberately does NOT do

- **No real `ConstitutionalGuard` or `DeterministicPolicy` rule.** Stubs stay always-`Approve`. The harness mechanism ships; the first real rule is a follow-up.
- **No on-disk machine-readable diff format.** Text table only. JSON output via `--json` is YAGNI until a CI consumer exists.
- **No "fail on delta" exit code.** Deltas are the harness's reason to exist.
- **No multi-baseline diffing** ("compare gemma4 captures to qwen3 captures"). One model per run via `--model`; the operator can manually compare runs side-by-side.
- **No history of past replay runs.** Re-run on demand; results are emitted on stdout only.
- **No CI integration.** The replay command requires no daemon / DB / LLM but the harness operates on captures the operator must produce. Wiring it into CI would either re-run captures (defeats the offline replay design) or pin captures-as-fixtures into source (rapidly stale; captures change with every prompt or model update).

### File-size watch

Predicted ~300–400 LOC for `core/src/observation/replay.rs`, well under the 500-LOC soft cap. If `VerdictSnapshot` serialization grows complex (it shouldn't), split into `replay/snapshot.rs` + `replay/report.rs`. Not warranted today.

### Acceptance criteria for Slice B

- `cargo test --workspace` green on Linux. Zero failures, zero warnings, zero `[SKIP]` lines.
- New library tests in `core/src/observation/replay.rs::tests` (~8–10).
- New integration test `core/tests/observation_replay_e2e.rs` (2 cases).
- New CLI integration test `core/tests/observation_replay_cli_e2e.rs` (2 cases — happy + malformed JSON).
- New CLI subcommand `kastellan-cli observation replay` reachable from the dispatcher; help text reflects it.
- HANDOVER + ROADMAP updated.

## TDD ordering

Per CLAUDE.md rule #2, each task lands RED → GREEN → commit.

**Slice A**:
1. Update one existing payload-shape pin in `cli_ask_e2e.rs` to expect the two new keys — confirms RED.
2. Wire `plan` + `classification_floor` into `write_audit_plan_formulate` — GREEN.
3. Update remaining test pins (scheduler_inner_loop_e2e, observation_capture extract-plans cases).
4. New unit test in `inner_loop.rs::tests` pinning the round-trip shape.
5. `cargo test --workspace` clean.
6. Commit + open PR.

**Slice B** (after Slice A merges):
1. Write the library skeleton + unit tests (RED).
2. Implement pure helpers (`VerdictSnapshot`, `is_delta`, `format_report_table`) — GREEN.
3. Implement `replay_capture` against `ChainReviewStage` — GREEN.
4. Write integration test for `replay_capture` against the two synthetic captures — GREEN.
5. Write `load_captures_from_dir` — GREEN.
6. Wire CLI subcommand into `kastellan-cli`'s dispatcher.
7. Write CLI integration test — GREEN.
8. Update help text, HANDOVER, ROADMAP.
9. Commit + open PR.

## Open questions parked for follow-up

- **First real `ConstitutionalGuard` rule** — design + landed in a follow-up slice. The harness gives empirical baseline. The first real rule likely keys on the instruction (prompt text) via `ReviewStageContext.instruction` rather than the plan steps, because the captures show the agent self-refused 6/7 fixtures *before* emitting actionable plan steps. A prompt-level guard catches the cases where the agent failed to self-refuse.
- **Multi-LLM capture comparison** — once a second model's captures land (e.g. `nemotron3:33b-q8`), the harness could grow a `--compare gemma4-... nemotron3-...` mode that runs the same chain against both and diffs verdict outputs. Out of scope today.
- **Detail-string diffing** — currently `is_delta` ignores detail strings. A future "tighten delta detection" follow-up could surface "same kind, different reason" rows as a third indicator (e.g. `~` for partial match). Not needed for the first iteration loop.
- **CI integration** — flagged above as out-of-scope. Worth revisiting once captures stabilize (i.e. once a real `ConstitutionalGuard` rule has landed and the captures-as-fixtures shape is stable across re-runs).
