# L3 skill crystallisation writer — first writer for `MemoryLayer::Skill` rows

**Date:** 2026-05-31
**Status:** Design, ready for plan.
**Branch (proposed):** `feat/l3-skill-crystallisation`
**Pre-reqs (all shipped):**
- PR #68 (memory-layer storage primitive, 2026-05-15) — `MemoryLayer::Skill = 3` enum variant + `insert_memory_at_layer` + `load_layer` already exist. `Skill` is reserved with no writer.
- The L1 promotion writer (spec `2026-05-17-l1-promotion-writer-design.md`, merged) — the direct precedent this slice copies one layer up: `Plan.l1_insight` → `InnerLoopResult.terminal_l1_insight` → `runner::drain_lane` hook → `MemoryLayer::Index` write + one typed audit row, with writer-claimed provenance. It also shipped the layer-guarded `db::memories::delete_memory_at_layer` we reuse here for `remove`.
- PR #61 (Slice A audit-payload bump, 2026-05-15) — `agent/plan.formulate` carries the full `Plan` body, so the new `Plan.l3_skill` field auto-propagates into the audit stream.
- PR #67 (`ConstitutionalGuard` first real rule, 2026-05-15) — reviewer-Approve is what gates a `Plan` onto `Outcome::Completed`, the emit gate this slice reuses.

## Why now

`MemoryLayer::Skill` (L3) has shipped as a storage primitive (the enum variant + `insert_memory_at_layer` + `load_layer`), but — exactly as L1 was before its writer landed — there is **no writer**. As of `main` at `98a5be0`:

```
$ psql -d hhagent -c "SELECT COUNT(*) FROM memories WHERE layer = 3"
 count
-------
     0
```

L3 is dead-on-arrival in production: no path populates it. The ROADMAP item ("L3 skill crystallisation — spec") names the goal — *distil successful multi-step trajectories into parameterised JSON-RPC tool-call templates stored as L3 memories* — and names the precedent: the L1 promotion writer, same `Plan` field → `drain_lane` hook → audit row pattern.

This slice closes the writer gap with the **smallest defensible v1**: on a successfully-completed task, the agent emits a parameterised skill template; `drain_lane` validates it, dedups on a canonical SHA-256, and stores it at `MemoryLayer::Skill` marked `trust: "untrusted"`. Each write emits a typed audit row so observation-phase SQL can grade what skills are being crystallised, across which tasks. A read-only operator CLI surface (`memory l3 list` / `remove`) gives visibility and a pruning lever during the observation phase.

Crucially, **crystallised skills are non-executable in this slice.** There is no path that invokes a stored skill template. Invocation is a major new attack surface (executing agent-authored tool-call sequences) that the ROADMAP deliberately sequences behind the **Skill trust enum** (`Untrusted | UserApproved | Pinned`); it is a separate later slice. This writer-only boundary mirrors the L1 precedent, which shipped storage + audit and left rendering to an already-existing path.

## Scope

In scope (this slice):

- New module [`core/src/memory/l3_crystallise.rs`](../../../core/src/memory/l3_crystallise.rs) — pure validator + async writer + audit-helper shape. Mirrors [`core/src/memory/l1_promote.rs`](../../../core/src/memory/l1_promote.rs) one layer up. The **only** writer for `MemoryLayer::Skill` rows.
- New structured candidate types on [`core/src/cassandra/types.rs`](../../../core/src/cassandra/types.rs): `L3SkillCandidate`, `L3Param`, `L3TemplateStep` (a flat `String` can't carry steps + params, so unlike `l1_insight` this is a struct).
- New `Plan.l3_skill: Option<L3SkillCandidate>` field. `#[serde(default, skip_serializing_if = "Option::is_none")]` so existing fixtures stay byte-stable.
- New `Plan::completion_skill() -> Option<&L3SkillCandidate>` accessor returning `Some(c)` iff `self.is_terminal() && self.l3_skill.is_some()`. Encapsulates the agent-raised gate so the inner-loop call site stays small. Mirrors `Plan::completion_insight()`.
- `agent_planner.md` prompt update: one paragraph teaching the model when and how to emit `l3_skill` (only on a terminal plan, abstracting the trajectory it just ran into a reusable template), plus `"l3_skill": null` in the JSON-schema example. The `agent_prompts` SHA-256 ledger records the new prompt on next daemon start (existing mechanism, no change).
- `InnerLoopResult.terminal_l3_skill: Option<L3SkillCandidate>` field. Populated only on the `Outcome::Completed` arm, **and only when the task executed ≥ 1 tool step** (the grounding gate — see below).
- `core::scheduler::inner_loop::build_plan_formulate_payload` gains a compact `l3_skill` payload key (`{name, step_count, param_count}` when present, explicit JSON `null` when absent — mirrors the `refused` / `l1_insight` precedent). **Pure-additive audit-row bump: +1 key.** (The exact running key count is re-derived from `build_plan_formulate_payload` at implementation time rather than hardcoded here, since intervening slices may have changed it.)
- `core::scheduler::runner::drain_lane` hook after the existing L1 hook: if `result.terminal_l3_skill.is_some()`, call `crystallise_l3` with `L3Source::AgentRaised { task_id }`, then emit one `actor='scheduler' action='l3.crystallised'` audit row. Best-effort posture (matches the L1 hook: `tracing::warn!` on Err, never abort finalize).
- Two new `core::scheduler::audit` action constants:
  - `ACTION_L3_CRYSTALLISED = "l3.crystallised"` (agent-raised path from `drain_lane`)
  - `ACTION_L3_REMOVED = "l3.removed"` (operator path via CLI)
- One new pure helper `build_l3_write_payload(outcome: &L3WriteOutcome, source: &L3Source, skill_name: &str, body_sha256: &str) -> serde_json::Value`.
- New `core::cli_audit::l3_remove_and_audit` helper (emits an `actor='cli'` row). Mirrors `l1_remove_and_audit`.
- New `hhagent-cli memory l3 {list, remove}` subcommand tree, hand-rolled (no clap dep), mirroring the `memory l1` precedent. **No `add`** — skills are agent-crystallised, never operator-authored.

Out of scope (filed as follow-ups, listed at the end of this doc):

- **Invocation.** No path executes a stored skill template. Needs the Skill trust enum + an approval gate first.
- **Skill trust enum (`Untrusted | UserApproved | Pinned`) + `memory l3 approve`.** This slice writes a forward-compatible `trust: "untrusted"` metadata key and nothing promotes it.
- **Recall surfacing.** No `<skills>` prompt block; crystallised skills do not yet reach the planner's prompt.
- **Tool/method existence validation against the `ToolRegistry`.** The writer stays pure (no registry dependency); existence is an invocation/approval-time concern.
- **Secret-plaintext scanning of step parameters.** The writer can't know which literals are secrets; deferred to the approval gate.
- **Mechanical trajectory grounding.** v1 trusts the agent-emitted template (gated to non-executable + untrusted). A hardening follow-up can capture the actual executed steps and diff them against the emitted template.
- **Per-task multiple skills (`Vec<L3SkillCandidate>`).** v1 caps the agent at one skill per task via `Option`.

## Shape decision: why a dedicated `l3_crystallise` module and why agent-emitted

**Module placement.** The pure validator + async writer + audit shape exactly mirrors `l1_promote` (and `l0_seed` before it). Folding the writer onto the `MemoryLayer` enum would push CLI / audit / `Plan`-field concerns into `db/`, breaking the layering invariant in CLAUDE.md (Rust core sits above db). `core::memory::l3_crystallise` is the symmetric home to `core::memory::l1_promote`.

**Agent-emitted, not mechanically extracted.** The raw material for a skill is the concrete `PlannedStep` sequence the task executed (`TaskContext.plans[*].0.steps`, each with its `StepOutcome`), with task-specific literal values baked in. Turning that into a *reusable* template means abstracting the task-specific values into named parameters — a semantic judgement the LLM is well-placed to make and a purely mechanical literal-diff is not (it misses derived values and over-matches common tokens). The L1 precedent made the same call: the agent emits the distilled artifact; the system validates and stores it. The integrity concern — that an agent-emitted template might not correspond to what actually ran — is real but **irrelevant while skills are non-executable**: it is caught later, at the approval/invocation gate, which does not exist yet. For a writer-only slice, agent-emitted is both the simplest and the truest mirror of L1.

## The `l3_skill` candidate

```rust
// core/src/cassandra/types.rs
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3SkillCandidate {
    /// snake_case identifier, e.g. "summarise_repo_readme".
    pub name: String,
    /// One-line human description; becomes the L3 memory `body`
    /// (the recall-matchable text once a recall slice lands).
    pub description: String,
    /// Declared parameters the template is abstracted over.
    pub parameters: Vec<L3Param>,
    /// The parameterised tool-call sequence (>= 1 step). Step
    /// `parameters` embed `{{param_name}}` placeholders.
    pub steps: Vec<L3TemplateStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3Param {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3TemplateStep {
    pub tool: String,
    pub method: String,
    pub parameters: serde_json::Value, // a JSON object; may embed {{placeholders}}
}

// on Plan:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub l3_skill: Option<L3SkillCandidate>,
```

The agent emits this on its **terminal** plan (`decision == "task_complete"`, empty `steps`, `result.is_some()`), looking back over the trajectory it just executed and abstracting it into a reusable template. The terminal plan's own (empty) `steps` are unrelated to the template's `steps`.

### Parameterisation syntax — `{{name}}` placeholders

Step `parameters` embed `{{param_name}}` mustache-style placeholders wherever a task-specific value appeared. The validator enforces a **closed-world invariant**: every `{{placeholder}}` token across all steps references a declared `parameters[].name`, **and** every declared parameter is referenced by ≥ 1 step (no dead params). This is a cheap, fully-checkable integrity rule that makes a template self-consistent without executing anything. Parsing is a one-line regex (`\{\{([a-z_][a-z0-9_]*)\}\}`).

**Why `{{name}}` and not `$name`.** A `$`-delimited placeholder collides with literal payload content. `shell-exec` is the canonical first tool, and `$repo_path` inside an argv is indistinguishable from a shell variable reference; `$` also appears in plenty of legitimate literal content (prices, regex, env-var references the agent genuinely wants to pass through). `{{ }}` almost never appears literally, so the closed-world scan stays unambiguous when (in a future slice) these templates are substituted at invocation time. The marginally simpler parse for `$name` does not outweigh that collision risk.

Example crystallised from a "summarise the README of repo X" task:

```json
{
  "name": "summarise_repo_readme",
  "description": "Read a repo's README and return a 3-bullet summary",
  "parameters": [{"name": "repo_path", "description": "absolute path to the repo"}],
  "steps": [{
    "tool": "shell-exec",
    "method": "shell.exec",
    "parameters": {"argv": ["cat", "{{repo_path}}/README.md"]}
  }]
}
```

## Validation rules for an L3 skill

One `validate_l3_skill(c: &L3SkillCandidate) -> Result<L3SkillCandidate, L3Error>` helper, same hardness as `validate_l1_body`. Rejections:

1. **name** — empty after trim → reject. Charset `[a-z0-9_]`, must start with `[a-z]` (a stable identifier for the future approval CLI and any future `<skills name=...>` render). Cap `L3_MAX_NAME_BYTES = 64`.
2. **description** — empty after trim → reject. Newlines (`\n`/`\r`) or other control chars (`< 0x20`, incl. `\t`) → reject (it becomes a single-line memory `body`). The literal substrings `<skills>` / `</skills>` (case-sensitive) → reject — defensive against the future `<skills>` render block being closed early by injected content (same threat-model scenario-6 logic as L1's `<l1_insights>`). Cap `L3_MAX_DESC_BYTES = 512` (matches `L1_MAX_BODY_BYTES`).
3. **parameters** — each `name` non-empty + snake_case + **unique** within the list; each `description` non-empty + capped at `L3_MAX_PARAM_DESC_BYTES = 256`. Count cap `L3_MAX_PARAMS = 16`.
4. **steps** — count in `1..=L3_MAX_STEPS` (`= 32`). The `1` lower bound is the grounding floor: a zero-step "skill" is meaningless. Each `tool` + `method` non-empty + charset-checked + capped at `L3_MAX_IDENT_BYTES = 64`; each step's `parameters` must be a JSON **object** (`serde_json::Value::Object`).
5. **placeholder closed-world** — collect every `{{name}}` token across all steps' serialised `parameters`. Every token must reference a declared parameter (`L3Error::Validation("undeclared placeholder {{x}}")`); every declared parameter must appear in ≥ 1 token (`L3Error::Validation("unused parameter x")`).
6. **total size** — the canonical-serialised template ≤ `L3_MAX_TEMPLATE_BYTES = 4096` (stays under the audit 4 KiB cap and a reasonable memory `body` envelope).

**Deliberately NOT validated here** (the writer stays pure; both move to the future approval/invocation gate, where the registry + secret vault are in scope): tool/method existence in the `ToolRegistry`; secret-plaintext scanning of step `parameters`. Both are named in the risk surface + follow-ups.

The validator returns the candidate with `name` / `description` trimmed and normalised, so the writer never stores leading/trailing whitespace. `body_sha256` and `insert_memory_at_layer` both see the normalised candidate.

## Dedup behaviour

1. Compute `body_sha256 = hex(sha256(canonical_json(candidate)))`, where `canonical_json` serialises with **deterministic key ordering** so two templates that differ only in JSON key order hash identically. (lowercase 64-char; matches the `l0_seed` / `l1_promote` SHA convention.)
2. `SELECT EXISTS (SELECT 1 FROM memories WHERE layer = 3 AND metadata->>'body_sha256' = $1) → existing_id`.
3. On hit, return `L3WriteOutcome::SkippedDuplicate(existing_id)`. No row written.
4. On miss, `insert_memory_at_layer(MemoryLayer::Skill, body = description, build_l3_metadata(source, candidate, body_sha256, now), None)` → `Inserted(new_id)`.

Two tasks that crystallise the same template get one row + two audit entries (the second carrying `action: "skipped_duplicate"`). This is the L1 idempotency pattern. No write-time eviction; observation-phase `list` + `remove` are the only ceiling in v1.

## Agent-raised provenance enforcement

Mirrors the L1 `L1Source::AgentRaised` discipline (itself the issue #71 / PR #72 precedent):

- The agent's plan supplies `Plan.l3_skill` (the **content**), but cannot supply provenance. The producer-side `plan.formulate` payload key for `l3_skill` is the plan field's compact summary, not a `source` claim.
- The **only** code path that constructs `L3Source::AgentRaised { task_id }` is `drain_lane` in `core::scheduler::runner`. A future operator/code path that needs to write through this provenance will need a code change visible in a `grep`, not a wire-side payload-key flip.
- There is no `memory l3 add` operator path and no `--as-agent-raised` flag. The audit row's `source` field is always the writer's own claim.

## Emit gate for the agent-raised path

Two conditions, both checked in the inner loop before populating `InnerLoopResult.terminal_l3_skill`:

1. **`Outcome::Completed`** — reviewer returned `Approve` or `Advisory`, the agent did not refuse, the plan terminated cleanly, and the plan-iteration cap was not exhausted. Same gate as L1 (see `2026-05-17-l1-promotion-writer-design.md` "Emit gate"). We treat `Advisory` as a green light for v1; if observation data shows Advisory-gated skills are noisy, the gate tightens to `Approve`-only as a follow-up.
2. **The task executed ≥ 1 tool step *cumulatively across its whole trajectory*** (the *grounding gate*) — a pure-text-answer task (terminal on the first plan, zero actions ever executed) has no trajectory to crystallise, so a skill emitted on it is ungrounded. The count is **cumulative over the task**, not the terminal iteration: the terminal plan itself always has empty `steps`, so a per-iteration check would always read zero. The signal already exists in `TaskContext.plans: Vec<(Plan, Vec<StepOutcome>)>` — the gate is "≥ 1 plan in the trajectory executed ≥ 1 step", i.e. `ctx.plans.iter().any(|(_, outcomes)| !outcomes.is_empty())`. The inner loop carries this as a running `total_steps_executed` accumulator (incremented in the existing step-execution loop) so the gate is a cheap `total_steps_executed >= 1` at the populate site rather than a re-scan. This is the one gate L3 adds over L1.

`Plan::completion_skill()` encapsulates condition (1)'s terminal half; the inner loop ANDs in condition (2)'s cumulative `total_steps_executed >= 1` at the populate site. All other outcomes (`Failed`, `Blocked`, `Refused`, `Cancelled`, `TimedOut`) leave `terminal_l3_skill` `None`. The `drain_lane` hook never inspects the `Plan` directly — it just reads `result.terminal_l3_skill`.

## Data flow

```
Agent-raised path (the only writer):

  RouterAgent::formulate_plan
    └── LLM emits terminal Plan { decision: "task_complete", result: Some(_),
                                  l3_skill: Some(L3SkillCandidate { .. }) }
    └── plan.formulate audit row: payload."l3_skill" = {name, step_count, param_count}

  inner_loop::run_to_terminal
    └── reviewer Approve/Advisory + not refused + terminal + total_steps_executed >= 1
    └── plan.completion_skill() → Some(candidate)   (AND total_steps_executed >= 1)
    └── InnerLoopResult { outcome: Completed(_), terminal_l3_skill: Some(candidate), .. }

  runner::drain_lane (after write_finalize_row, after the existing L1 hook)
    └── if let Some(c) = result.terminal_l3_skill {
          memory::l3_crystallise::crystallise_l3(pool, c, L3Source::AgentRaised { task_id: claimed.id })
            ├── validate_l3_skill   (structural + placeholder-integrity + reserved-tag + caps)
            ├── canonical_json → body_sha256
            ├── EXISTS-check at layer=3 on metadata->>'body_sha256'      (dedup)
            └── insert_memory_at_layer(Skill, body = description,
                  metadata = { template, trust: "untrusted", source, task_id, body_sha256 }) | skip
          audit::insert(pool, "scheduler", "l3.crystallised", build_l3_write_payload(..))
        }   // best-effort: tracing::warn! on Err, never abort finalize


Operator path (read-only + prune):

  hhagent-cli memory l3 list            → load_layer(pool, MemoryLayer::Skill, ..)   (no audit row)
  hhagent-cli memory l3 remove <id>
    └── cli_audit::l3_remove_and_audit(pool, id)
         └── db::memories::delete_memory_at_layer(pool, id, MemoryLayer::Skill)   (layer-guarded; fires deleted_memories trigger)
         └── audit::insert(pool, "cli", "l3.removed", { memory_id, deleted })
```

## Files touched

NEW (5):
- `core/src/memory/l3_crystallise.rs` — pure validator + writer + helpers + module-internal unit tests.
- `core/tests/memory_l3_crystallise_e2e.rs` — DB integration tests (agent-raised path).
- `core/tests/cli_memory_l3_e2e.rs` — CLI integration tests.
- This spec.
- The implementation plan that follows it.

MODIFIED (~10):
- `core/src/memory/mod.rs` — `pub mod l3_crystallise;`.
- `core/src/cassandra/types.rs` — `L3SkillCandidate` + `L3Param` + `L3TemplateStep` types, `Plan.l3_skill` field, `Plan::completion_skill()` accessor, + unit tests pinning the accessor gate and a serde round-trip.
- `prompts/agent_planner.md` — one paragraph + `"l3_skill": null` in the JSON-schema example.
- `core/src/scheduler/inner_loop.rs` — `InnerLoopResult.terminal_l3_skill` + populate on the `Completed` arm under the cumulative `total_steps_executed >= 1` gate (new running accumulator in the step loop) + `build_plan_formulate_payload` adds the compact `l3_skill` key + pin-test updates + 1 new pin for the new key.
- `core/src/scheduler/runner.rs` — `drain_lane` hook after the existing L1 hook (`write_l3_crystallised_row` helper, mirroring `write_l1_promoted_row`).
- `core/src/scheduler/audit.rs` — `ACTION_L3_CRYSTALLISED` + `ACTION_L3_REMOVED` constants + `build_l3_write_payload` helper + unit tests.
- `core/src/cli_audit.rs` — `l3_remove_and_audit` helper.
- `core/src/bin/hhagent-cli/memory_l3.rs` (NEW sibling to `memory_l1.rs`) + `core/src/bin/hhagent-cli/main.rs` dispatch wiring for the `memory l3 {list, remove}` subtree.
- Scheduler e2e test literals (`core/tests/scheduler_inner_loop_e2e.rs`, `cli_ask_e2e.rs`, `router_agent_mock_e2e.rs`, `scheduler_lanes_e2e.rs`) — `InnerLoopResult { .. }` / `FormulationMeta { .. }` literals gain `terminal_l3_skill: None`; the mid-tier audit-payload gate test in `scheduler_inner_loop_e2e` gains an assertion on the new `l3_skill` key.

DOCS (2):
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — session-end update.

No new `db/` helper is required: `insert_memory_at_layer`, `delete_memory_at_layer` (layer-guarded, shipped by the L1 slice), and `load_layer` all already exist.

## Audit-row contract (the headline)

| Actor       | Action            | Payload keys                                                    | When                                                                            |
|-------------|-------------------|----------------------------------------------------------------|---------------------------------------------------------------------------------|
| `scheduler` | `l3.crystallised` | `{source, task_id, skill_name, body_sha256, action, memory_id?}` | `drain_lane` — `Outcome::Completed` + cumulative `total_steps_executed >= 1` + valid candidate |
| `cli`       | `l3.removed`      | `{memory_id, deleted}`                                          | `hhagent-cli memory l3 remove` — DELETE … WHERE id AND layer = 3                |
| `agent`     | `plan.formulate`  | gains compact `l3_skill` key `{name, step_count, param_count}` \| `null` — **pure-additive (+1 key)** | every plan formulation                                |

Where `action` is one of:
- `"inserted"` — new row at layer = 3 (carries `memory_id`).
- `"skipped_duplicate"` — `body_sha256` already present at layer = 3 (carries the existing `memory_id`).

And `source` is the writer's own claim, never producer-supplied:
- `"agent_raised"` — `L3Source::AgentRaised { task_id }`, written exclusively by `runner::drain_lane`.

Validation failures do **not** audit (the agent-raised path emits a `tracing::warn!` and writes no row; the operator `remove` path surfaces errors on the CLI's stderr) — mirrors the L0/L1 precedent.

## Test budget

Estimate: **+25 to +32 tests**, workspace 1157 → ~1182–1189.

- ~12–14 unit tests in `core/src/memory/l3_crystallise.rs::tests` — each validator rejection (bad name charset / empty desc / newline desc / reserved-tag desc / undeclared placeholder / unused parameter / zero steps / too-many steps / non-object step params / oversized template / duplicate param name); `canonical_json` key-order determinism + SHA stability; `build_l3_metadata` key-set; `crystallise_l3` happy / dedup-existing / validation-rejected paths (unit-tier — the EXISTS check + insert use the harness `PgPool`, same as `l1_promote` tests).
- ~3–4 unit tests in `core/src/cassandra/types.rs::tests` — `Plan::completion_skill` positive + each negative-gate path; `L3SkillCandidate` serde round-trip; `skip_serializing_if` keeps `l3_skill: None` out of the wire form.
- ~3 unit tests in `core/src/scheduler/audit.rs::tests` — `build_l3_write_payload` shape for `inserted` / `skipped_duplicate` / the `l3.removed` payload.
- ~6–8 DB integration tests in `core/tests/memory_l3_crystallise_e2e.rs` — agent-raised happy (terminal Plan with `l3_skill` + ≥1 executed step → 1 L3 row + 1 `l3.crystallised` audit row, `action: "inserted"`, `trust: "untrusted"`); dedup (two tasks, same template → second `action: "skipped_duplicate"`, 0 new rows); validation-rejected (malformed template → 0 rows, no audit row); **grounding gate** (a pure-text-answer task that emits `l3_skill` with zero executed steps → 0 rows, no audit row); operator `remove` (1 row deleted + `deleted_memories` journalled + 1 `l3.removed` audit row); wrong-layer guard (`remove` of an L1 row id via the L3 path → `deleted = false`, row untouched); `list` returns layer-3 rows with their `trust` field.
- ~3–4 CLI integration tests in `core/tests/cli_memory_l3_e2e.rs` — `memory l3 list` end-to-end, `memory l3 remove <id>`, `remove` of a non-existent / wrong-layer id.
- 1–2 audit-payload pin updates in `scheduler_inner_loop_e2e` — the mid-tier gate gains `l3_skill`-key assertions (present on a skill-emitting plan; explicit `null` on a plain one).

## Risk surface

- **Template ≠ what actually ran.** An agent-emitted template could describe a sequence the task never executed. Accepted for v1 because skills are **non-executable + untrusted + operator-visible**; the future approval/invocation gate is where match-to-trajectory is enforced. The cumulative `total_steps_executed >= 1` grounding gate is a partial mitigation (only tasks that did real work crystallise). A full mitigation (mechanical trajectory capture + diff) is a named follow-up.
- **Secret leakage into a step parameter.** A step's `parameters` could embed a literal secret the agent saw during the task. v1 cannot scan for this (the writer has no view of which literals are secret values, and the secret vault stores opaque refs, not plaintext). Mitigated by: non-executable + untrusted storage; operator visibility via `list`; the future approval gate **must** scan step params before any promotion. Added as a new threat-model line + a blocking note on the trust-enum follow-up.
- **Disk / audit growth.** Same shape as L1: `Outcome::Completed`-gated (only successful tasks emit) + `body_sha256` dedup. At the L1 spec's steady-state estimate (~30% completion at 100 tasks/day), and only a fraction of completions emitting a skill, this is well below the existing `task.finalize` cardinality. No schema or index changes.
- **Dedup race.** EXISTS-check + INSERT is two statements; two concurrent writers of the same template could both pass and both insert. Cost is one redundant row, which a future `list`/recall read-cap silently tolerates. No UNIQUE constraint on `metadata->>'body_sha256'` (matches L0/L1). Not worth a partial-unique index for v1.
- **Reserved-tag defence for a render block that doesn't exist yet.** Rejecting `<skills>` / `</skills>` in the description is defensive now so the future recall-surfacing slice inherits a clean invariant; no behavioural cost today.

## Open questions for the implementer

None blocking. The design commits on:
- Agent-emitted structured candidate (not mechanical extraction; not a flat string).
- `{{name}}` placeholder syntax with the closed-world declared-vs-referenced invariant.
- Two emit gates: `Outcome::Completed` **and** a cumulative `total_steps_executed >= 1` (grounding) over the task's whole trajectory.
- `trust: "untrusted"` as a metadata string placeholder (not a typed enum — the enum is a later slice).
- Dedup via `canonical_json` → `metadata->>'body_sha256'` JSONB lookup (deterministic key ordering is load-bearing; a non-canonical serialiser would under-dedup).
- CLI surface: `list` + `remove` only (no `add`, no `approve`).

If any of these turn out wrong during implementation, file the correction inline.

## Self-review checklist (done before commit)

- [x] No placeholders / TBD / TODO in body text.
- [x] `plan.formulate` payload bump described as pure-additive (+1 key); the exact running count is deferred to implementation-time re-derivation rather than hardcoded (intervening slices may have changed it since the L1 spec's 21/22).
- [x] File-touch list cross-checked against the precedent (`l1_promote.rs`, `cassandra/types.rs::Plan`, `inner_loop.rs::InnerLoopResult`, `runner.rs::drain_lane`, `audit.rs`, `cli_audit.rs`, `bin/hhagent-cli/memory_l1.rs`).
- [x] No new `db/` helper claimed — `insert_memory_at_layer` / `delete_memory_at_layer` / `load_layer` confirmed already shipped.
- [x] No contradiction between "audit-row `source` is writer-side, never producer" and "`Plan.l3_skill` is producer-supplied" — the producer supplies the **content**, the writer supplies the **provenance**.
- [x] Writer-only boundary is explicit and consistent: stored skills are non-executable; no invocation path is introduced; `trust: "untrusted"` is inert this slice.
- [x] Scope check: ~25–32 tests + ~1 new module (~350–450 LOC) + 1 new CLI module + 2 audit constants is one session's worth, sized like the L1 writer slice.
- [x] Cross-references use the `path#Lline` clickable-link shape.
