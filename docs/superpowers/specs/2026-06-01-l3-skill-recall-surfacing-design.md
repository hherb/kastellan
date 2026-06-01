# L3 skill recall surfacing — the `<skills>` planner block

**Date:** 2026-06-01
**Status:** Design, ready for plan.
**Branch:** `feat/l3-skill-recall-surfacing`
**Roadmap item:** 10(c) — "Recall surfacing: a `<skills>` prompt block that surfaces L3 rows to the planner."

## Pre-reqs (all shipped)

- **L3 writer** (spec [`2026-05-31-l3-skill-crystallisation-design.md`](2026-05-31-l3-skill-crystallisation-design.md), PR #173, merged at `6eb966e`). It populates `MemoryLayer::Skill` (L3) rows whose `body` is the skill *description* and whose `metadata` is `{source, task_id, trust, body_sha256, created_at, template}`. The `template` value is exactly a serialised [`L3SkillCandidate`](../../../core/src/cassandra/types.rs#L96) — `{name, description, parameters:[{name, description}], steps:[…]}`.
- **L3 approval gate + `SkillTrust`** (spec [`2026-05-31-l3-skill-approval-gate-design.md`](2026-05-31-l3-skill-approval-gate-design.md), PR #176, merged at `bbcc7b3`). [`core/src/memory/l3_approval.rs`](../../../core/src/memory/l3_approval.rs) ships the `SkillTrust` enum (`Untrusted | UserApproved | Pinned`) with a **total, fail-safe** `from_metadata_str` (unknown/absent ⇒ `Untrusted`) and `as_str`. `hhagent-cli memory l3 approve <id>` is the only path that flips a row to `user_approved`.
- **The `<l1_insights>` surfacing pattern** — the exact precedent this slice mirrors one layer over:
  - Pure renderer [`assemble_system_prompt`](../../../core/src/prompt_assembly/assemble.rs#L75): `L0 → L1 → recalled → base`, each block omitted when its slice is empty.
  - Query-independent loader [`load_l1_default`](../../../core/src/memory/layers.rs#L124) → [`load_l1`](../../../core/src/memory/layers.rs#L73) with `L1_DEFAULT_CAP_ROWS = 32` / `L1_DEFAULT_CAP_BYTES = 4096` and a byte-accumulate loop. **L1 is a separate unconditional load, not a fourth recall lane** (see the `layers.rs` module docstring).
  - [`PgSystemPromptBuilder::build_with_recalled`](../../../core/src/prompt_assembly/pg_builder.rs#L45) loads L0 + L1, calls the assembler, returns [`AssembledPrompt`](../../../core/src/prompt_assembly/mod.rs#L65) with `l0_count` / `l1_count` / `recalled_count`.
  - [`RouterAgent::formulate_plan`](../../../core/src/scheduler/agent.rs#L112) records `l1_count: assembled.l1_count` into the [`FormulationMeta`](../../../core/src/scheduler/agent.rs#L46) audit struct.

## Why now

The writer crystallises skills; the gate lets an operator approve them. But an approved skill is still invisible — nothing surfaces it to the planner, so it can never be reused. This slice closes that gap **for awareness only**: it renders approved skills into every planner prompt so the planner *knows they exist* and *what they do*, which is the prerequisite the invocation slice (10b-next) builds on (the planner cannot invoke a skill it cannot see).

This is deliberately **surfacing, not invocation**: there is no field a planner can emit to call a skill, and the prompt explicitly tells the planner not to attempt one. Exactly as the writer shipped storage before the gate, and the gate shipped approval before execution, this slice ships visibility before invocation.

## Scope

In scope (this slice):

- **New module** [`core/src/memory/l3_surface.rs`](../../../core/src/memory/l3_surface.rs) — the typed `SurfacedSkill` projection, a **pure** metadata parser, a **pure** trust gate predicate, and the query-independent loader. Kept separate from the writer (`l3_crystallise.rs`, 467 LOC) and the gate (`l3_approval.rs`) so each L3 module stays focused and under the 500-LOC cap.
- **Extend the pure renderer** `assemble_system_prompt` with a `skills: &[SurfacedSkill]` slice that renders a `<skills>` block.
- **Extend** `PgSystemPromptBuilder::build_with_recalled` to load surfaced skills and thread them through; `AssembledPrompt` gains `skill_count`.
- **Extend** `FormulationMeta` with `skill_count` (audit trail, mirrors `l1_count`).
- **Document** the `<skills>` block in [`prompts/agent_planner.md`](../../../prompts/agent_planner.md) with reference-only / explicit-no-invoke wording.

Out of scope (named follow-ups):

- **Invocation / execution (10b-next).** No path lets the planner call a surfaced skill; no parameter substitution; no step dispatch. The next slice.
- **The `pin` command.** `Pinned` rows surface exactly like `UserApproved` ones here, but no command produces a `Pinned` row yet (inherited from the gate slice).
- **Semantic/relevance ranking of skills.** L3 surfacing is unconditional (like L1), not query-ranked. L3 rows have no embedding. A future "surface only skills relevant to this instruction" refinement is a separate item.
- **Per-skill enable/disable beyond trust.** Trust (`user_approved`/`pinned`) is the only surfacing gate.

## What surfaces (the central content decision)

Each surfaced skill renders as **name + description + parameter manifest** — *not* the step template. Rationale:

- Name + description lets the planner judge whether a skill fits the task.
- The parameter manifest (each param's name + description) is what the *invocation* slice will need the planner to have seen, so it can later supply argument values. Surfacing it now means the invocation slice adds only the call mechanism, not new visible context.
- The **steps** (tool / method / parameters per step) are dispatch-time mechanics, not a planning concern. Omitting them keeps the prompt lean and keeps the renderer's contract honest: it renders a *capability summary*, not an execution recipe.

This is encoded at the type level: `SurfacedSkill` carries only `{name, description, params}`, so "we deliberately do not surface steps" is a compile-time fact, not a renderer convention.

## The `SurfacedSkill` projection + pure parser

```rust
// core/src/memory/l3_surface.rs
use hhagent_db::memories::{Memory, MemoryLayer};
use crate::cassandra::types::{L3Param, L3SkillCandidate};
use crate::memory::l3_approval::SkillTrust;

/// A trust-gated L3 skill projected to exactly what the planner sees:
/// name, description, and the parameter manifest. Steps are
/// deliberately absent — surfacing summarises a capability, it does
/// not expose the execution recipe (that is an invocation concern).
pub struct SurfacedSkill {
    pub name: String,
    pub description: String,
    pub params: Vec<L3Param>,
}

/// Project a stored L3 row's `metadata.template` into a `SurfacedSkill`.
/// PURE + fail-safe: a row whose `metadata.template` is missing or does
/// not deserialise into an `L3SkillCandidate` yields `None` and is
/// silently skipped — a malformed skill must never crash prompt
/// assembly or surface garbage to the planner.
pub fn parse_surfaced_skill(metadata: &serde_json::Value) -> Option<SurfacedSkill> {
    let template = metadata.get("template")?;
    let cand: L3SkillCandidate = serde_json::from_value(template.clone()).ok()?;
    Some(SurfacedSkill { name: cand.name, description: cand.description, params: cand.parameters })
}

/// PURE trust gate: only operator-approved or pinned skills surface to
/// the planner. The single source of truth for "is this skill allowed
/// in the prompt." Reuses the gate slice's fail-safe parse so an
/// unknown/absent trust marker reads `Untrusted` ⇒ never surfaced.
pub fn is_surfaceable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}
```

Because `metadata.template` is written by the crystalliser as a serialised `L3SkillCandidate` ([`build_l3_metadata`](../../../core/src/memory/l3_crystallise.rs#L348)), the parse is a direct `from_value` of the existing type — no hand-rolled field extraction, and the writer's caps/normalisation already bound every field.

## The loader (query-independent, like L1)

```rust
/// Default caps. Approved skills are operator-gated and therefore few;
/// a tighter row cap than L1's 32 reflects that, while the byte cap
/// matches L1's "fits in context unconditionally" budget.
pub const L3_SKILLS_CAP_ROWS: usize = 16;
pub const L3_SKILLS_CAP_BYTES: usize = 4096;

/// Load operator-approved/pinned L3 skills for the planner prompt.
/// Newest-first (mirrors L1; pinned-first ordering deferred — no
/// `Pinned` producer exists yet). Untrusted rows never surface.
pub async fn load_l3_skills_for_prompt(pool: &PgPool) -> Result<Vec<SurfacedSkill>, DbError> {
    // 1. load_layer(pool, MemoryLayer::Skill, usize::MAX) — newest-first.
    //    Fetch all L3 rows (same as list_l3): the trust filter runs after,
    //    so capping the fetch could starve the row cap if newer rows are
    //    untrusted. Operator-gated volume is low, so fetch-all-then-cap is
    //    both simplest and correct.
    // 2. filter: is_surfaceable(SkillTrust::from_metadata_str(metadata.trust))
    // 3. parse_surfaced_skill(metadata) — drop None (malformed)
    // 4. take newest-first up to L3_SKILLS_CAP_ROWS, byte-accumulate the
    //    rendered length up to L3_SKILLS_CAP_BYTES.
}

pub async fn load_l3_skills_default(pool: &PgPool) -> Result<Vec<SurfacedSkill>, DbError> {
    load_l3_skills_for_prompt(pool).await
}
```

Reuses [`load_layer`](../../../db/src/memories/search.rs) (the same helper `load_l1` and `list_l3` already use). The trust filter runs **in Rust** via `SkillTrust::from_metadata_str` — not as a SQL `WHERE metadata->>'trust' IN (…)` — so the trust-string vocabulary lives in exactly one place (the enum) and the fail-safe downgrade is reused, not duplicated. Volume is low (operator-gated), so loading the layer then filtering is cheap.

**Byte-cap basis.** The accumulator measures the *rendered* length of each entry (name + description + params line), mirroring `load_l1`'s body-length accounting, so the cap bounds actual prompt bytes.

## The renderer extension (pure)

```rust
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],   // NEW — between L1 and recalled
    recalled: &RecalledContext,
    base: &str,
) -> String
```

Block placement: **`L0 → L1 → skills → recalled → base`**. Skills sit with the curated/high-trust layers (operator-approved, like L0/L1) and *before* `recalled` (unverified recall output), matching the trust gradient the `assemble.rs` module docstring describes. Empty `skills` omits the block entirely (byte-identical to today), exactly like the other optional blocks.

Rendered shape (per entry):

```
<skills>
- summarise_repo_readme: Read a repo's README and return a short summary.
  params: repo_path (absolute path to the repo)
- run_workspace_tests: Run the workspace test suite and report pass/fail counts.
</skills>
```

The `params:` line is omitted for a zero-parameter skill. Params render as `name (description)` joined by `, `.

**Injection defence already exists at write time.** `validate_l3_skill` bans the literal `<skills>` / `</skills>` tags in the description and normalises/bounds the name and every param name (snake_case, length-capped) before a row is ever stored, so surfaced text cannot break out of the block. This slice adds a renderer test asserting a benign skill renders cleanly and relies on the write-time guard for the adversarial case (no new escaping logic — escaping at render time would diverge from the L1/L0 blocks, which also trust their write-time validation).

## Wiring

```rust
// pg_builder.rs::build_with_recalled
let l0 = load_l0_active_default(&self.pool).await?;
let l1 = load_l1_default(&self.pool).await?;
let skills = load_l3_skills_default(&self.pool).await?;          // NEW
let system_prompt = assemble_system_prompt(&l0, &l1, &skills, recalled, base);
Ok(AssembledPrompt {
    system_prompt,
    l0_count: l0.len(),
    l1_count: l1.len(),
    skill_count: skills.len(),                                   // NEW
    recalled_count: recalled.len(),
})
```

`AssembledPrompt` gains `pub skill_count: usize`. `FormulationMeta` gains `pub skill_count: usize`, populated `skill_count: assembled.skill_count` next to the existing `l1_count` line in `formulate_plan`. The ~15 existing `assemble_system_prompt` test call sites in `assemble.rs` are updated to pass `&[]` for the new slice (mechanical).

## Data flow

```
RouterAgent::formulate_plan(ctx)
  └─ recall_builder.build_with_seeds(instruction, seeds) → RecalledContext   (unchanged)
  └─ prompt_builder.build_with_recalled(base, recalled)
       └─ load_l0_active_default(pool)           → l0
       └─ load_l1_default(pool)                  → l1
       └─ load_l3_skills_default(pool)           → skills        [NEW]
            └─ load_layer(Skill) → filter is_surfaceable(from_metadata_str)
               → parse_surfaced_skill → cap rows+bytes
       └─ assemble_system_prompt(l0, l1, skills, recalled, base) → system_prompt
       └─ AssembledPrompt { …, skill_count }                     [NEW field]
  └─ FormulationMeta { …, skill_count }                          [NEW field, audited]
```

## The planner prompt block (reference-only, explicit no-invoke)

`prompts/agent_planner.md` gains a short section, in the spirit of its existing L0/L1 documentation, with this contract:

> A `<skills>` block may precede your base instructions. It lists skills you previously crystallised that an operator has **approved**, each with its name, description, and parameters. They are surfaced for your **awareness only** — there is **no skill-invocation field** in the plan schema yet. Do **not** attempt to "call" a skill or emit any invoke/skill-reference field; plan with normal `steps` as usual. If a surfaced skill matches the task, you may reproduce its approach through ordinary steps.

This wording prevents the planner from hallucinating an invoke field the runner would silently drop, and is honest about the current (no-execution) capability.

## Files touched

NEW (2):
- `core/src/memory/l3_surface.rs` — `SurfacedSkill`, `parse_surfaced_skill`, `is_surfaceable`, the loader + caps, module-internal unit tests.
- This spec + the plan that follows it.

MODIFIED (~6):
- `core/src/memory/mod.rs` — `pub mod l3_surface;`.
- `core/src/prompt_assembly/assemble.rs` — new `skills` param + `<skills>` block + renderer tests; update in-file test call sites.
- `core/src/prompt_assembly/mod.rs` — `AssembledPrompt::skill_count`.
- `core/src/prompt_assembly/pg_builder.rs` — load skills + thread through + set `skill_count`.
- `core/src/scheduler/agent.rs` — `FormulationMeta::skill_count` + populate it.
- `prompts/agent_planner.md` — the `<skills>` documentation block.

TESTS (NEW + extended):
- `core/tests/l3_surface_e2e.rs` (NEW) — live-PG loader behaviour.
- a mock-e2e `skill_count` audit pin (extend the existing `router_agent_mock_e2e.rs` precedent that pins `l1_count`-style meta).

DOCS (2): `HANDOVER.md` + `ROADMAP.md` session-end update.

No new migration. No new `db/` table. No change to recall fusion. `load_layer` + the `metadata` column already exist.

## Test budget

Estimate **+18 to +24**, workspace ~1220 → ~1238–1244.

- ~6–8 unit (`l3_surface.rs::tests`): `parse_surfaced_skill` (well-formed → name/desc/params; missing `template` → None; `template` not deserialisable → None; zero-param skill → empty params); `is_surfaceable` (UserApproved/Pinned → true, Untrusted → false).
- ~6–8 unit (`assemble.rs::tests`): `<skills>` block present with one + multiple skills; absent when slice empty (byte-identical to today); params line rendered; params line omitted for zero-param skill; **ordering** — skills render after `</l1_insights>` and before `<recalled>`; full `L0 + L1 + skills + recalled + base` integration string.
- ~1 unit: `AssembledPrompt`/`FormulationMeta` `skill_count` default/threading (where a pure seam exists; else covered by the mock e2e).
- ~4–6 live-PG e2e (`l3_surface_e2e`): seed L3 rows at `untrusted` / `user_approved` / `pinned` → loader returns only the latter two, parsed correctly; a malformed-`template` approved row is skipped (not surfaced, no error); row cap honoured; byte cap honoured; (optionally) an end-to-end `build_with_recalled` asserting the `<skills>` block appears in the assembled prompt.
- ~1 mock-e2e: `FormulationMeta.skill_count` equals the number of surfaced skills (mirrors the graph-seed-thread / `l1_count` audit pins).

## Risk surface

- **Trust gate is the load-bearing invariant.** Only `user_approved`/`pinned` may surface; an `untrusted` skill reaching the planner is the failure this slice must prevent. Directly tested at the predicate (`is_surfaceable`) and live (an untrusted seeded row is absent from the loader output). The fail-safe `from_metadata_str` means a corrupted trust marker reads `Untrusted` ⇒ not surfaced.
- **Malformed rows must degrade silently.** A hand-edited or older-schema L3 row whose `template` no longer deserialises is skipped, never panics prompt assembly. Tested.
- **Surfacing ≠ executable.** No code path runs a surfaced skill; the prompt forbids invocation. No execution attack surface is introduced. The single future consumer (invocation) carries its own live re-validation.
- **Prompt-bloat from many skills.** Bounded by `L3_SKILLS_CAP_ROWS`/`L3_SKILLS_CAP_BYTES`. Because skills are operator-gated, real counts are expected far below the cap; the cap is a backstop, and (like L1) shares the deferred global-token-cap follow-up (issue #78).
- **Signature churn.** Adding a positional param to `assemble_system_prompt` touches ~15 in-file test callers. Mechanical (`&[]`), caught immediately by the compiler; no behaviour change to existing blocks (empty `skills` is byte-identical).
- **Planner ignores the no-invoke instruction.** An LLM could still emit a stray invoke-like field; the runner already ignores unknown `Plan` fields, so the worst case is a dropped field, not an action. The explicit prompt wording minimises it.

## Open questions for the implementer

None blocking. The design commits on:
- Surface **name + description + params**, never steps (`SurfacedSkill` is the typed projection).
- Block placement **`L0 → L1 → skills → recalled → base`**; empty slice omits the block (byte-identical to today).
- Trust filter **in Rust** via `SkillTrust::from_metadata_str` + `is_surfaceable` (single vocabulary source), not in SQL.
- Caps `L3_SKILLS_CAP_ROWS = 16` / `L3_SKILLS_CAP_BYTES = 4096`; newest-first; pinned-first ordering deferred.
- Malformed `template` ⇒ row silently skipped (fail-safe parse).
- Prompt documents the block as **reference-only with an explicit no-invoke instruction**.
- Audit: `FormulationMeta.skill_count` only (no new audit *action* — surfacing is read-only context assembly, like `l1_count`).

If any of these turn out wrong during implementation, file the correction inline.

## Self-review checklist (done before commit)

- [x] No placeholders / TBD / TODO in body text.
- [x] Content decision (name+desc+params, not steps) stated and justified, and encoded at the type level (`SurfacedSkill`).
- [x] Trust gate identified as the load-bearing invariant, with both a pure-predicate test and a live-absence test named.
- [x] Block placement fixed and justified against the `assemble.rs` trust-gradient docstring; empty-slice byte-identity preserved.
- [x] File-touch list cross-checked against the L1 precedent (`load_l1_default`, `AssembledPrompt`, `FormulationMeta.l1_count`, the `assemble_system_prompt` caller set).
- [x] No new migration / `db/` table — `load_layer` + `metadata` column confirmed already shipped; trust filter reuses the gate slice's enum.
- [x] Surfacing-only boundary explicit: nothing executes; prompt forbids invocation; invocation named as the next slice.
- [x] Scope check: ~18–24 tests + 1 new pure-ish module (~200–300 LOC incl. loader) + a 1-param renderer change + 2 struct fields + 1 prompt block is one session, sized like the gate slice.
- [x] Cross-references use the `path#Lline` clickable-link shape.
