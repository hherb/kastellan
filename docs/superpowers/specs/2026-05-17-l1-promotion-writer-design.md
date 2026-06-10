# L1 promotion writer — first writer for `MemoryLayer::Index` rows

**Date:** 2026-05-17
**Status:** Design, ready for plan.
**Branch (proposed):** `feat/l1-promotion-writer`
**Pre-reqs (all shipped):**
- PR #68 (L1 memory-layer storage primitive, 2026-05-15) — `MemoryLayer::Index` enum variant + `insert_memory_at_layer` + `load_layer` + `load_l1` + `load_l1_default` + `0013/0014` migrations.
- PR #61 (Slice A audit-payload bump, 2026-05-15) — `agent/plan.formulate` carries the full `Plan` body, so any new `Plan` field auto-propagates into the audit stream.
- PR #67 (`ConstitutionalGuard` first real rule, 2026-05-15) — establishes that reviewer-Approve is what gates a Plan onto `Outcome::Completed`.
- PR #74 (prompt assembler L0 + L1, 2026-05-16) — `<l1_insights>` block renders L1 rows as bullets via `assemble_system_prompt`.
- PR #79 (recall-lane wiring, 2026-05-17) — `<recalled>` block ships; widens `assemble_system_prompt` to 4-arg.

## Why now

`MemoryLayer::Index` (L1) has shipped as a storage primitive (PR #68), a loader (`load_l1_default`, also PR #68), and a renderer (`<l1_insights>` block in the prompt assembler, PR #74). What's still missing is a **writer**. As of `main` at `a2e97a0`:

```
$ psql -d kastellan -c "SELECT COUNT(*) FROM memories WHERE layer = 1"
 count
-------
     0
```

Every production prompt that goes through the assembler today carries `<l1_insights>` empty (omitted via `if !l1.is_empty()`), and every `agent/plan.formulate` audit row carries `l1_count: 0`. The L1 lane is dead-on-arrival in production — there is no path that populates it.

The simplest defensible v1 closes this gap: two writers (one operator-explicit, one agent-raised), both idempotent on body SHA-256, both dropping rows into `MemoryLayer::Index` via the existing `insert_memory_at_layer` admin function. Each write emits a typed audit row so observation-phase SQL can grade what kinds of insights are being promoted, and across what tasks.

Until this slice lands, the `<l1_insights>` block is unreachable from any code path. After this slice lands, the operator (and the agent, opportunistically, on Approved task completion) can seed insights that show up in **every** subsequent plan iteration's prompt.

## Scope

In scope (this slice):

- New module [`core/src/memory/l1_promote.rs`](../../../core/src/memory/l1_promote.rs) — pure validator + async writer + audit-helper shape. Mirrors the [`core/src/memory/l0_seed.rs`](../../../core/src/memory/l0_seed.rs) precedent from PR #77.
- New `db::memories::delete_memory_at_layer(executor, id, layer) -> Result<bool, DbError>` async helper. DELETE with the `layer = $2` guard (defense against an operator deleting an L0 / L3 row via the L1 CLI path); fires the existing AFTER DELETE trigger so the journal entry lands in `deleted_memories`.
- New `Plan::l1_insight: Option<String>` field. `#[serde(default, skip_serializing_if = "Option::is_none")]` so existing fixtures stay byte-stable.
- New `Plan::completion_insight() -> Option<&str>` accessor that returns `Some(insight)` iff `self.is_terminal() && self.l1_insight.is_some()`. Encapsulates the agent-raised gate so the inner-loop call site stays small. Named `completion_insight` (noun form) rather than `is_completion_with_insight` to follow Rust convention that `is_*` methods return `bool`. (`Plan::is_terminal()` is the existing helper at [`core/src/cassandra/types.rs:142`](../../../core/src/cassandra/types.rs#L142) — checks `decision == "task_complete"`, `steps.is_empty()`, `result.is_some()`.)
- `agent_planner.md` prompt update: one paragraph teaching the model when to set `l1_insight`, plus `"l1_insight": null` in the JSON-schema example. The agent_prompts SHA-256 ledger records the new prompt on next daemon start (existing mechanism, no change).
- `InnerLoopResult.terminal_l1_insight: Option<String>` field. Populated only on `Outcome::Completed`.
- `core::scheduler::inner_loop::build_plan_formulate_payload` gains an `l1_insight` payload key (pulled from `plan.l1_insight`, explicit JSON `null` when absent — mirrors the `refused` precedent). **Audit-row bump: 20/21 keys → 21/22 keys, pure-additive.**
- `core::scheduler::runner::drain_lane` hook after `write_finalize_row`: if `result.terminal_l1_insight.is_some()`, call `promote_l1` with `L1Source::AgentRaised { task_id }`, then emit one `actor='scheduler' action='l1.promoted'` audit row. Best-effort posture (matches the chokepoint: `tracing::warn!` on Err, never abort finalize).
- Three new `core::scheduler::audit` action constants:
  - `ACTION_L1_ADDED = "l1.added"` (operator path via CLI)
  - `ACTION_L1_REMOVED = "l1.removed"` (operator path via CLI)
  - `ACTION_L1_PROMOTED = "l1.promoted"` (agent-raised path from `drain_lane`)
- One new pure helper `build_l1_write_payload(outcome: &L1WriteOutcome, source: &L1Source, body_sha256: &str) -> serde_json::Value`. Shared between operator + agent paths so the payload key-set stays in lockstep.
- New `core::cli_audit` helpers `l1_add_and_audit` + `l1_remove_and_audit`. Both emit `actor='cli'` audit rows. Mirrors the `tools_allowlist_{add,remove}_and_audit` precedent from PR #53.
- New `kastellan-cli memory l1 {add, list, remove}` subcommand tree, hand-rolled (no clap dep), mirroring the `tools allowlist` precedent.

Out of scope (filed as follow-ups, listed at the end of this doc):

- **Auto-eviction at write time.** No LRU/TTL cap on stored row count. Read-time `load_l1_default` cap (32 rows / 4 KiB) remains the only ceiling visible to the prompt.
- **Trust-tier differentiation in the prompt assembler.** Operator-curated + agent-raised rows render in the same `<l1_insights>` block.
- **Operator approval gate for agent-raised rows.** Agent self-distilled insights flow directly to L1 when `Outcome::Completed`.
- **L3 skill crystallisation.** The trajectory-distillation pattern in this slice sets the precedent that L3 will reuse; L3 is a separate slice.
- **Per-task multi-insight (`Vec<String>`).** v1 caps the agent at one insight per task via `Option<String>`.

## Shape decision: why a dedicated `l1_promote` module and not a method on `MemoryLayer`

The pure validator + async writer + audit shape exactly mirrors `l0_seed` (PR #77). Cross-reading those two modules at session-end (which the audit log makes practical via the SHA-256 ledger on the prompts) should reveal a single design idiom — "memory layer X has a curated TOML or operator+agent feed, idempotent on body SHA-256, with a typed audit row per write." Folding the writer onto the `MemoryLayer` enum would push CLI / audit / Plan-field concerns into `db/`, breaking the layering invariant in CLAUDE.md (Rust core sits above db).

Symmetric to the writer side: the read side (`load_l1`, `load_l0_active`) already lives in `core::memory::layers` + `core::memory::l0_seed::load_l0_active*`. New module `core::memory::l1_promote` is the right home.

## Validation rules for L1 body

Both paths (operator + agent) call into one `validate_l1_body(s: &str) -> Result<&str, L1Error>` helper. Rejections:

1. **Empty after trim** — accidentally adding `""` would write an empty bullet to every future prompt. Reject with `L1Error::Validation("body is empty after trim")`.
2. **Newlines (`\n` / `\r`)** — the renderer outputs `\n` after each row body, so a body with newlines would render as multi-line within a single bullet, breaking the implied "one bullet per row" contract. Reject with `L1Error::Validation("body contains newline")`. Justification: a future operator who needs multi-line content can split into multiple L1 rows.
3. **Other control chars (< 0x20, excluding the already-rejected `\t` / `\n` / `\r`)** — defensive; tab is also rejected (bullet indentation should be uniform).
4. **The literal substrings `<l1_insights>` and `</l1_insights>` (case-sensitive)** — threat-model scenario 6 defence. An agent-raised body that contained `</l1_insights>` followed by injected content would close the trust-marked block early. Reject with `L1Error::Validation("body contains reserved tag substring")`. Other XML-like tags (e.g. `<recalled>`) are NOT rejected here; if they become injection vectors in their own right, the L1 validator is not the right defence layer (sanitisation should live at render time for that).
5. **Length cap `L1_MAX_BODY_BYTES = 512`** — half of `L0_MAX_BODY_BYTES = 1024`. The L1 read cap is 4 KiB total across all rows; a 512-byte limit means ~8 rows of typical length fit in the prompt slice. Reject with `L1Error::Validation("body exceeds 512 bytes (N)")`.

The validator returns the **trimmed** slice on success so the writer never inserts leading/trailing whitespace. `compute_body_sha256` and `insert_memory_at_layer` both see the trimmed body.

## Dedup behaviour

Both paths share the same dedup discipline:

1. Compute `body_sha256 = hex(sha256(validated_body))` (lowercase 64-char; matches `l0_seed`).
2. `SELECT EXISTS (SELECT 1 FROM memories WHERE layer = 1 AND metadata->>'body_sha256' = $1) → existing_id`.
3. On hit, return `L1WriteOutcome::SkippedDuplicate(existing_id)`. No row written.
4. On miss, `insert_memory_at_layer(MemoryLayer::Index, body, build_l1_metadata(source, body_sha256, now), None)` → `Inserted(new_id)`.

Operator who runs `kastellan-cli memory l1 add 'X'` twice gets one row + two audit entries (the second carrying `action: "skipped_duplicate"`). Agent that re-emits the same insight on a second task gets one row + two audit entries with `action: "skipped_duplicate"` on the second.

This is the L0 idempotency pattern. Auto-eviction at write time is deliberately out of scope; the read-time cap is the only ceiling visible to the prompt. Filed as a follow-up.

## Agent-raised provenance enforcement

Mirrors the issue #71 / PR #72 precedent established 2026-05-16 for `ClassificationFloorSource::AgentRaised`:

- The agent's plan can supply `Plan.l1_insight: Option<String>`, but it cannot supply provenance. The producer-side audit-row payload key for `l1_insight` is the **plan field's value**, not a `source` claim.
- The **only** code path that constructs `L1Source::AgentRaised { task_id }` is `drain_lane` in `core::scheduler::runner`. Operators / future code paths that need to write through this provenance value will need a code change visible in a `grep`, not a wire-side payload key flip.
- The operator CLI path constructs `L1Source::Operator` exclusively. There is no `kastellan-cli memory l1 promote --as-agent-raised` flag; deliberately not added.

The audit row's `source` field is therefore always the writer's own claim, not a producer's claim. This matches the integrity discipline in [`core/src/scheduler/runner.rs`](../../../core/src/scheduler/runner.rs)'s `parse_classification_floor_source_from_payload` (which rejects producer-supplied `agent_raised`).

## Emit gate for the agent-raised path

Single gate: `Outcome::Completed`. Justification:

- `Outcome::Completed` is the reviewer-passed-plus-not-refused-plus-terminal path. The inner-loop construction in [`core/src/scheduler/inner_loop.rs:362-366`](../../../core/src/scheduler/inner_loop.rs#L362-L366) only emits `Outcome::Completed` when:
  - The reviewer returned `Verdict::Approve` **or** `Verdict::Advisory(...)` (both fall through; the Advisory variant just appends to `ctx.advisories`)
  - `plan.refused.is_none()` (a refusal short-circuits before the terminal check; see [`inner_loop.rs:348-358`](../../../core/src/scheduler/inner_loop.rs#L348-L358))
  - `plan.is_terminal()` (decision = `"task_complete"`, `steps.is_empty()`, `result.is_some()`)
  - The plan-iteration cap was not exhausted

  So `Outcome::Completed` is equivalent to "reviewer didn't Block/Escalate/ConstitutionalBlock + agent didn't refuse + plan terminated cleanly". For v1 we treat `Verdict::Advisory` as a green-light for L1 promotion — the reviewer chose to advise, not block; the plan still reached a terminal answer. If observation-phase data later shows that Advisory-gated insights pollute L1, the gate tightens to `Approve`-only as a follow-up.

- All other outcomes (`Failed`, `Blocked`, `Refused`, `Cancelled`) silently drop `terminal_l1_insight` if the plan had one. The inner loop only populates `terminal_l1_insight` on the `Outcome::Completed` arm.

- The `Plan::completion_insight()` accessor encapsulates the gate. It returns `Some(insight)` iff the plan would have produced `Outcome::Completed` AND carries `l1_insight`. The inner loop calls this exactly once at the point where it has both signals available.

The drain_lane hook never needs to inspect the Plan directly; it just reads `result.terminal_l1_insight`.

## Data flow

```
Operator path:

  kastellan-cli memory l1 add "foo"
    └── cli_audit::l1_add_and_audit(pool, body)
         └── memory::l1_promote::promote_l1(pool, body, L1Source::Operator)
              ├── validate_l1_body
              ├── compute_body_sha256
              ├── EXISTS-check
              └── insert_memory_at_layer(MemoryLayer::Index, ...) | skip
         └── audit::insert(pool, "cli", "l1.added", build_l1_write_payload(...))


Agent-raised path:

  RouterAgent::formulate_plan
    └── LLM emits Plan { decision: "task_complete", result: Some(_),
                          l1_insight: Some("learned X") }
    └── plan.formulate audit row: payload."l1_insight" = "learned X"

  inner_loop::run_to_terminal
    └── reviewer: Verdict::Approve
    └── plan.completion_insight() → Some("learned X")
    └── InnerLoopResult { outcome: Completed(result), terminal_l1_insight: Some("learned X"), ... }

  runner::drain_lane (after write_finalize_row)
    └── if let Some(insight) = result.terminal_l1_insight {
          memory::l1_promote::promote_l1(pool, insight, L1Source::AgentRaised { task_id: claimed.id })
          audit::insert(pool, "scheduler", "l1.promoted", build_l1_write_payload(...))
        }
```

## Files touched

NEW (5):
- `core/src/memory/l1_promote.rs` — pure validator + writer + helpers + module-internal tests.
- `core/tests/memory_l1_promote_e2e.rs` — DB integration tests.
- `core/tests/cli_memory_l1_e2e.rs` — CLI integration tests.
- This spec.
- The implementation plan that follows it.

MODIFIED (8):
- `db/src/memories.rs` — `delete_memory_at_layer` async helper.
- `core/src/memory/mod.rs` — `pub mod l1_promote;`.
- `core/src/cassandra/types.rs` — `Plan.l1_insight` field + `Plan::completion_insight()` accessor + a couple of new unit tests pinning the accessor.
- `prompts/agent_planner.md` — one paragraph + JSON-schema example update.
- `core/src/scheduler/inner_loop.rs` — `InnerLoopResult.terminal_l1_insight` + populate on the Completed arm + `build_plan_formulate_payload` adds `l1_insight` key + update the 4 pin tests + add 1 new pin test for the new key.
- `core/src/scheduler/runner.rs` — `drain_lane` hook after `write_finalize_row`.
- `core/src/scheduler/audit.rs` — 3 new action constants + `build_l1_write_payload` helper + new unit tests.
- `core/src/cli_audit.rs` — `l1_add_and_audit` + `l1_remove_and_audit` helpers.
- `core/src/main.rs` — `kastellan-cli memory l1 {add, list, remove}` subcommand wiring (this file already houses the hand-rolled subcommand tree).
- `core/tests/scheduler_inner_loop_e2e.rs` + `core/tests/cli_ask_e2e.rs` + `core/tests/router_agent_mock_e2e.rs` + `core/tests/scheduler_lanes_e2e.rs` — `FormulationMeta {}` literal updates (gain `terminal_l1_insight: None` default) and the audit-payload mid-tier gate test in `scheduler_inner_loop_e2e` gains assertions on the new `l1_insight` key.

DOCS (2):
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — session-end update.

## Audit-row contract (the headline)

| Actor       | Action         | Payload keys                                                            | When                                                                          |
|-------------|----------------|-------------------------------------------------------------------------|-------------------------------------------------------------------------------|
| `cli`       | `l1.added`     | `{source, body_sha256, action, memory_id?}`                             | `kastellan-cli memory l1 add` — Operator path, validation passes, EXISTS-check resolves |
| `cli`       | `l1.removed`   | `{memory_id, deleted}`                                                  | `kastellan-cli memory l1 remove` — Operator path, DELETE …WHERE id AND layer=1 |
| `scheduler` | `l1.promoted`  | `{source, task_id, body_sha256, action, memory_id?}`                    | `drain_lane` — Outcome::Completed + terminal Plan.l1_insight.is_some()       |
| `agent`     | `plan.formulate` | 20/21 → **21/22 keys** (gains `l1_insight: Option<String>`)            | Every plan formulation — pure-additive payload bump                          |

Where `action` is one of:
- `"inserted"` — new row at layer=1 (carries `memory_id`)
- `"skipped_duplicate"` — body_sha256 already present at layer=1 (carries the existing `memory_id`)

And `source` is the writer's own claim, never producer-supplied:
- `"operator"` — `L1Source::Operator`, written by `cli_audit::l1_add_and_audit`
- `"agent_raised"` — `L1Source::AgentRaised { task_id }`, written exclusively by `runner::drain_lane`

## Test budget

Estimate: **+28 to +35 tests**, workspace 674 → ~702-709.

- ~12 unit tests in `core/src/memory/l1_promote.rs::tests` (validator rejections + accepts trim; SHA-256 determinism; build_l1_metadata key-set; promote_l1 happy/dedup-existing/validation-rejected paths — these are unit-tier because the EXISTS check + insert can use the `PgPool` constructed by the test harness, same way `l0_seed` tests work).
- 4 unit tests in `core/src/scheduler/audit.rs::tests` covering `build_l1_write_payload` shape for each `(Source, Outcome)` combination.
- 2 unit tests in `core/src/cassandra/types.rs::tests` for `Plan::completion_insight` (positive + each negative-gate path).
- 6-8 DB integration tests in `core/tests/memory_l1_promote_e2e.rs`:
  - Operator add path → 1 L1 row + 1 audit row, `action: "inserted"`.
  - Operator add idempotency → second call returns SkippedDuplicate, 0 new rows, 1 new audit row with `action: "skipped_duplicate"`.
  - Operator add validation rejection (newline / `</l1_insights>` / over-length) → 0 rows, no audit row (validation errors do NOT audit; mirrors L0 precedent — the operator sees the CLI error on stderr).
  - Operator remove path → 1 row deleted, deleted_memories trigger journals it, 1 audit row.
  - Operator remove wrong-layer guard → trying to remove an L2 row through `remove_l1` returns `false`, row untouched (mirrors the SQL guard `WHERE layer = 1`).
  - `list_l1(false)` returns load_l1_default rows; `list_l1(true)` returns all layer=1 rows.
  - Agent-raised happy path via scripted `RouterAgent` mock → terminal Plan with `l1_insight` → 1 L1 row + 1 `actor='scheduler' action='l1.promoted'` audit row.
  - Agent-raised dedup → two tasks emit same insight → second task's audit row has `action: "skipped_duplicate"`.
- 3-4 CLI integration tests in `core/tests/cli_memory_l1_e2e.rs` exercising `kastellan-cli memory l1 add/list/remove` end-to-end (mirrors `cli_memory_l1_e2e` precedent from PR #53).
- 1-2 audit-payload pin updates in `scheduler_inner_loop_e2e` (mid-tier gate gains `l1_insight` key assertions; happy path + refusal path).

## What this slice deliberately does NOT do

Filed as follow-ups, separate slices each:

1. **Auto-eviction at write time.** No LRU/TTL cap on row count at layer=1. Read-time `load_l1_default` cap (32 rows / 4 KiB) remains the only ceiling visible to the prompt. When DB row count at layer=1 materially exceeds the read cap, a separate slice adds writer-side eviction with priority weighting (operator > agent-raised).
2. **Trust-tier differentiation in the prompt assembler.** Both operator-curated + agent-raised rows render in the same `<l1_insights>` block. A future hardening splits into `<l1_insights_operator>` + `<l1_insights_agent>` with documented trust tiers (threat-model scenario 6 explicitly anticipates this).
3. **Operator approval gate for agent-raised rows.** Agent self-distilled insights write directly to L1 on `Outcome::Completed`. Future hardening: queue agent-raised rows in a pending state for operator review before they reach `<l1_insights>`.
4. **L3 skill crystallisation.** Distil successful trajectories into parameterised JSON-RPC tool-call templates at layer=3. The L1 promotion writer here is the simpler precedent that L3 will follow.
5. **Per-task multi-insight (`Vec<String>`).** v1 caps the agent at one insight per task via `Option<String>`. A `Vec<String>` would broaden the surface; deferred until observation phase shows the cap is the constraint.
6. **CLI `--source agent_raised` flag.** Deliberately no operator-side way to forge `agent_raised` provenance. Mirrors the issue #71 enum-binding discipline.
7. **`memory l0 list` / `memory l3 list` / `memory l4 list` subcommands.** This slice adds `memory l1 {add, list, remove}` only. Symmetric L0/L3/L4 CLI surfaces follow the same shape if/when needed; L0 has its own startup-time seeding loader already, so a list subcommand against it is the next natural addition.

## Risk surface

- **Agent emits `l1_insight` on the bare-text-plan fallback.** Today the inner loop has a "decision: task_complete + empty steps + result.is_some()" fallback that accepts a raw text answer without strict schema. If the agent emits `l1_insight` on such a fallback, the gate behaves correctly (the accessor returns Some when all three conditions hold). No additional handling.
- **Bullet-rendering corner cases.** The validator rejects newlines and reserved-tag substrings; nothing else (e.g., literal `<` / `>` for non-tag-completing content) is rejected. The renderer is a simple bullet writer; future XSS-like injection vectors (if any) are a render-layer concern, not a write-layer one.
- **Disk growth.** No write-time cap means a chatty agent can pile up tens of layer=1 rows per task, of which only the newest ~8 surface in the prompt. Mitigated by: (a) `Outcome::Completed` gate (only successful tasks emit), (b) body_sha256 dedup, (c) `load_l1_default` read-time cap.
- **Audit-row volume.** Every operator add + every agent-raised completion writes one new audit row. At 100 tasks/day and ~30% completion rate, that's +30 `l1.promoted` rows/day in steady state. Comparable to today's `task.finalize` cardinality; no schema or index changes needed.
- **Race on dedup.** EXISTS-check + INSERT is two SQL statements. Two concurrent writers of the same body could both pass EXISTS and both INSERT. Mitigation: layer=1 has no UNIQUE constraint on `metadata->>'body_sha256'` (matches L0). Cost of the race is one redundant row, which the next `load_l1_default` read-cap silently drops. Not worth the schema complexity of a partial-unique index for v1.

## Open questions for the implementer

None blocking. The design above commits on:
- Two CLI subcommands + one daemon-side hook (not three subcommands + zero hooks; not zero subcommands + one hook).
- Dedup via `metadata->>'body_sha256'` JSONB lookup (not a separate column; mirrors how L0 dedups on `(l0_rule_id, body_sha256)`).
- Validation length cap = 512 bytes (between L0's 1024 and prompt-assembler's per-bullet practical limit).
- Audit-row action separation: three actions (`l1.added`, `l1.removed`, `l1.promoted`), not one umbrella `l1.write` action. SQL grouping queries on `action` stay precise.

If any of these turn out wrong during implementation, file the correction inline.

## Self-review checklist (done before commit)

- [x] No placeholders / TBD / TODO in body text.
- [x] Audit-row payload key counts cross-checked against `core/src/scheduler/inner_loop.rs::build_plan_formulate_payload` (20/21 + 1 = 21/22).
- [x] File-touch list cross-checked with grep (`cassandra/types.rs::Plan`, `inner_loop.rs::InnerLoopResult`, `runner.rs::drain_lane`).
- [x] No contradiction between "audit-row 'source' is writer-side, never producer" and "Plan.l1_insight is producer-supplied" — the producer supplies the **content**, the writer supplies the **provenance**.
- [x] Scope check: 28-35 tests + ~350 LOC of new module + 4 audit constants is one session's worth, not two. Confirmed sized like prior `feat/l0-seed-loader` (~660 LOC + 31 tests).
- [x] Cross-references to existing code use the `path#Lline` clickable-link shape.
