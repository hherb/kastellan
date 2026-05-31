# L3 Skill Recall Surfacing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface operator-approved/pinned L3 skills (name + description + params) into every planner prompt via a query-independent `<skills>` block, mirroring the `<l1_insights>` path. Surfacing only — no invocation.

**Architecture:** A new pure-ish module `core/src/memory/l3_surface.rs` holds a typed `SurfacedSkill` projection, a fail-safe metadata parser, a trust-gate predicate, the rendered-entry formatter, a pure cap helper, and a query-independent PG loader. The pure renderer `assemble_system_prompt` gains a `skills` slice and emits the `<skills>` block between `<l1_insights>` and `<recalled>`. `PgSystemPromptBuilder` loads skills and threads them; `AssembledPrompt` and `FormulationMeta` gain a `skill_count` audit field. The planner prompt documents the block as reference-only with an explicit no-invoke instruction.

**Tech Stack:** Rust (workspace), sqlx + Postgres, serde_json, `hhagent-core` + `hhagent-db` crates, `tests-common` PG harness.

**Spec:** [`docs/superpowers/specs/2026-06-01-l3-skill-recall-surfacing-design.md`](../specs/2026-06-01-l3-skill-recall-surfacing-design.md)

**Standing setup for every task:**
```sh
source "$HOME/.cargo/env"
```
Live-PG tests use the session-local override (Postgres.app v18, deliberately excluded from the default candidates):
```sh
export HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
```

**Commit discipline:** stage only the files each task names (`git add <files>`). NEVER `git add -A` — the untracked `docs/essay-medium-draft.md` must stay out of every commit.

---

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `core/src/memory/l3_surface.rs` (NEW) | `SurfacedSkill`, `parse_surfaced_skill`, `is_surfaceable`, `render_skill_entry`, caps, `cap_surfaced`, `load_l3_skills_for_prompt`/`_default` + unit tests | 1, 2, 3 |
| `core/src/memory/mod.rs` | register `pub mod l3_surface;` | 1 |
| `core/src/prompt_assembly/assemble.rs` | `<skills>` block in the pure renderer + tests | 4 |
| `core/src/prompt_assembly/mod.rs` | `AssembledPrompt::skill_count` | 5 |
| `core/src/prompt_assembly/pg_builder.rs` | load skills + thread + set `skill_count` (2 construction sites) | 5 |
| `core/src/scheduler/agent.rs` | `FormulationMeta::skill_count` + populate | 5 |
| `core/tests/l3_surface_e2e.rs` (NEW) | live-PG loader behaviour (trust filter, malformed skip, caps, end-to-end block) | 3, 5 |
| `prompts/agent_planner.md` | `<skills>` documentation block (reference-only, no-invoke) | 6 |

---

## Task 1: `SurfacedSkill` projection + pure parser + trust gate

**Files:**
- Create: `core/src/memory/l3_surface.rs`
- Modify: `core/src/memory/mod.rs` (after line 50, alongside `pub mod l3_approval;`)

- [ ] **Step 1: Register the module**

In `core/src/memory/mod.rs`, add the declaration next to the other L3 modules (keep alphabetical-ish ordering with `l3_approval` / `l3_crystallise`):

```rust
pub mod l3_approval;
pub mod l3_crystallise;
pub mod l3_surface;
pub mod layers;
```

- [ ] **Step 2: Write the new module with the projection, parser, and gate + failing tests**

Create `core/src/memory/l3_surface.rs`:

```rust
//! L3 skill recall surfacing — the `<skills>` planner block.
//!
//! Mirrors the L1 insight-index loader ([`crate::memory::layers`]) one
//! layer over: a query-independent load of operator-approved L3 skills
//! that the prompt assembler concatenates into every system prompt.
//!
//! ## Surfacing, not invocation
//!
//! This module makes approved skills *visible* to the planner (name +
//! description + parameter manifest). It does NOT execute them and does
//! NOT expose their step templates — surfacing summarises a capability,
//! it is not an execution recipe. Invocation is a later slice.
//!
//! ## Trust is the load-bearing gate
//!
//! Only `user_approved` / `pinned` rows surface ([`is_surfaceable`]).
//! An `untrusted` skill — or any row whose trust marker is corrupted or
//! absent (the fail-safe [`crate::memory::l3_approval::SkillTrust::from_metadata_str`]
//! downgrades it to `Untrusted`) — never reaches the planner.

use crate::cassandra::types::{L3Param, L3SkillCandidate};
use crate::memory::l3_approval::SkillTrust;

/// A trust-gated L3 skill projected to exactly what the planner sees:
/// name, description, and the parameter manifest.
///
/// Steps are deliberately absent — surfacing summarises a capability,
/// it does not expose the execution recipe (that is an invocation
/// concern). Encoding the omission in the type makes "we do not surface
/// steps" a compile-time fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfacedSkill {
    pub name: String,
    pub description: String,
    pub params: Vec<L3Param>,
}

/// Project a stored L3 row's `metadata.template` into a [`SurfacedSkill`].
///
/// PURE + fail-safe: a row whose `metadata` lacks a `template` key, or
/// whose `template` does not deserialise into an [`L3SkillCandidate`],
/// yields `None` and is silently skipped by the loader. A malformed
/// skill must never crash prompt assembly or surface garbage.
pub fn parse_surfaced_skill(metadata: &serde_json::Value) -> Option<SurfacedSkill> {
    let template = metadata.get("template")?;
    let cand: L3SkillCandidate = serde_json::from_value(template.clone()).ok()?;
    Some(SurfacedSkill {
        name: cand.name,
        description: cand.description,
        params: cand.parameters,
    })
}

/// PURE trust gate: only operator-approved or pinned skills surface to
/// the planner. The single source of truth for "is this skill allowed
/// in the prompt." Reuses the gate slice's fail-safe trust parse so an
/// unknown/absent marker reads `Untrusted` ⇒ never surfaced.
pub fn is_surfaceable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn template_meta(name: &str, desc: &str, params: serde_json::Value) -> serde_json::Value {
        json!({
            "trust": "user_approved",
            "template": {
                "name": name,
                "description": desc,
                "parameters": params,
                "steps": [
                    { "tool": "shell-exec", "method": "shell.exec",
                      "parameters": { "argv": ["echo", "{{x}}"] } }
                ]
            }
        })
    }

    #[test]
    fn parse_well_formed_projects_name_desc_params() {
        let meta = template_meta(
            "summarise_repo_readme",
            "Read a repo's README and return a short summary.",
            json!([{ "name": "repo_path", "description": "absolute path to the repo" }]),
        );
        let s = parse_surfaced_skill(&meta).expect("well-formed template parses");
        assert_eq!(s.name, "summarise_repo_readme");
        assert_eq!(s.description, "Read a repo's README and return a short summary.");
        assert_eq!(s.params.len(), 1);
        assert_eq!(s.params[0].name, "repo_path");
        assert_eq!(s.params[0].description, "absolute path to the repo");
    }

    #[test]
    fn parse_zero_param_skill_yields_empty_params() {
        let meta = template_meta("run_tests", "Run the suite.", json!([]));
        let s = parse_surfaced_skill(&meta).expect("zero-param template parses");
        assert!(s.params.is_empty());
    }

    #[test]
    fn parse_missing_template_key_is_none() {
        let meta = json!({ "trust": "user_approved", "source": "agent_raised" });
        assert!(parse_surfaced_skill(&meta).is_none());
    }

    #[test]
    fn parse_undeserialisable_template_is_none() {
        // `parameters` is a string, not an array of L3Param → from_value fails.
        let meta = json!({
            "template": { "name": "x", "description": "y", "parameters": "nope", "steps": [] }
        });
        assert!(parse_surfaced_skill(&meta).is_none());
    }

    #[test]
    fn is_surfaceable_only_approved_and_pinned() {
        assert!(is_surfaceable(SkillTrust::UserApproved));
        assert!(is_surfaceable(SkillTrust::Pinned));
        assert!(!is_surfaceable(SkillTrust::Untrusted));
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass (this is RED→GREEN in one shot — the code is small and pure)**

Run:
```sh
cargo test -p hhagent-core memory::l3_surface::tests -- --nocapture
```
Expected: 5 tests pass (`parse_well_formed_projects_name_desc_params`, `parse_zero_param_skill_yields_empty_params`, `parse_missing_template_key_is_none`, `parse_undeserialisable_template_is_none`, `is_surfaceable_only_approved_and_pinned`).

> TDD note: these are pure functions with the assertions written before the bodies were settled; if a body is wrong the test fails. If you prefer strict RED first, comment out the function bodies (`todo!()`), confirm failure, then restore.

- [ ] **Step 4: Clippy the crate**

Run:
```sh
cargo clippy -p hhagent-core --all-targets -- -D warnings
```
Expected: exit 0.

- [ ] **Step 5: Commit**

```sh
git add core/src/memory/l3_surface.rs core/src/memory/mod.rs
git commit -m "feat(memory): SurfacedSkill projection + parse + trust gate (l3_surface)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Pure rendered-entry formatter + cap helper

**Files:**
- Modify: `core/src/memory/l3_surface.rs`

- [ ] **Step 1: Add the caps, the entry formatter, and the cap helper (append above the `#[cfg(test)] mod tests` block)**

```rust
/// Default upper bound on the number of L3 skills surfaced into a
/// prompt. Tighter than L1's 32 because approved skills are
/// operator-gated and therefore few; a smaller list keeps the
/// `<skills>` block scannable.
pub const L3_SKILLS_CAP_ROWS: usize = 16;

/// Default upper bound on the cumulative *rendered* byte length of the
/// surfaced skills. Matches L1's 4 KiB "fits in context unconditionally"
/// budget. Bounds actual prompt bytes because the accumulator measures
/// [`render_skill_entry`] output, which is exactly what the assembler
/// emits.
pub const L3_SKILLS_CAP_BYTES: usize = 4096;

/// Render a single skill into its `<skills>`-block lines:
///
/// ```text
/// - <name>: <description>
///   params: <p0.name> (<p0.description>), <p1.name> (<p1.description>)
/// ```
///
/// The `params:` line is omitted entirely for a zero-parameter skill.
/// PURE; the cap accumulator and the assembler both call this so the
/// byte budget and the emitted prompt never diverge.
pub fn render_skill_entry(skill: &SurfacedSkill) -> String {
    let mut out = String::new();
    out.push_str("- ");
    out.push_str(&skill.name);
    out.push_str(": ");
    out.push_str(&skill.description);
    out.push('\n');
    if !skill.params.is_empty() {
        out.push_str("  params: ");
        let rendered: Vec<String> = skill
            .params
            .iter()
            .map(|p| format!("{} ({})", p.name, p.description))
            .collect();
        out.push_str(&rendered.join(", "));
        out.push('\n');
    }
    out
}

/// Apply the row + rendered-byte caps to a trust-filtered, parsed skill
/// list (newest-first on input). PURE.
///
/// Row cap first, then a byte-accumulate loop over [`render_skill_entry`]
/// length: pushing the next entry stops once it would make the
/// cumulative rendered length *strictly exceed* `cap_bytes` (inclusive
/// boundary — an entry that fills the budget exactly still fits), mirroring
/// [`crate::memory::layers::load_l1`]. `cap_rows == 0` or `cap_bytes == 0`
/// returns empty.
pub fn cap_surfaced(
    skills: Vec<SurfacedSkill>,
    cap_rows: usize,
    cap_bytes: usize,
) -> Vec<SurfacedSkill> {
    if cap_rows == 0 || cap_bytes == 0 {
        return Vec::new();
    }
    let mut acc: Vec<SurfacedSkill> = Vec::new();
    let mut bytes_used: usize = 0;
    for skill in skills {
        if acc.len() == cap_rows {
            break;
        }
        let entry_bytes = render_skill_entry(&skill).len();
        if bytes_used.saturating_add(entry_bytes) > cap_bytes {
            break;
        }
        bytes_used += entry_bytes;
        acc.push(skill);
    }
    acc
}
```

- [ ] **Step 2: Add unit tests inside the existing `mod tests` block**

```rust
    fn skill(name: &str, desc: &str, params: &[(&str, &str)]) -> SurfacedSkill {
        SurfacedSkill {
            name: name.into(),
            description: desc.into(),
            params: params
                .iter()
                .map(|(n, d)| L3Param { name: (*n).into(), description: (*d).into() })
                .collect(),
        }
    }

    #[test]
    fn render_entry_with_params() {
        let s = skill("foo", "does foo.", &[("x", "the x"), ("y", "the y")]);
        assert_eq!(render_skill_entry(&s), "- foo: does foo.\n  params: x (the x), y (the y)\n");
    }

    #[test]
    fn render_entry_zero_params_omits_params_line() {
        let s = skill("bar", "does bar.", &[]);
        assert_eq!(render_skill_entry(&s), "- bar: does bar.\n");
    }

    #[test]
    fn cap_surfaced_honours_row_cap() {
        let skills = vec![skill("a", "a.", &[]), skill("b", "b.", &[]), skill("c", "c.", &[])];
        let capped = cap_surfaced(skills, 2, 4096);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].name, "a");
        assert_eq!(capped[1].name, "b");
    }

    #[test]
    fn cap_surfaced_honours_byte_cap() {
        // Each "- a: a.\n" entry is 8 bytes. cap_bytes = 8 admits exactly one.
        let one = render_skill_entry(&skill("a", "a.", &[])).len();
        let skills = vec![skill("a", "a.", &[]), skill("b", "b.", &[])];
        let capped = cap_surfaced(skills, 16, one);
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn cap_surfaced_zero_caps_return_empty() {
        let skills = vec![skill("a", "a.", &[])];
        assert!(cap_surfaced(skills.clone(), 0, 4096).is_empty());
        assert!(cap_surfaced(skills, 16, 0).is_empty());
    }

    #[test]
    fn caps_pinned_to_documented_defaults() {
        assert_eq!(L3_SKILLS_CAP_ROWS, 16);
        assert_eq!(L3_SKILLS_CAP_BYTES, 4096);
    }
```

- [ ] **Step 3: Run the tests**

Run:
```sh
cargo test -p hhagent-core memory::l3_surface::tests -- --nocapture
```
Expected: 11 tests pass (5 from Task 1 + 6 new).

- [ ] **Step 4: Clippy**

Run:
```sh
cargo clippy -p hhagent-core --all-targets -- -D warnings
```
Expected: exit 0.

- [ ] **Step 5: Commit**

```sh
git add core/src/memory/l3_surface.rs
git commit -m "feat(memory): pure render_skill_entry + cap_surfaced + caps (l3_surface)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: The query-independent PG loader + live e2e

**Files:**
- Modify: `core/src/memory/l3_surface.rs` (loader functions + imports)
- Create: `core/tests/l3_surface_e2e.rs`

- [ ] **Step 1: Add the loader imports and functions (above the `#[cfg(test)]` block)**

Extend the top-of-file imports:

```rust
use hhagent_db::memories::{load_layer, MemoryLayer};
use hhagent_db::DbError;
use sqlx::PgPool;
```

Add the loaders:

```rust
/// Load operator-approved/pinned L3 skills for the planner prompt.
///
/// Fetches every L3 row (newest-first, same as
/// [`crate::memory::l3_crystallise::list_l3`]), drops any whose trust is
/// not surfaceable, parses each surviving row's `metadata.template`
/// (malformed rows skipped fail-safe), then applies the row + rendered-byte
/// caps. Fetch-all-then-filter is correct here: the trust filter runs
/// after the fetch, so a capped fetch could starve the row cap when newer
/// rows are untrusted; operator-gated volume is low, so this is cheap.
///
/// Returns `Ok(vec![])` when no approved skill exists — the expected state
/// until an operator approves one. Not an error.
pub async fn load_l3_skills_for_prompt(
    pool: &PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<SurfacedSkill>, DbError> {
    if cap_rows == 0 || cap_bytes == 0 {
        return Ok(Vec::new());
    }
    let rows = load_layer(pool, MemoryLayer::Skill, usize::MAX).await?;
    let surfaced: Vec<SurfacedSkill> = rows
        .into_iter()
        .filter(|row| {
            let trust = row
                .metadata
                .get("trust")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            is_surfaceable(SkillTrust::from_metadata_str(trust))
        })
        .filter_map(|row| parse_surfaced_skill(&row.metadata))
        .collect();
    Ok(cap_surfaced(surfaced, cap_rows, cap_bytes))
}

/// Convenience wrapper pinning the published caps. Prefer this from the
/// prompt assembler (mirrors [`crate::memory::layers::load_l1_default`]).
pub async fn load_l3_skills_default(pool: &PgPool) -> Result<Vec<SurfacedSkill>, DbError> {
    load_l3_skills_for_prompt(pool, L3_SKILLS_CAP_ROWS, L3_SKILLS_CAP_BYTES).await
}
```

- [ ] **Step 2: Write the failing live-PG e2e**

Create `core/tests/l3_surface_e2e.rs`. Mirror the existing L3 e2e harness conventions in `core/tests/cli_memory_l3_e2e.rs` / `core/tests/memory_recall_e2e.rs` for cluster bring-up and the skip-without-PG posture. The exact bring-up helper names live in `tests-common`; inspect a sibling e2e (e.g. `core/tests/memory_recall_e2e.rs`) for the current `bring_up_pg_cluster` / skip-guard idiom and copy it verbatim — do not invent helper names.

```rust
//! Live-PG e2e for L3 skill recall surfacing (`load_l3_skills_*`).
//!
//! Verifies the trust gate (only user_approved/pinned surface), fail-safe
//! skip of a malformed-template row, and the row/byte caps — against a real
//! Postgres cluster. Skips-as-pass without `HHAGENT_PG_BIN_DIR`.

use hhagent_core::memory::l3_surface::{
    load_l3_skills_default, load_l3_skills_for_prompt, L3_SKILLS_CAP_BYTES,
};
use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};
use serde_json::json;

// --- COPY the cluster bring-up + skip guard from core/tests/memory_recall_e2e.rs ---
// (helper imports from hhagent_tests_common; e.g. bring_up_pg_cluster, a guard
//  that returns early when HHAGENT_PG_BIN_DIR is unset). Use the SAME names the
//  sibling test uses.

/// Insert an L3 row with the given trust + a one-step template naming `tool`.
async fn seed_l3(
    pool: &sqlx::PgPool,
    name: &str,
    trust: &str,
    tool: &str,
) -> i64 {
    let metadata = json!({
        "source": "agent_raised",
        "task_id": 1,
        "trust": trust,
        "body_sha256": format!("sha-{name}"),
        "created_at": "2026-06-01T00:00:00Z",
        "template": {
            "name": name,
            "description": format!("desc for {name}"),
            "parameters": [{ "name": "x", "description": "the x" }],
            "steps": [
                { "tool": tool, "method": "do.it", "parameters": { "v": "{{x}}" } }
            ]
        }
    });
    insert_memory_at_layer(pool, &format!("desc for {name}"), metadata, None, MemoryLayer::Skill)
        .await
        .expect("seed L3 row")
}

#[tokio::test]
async fn surfaces_only_approved_and_pinned() {
    // <bring up cluster + obtain `pool`, or return early without PG>
    seed_l3(&pool, "untrusted_skill", "untrusted", "shell-exec").await;
    seed_l3(&pool, "approved_skill", "user_approved", "shell-exec").await;
    seed_l3(&pool, "pinned_skill", "pinned", "shell-exec").await;

    let surfaced = load_l3_skills_default(&pool).await.expect("load surfaced");
    let names: Vec<&str> = surfaced.iter().map(|s| s.name.as_str()).collect();

    assert!(names.contains(&"approved_skill"));
    assert!(names.contains(&"pinned_skill"));
    assert!(!names.contains(&"untrusted_skill"), "untrusted must never surface");
}

#[tokio::test]
async fn malformed_template_row_is_skipped_not_surfaced() {
    // <bring up cluster + obtain `pool`, or return early without PG>
    // Approved row whose template has a non-array `parameters` → parse None.
    let bad = json!({
        "trust": "user_approved",
        "template": { "name": "broken", "description": "x", "parameters": "nope", "steps": [] }
    });
    insert_memory_at_layer(&pool, "x", bad, None, MemoryLayer::Skill)
        .await
        .expect("seed malformed row");
    seed_l3(&pool, "good_skill", "user_approved", "shell-exec").await;

    let surfaced = load_l3_skills_default(&pool).await.expect("load surfaced");
    let names: Vec<&str> = surfaced.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"good_skill"));
    assert!(!names.contains(&"broken"), "malformed row must be skipped, not error");
}

#[tokio::test]
async fn row_cap_is_honoured() {
    // <bring up cluster + obtain `pool`, or return early without PG>
    for i in 0..5 {
        seed_l3(&pool, &format!("skill_{i}"), "user_approved", "shell-exec").await;
    }
    let surfaced = load_l3_skills_for_prompt(&pool, 3, L3_SKILLS_CAP_BYTES)
        .await
        .expect("load with row cap 3");
    assert_eq!(surfaced.len(), 3);
}
```

> Note: `insert_memory_at_layer` is the writer the crystalliser uses (`core/src/memory/l3_crystallise.rs:442`). Confirm its exact path/signature via `grep -n "pub async fn insert_memory_at_layer" db/src/memories/write.rs` and adjust the import if the re-export path differs.

- [ ] **Step 3: Run the e2e to verify it fails WITHOUT the loader, then passes WITH it**

First confirm it compiles against the loader you added in Step 1, then run live:
```sh
export HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p hhagent-core --test l3_surface_e2e -- --nocapture
```
Expected: 3 tests pass, **zero `[SKIP]` lines** (the override is set). If you see `[SKIP]`, the env var is not exported into the test process — fix before proceeding.

- [ ] **Step 4: Confirm skip-as-pass without PG**

Run (no override):
```sh
unset HHAGENT_PG_BIN_DIR
cargo test -p hhagent-core --test l3_surface_e2e -- --nocapture
```
Expected: tests skip-as-pass (the guard prints `[SKIP]` and returns).

- [ ] **Step 5: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets -- -D warnings
git add core/src/memory/l3_surface.rs core/tests/l3_surface_e2e.rs
git commit -m "feat(memory): load_l3_skills_for_prompt loader + live e2e (trust gate, skip, caps)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `<skills>` block in the pure renderer

**Files:**
- Modify: `core/src/prompt_assembly/assemble.rs` (signature at `:75`, block insertion after the L1 block ~`:101`, ~15 in-file test call sites)

- [ ] **Step 1: Write the failing renderer tests first**

Add to the `#[cfg(test)] mod tests` block in `assemble.rs`. Use the `SurfacedSkill` type:

```rust
    use crate::memory::l3_surface::SurfacedSkill;
    use hhagent_db::memories::MemoryLayer; // if not already imported for fixtures

    fn surfaced(name: &str, desc: &str) -> SurfacedSkill {
        SurfacedSkill { name: name.into(), description: desc.into(), params: vec![] }
    }

    #[test]
    fn skills_block_present_with_one_skill() {
        let skills = vec![surfaced("foo", "does foo.")];
        let out = assemble_system_prompt(&[], &[], &skills, &RecalledContext::empty(), "BASE");
        assert!(out.contains("<skills>\n- foo: does foo.\n</skills>\n\n"));
    }

    #[test]
    fn skills_block_absent_when_empty_is_byte_identical() {
        let with_empty = assemble_system_prompt(&[], &[], &[], &RecalledContext::empty(), "BASE");
        // Compare against a string built without any skills concept — must match
        // the pre-existing no-skills output exactly.
        assert!(!with_empty.contains("<skills>"));
        assert_eq!(with_empty, "<base>\nBASE\n</base>\n");
    }

    #[test]
    fn skills_render_after_l1_and_before_recalled() {
        let l1 = vec![/* a Memory with body "L1ROW" — build via the same helper
                         the existing L1 tests use in this file */];
        let mut recalled = RecalledContext::empty();
        // populate recalled with one body "RECALLROW" using the same constructor
        // the existing recalled tests use.
        let skills = vec![surfaced("skillname", "skill desc.")];
        let out = assemble_system_prompt(&[], &l1, &skills, &recalled, "BASE");
        let l1_idx = out.find("</l1_insights>").expect("l1 block present");
        let skills_idx = out.find("<skills>").expect("skills block present");
        let recall_idx = out.find("<recalled>").expect("recalled block present");
        assert!(l1_idx < skills_idx, "skills must come after l1");
        assert!(skills_idx < recall_idx, "skills must come before recalled");
    }
```

> For the `skills_render_after_l1_and_before_recalled` test, reuse whatever `Memory`-fixture and `RecalledContext`-population helpers the existing tests in this file already use (search the file for how the L1/recalled tests construct their inputs). Do not invent new constructors.

- [ ] **Step 2: Run to verify failure (signature mismatch)**

Run:
```sh
cargo test -p hhagent-core --lib prompt_assembly::assemble 2>&1 | head -30
```
Expected: COMPILE FAIL — `assemble_system_prompt` takes 4 args, the new tests pass 5. This is the RED that drives the signature change.

- [ ] **Step 3: Add the `skills` parameter and the `<skills>` block**

Change the signature (`assemble.rs:75`):

```rust
use crate::memory::l3_surface::{render_skill_entry, SurfacedSkill};

pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],
    recalled: &RecalledContext,
    base: &str,
) -> String {
```

Insert the block immediately after the existing `</l1_insights>` block and before the `<recalled>` block:

```rust
    if !skills.is_empty() {
        out.push_str("<skills>\n");
        for skill in skills {
            out.push_str(&render_skill_entry(skill));
        }
        out.push_str("</skills>\n\n");
    }
```

Update the module-level docstring's ordering note (top of `assemble.rs`) from `L0 → L1 → recalled → base` to `L0 → L1 → skills → recalled → base`, and add one sentence: surfaced skills are operator-approved (high-trust) and therefore sit with the curated layers, before unverified `recalled` output.

- [ ] **Step 4: Update the ~15 existing in-file call sites**

Every existing `assemble_system_prompt(l0, l1, recalled, base)` call in this file's tests gains a `&[]` in the new third position, e.g.:

```rust
let out = assemble_system_prompt(&l0, &l1, &[], &recalled, "BASE");
```

Find them all:
```sh
grep -n "assemble_system_prompt(" core/src/prompt_assembly/assemble.rs
```
Update each (they are all in `#[cfg(test)]`). The non-test caller in `pg_builder.rs` is handled in Task 5.

- [ ] **Step 5: Run the renderer tests to verify they pass**

Run:
```sh
cargo test -p hhagent-core --lib prompt_assembly::assemble -- --nocapture
```
Expected: all existing assemble tests + the 3 new skills tests pass.

> If `pg_builder.rs` now fails to compile (it still calls the 4-arg form), that is expected and fixed in Task 5. To keep this task green in isolation, you may run only the assemble module tests as above; the full crate build goes green at the end of Task 5.

- [ ] **Step 6: Commit**

```sh
git add core/src/prompt_assembly/assemble.rs
git commit -m "feat(prompt): <skills> block in assemble_system_prompt (after l1, before recalled)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Wire the loader + `skill_count` audit field end-to-end

**Files:**
- Modify: `core/src/prompt_assembly/mod.rs` (`AssembledPrompt` struct, ~`:65`)
- Modify: `core/src/prompt_assembly/pg_builder.rs` (real builder `:45-69`, static builder `:98-110`)
- Modify: `core/src/scheduler/agent.rs` (`FormulationMeta` struct `:46`, construction `:209`)
- Modify: `core/tests/l3_surface_e2e.rs` (add an end-to-end assertion)

- [ ] **Step 1: Add `skill_count` to `AssembledPrompt`**

In `core/src/prompt_assembly/mod.rs`, after the `l1_count` field:

```rust
    /// Number of L3 skill rows the assembler folded into the `<skills>`
    /// block. Stays 0 until an operator approves a crystallised skill.
    /// RouterAgent records this into `FormulationMeta::skill_count`.
    pub skill_count: usize,
```

- [ ] **Step 2: Wire the real builder (`PgSystemPromptBuilder::build_with_recalled`, pg_builder.rs:45-69)**

```rust
        let l0 = load_l0_active_default(&self.pool).await?;
        let l1 = load_l1_default(&self.pool).await?;
        let skills = crate::memory::l3_surface::load_l3_skills_default(&self.pool).await?;
        let system_prompt = assemble_system_prompt(&l0, &l1, &skills, recalled, base);
        Ok(AssembledPrompt {
            system_prompt,
            l0_count: l0.len(),
            l1_count: l1.len(),
            skill_count: skills.len(),
            recalled_count: recalled.len(),
        })
```

Map the loader's `DbError` the same way `load_l1_default`'s error is mapped here (the `?` should already convert via the existing `From<DbError>`/`PromptAssemblyError` path used for `load_l1_default` — confirm the existing function uses `?` on `load_l1_default` and follow suit).

- [ ] **Step 3: Wire the static test builder (pg_builder.rs:98-110)**

The `StaticSystemPromptBuilder` constructs an `AssembledPrompt` with fixed/zero counts. Add `skill_count: 0,` alongside its existing `l0_count: 0, l1_count: 0` (a fixed-string builder surfaces no skills):

```rust
        Ok(AssembledPrompt {
            system_prompt: self.fixed.clone(),
            l0_count: 0,
            l1_count: 0,
            skill_count: 0,
            recalled_count: 0,
        })
```

- [ ] **Step 4: Add `skill_count` to `FormulationMeta` + populate it**

In `core/src/scheduler/agent.rs`, after the `l1_count` field (`:63`):

```rust
    /// Number of L3 skill rows surfaced into the `<skills>` block. Stays
    /// 0 in production until an operator approves a crystallised skill.
    pub skill_count: usize,
```

In the `FormulationMeta { … }` construction (`:209`), after `l1_count: assembled.l1_count,`:

```rust
            skill_count: assembled.skill_count,
```

- [ ] **Step 5: Build the whole crate (everything should now compile)**

Run:
```sh
cargo build -p hhagent-core
```
Expected: clean build. If any other `FormulationMeta { … }` or `AssembledPrompt { … }` literal exists (e.g. in tests), the compiler names it — add the new field there too. Find them:
```sh
grep -rn "FormulationMeta {" core/
grep -rn "AssembledPrompt {" core/
```

- [ ] **Step 6: Add an end-to-end surfacing assertion to the live e2e**

Append to `core/tests/l3_surface_e2e.rs` a test that drives the real `PgSystemPromptBuilder` and asserts the `<skills>` block + `skill_count`:

```rust
#[tokio::test]
async fn build_with_recalled_emits_skills_block_and_counts() {
    // <bring up cluster + obtain `pool`, or return early without PG>
    seed_l3(&pool, "approved_skill", "user_approved", "shell-exec").await;

    let builder = hhagent_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone());
    let recalled = hhagent_core::recall_assembly::RecalledContext::empty();
    let assembled = builder
        .build_with_recalled("BASE", &recalled)
        .await
        .expect("assemble");

    assert!(assembled.system_prompt.contains("<skills>"));
    assert!(assembled.system_prompt.contains("approved_skill"));
    assert_eq!(assembled.skill_count, 1);
}
```

> Confirm `PgSystemPromptBuilder::new` and the `SystemPromptBuilder` trait import paths via `grep -n "impl PgSystemPromptBuilder" core/src/prompt_assembly/pg_builder.rs` and the trait's `build_with_recalled` location; adjust `use` lines to match. The trait method may require `use hhagent_core::prompt_assembly::SystemPromptBuilder;` in scope.

- [ ] **Step 7: Run the full e2e + the crate test suite**

```sh
export HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p hhagent-core --test l3_surface_e2e -- --nocapture
cargo test -p hhagent-core --lib prompt_assembly -- --nocapture
```
Expected: 4 e2e tests pass (3 from Task 3 + 1 new), zero `[SKIP]`; all prompt_assembly lib tests pass.

- [ ] **Step 8: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets -- -D warnings
git add core/src/prompt_assembly/mod.rs core/src/prompt_assembly/pg_builder.rs core/src/scheduler/agent.rs core/tests/l3_surface_e2e.rs
git commit -m "feat(prompt): wire l3 skill loader + skill_count audit field through assembly

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Document the `<skills>` block in the planner prompt

**Files:**
- Modify: `prompts/agent_planner.md`

- [ ] **Step 1: Inspect the existing L0/L1 documentation in the prompt**

Run:
```sh
grep -n "l1_insights\|l0_meta\|<recalled>\|<base>\|l1_insight\b" prompts/agent_planner.md
```
Find the section that explains the surrounding context blocks (L0/L1/recalled) so the new `<skills>` paragraph sits with them, in the same voice.

- [ ] **Step 2: Add the `<skills>` documentation block**

Insert a short subsection adjacent to where the prompt describes the other context blocks:

```markdown
**The `<skills>` block (reference only).** A `<skills>` block may precede
your base instructions. It lists skills you previously crystallised that an
operator has **approved**, each with its name, a one-line description, and
its parameters. They are surfaced for your **awareness only** — there is
**no skill-invocation field** in the plan schema. Do **not** attempt to
"call" a skill or emit any invoke/skill-reference field; the runner will
ignore it. Plan with normal `steps` as usual. If a surfaced skill matches
the task, you may reproduce its approach through ordinary steps.
```

- [ ] **Step 3: Verify nothing that hashes the prompt breaks**

The planner prompt's SHA-256 feeds `FormulationMeta.prompt_sha256`, but no test pins a *literal* hash of `agent_planner.md` (the hash is computed at runtime). Confirm no test embeds the file's hash:
```sh
grep -rn "agent_planner" core/ --include=*.rs | grep -i "sha\|hash" || echo "no literal-hash pin — safe"
```
Expected: `no literal-hash pin — safe` (or any matches are runtime reads, not embedded constants). If a literal-hash test exists, update its expected value.

- [ ] **Step 4: Build + the agent_prompts e2e (smoke that the prompt still loads)**

```sh
cargo test -p hhagent-core --test scheduler_agent_prompts_e2e -- --nocapture 2>/dev/null \
  || cargo test -p hhagent-core agent_prompts -- --nocapture
```
Expected: passes (or skip-as-pass without PG). If the exact test name differs, find it: `grep -rln "agent_planner" core/tests/`.

- [ ] **Step 5: Commit**

```sh
git add prompts/agent_planner.md
git commit -m "docs(prompt): document the <skills> block (reference-only, no-invoke)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (after all tasks)

- [ ] **Full workspace test (no PG): skip-as-pass posture**

```sh
unset HHAGENT_PG_BIN_DIR
cargo test --workspace 2>&1 | tail -20
```
Expected: **all pass / 0 failed / 3 ignored**; count ≈ 1220 baseline + new non-PG unit tests (l3_surface 11 + assemble 3 ≈ +14). Record the exact number for HANDOVER.

- [ ] **Full workspace clippy gate**

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
```
Expected: exit 0.

- [ ] **Live-PG suites (override set)**

```sh
export HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test -p hhagent-core --test l3_surface_e2e -- --nocapture
cargo test -p hhagent-core --test cli_memory_l3_e2e -- --nocapture       # writer/gate regression
cargo test -p hhagent-core --test memory_l3_crystallise_e2e -- --nocapture
```
Expected: `l3_surface_e2e` 4/4 zero `[SKIP]`; the two regression suites unchanged (8/8 and 7/7 per the gate-slice baseline).

- [ ] **Doc-link check (no new broken intra-doc links)**

```sh
RUSTDOCFLAGS=-D rustdoc::broken_intra_doc_links cargo doc -p hhagent-core --no-deps --document-private-items 2>&1 | grep -c "unresolved" || true
```
Expected: **21** (matches `main`). If higher, the new module's `[`...`]` intra-doc links need repointing (e.g. `super::` or full paths).

- [ ] **Session-end docs (separate from the feature commits)**

Update `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md`: move item 10(c) to Recently-completed, record the new test count, mark ROADMAP's L3 arc "recall surfacing ✅ → invocation (10b-next) remaining". Stage only those two files. Do NOT `git add -A`.

---

## Self-Review (plan vs spec)

- **Spec coverage:** `SurfacedSkill`/parser/gate → Task 1; render + caps → Task 2; loader + trust filter + fail-safe skip → Task 3; `<skills>` block + placement → Task 4; `AssembledPrompt`/`FormulationMeta` `skill_count` + wiring → Task 5; prompt doc (reference-only/no-invoke) → Task 6. All spec sections mapped.
- **Trust gate (load-bearing invariant):** pure predicate tested (Task 1) + live absence of an untrusted row (Task 3) + fail-safe corrupted marker (covered by `from_metadata_str` + `is_surfaceable`). ✓
- **Type consistency:** `SurfacedSkill { name, description, params: Vec<L3Param> }` defined Task 1, used identically Tasks 2/4/5; `load_l3_skills_for_prompt(pool, cap_rows, cap_bytes)` + `load_l3_skills_default(pool)` consistent Tasks 3/5; `render_skill_entry(&SurfacedSkill) -> String` used by `cap_surfaced` (Task 2) and the renderer (Task 4); `skill_count` field name identical across `AssembledPrompt`/`FormulationMeta`. ✓
- **Placeholder scan:** the e2e bring-up is intentionally delegated to "copy the sibling test's harness verbatim" rather than inventing helper names — this is a real instruction (the harness names live in `tests-common` and must match the live API), not a TODO. Every code step shows complete code.
- **Empty-slice byte-identity:** asserted in Task 4 Step 1 (`skills_block_absent_when_empty_is_byte_identical`). ✓
