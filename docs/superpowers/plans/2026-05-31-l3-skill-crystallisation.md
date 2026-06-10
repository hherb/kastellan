# L3 Skill Crystallisation Writer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the first writer for `MemoryLayer::Skill` (L3) rows: on a successfully-completed, multi-step task the agent emits a parameterised tool-call template, which `runner::drain_lane` validates, dedups, and stores at L3 marked `trust: "untrusted"` with one typed audit row. Crystallised skills are non-executable in this slice.

**Architecture:** Mirror the shipped L1 promotion writer one layer up. A new pure module `core/src/memory/l3_crystallise.rs` (validator + canonical-SHA + async writer + list/remove) is driven by a new structured `Plan.l3_skill` candidate, captured into `InnerLoopResult.terminal_l3_skill` on the `Outcome::Completed` arm under a `dispatch_count >= 1` grounding gate, and written by a best-effort `drain_lane` hook. Operator visibility via `kastellan-cli memory l3 {list, remove}`. No new `db/` helper — `insert_memory_at_layer` / `delete_memory_at_layer` / `load_layer` all exist.

**Tech Stack:** Rust (workspace), `sqlx` (Postgres), `serde_json`, `sha2`, `time` (RFC3339). TDD throughout; existing tests are the regression pin for the mechanical churn.

**Spec:** [`docs/superpowers/specs/2026-05-31-l3-skill-crystallisation-design.md`](../specs/2026-05-31-l3-skill-crystallisation-design.md)

**Precedent files to keep open while implementing:**
- `core/src/memory/l1_promote.rs` (the module template)
- `core/src/bin/kastellan-cli/memory_l1.rs` (the CLI template)
- `core/src/cli_audit.rs:543-597` (`l1_add_and_audit` / `l1_remove_and_audit`)
- `core/src/scheduler/audit.rs:385-423` (`build_l1_write_payload`) + `:99-110` (action consts)
- `core/src/scheduler/runner.rs:233-340` (`drain_lane` L1 hook + `write_l1_promoted_row`)
- `core/src/scheduler/inner_loop.rs:120-132, 217-230, 378-387` (`InnerLoopResult`, `finish!`, Completed arm)
- `core/src/scheduler/inner_loop_audit.rs:67-128, 353-410` (`build_plan_formulate_payload` + key-count pins)
- `core/tests/memory_l1_promote_e2e.rs` + `core/tests/cli_memory_l1_e2e.rs` (e2e harness templates)

**Build/test prelude (every Run step assumes this):**
```sh
source "$HOME/.cargo/env"
```
PG-gated tests need `KASTELLAN_PG_BIN_DIR` set to a Postgres bin dir (e.g. `/Applications/Postgres 2.app/Contents/Versions/18/bin/`); without it they skip-as-pass.

---

## Task 1: `L3SkillCandidate` types + `Plan.l3_skill` field + `completion_skill()` accessor

**Files:**
- Modify: `core/src/cassandra/types.rs` (add types after `RefusedReason` ~L90; add field to `Plan` after `l1_insight` ~L143; add accessor to `impl Plan` after `completion_insight` ~L186; add tests in the `#[cfg(test)] mod tests`)
- Modify (compiler-driven, mechanical): every `Plan { .. }` literal across the tree (≈10 files) gains `l3_skill: None`

- [ ] **Step 1: Write the failing tests** (append to the `tests` module in `core/src/cassandra/types.rs`)

```rust
#[test]
fn completion_skill_some_on_terminal_with_skill() {
    let mut plan = make_terminal_plan(); // existing test helper that yields a terminal Plan
    plan.l3_skill = Some(L3SkillCandidate {
        name: "summarise_repo_readme".into(),
        description: "Read a repo README and summarise".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
        }],
    });
    assert!(plan.completion_skill().is_some());
}

#[test]
fn completion_skill_none_when_not_terminal() {
    // A non-terminal plan (has steps) must never surface a skill even if set.
    let mut plan = make_action_plan(); // existing helper: decision != task_complete OR steps non-empty
    plan.l3_skill = Some(L3SkillCandidate {
        name: "x".into(), description: "y".into(), parameters: vec![], steps: vec![],
    });
    assert!(plan.completion_skill().is_none());
}

#[test]
fn completion_skill_none_when_unset() {
    let plan = make_terminal_plan();
    assert!(plan.completion_skill().is_none());
}

#[test]
fn l3_skill_none_is_omitted_from_wire_form() {
    let plan = make_terminal_plan(); // l3_skill: None
    let v = serde_json::to_value(&plan).expect("serialize");
    assert!(v.get("l3_skill").is_none(), "skip_serializing_if must omit None");
}

#[test]
fn l3_skill_candidate_round_trips_through_serde() {
    let c = L3SkillCandidate {
        name: "n".into(), description: "d".into(),
        parameters: vec![L3Param { name: "p".into(), description: "pd".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(), method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["echo", "{{p}}"] }),
        }],
    };
    let v = serde_json::to_value(&c).expect("ser");
    let back: L3SkillCandidate = serde_json::from_value(v).expect("de");
    assert_eq!(c, back);
}
```

> If `make_terminal_plan` / `make_action_plan` helpers don't exist with those exact names, locate the existing Plan-construction test helpers in this module (search `fn make_` in `core/src/cassandra/types.rs`) and reuse them; adjust names to match.

- [ ] **Step 2: Run the tests to verify they fail to compile** (types/field/accessor don't exist yet)

Run: `cargo test -p kastellan-core --lib cassandra::types::tests::completion_skill 2>&1 | head -20`
Expected: FAIL — `cannot find type L3SkillCandidate` / `no field l3_skill` / `no method completion_skill`.

- [ ] **Step 3: Add the three types** (in `core/src/cassandra/types.rs`, immediately after the `RefusedReason` struct)

```rust
/// One declared parameter of an [`L3SkillCandidate`]. The skill's
/// step `parameters` abstract task-specific values behind `{{name}}`
/// placeholders that reference these.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3Param {
    pub name: String,
    pub description: String,
}

/// One step of an [`L3SkillCandidate`] template — a parameterised
/// JSON-RPC tool call. `parameters` is a JSON object that may embed
/// `{{param_name}}` placeholders.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3TemplateStep {
    pub tool: String,
    pub method: String,
    pub parameters: serde_json::Value,
}

/// Agent-emitted candidate for an L3 (skill-layer) memory. The agent
/// emits this on a TERMINAL plan, abstracting the multi-step trajectory
/// it just executed into a reusable, parameterised tool-call template.
///
/// Validation rules + caps live in [`crate::memory::l3_crystallise`];
/// a candidate that fails validation causes the crystallise write to be
/// skipped (a `tracing::warn!` is emitted by
/// `runner::write_l3_crystallised_row` but no audit row is written).
/// Stored skills are non-executable in this slice (no invocation path).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct L3SkillCandidate {
    pub name: String,
    pub description: String,
    pub parameters: Vec<L3Param>,
    pub steps: Vec<L3TemplateStep>,
}
```

- [ ] **Step 4: Add the `Plan.l3_skill` field** (in `struct Plan`, immediately after the `l1_insight` field at ~L143)

```rust
    /// Agent-raised L3 skill candidate. Only honoured on terminal
    /// plans that reach `Outcome::Completed` AND whose task executed
    /// >= 1 tool step (the grounding gate; see
    /// `scheduler::inner_loop`). Captured into
    /// `InnerLoopResult.terminal_l3_skill`; `runner::drain_lane` writes
    /// it to `MemoryLayer::Skill` with provenance
    /// `L3Source::AgentRaised { task_id }`, marked `trust: "untrusted"`.
    ///
    /// Round-trips through serde with `skip_serializing_if = Option::is_none`
    /// so existing fixtures stay byte-stable when the agent doesn't emit one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l3_skill: Option<L3SkillCandidate>,
```

- [ ] **Step 5: Add the `completion_skill()` accessor** (in `impl Plan`, after `completion_insight`)

```rust
    /// Returns `Some(candidate)` iff this plan would produce
    /// `Outcome::Completed` AND carries an `l3_skill`. The inner loop
    /// ANDs in the `dispatch_count >= 1` grounding gate at the call
    /// site. Mirrors [`Plan::completion_insight`].
    pub fn completion_skill(&self) -> Option<&L3SkillCandidate> {
        if self.is_terminal() {
            self.l3_skill.as_ref()
        } else {
            None
        }
    }
```

- [ ] **Step 6: Build the workspace and fix every flagged `Plan { .. }` literal**

Run: `cargo build --workspace 2>&1 | grep -A2 "missing field \`l3_skill\`" | head -60`
Expected: a list of `Plan { .. }` literals (in `inner_loop_audit.rs`, `inner_loop/tests.rs`, `cassandra/review.rs`, `cassandra/deterministic.rs`, `observation/replay.rs`, `observation/capture.rs`, `observation/replay/tests.rs`, `core/tests/observation_replay_e2e.rs`, `core/tests/scheduler_lanes_e2e.rs`, and any others).

For each flagged literal, add `l3_skill: None,` alongside the existing `l1_insight: None,` (or wherever the struct's fields end). This is purely mechanical and behaviour-preserving — the same churn the L1 slice did for `l1_insight`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p kastellan-core --lib cassandra::types::tests 2>&1 | tail -15`
Expected: PASS (all `completion_skill_*` + `l3_skill_*` green; pre-existing types tests still green).

- [ ] **Step 8: Commit**

```bash
git add core/src/cassandra/types.rs core/src/scheduler core/src/observation core/tests
git commit -m "feat(types): L3SkillCandidate + Plan.l3_skill + completion_skill() accessor"
```

---

## Task 2: `l3_crystallise` module — error/source/outcome types, constants, validator

**Files:**
- Create: `core/src/memory/l3_crystallise.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod l3_crystallise;`)

- [ ] **Step 1: Register the module** (in `core/src/memory/mod.rs`, alongside `pub mod l1_promote;`)

```rust
pub mod l3_crystallise;
```

- [ ] **Step 2: Write the module head + validator with failing tests** (create `core/src/memory/l3_crystallise.rs`)

```rust
//! Writer for `MemoryLayer::Skill` (L3) rows. One caller:
//!
//! **Agent-raised** — via `Plan.l3_skill` consumed by
//! `crate::scheduler::runner::drain_lane` on `Outcome::Completed`
//! when the task executed >= 1 tool step (the grounding gate).
//!
//! The agent emits a parameterised tool-call template on a terminal
//! plan; this module validates it (structural + `{{placeholder}}`
//! closed-world integrity + reserved-tag + caps), dedups on a
//! canonical SHA-256 over the template, and inserts at `layer = 3`
//! marked `trust: "untrusted"` via
//! [`kastellan_db::memories::insert_memory_at_layer`].
//!
//! **Crystallised skills are non-executable in this slice.** There is
//! no invocation path; `trust: "untrusted"` is a forward-compatible
//! placeholder for the future Skill trust enum. Unlike `l1_promote`,
//! there is NO entity auto-link and NO operator write path.
//!
//! See `docs/superpowers/specs/2026-05-31-l3-skill-crystallisation-design.md`.

use std::collections::BTreeSet;

use kastellan_db::memories::{insert_memory_at_layer, load_layer, Memory, MemoryLayer};
use kastellan_db::DbError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::cassandra::types::L3SkillCandidate;

/// Max bytes for the skill `name` (a stable identifier).
pub const L3_MAX_NAME_BYTES: usize = 64;
/// Max bytes for the skill `description` (becomes the memory `body`).
pub const L3_MAX_DESC_BYTES: usize = 512;
/// Max bytes for a single parameter's `description`.
pub const L3_MAX_PARAM_DESC_BYTES: usize = 256;
/// Max declared parameters.
pub const L3_MAX_PARAMS: usize = 16;
/// Max template steps (lower bound is 1 — the grounding floor).
pub const L3_MAX_STEPS: usize = 32;
/// Max bytes for a `tool` or `method` identifier.
pub const L3_MAX_IDENT_BYTES: usize = 64;
/// Max bytes for the canonical-serialised template (under the audit 4 KiB cap).
pub const L3_MAX_TEMPLATE_BYTES: usize = 4096;

/// Reserved substrings that would close/open the future `<skills>`
/// render block. Defensive against prompt-injection (threat-model §6),
/// symmetric with `l1_promote`'s `<l1_insights>` defence.
const RESERVED_TAG_OPEN: &str = "<skills>";
const RESERVED_TAG_CLOSE: &str = "</skills>";

/// Provenance for an L3 row write. The audit-row `source` field is
/// never producer-supplied; only `runner::drain_lane` constructs this.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum L3Source {
    /// Agent-raised write from `runner::drain_lane` after
    /// `Outcome::Completed`. The originating `task_id` rides in the
    /// audit-row payload for cross-restart trace stitching.
    AgentRaised { task_id: i64 },
}

/// Error kinds the L3 writer can produce.
#[derive(Debug, thiserror::Error)]
pub enum L3Error {
    #[error("L3 skill validation failed: {0}")]
    Validation(String),
    #[error("L3 db error: {0}")]
    Db(#[from] DbError),
}

/// Outcome of a single `crystallise_l3` call.
#[derive(Clone, Debug)]
pub enum L3WriteOutcome {
    /// New L3 row inserted at the carried `memory_id`.
    Inserted { memory_id: i64 },
    /// A row with the same `body_sha256` already exists at `layer = 3`.
    SkippedDuplicate { memory_id: i64 },
}

impl L3WriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            L3WriteOutcome::Inserted { memory_id }
            | L3WriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}

/// `true` iff `s` is a strict snake_case identifier: starts with
/// `[a-z]`, then `[a-z0-9_]*`. Used for skill name + parameter names.
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `true` iff `s` is a tool/method identifier: starts with `[a-z0-9]`,
/// then `[a-z0-9_.-]*`. Looser than snake_case because tool names carry
/// hyphens (`shell-exec`) and methods carry dots (`shell.exec`).
fn is_tool_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-')
}

/// Scan a single string for `{{name}}` placeholders, inserting each
/// referenced name into `out`. Rejects an unterminated `{{` and a
/// placeholder whose body is not a snake_case identifier.
fn scan_placeholders(s: &str, out: &mut BTreeSet<String>) -> Result<(), L3Error> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if !(j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}') {
                return Err(L3Error::Validation("unterminated placeholder '{{'".into()));
            }
            let ident = &s[start..j];
            if !is_snake_ident(ident) {
                return Err(L3Error::Validation(format!(
                    "malformed placeholder name '{ident}'"
                )));
            }
            out.insert(ident.to_string());
            i = j + 2;
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Recursively collect every `{{name}}` placeholder from a step's
/// `parameters` JSON value (placeholders only live in string leaves).
fn collect_placeholders(v: &serde_json::Value, out: &mut BTreeSet<String>) -> Result<(), L3Error> {
    match v {
        serde_json::Value::String(s) => scan_placeholders(s, out),
        serde_json::Value::Array(a) => {
            for e in a {
                collect_placeholders(e, out)?;
            }
            Ok(())
        }
        serde_json::Value::Object(m) => {
            for e in m.values() {
                collect_placeholders(e, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate an L3 skill candidate. On success returns a normalised
/// candidate (trimmed string fields) so the writer never stores
/// leading/trailing whitespace. On failure returns
/// [`L3Error::Validation`] with a human-readable reason.
pub fn validate_l3_skill(c: &L3SkillCandidate) -> Result<L3SkillCandidate, L3Error> {
    // --- name ---
    let name = c.name.trim();
    if name.is_empty() {
        return Err(L3Error::Validation("name is empty after trim".into()));
    }
    if name.len() > L3_MAX_NAME_BYTES {
        return Err(L3Error::Validation(format!(
            "name exceeds {L3_MAX_NAME_BYTES} bytes ({})",
            name.len()
        )));
    }
    if !is_snake_ident(name) {
        return Err(L3Error::Validation(format!(
            "name '{name}' is not snake_case ([a-z][a-z0-9_]*)"
        )));
    }

    // --- description ---
    if c.description.contains('\n') || c.description.contains('\r') {
        return Err(L3Error::Validation("description contains newline".into()));
    }
    let description = c.description.trim();
    if description.is_empty() {
        return Err(L3Error::Validation("description is empty after trim".into()));
    }
    if description.bytes().any(|b| b < 0x20) {
        return Err(L3Error::Validation("description contains control character".into()));
    }
    if description.contains(RESERVED_TAG_OPEN) || description.contains(RESERVED_TAG_CLOSE) {
        return Err(L3Error::Validation("description contains reserved tag substring".into()));
    }
    if description.len() > L3_MAX_DESC_BYTES {
        return Err(L3Error::Validation(format!(
            "description exceeds {L3_MAX_DESC_BYTES} bytes ({})",
            description.len()
        )));
    }

    // --- parameters ---
    if c.parameters.len() > L3_MAX_PARAMS {
        return Err(L3Error::Validation(format!(
            "too many parameters ({} > {L3_MAX_PARAMS})",
            c.parameters.len()
        )));
    }
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut norm_params: Vec<crate::cassandra::types::L3Param> = Vec::with_capacity(c.parameters.len());
    for p in &c.parameters {
        let pn = p.name.trim();
        if !is_snake_ident(pn) {
            return Err(L3Error::Validation(format!(
                "parameter name '{pn}' is not snake_case"
            )));
        }
        if !declared.insert(pn.to_string()) {
            return Err(L3Error::Validation(format!("duplicate parameter '{pn}'")));
        }
        let pd = p.description.trim();
        if pd.is_empty() {
            return Err(L3Error::Validation(format!("parameter '{pn}' has empty description")));
        }
        if pd.len() > L3_MAX_PARAM_DESC_BYTES {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' description exceeds {L3_MAX_PARAM_DESC_BYTES} bytes"
            )));
        }
        norm_params.push(crate::cassandra::types::L3Param {
            name: pn.to_string(),
            description: pd.to_string(),
        });
    }

    // --- steps ---
    if c.steps.is_empty() {
        return Err(L3Error::Validation("skill must have at least one step".into()));
    }
    if c.steps.len() > L3_MAX_STEPS {
        return Err(L3Error::Validation(format!(
            "too many steps ({} > {L3_MAX_STEPS})",
            c.steps.len()
        )));
    }
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut norm_steps: Vec<crate::cassandra::types::L3TemplateStep> = Vec::with_capacity(c.steps.len());
    for s in &c.steps {
        let tool = s.tool.trim();
        let method = s.method.trim();
        if tool.len() > L3_MAX_IDENT_BYTES || !is_tool_ident(tool) {
            return Err(L3Error::Validation(format!("step tool '{tool}' is invalid")));
        }
        if method.len() > L3_MAX_IDENT_BYTES || !is_tool_ident(method) {
            return Err(L3Error::Validation(format!("step method '{method}' is invalid")));
        }
        if !s.parameters.is_object() {
            return Err(L3Error::Validation("step parameters must be a JSON object".into()));
        }
        collect_placeholders(&s.parameters, &mut referenced)?;
        norm_steps.push(crate::cassandra::types::L3TemplateStep {
            tool: tool.to_string(),
            method: method.to_string(),
            parameters: s.parameters.clone(),
        });
    }

    // --- closed-world placeholder invariant ---
    for r in &referenced {
        if !declared.contains(r) {
            return Err(L3Error::Validation(format!("undeclared placeholder '{r}'")));
        }
    }
    for d in &declared {
        if !referenced.contains(d) {
            return Err(L3Error::Validation(format!("unused parameter '{d}'")));
        }
    }

    let normalised = L3SkillCandidate {
        name: name.to_string(),
        description: description.to_string(),
        parameters: norm_params,
        steps: norm_steps,
    };

    // --- total size cap (canonical form) ---
    let canonical = canonical_json(&normalised);
    if canonical.len() > L3_MAX_TEMPLATE_BYTES {
        return Err(L3Error::Validation(format!(
            "template exceeds {L3_MAX_TEMPLATE_BYTES} bytes ({})",
            canonical.len()
        )));
    }

    Ok(normalised)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{L3Param, L3TemplateStep};

    fn valid_candidate() -> L3SkillCandidate {
        L3SkillCandidate {
            name: "summarise_repo_readme".into(),
            description: "Read a repo README and return a summary".into(),
            parameters: vec![L3Param {
                name: "repo_path".into(),
                description: "absolute path to the repo".into(),
            }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
            }],
        }
    }

    #[test]
    fn accepts_a_well_formed_candidate() {
        let c = valid_candidate();
        let n = validate_l3_skill(&c).expect("valid");
        assert_eq!(n.name, "summarise_repo_readme");
        assert_eq!(n.steps.len(), 1);
    }

    #[test]
    fn rejects_non_snake_name() {
        let mut c = valid_candidate();
        c.name = "Summarise-Repo".into();
        let e = validate_l3_skill(&c).expect_err("bad name");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("snake_case")));
    }

    #[test]
    fn rejects_empty_description() {
        let mut c = valid_candidate();
        c.description = "   ".into();
        let e = validate_l3_skill(&c).expect_err("empty desc");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("empty")));
    }

    #[test]
    fn rejects_newline_description() {
        let mut c = valid_candidate();
        c.description = "line1\nline2".into();
        let e = validate_l3_skill(&c).expect_err("newline");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("newline")));
    }

    #[test]
    fn rejects_reserved_tag_description() {
        let mut c = valid_candidate();
        c.description = "before </skills> after".into();
        let e = validate_l3_skill(&c).expect_err("reserved");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("reserved tag")));
    }

    #[test]
    fn rejects_zero_steps() {
        let mut c = valid_candidate();
        c.steps = vec![];
        let e = validate_l3_skill(&c).expect_err("no steps");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("at least one step")));
    }

    #[test]
    fn rejects_too_many_steps() {
        let mut c = valid_candidate();
        let step = c.steps[0].clone();
        c.steps = std::iter::repeat(step).take(L3_MAX_STEPS + 1).collect();
        let e = validate_l3_skill(&c).expect_err("too many");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("too many steps")));
    }

    #[test]
    fn rejects_non_object_step_params() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!(["not", "an", "object"]);
        let e = validate_l3_skill(&c).expect_err("non-object");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("must be a JSON object")));
    }

    #[test]
    fn rejects_undeclared_placeholder() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!({ "argv": ["cat", "{{unknown}}"] });
        let e = validate_l3_skill(&c).expect_err("undeclared");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("undeclared placeholder 'unknown'")));
    }

    #[test]
    fn rejects_unused_parameter() {
        let mut c = valid_candidate();
        c.parameters.push(L3Param { name: "extra".into(), description: "never used".into() });
        let e = validate_l3_skill(&c).expect_err("unused");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("unused parameter 'extra'")));
    }

    #[test]
    fn rejects_duplicate_parameter() {
        let mut c = valid_candidate();
        c.parameters.push(L3Param { name: "repo_path".into(), description: "dup".into() });
        let e = validate_l3_skill(&c).expect_err("dup");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("duplicate parameter")));
    }

    #[test]
    fn rejects_malformed_placeholder() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!({ "argv": ["cat", "{{repo-path}}"] });
        let e = validate_l3_skill(&c).expect_err("malformed");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("malformed placeholder")));
    }

    #[test]
    fn accepts_tool_and_method_with_hyphen_and_dot() {
        let c = valid_candidate(); // shell-exec / shell.exec
        assert!(validate_l3_skill(&c).is_ok());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail** (compile error — `canonical_json` not defined yet)

Run: `cargo test -p kastellan-core --lib memory::l3_crystallise 2>&1 | head -20`
Expected: FAIL — `cannot find function canonical_json`. (Defined in Task 3.) This is expected; proceed to Task 3 which makes the module compile, then re-run.

- [ ] **Step 4: Commit (WIP — module compiles after Task 3)**

> Do NOT commit yet — Task 3 completes the module so it compiles. Tasks 2+3 land in one commit at the end of Task 3.

---

## Task 3: `l3_crystallise` — canonical JSON, SHA-256, metadata builder

**Files:**
- Modify: `core/src/memory/l3_crystallise.rs`

- [ ] **Step 1: Add canonical-JSON + SHA + metadata helpers** (after `validate_l3_skill`, before `#[cfg(test)]`)

```rust
/// Recursively sort all object keys so two candidates that differ only
/// in JSON key order serialise identically. Load-bearing for dedup:
/// a non-canonical serialiser would under-dedup.
fn sort_value_keys(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut entries: Vec<(String, serde_json::Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::new();
            for (k, val) in entries {
                sorted.insert(k, sort_value_keys(val));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(sort_value_keys).collect())
        }
        other => other,
    }
}

/// Deterministic JSON string for a candidate (object keys sorted at
/// every depth; array order — i.e. step order — preserved).
pub fn canonical_json(c: &L3SkillCandidate) -> String {
    let v = serde_json::to_value(c).expect("L3SkillCandidate serialises");
    serde_json::to_string(&sort_value_keys(v)).expect("canonical serialise")
}

/// SHA-256 over the canonical template, lowercase 64-char hex.
pub fn compute_template_sha256(c: &L3SkillCandidate) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(c).as_bytes());
    format!("{:x}", h.finalize())
}

/// Build the `metadata` JSONB for a new L3 row. Schema:
/// `{source, task_id, trust, body_sha256, created_at, template}`.
/// `template` is the full normalised candidate (name/parameters/steps;
/// the `description` is duplicated there + as the memory `body`).
///
/// **Coupling note:** the literal `"agent_raised"` MUST match
/// `L3Source`'s serde `rename_all = "snake_case"` output. Cross-pinned
/// by `build_l3_metadata_serde_agrees_with_l3_source`.
pub(crate) fn build_l3_metadata(
    source: &L3Source,
    candidate: &L3SkillCandidate,
    body_sha256: &str,
    created_at_rfc3339: &str,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match source {
        L3Source::AgentRaised { task_id } => {
            obj.insert("source".into(), serde_json::Value::String("agent_raised".into()));
            obj.insert(
                "task_id".into(),
                serde_json::Value::Number(serde_json::Number::from(*task_id)),
            );
        }
    }
    obj.insert("trust".into(), serde_json::Value::String("untrusted".into()));
    obj.insert("body_sha256".into(), serde_json::Value::String(body_sha256.into()));
    obj.insert("created_at".into(), serde_json::Value::String(created_at_rfc3339.into()));
    obj.insert(
        "template".into(),
        serde_json::to_value(candidate).expect("candidate serialises"),
    );
    serde_json::Value::Object(obj)
}
```

- [ ] **Step 2: Add the helper tests** (inside the `tests` module)

```rust
    #[test]
    fn canonical_json_is_key_order_independent() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!({ "b": "{{repo_path}}", "a": 1 });
        c.parameters[0] = L3Param { name: "repo_path".into(), description: "p".into() };
        let s1 = canonical_json(&c);
        // Rebuild the same logical params with keys in the other order:
        c.steps[0].parameters = serde_json::json!({ "a": 1, "b": "{{repo_path}}" });
        let s2 = canonical_json(&c);
        assert_eq!(s1, s2, "canonical_json must be key-order independent");
    }

    #[test]
    fn compute_template_sha256_is_deterministic_and_64_hex() {
        let c = valid_candidate();
        let a = compute_template_sha256(&c);
        let b = compute_template_sha256(&c);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    #[test]
    fn build_l3_metadata_has_expected_keys() {
        let c = validate_l3_skill(&valid_candidate()).expect("valid");
        let m = build_l3_metadata(
            &L3Source::AgentRaised { task_id: 7 }, &c, "abc", "2026-05-31T00:00:00Z",
        );
        let obj = m.as_object().expect("object");
        assert_eq!(obj.get("source").unwrap(), "agent_raised");
        assert_eq!(obj.get("task_id").unwrap(), 7);
        assert_eq!(obj.get("trust").unwrap(), "untrusted");
        assert_eq!(obj.get("body_sha256").unwrap(), "abc");
        assert_eq!(obj.get("created_at").unwrap(), "2026-05-31T00:00:00Z");
        assert!(obj.get("template").unwrap().get("name").is_some());
        assert_eq!(obj.len(), 6, "exactly 6 metadata keys");
    }

    #[test]
    fn build_l3_metadata_serde_agrees_with_l3_source() {
        let v = serde_json::to_value(L3Source::AgentRaised { task_id: 1 }).expect("ser");
        assert_eq!(v.get("source").unwrap().as_str().unwrap(), "agent_raised");
        let c = validate_l3_skill(&valid_candidate()).expect("valid");
        let m = build_l3_metadata(&L3Source::AgentRaised { task_id: 1 }, &c, "s", "2026-05-31T00:00:00Z");
        assert_eq!(m.get("source").unwrap().as_str().unwrap(), "agent_raised");
    }
```

- [ ] **Step 3: Run the module's unit tests** (now compiles)

Run: `cargo test -p kastellan-core --lib memory::l3_crystallise 2>&1 | tail -20`
Expected: PASS (validator + canonical/SHA/metadata tests all green).

- [ ] **Step 4: Commit (Tasks 2+3)**

```bash
git add core/src/memory/mod.rs core/src/memory/l3_crystallise.rs
git commit -m "feat(memory): l3_crystallise validator + canonical-SHA + metadata builder"
```

---

## Task 4: `l3_crystallise` — async writer (`crystallise_l3`) + `list_l3` + `remove_l3`

**Files:**
- Modify: `core/src/memory/l3_crystallise.rs`

- [ ] **Step 1: Add the async writer + list/remove** (after `build_l3_metadata`)

```rust
/// Crystallise a single L3 skill. Validates, computes the canonical
/// SHA-256, EXISTS-checks against `layer = 3` rows by
/// `metadata->>'body_sha256'`, inserts on miss with `body = description`
/// and `trust: "untrusted"`. Idempotent on the template SHA.
///
/// **No entity auto-link** (unlike `promote_l1`): a skill's description
/// is not an entity-bearing insight, and recall surfacing is out of
/// scope this slice.
pub async fn crystallise_l3(
    pool: &PgPool,
    candidate: &L3SkillCandidate,
    source: L3Source,
) -> Result<L3WriteOutcome, L3Error> {
    let normalised = validate_l3_skill(candidate)?;
    let body_sha256 = compute_template_sha256(&normalised);

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM memories \
         WHERE layer = $1 AND metadata->>'body_sha256' = $2 \
         LIMIT 1",
    )
    .bind(MemoryLayer::Skill.as_db())
    .bind(&body_sha256)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        L3Error::Db(kastellan_db::DbError::Query(format!(
            "crystallise_l3 EXISTS-check body_sha256={body_sha256}: {e}"
        )))
    })?;

    if let Some(existing_id) = existing {
        return Ok(L3WriteOutcome::SkippedDuplicate { memory_id: existing_id });
    }

    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 format");
    let metadata = build_l3_metadata(&source, &normalised, &body_sha256, &created_at);

    let new_id = insert_memory_at_layer(
        pool,
        &normalised.description, // the body is the human description
        &metadata,
        None, // no embedding for L3 v1
        MemoryLayer::Skill,
    )
    .await?;

    Ok(L3WriteOutcome::Inserted { memory_id: new_id })
}

/// Operator-facing list view: every row at `layer = 3`, newest-first.
pub async fn list_l3(pool: &PgPool) -> Result<Vec<Memory>, DbError> {
    load_layer(pool, MemoryLayer::Skill, usize::MAX).await
}

/// Operator-facing remove, layer-guarded via
/// `kastellan_db::memories::delete_memory_at_layer` (cannot delete an
/// L0/L1/L2 row even on a typoed id). Returns `true` iff a row was deleted.
pub async fn remove_l3(pool: &PgPool, id: i64) -> Result<bool, DbError> {
    kastellan_db::memories::delete_memory_at_layer(pool, id, MemoryLayer::Skill).await
}
```

- [ ] **Step 2: Add signature-pin unit tests** (inside `tests`; compile-only, full DB coverage in Task 12)

```rust
    #[test]
    fn crystallise_l3_signature_compile_pin() {
        fn _pin<'a>(
            pool: &'a sqlx::PgPool,
            c: &'a L3SkillCandidate,
            source: L3Source,
        ) -> impl std::future::Future<Output = Result<L3WriteOutcome, L3Error>> + 'a {
            crystallise_l3(pool, c, source)
        }
        let _ = _pin;
    }

    #[test]
    fn list_remove_signature_compile_pins() {
        fn _list<'a>(p: &'a sqlx::PgPool)
            -> impl std::future::Future<Output = Result<Vec<Memory>, DbError>> + 'a { list_l3(p) }
        fn _remove<'a>(p: &'a sqlx::PgPool, id: i64)
            -> impl std::future::Future<Output = Result<bool, DbError>> + 'a { remove_l3(p, id) }
        let _ = (_list, _remove);
    }
```

- [ ] **Step 3: Build + run module tests + clippy**

Run: `cargo test -p kastellan-core --lib memory::l3_crystallise 2>&1 | tail -10 && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5`
Expected: tests PASS; clippy exit 0.

- [ ] **Step 4: Commit**

```bash
git add core/src/memory/l3_crystallise.rs
git commit -m "feat(memory): crystallise_l3 writer + list_l3/remove_l3"
```

---

## Task 5: Audit constants + `build_l3_write_payload`

**Files:**
- Modify: `core/src/scheduler/audit.rs` (add consts near `ACTION_L1_*` ~L99-110; add `use` near L70; add helper near `build_l1_write_payload` ~L391; add tests)

- [ ] **Step 1: Write the failing payload tests** (in `core/src/scheduler/audit.rs` `tests` module)

```rust
    #[test]
    fn build_l3_write_payload_inserted_agent_raised() {
        use crate::memory::l3_crystallise::{L3Source, L3WriteOutcome};
        let p = build_l3_write_payload(
            &L3WriteOutcome::Inserted { memory_id: 11 },
            &L3Source::AgentRaised { task_id: 42 },
            "summarise_repo_readme",
            "abc123",
        );
        let o = p.as_object().expect("object");
        assert_eq!(o.get("source").unwrap(), "agent_raised");
        assert_eq!(o.get("task_id").unwrap(), 42);
        assert_eq!(o.get("skill_name").unwrap(), "summarise_repo_readme");
        assert_eq!(o.get("action").unwrap(), "inserted");
        assert_eq!(o.get("memory_id").unwrap(), 11);
        assert_eq!(o.get("body_sha256").unwrap(), "abc123");
        assert_eq!(o.len(), 6, "exactly 6 payload keys");
    }

    #[test]
    fn build_l3_write_payload_skipped_duplicate() {
        use crate::memory::l3_crystallise::{L3Source, L3WriteOutcome};
        let p = build_l3_write_payload(
            &L3WriteOutcome::SkippedDuplicate { memory_id: 9 },
            &L3Source::AgentRaised { task_id: 1 },
            "n", "s",
        );
        assert_eq!(p.get("action").unwrap(), "skipped_duplicate");
        assert_eq!(p.get("memory_id").unwrap(), 9);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p kastellan-core --lib scheduler::audit::tests::build_l3 2>&1 | head -15`
Expected: FAIL — `cannot find function build_l3_write_payload` + `ACTION_L3_*`.

- [ ] **Step 3: Add the imports, constants, and helper**

Add to the existing `use crate::memory::l1_promote::{L1Source, L1WriteOutcome};` line (L70) a sibling import:
```rust
use crate::memory::l3_crystallise::{L3Source, L3WriteOutcome};
```
Add near the `ACTION_L1_*` constants (~L99-110):
```rust
/// Action verb for the agent-raised L3 crystallisation row written by
/// `runner::drain_lane`. Payload built by [`build_l3_write_payload`].
pub const ACTION_L3_CRYSTALLISED: &str = "l3.crystallised";
/// Action verb for the operator `memory l3 remove` audit row.
pub const ACTION_L3_REMOVED: &str = "l3.removed";
```
Add near `build_l1_write_payload` (~L391):
```rust
/// Build the payload for the `l3.crystallised` audit row. Shape:
/// `{source: "agent_raised", task_id, skill_name, action, memory_id, body_sha256}` (6 keys).
pub fn build_l3_write_payload(
    outcome: &L3WriteOutcome,
    source: &L3Source,
    skill_name: &str,
    body_sha256: &str,
) -> Value {
    let mut obj = serde_json::Map::new();
    match source {
        L3Source::AgentRaised { task_id } => {
            obj.insert("source".into(), Value::String("agent_raised".into()));
            obj.insert("task_id".into(), Value::Number(serde_json::Number::from(*task_id)));
        }
    }
    let (action_str, memory_id) = match outcome {
        L3WriteOutcome::Inserted { memory_id } => ("inserted", *memory_id),
        L3WriteOutcome::SkippedDuplicate { memory_id } => ("skipped_duplicate", *memory_id),
    };
    obj.insert("skill_name".into(), Value::String(skill_name.into()));
    obj.insert("action".into(), Value::String(action_str.into()));
    obj.insert("memory_id".into(), Value::Number(serde_json::Number::from(memory_id)));
    obj.insert("body_sha256".into(), Value::String(body_sha256.into()));
    Value::Object(obj)
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p kastellan-core --lib scheduler::audit::tests::build_l3 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/audit.rs
git commit -m "feat(audit): ACTION_L3_* constants + build_l3_write_payload"
```

---

## Task 6: `InnerLoopResult.terminal_l3_skill` + `finish!` macro + Completed-arm grounding gate

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (struct ~L121-132; macro ~L217-230; Completed arm ~L382-386)
- Modify: `core/src/scheduler/runner.rs` (InnerLoopResult test literal ~L433)
- Modify: `core/src/scheduler/inner_loop/tests.rs` (any InnerLoopResult literals)

- [ ] **Step 1: Add the struct field** (in `InnerLoopResult`, after `terminal_l1_insight`)

```rust
    /// `l3_skill` from the terminal plan, captured only when the inner
    /// loop reaches `Outcome::Completed` AND the task executed >= 1 tool
    /// step (`dispatch_count >= 1`). The lane runner reads this in
    /// `drain_lane` and writes one `actor='scheduler'
    /// action='l3.crystallised'` audit row if `Some`. `None` otherwise.
    pub terminal_l3_skill: Option<crate::cassandra::types::L3SkillCandidate>,
```

- [ ] **Step 2: Update the `finish!` macro** (3-arg primary + 1-arg convenience)

Replace the macro body so the primary form takes `($outcome, $insight, $skill)` and the convenience form defaults both to `None`:
```rust
    macro_rules! finish {
        ($outcome:expr, $insight:expr, $skill:expr) => {
            Ok(InnerLoopResult {
                outcome: $outcome,
                plan_count: ctx.plan_count,
                dispatch_count,
                terminal_l1_insight: $insight,
                terminal_l3_skill: $skill,
            })
        };
        // Convenience form for all non-Completed arms: both None.
        ($outcome:expr) => {
            finish!($outcome, None, None)
        };
    }
```

- [ ] **Step 3: Update the Completed arm** (the only 2-arg `finish!` call, ~L382-386)

```rust
            let captured_l1_insight: Option<String> = plan.completion_insight().map(|s| s.to_string());
            // Grounding gate: only crystallise a skill if the task
            // actually executed >= 1 tool step (dispatch_count is the
            // running per-task counter). A pure-text-answer task
            // (terminal on plan 1, zero dispatches) emits no skill.
            let captured_l3_skill: Option<crate::cassandra::types::L3SkillCandidate> =
                if dispatch_count >= 1 {
                    plan.completion_skill().cloned()
                } else {
                    None
                };
            return finish!(Outcome::Completed(result), captured_l1_insight, captured_l3_skill);
```

- [ ] **Step 4: Build and fix the flagged `InnerLoopResult` literals**

Run: `cargo build -p kastellan-core 2>&1 | grep -A2 "missing field \`terminal_l3_skill\`" | head -30`
Expected: literals in `core/src/scheduler/runner.rs` (~L433, a test) and `core/src/scheduler/inner_loop/tests.rs`. Add `terminal_l3_skill: None,` next to each `terminal_l1_insight: None,`.

- [ ] **Step 5: Run the inner-loop unit tests**

Run: `cargo test -p kastellan-core --lib scheduler::inner_loop 2>&1 | tail -12`
Expected: PASS (22/22 — unchanged; the macro change is behaviour-preserving for every non-Completed arm, and the Completed arm now also captures the skill).

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/inner_loop.rs core/src/scheduler/inner_loop/tests.rs core/src/scheduler/runner.rs
git commit -m "feat(scheduler): InnerLoopResult.terminal_l3_skill + dispatch_count grounding gate"
```

---

## Task 7: `build_plan_formulate_payload` gains the compact `l3_skill` key

**Files:**
- Modify: `core/src/scheduler/inner_loop_audit.rs` (insert key after `l1_insight` ~L128; update key-count pins ~L353, ~L393; add 1 new pin test)

- [ ] **Step 1: Update the two key-count pin tests + add a shape pin** (in `inner_loop_audit.rs` `tests`)

In `build_plan_formulate_payload_pins_twenty_four_keys_for_default_source` (~L353): rename to `..._twenty_five_keys_...`, add `"l3_skill",` to the `expected` set, and change the `24 keys` message to `25 keys`.

In `build_plan_formulate_payload_cli_inferred_source_has_25_keys_with_signals` (~L393): rename to `..._26_keys_...`, change `assert_eq!(obj.len(), 25` → `26` and the message `25 keys (24 default + signals)` → `26 keys (25 default + signals)`.

Add a new shape pin:
```rust
    #[test]
    fn build_plan_formulate_payload_l3_skill_compact_shape() {
        use crate::cassandra::types::{L3Param, L3SkillCandidate, L3TemplateStep};
        let mut plan = make_text_plan();
        plan.l3_skill = Some(L3SkillCandidate {
            name: "summarise_repo_readme".into(),
            description: "d".into(),
            parameters: vec![L3Param { name: "repo_path".into(), description: "p".into() }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(), method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}"] }),
            }],
        });
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &plan, &make_default_meta(),
        );
        assert_eq!(payload["l3_skill"], serde_json::json!({
            "name": "summarise_repo_readme", "step_count": 1, "param_count": 1
        }));

        // None case: explicit JSON null, not key-absent.
        let none_payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &make_default_meta(),
        );
        assert_eq!(none_payload["l3_skill"], serde_json::Value::Null);
        assert!(none_payload.as_object().unwrap().contains_key("l3_skill"));
    }
```

- [ ] **Step 2: Run to verify the count pins now fail** (key not added to production yet)

Run: `cargo test -p kastellan-core --lib scheduler::inner_loop_audit 2>&1 | tail -20`
Expected: FAIL — the renamed `..._twenty_five_keys_...` test reports `l3_skill` missing; the new shape pin fails.

- [ ] **Step 3: Add the `l3_skill` key to `build_plan_formulate_payload`** (immediately after the `l1_insight` insertion, ~L128)

```rust
    // Slice (l3-skill-crystallisation, 2026-05-31): compact summary of
    // the agent-raised L3 skill candidate on the terminal plan. Explicit
    // JSON null (not key-absent) so `WHERE payload ? 'l3_skill'` finds
    // every row — mirrors the `l1_insight` / `refused` precedent. The
    // full template lives in the crystallised memories row, not here.
    obj.insert(
        "l3_skill".into(),
        match &plan.l3_skill {
            Some(s) => serde_json::json!({
                "name": s.name,
                "step_count": s.steps.len(),
                "param_count": s.parameters.len(),
            }),
            None => serde_json::Value::Null,
        },
    );
```

- [ ] **Step 4: Run to verify all pins pass**

Run: `cargo test -p kastellan-core --lib scheduler::inner_loop_audit 2>&1 | tail -12`
Expected: PASS (count pins at 25/26; new shape pin green).

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/inner_loop_audit.rs
git commit -m "feat(audit): plan.formulate gains compact l3_skill key (25/26 keys)"
```

---

## Task 8: `drain_lane` L3 hook + `write_l3_crystallised_row`

**Files:**
- Modify: `core/src/scheduler/runner.rs` (hook after the L1 hook ~L240; new fn after `write_l1_promoted_row` ~L340)

- [ ] **Step 1: Add the hook** (in `drain_lane`, immediately after the L1 `if let Some(insight) ...` block ~L240)

```rust
        // Agent-raised L3 skill crystallisation. Best-effort, same
        // posture as the L1 hook. terminal_l3_skill is Some only on
        // Outcome::Completed + dispatch_count >= 1 (the grounding gate);
        // all other outcomes leave it None, so this is a no-op for them.
        if let Some(skill) = result.terminal_l3_skill.as_ref() {
            write_l3_crystallised_row(pool, claimed.id, skill).await;
        }
```

- [ ] **Step 2: Add the writer fn** (after `write_l1_promoted_row` ~L340)

```rust
/// Crystallise the agent-raised L3 skill + emit one `actor='scheduler'
/// action='l3.crystallised'` audit row. Best-effort: errors (validation
/// or DB) are logged at WARN and swallowed — the task is already
/// finalized; the L3 row + audit row are observability aids.
async fn write_l3_crystallised_row(
    pool: &PgPool,
    task_id: i64,
    skill: &crate::cassandra::types::L3SkillCandidate,
) {
    use crate::memory::l3_crystallise::{crystallise_l3, compute_template_sha256, L3Error, L3Source};

    let source = L3Source::AgentRaised { task_id };
    let outcome = match crystallise_l3(pool, skill, source.clone()).await {
        Ok(o) => o,
        Err(L3Error::Validation(msg)) => {
            tracing::warn!(
                task_id, error = %msg,
                "agent-raised L3 crystallisation rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(L3Error::Db(e)) => {
            tracing::warn!(
                task_id, error = %e,
                "agent-raised L3 crystallisation DB error (skipping audit row)"
            );
            return;
        }
    };

    // SHA over the SAME normalised candidate the writer stored. crystallise_l3
    // validates internally; re-validate here to obtain the normalised form so
    // the audited SHA matches the stored row. Validation cannot fail now (it
    // already passed inside crystallise_l3), but handle defensively.
    let body_sha256 = match crate::memory::l3_crystallise::validate_l3_skill(skill) {
        Ok(normalised) => compute_template_sha256(&normalised),
        Err(_) => return, // unreachable: crystallise_l3 already validated
    };
    let skill_name = skill.name.trim();
    let payload = build_l3_write_payload(&outcome, &source, skill_name, &body_sha256);

    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_CRYSTALLISED, payload,
    ).await {
        tracing::warn!(
            task_id, error = %e,
            "audit insert for scheduler l3.crystallised row failed (best-effort)"
        );
    }
}
```

- [ ] **Step 3: Add the imports** (extend runner.rs's existing `use crate::scheduler::audit::{...}` to include `ACTION_L3_CRYSTALLISED` and `build_l3_write_payload`)

Search the top-of-file `use crate::scheduler::audit::{` block and add `ACTION_L3_CRYSTALLISED, build_l3_write_payload,` (next to the `ACTION_L1_PROMOTED, build_l1_write_payload` already imported there).

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p kastellan-core 2>&1 | tail -5 && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5`
Expected: builds clean; clippy exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler/runner.rs
git commit -m "feat(scheduler): drain_lane L3 crystallisation hook + write_l3_crystallised_row"
```

---

## Task 9: `cli_audit::l3_remove_and_audit`

**Files:**
- Modify: `core/src/cli_audit.rs` (add helper near `l1_remove_and_audit` ~L577; extend audit-consts `use` ~L102; add signature pin test)

- [ ] **Step 1: Write the failing signature-pin test** (in `cli_audit.rs` `tests`)

```rust
    #[test]
    fn l3_remove_and_audit_signature_compile_pin() {
        fn _pin<'a>(pool: &'a sqlx::PgPool, id: i64)
            -> impl std::future::Future<Output = Result<(bool, i64), kastellan_db::DbError>> + 'a {
            super::l3_remove_and_audit(pool, id)
        }
        let _ = _pin;
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kastellan-core --lib cli_audit::tests::l3_remove 2>&1 | head -12`
Expected: FAIL — `cannot find function l3_remove_and_audit`.

- [ ] **Step 3: Add `ACTION_L3_REMOVED` to the audit-consts import** (the `use crate::scheduler::audit::{ ... }` block ~L97-103)

Add `ACTION_L3_REMOVED,` to the imported set.

- [ ] **Step 4: Add the helper** (after `l1_remove_and_audit` ~L597)

```rust
/// Compose `memory::l3_crystallise::remove_l3` with one `actor='cli'
/// action='l3.removed'` audit row. The row is written even when
/// `deleted = false` (records the operator intent + missing-id outcome).
pub async fn l3_remove_and_audit(
    pool: &PgPool,
    memory_id: i64,
) -> Result<(bool, i64), kastellan_db::DbError> {
    use crate::memory::l3_crystallise::remove_l3;

    let deleted = remove_l3(pool, memory_id).await?;
    let payload = serde_json::json!({"memory_id": memory_id, "deleted": deleted});

    let audit_id = match kastellan_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_REMOVED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.removed audit insert failed (best-effort)");
            0
        }
    };

    Ok((deleted, audit_id))
}
```

- [ ] **Step 5: Run to verify it passes + clippy**

Run: `cargo test -p kastellan-core --lib cli_audit::tests::l3_remove 2>&1 | tail -6 && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -3`
Expected: PASS; clippy exit 0.

- [ ] **Step 6: Commit**

```bash
git add core/src/cli_audit.rs
git commit -m "feat(cli_audit): l3_remove_and_audit"
```

---

## Task 10: CLI — `memory l3 {list, remove}`

**Files:**
- Create: `core/src/bin/kastellan-cli/memory_l3.rs`
- Modify: `core/src/bin/kastellan-cli/main.rs` (add `mod memory_l3;` near `mod memory_l1;` ~L124)
- Modify: `core/src/bin/kastellan-cli/memory_l1.rs` (extend `run_memory` dispatch to route `l3`)

- [ ] **Step 1: Create the CLI module** (`core/src/bin/kastellan-cli/memory_l3.rs`)

```rust
//! `memory l3 {list,remove}` — operator-facing inspection + pruning of
//! layer-3 (crystallised skill) memories. Skills are agent-crystallised,
//! never operator-authored, so there is no `add`. `remove` emits one
//! `actor='cli' action='l3.removed'` audit row.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

pub(crate) fn run_memory_l3(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli memory l3 <list|remove> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"   => with_runtime("memory l3", memory_l3_list(&args[1..])),
        "remove" => with_runtime("memory l3", memory_l3_remove(&args[1..])),
        other    => {
            eprintln!("memory l3: unknown action '{other}'; expected: list | remove");
            ExitCode::from(2)
        }
    }
}

async fn memory_l3_list(args: &[String]) -> ExitCode {
    use kastellan_core::memory::l3_crystallise::list_l3;
    use kastellan_db::pool::connect_runtime_pool;

    if !args.is_empty() {
        eprintln!("memory l3 list: takes no arguments");
        return ExitCode::from(2);
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let rows = match list_l3(&pool).await {
        Ok(r) => r,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    println!("{:<8}  {:<24}  {:<10}  NAME / DESCRIPTION", "ID", "CREATED_AT", "TRUST");
    for r in rows {
        let trust = r.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or("?");
        let name = r.metadata
            .get("template").and_then(|t| t.get("name")).and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("{:<8}  {:<24}  {:<10}  {} — {}", r.id, r.created_at, trust, name, r.body);
    }
    ExitCode::from(0)
}

async fn memory_l3_remove(args: &[String]) -> ExitCode {
    use kastellan_core::cli_audit::l3_remove_and_audit;
    use kastellan_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: kastellan-cli memory l3 remove <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("memory l3 remove: invalid id '{id_str}': {e}");
            return ExitCode::from(2);
        }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match l3_remove_and_audit(&pool, id).await {
        Ok((true, _))  => { println!("removed id={id}"); ExitCode::from(0) }
        Ok((false, _)) => {
            println!("no row at layer 3 with id={id} (already gone or wrong layer)");
            ExitCode::from(0)
        }
        Err(e) => { eprintln!("memory l3 remove: {e}"); ExitCode::from(1) }
    }
}
```

- [ ] **Step 2: Declare the module in `main.rs`** (next to `mod memory_l1;` ~L124)

```rust
mod memory_l3;
```

- [ ] **Step 3: Route `l3` in the `run_memory` dispatcher** (`core/src/bin/kastellan-cli/memory_l1.rs`, the `run_memory` match ~L17-23)

Replace the match in `run_memory` with:
```rust
    match args[0].as_str() {
        "l1"  => run_memory_l1(&args[1..]),
        "l3"  => crate::memory_l3::run_memory_l3(&args[1..]),
        other => {
            eprintln!("memory: unknown subgroup '{other}'; expected: l1 | l3");
            ExitCode::from(2)
        }
    }
```
And update the two `usage:` lines in `run_memory` / `run_memory_l1` that say `memory l1 <add|list|remove>` to keep them accurate (the top-level one should read `usage: kastellan-cli memory <l1|l3> ...`).

- [ ] **Step 4: Build + smoke-test the dispatch (no PG needed for arg-parse paths)**

Run: `cargo build -p kastellan-core --bin kastellan-cli 2>&1 | tail -5`
Then: `./target/debug/kastellan-cli memory l3 2>&1; echo "exit=$?"`
Expected: prints the `usage: ... memory l3 <list|remove>` line, `exit=2`.
Then: `./target/debug/kastellan-cli memory l3 bogus 2>&1; echo "exit=$?"`
Expected: `unknown action 'bogus'`, `exit=2`.

- [ ] **Step 5: Commit**

```bash
git add core/src/bin/kastellan-cli/memory_l3.rs core/src/bin/kastellan-cli/main.rs core/src/bin/kastellan-cli/memory_l1.rs
git commit -m "feat(cli): memory l3 {list,remove} subcommands"
```

---

## Task 11: Teach the planner to emit `l3_skill`

**Files:**
- Modify: `prompts/agent_planner.md`

- [ ] **Step 1: Locate the `l1_insight` teaching** in `prompts/agent_planner.md` (search `l1_insight`). Immediately after it, add the L3 teaching paragraph:

```markdown
### Optional: `l3_skill` (crystallise a reusable skill)

On a TERMINAL plan (`decision: "task_complete"`) that completed a
**multi-step** task you expect to recur, you MAY emit an `l3_skill`
object that abstracts the tool-call sequence you just ran into a
reusable, parameterised template. Omit it (or set `null`) for trivial,
one-off, or pure-text-answer tasks.

Shape:

- `name`: a snake_case identifier (`[a-z][a-z0-9_]*`, ≤ 64 chars), e.g. `summarise_repo_readme`.
- `description`: one line (≤ 512 chars, no newlines) describing what the skill does.
- `parameters`: the task-specific values you abstracted, each `{name, description}`. `name` is snake_case. Declare every value that would change between runs.
- `steps`: the tool-call sequence (1–32 steps), each `{tool, method, parameters}`. In `parameters`, write `{{param_name}}` wherever a declared parameter's value belongs. Every `{{placeholder}}` must reference a declared parameter, and every declared parameter must be used at least once.

Example:

```json
"l3_skill": {
  "name": "summarise_repo_readme",
  "description": "Read a repo's README and return a short summary",
  "parameters": [{"name": "repo_path", "description": "absolute path to the repo"}],
  "steps": [
    {"tool": "shell-exec", "method": "shell.exec",
     "parameters": {"argv": ["cat", "{{repo_path}}/README.md"]}}
  ]
}
```

Crystallised skills are stored for later operator review; they are NOT
executed automatically. Emit at most one `l3_skill` per task.
```

- [ ] **Step 2: Update the JSON-schema example** in the same prompt — wherever the terminal-plan example lists optional fields like `"l1_insight": null`, add a sibling line `"l3_skill": null` so the model sees the field name in the canonical schema.

- [ ] **Step 3: Sanity-check the prompt still parses as the daemon expects** (the prompt is plain Markdown; no compile step). Re-read the edited section to confirm the JSON example is valid JSON.

- [ ] **Step 4: Commit**

```bash
git add prompts/agent_planner.md
git commit -m "docs(prompt): teach planner to emit l3_skill on terminal multi-step plans"
```

---

## Task 12: DB integration tests — `memory_l3_crystallise_e2e.rs`

**Files:**
- Create: `core/tests/memory_l3_crystallise_e2e.rs`

> **Harness:** copy the scaffolding (PgCluster bring-up via `kastellan_tests_common::bring_up_pg_cluster`, the scripted-`PlanFormulator`/scheduler driver, and the `pg_bin_dir_or_skip` skip-as-pass guard) from `core/tests/memory_l1_promote_e2e.rs` — specifically its agent-raised test (`agent_raised_*`). Reuse that file's helper that runs a task end-to-end through `run_to_terminal` + `drain_lane` with a scripted terminal plan. Substitute the `l3_skill` candidate + assertions below. Where the L1 test sets `plan.l1_insight = Some(...)`, set `plan.l3_skill = Some(...)` instead; where it asserts a layer-1 row + `l1.promoted` audit row, assert a layer-3 row + `l3.crystallised` row.

- [ ] **Step 1: Write the tests** (one fixture builder + the scenarios)

```rust
// Shared fixture builder (top of the test file):
fn valid_skill() -> kastellan_core::cassandra::types::L3SkillCandidate {
    use kastellan_core::cassandra::types::{L3Param, L3SkillCandidate, L3TemplateStep};
    L3SkillCandidate {
        name: "summarise_repo_readme".into(),
        description: "Read a repo README and summarise".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(), method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
        }],
    }
}
```

Scenarios (each `#[tokio::test]`, each guarded by the file's `pg_bin_dir_or_skip` early-return):

1. **`agent_raised_happy_inserts_l3_row_and_audit`** — drive a task whose terminal plan sets `l3_skill = Some(valid_skill())` AND whose trajectory executed ≥ 1 step (the scripted dispatcher returns one `Ok` step on a non-terminal plan, then a terminal plan). Assert:
   - exactly one row at `layer = 3`; its `body == "Read a repo README and summarise"`; its `metadata->>'trust' == "untrusted"`; `metadata->'template'->>'name' == "summarise_repo_readme"`.
   - exactly one audit row `actor='scheduler' action='l3.crystallised'` with `payload->>'action' == "inserted"`, `payload->>'source' == "agent_raised"`, `payload->>'skill_name' == "summarise_repo_readme"`.

2. **`agent_raised_dedup_skips_second`** — run the same task twice. Assert: still exactly one `layer = 3` row; the second task's `l3.crystallised` audit row has `payload->>'action' == "skipped_duplicate"`.

3. **`grounding_gate_drops_pure_text_task`** — drive a task whose FIRST plan is terminal (zero steps executed, `dispatch_count == 0`) but sets `l3_skill = Some(valid_skill())`. Assert: zero rows at `layer = 3`; zero `l3.crystallised` audit rows. (The `dispatch_count >= 1` gate suppressed it.)

4. **`invalid_skill_writes_nothing`** — terminal plan with ≥1 step executed, but `l3_skill` has an undeclared placeholder (`parameters` reference `{{missing}}`). Assert: zero `layer = 3` rows; zero `l3.crystallised` audit rows (validation failure is WARN-only).

5. **`remove_deletes_and_journals`** — insert one skill (scenario 1), then call `kastellan_core::cli_audit::l3_remove_and_audit(&pool, id)`. Assert: returns `(true, _)`; zero `layer = 3` rows remain; one row appears in `deleted_memories`; one `actor='cli' action='l3.removed'` audit row with `payload->>'deleted' == "true"`.

6. **`remove_wrong_layer_is_noop`** — insert an L1 row (via `memory::l1_promote::promote_l1` with a `NoOpEntityExtractor`), capture its id, then `l3_remove_and_audit(&pool, that_l1_id)`. Assert: returns `(false, _)`; the L1 row still exists; one `l3.removed` audit row with `payload->>'deleted' == "false"`.

7. **`list_returns_layer3_with_trust`** — insert two distinct skills; `list_l3(&pool)` returns 2 rows, each with `metadata->>'trust' == "untrusted"`.

> For the exact scripted-plan driver: mirror `memory_l1_promote_e2e.rs`'s agent-raised harness. The "≥1 step executed" requirement means the scripted formulator must return a non-terminal plan with one step (the dispatcher returns `Ok`), THEN a terminal plan carrying `l3_skill`. For the grounding-gate scenario, return a terminal plan on the first formulate call (no step ever dispatched).

- [ ] **Step 2: Run the tests with PG live**

Run:
```sh
KASTELLAN_PG_BIN_DIR='/Applications/Postgres 2.app/Contents/Versions/18/bin' \
  cargo test -p kastellan-core --test memory_l3_crystallise_e2e -- --nocapture 2>&1 | tail -30
```
Expected: all 7 PASS (no `[SKIP]` since PG bin dir is set).

- [ ] **Step 3: Commit**

```bash
git add core/tests/memory_l3_crystallise_e2e.rs
git commit -m "test(e2e): L3 crystallisation DB integration (happy/dedup/grounding/invalid/remove/list)"
```

---

## Task 13: CLI integration tests — `cli_memory_l3_e2e.rs`

**Files:**
- Create: `core/tests/cli_memory_l3_e2e.rs`

> **Harness:** copy the scaffolding from `core/tests/cli_memory_l1_e2e.rs` (it brings up PG, sets `KASTELLAN_DB_*` env for the spawned `kastellan-cli` binary via `kastellan_tests_common::workspace_target_binary("kastellan-cli")`, and asserts on stdout/exit). Substitute the `l3` subcommand + assertions.

- [ ] **Step 1: Write the tests** (each guarded by the skip helper)

1. **`cli_memory_l3_list_empty_then_populated`** — `memory l3 list` on an empty DB exits 0 with just the header. Then insert one skill via `kastellan_core::memory::l3_crystallise::crystallise_l3` directly against the test pool; `memory l3 list` now prints a row containing `untrusted` and the skill name; exit 0.

2. **`cli_memory_l3_remove_existing`** — insert a skill, capture id; `memory l3 remove <id>` prints `removed id=<id>`, exit 0; `list` is empty afterwards.

3. **`cli_memory_l3_remove_missing_id`** — `memory l3 remove 999999` prints `no row at layer 3 with id=999999 ...`, exit 0.

4. **`cli_memory_l3_remove_bad_arg`** — `memory l3 remove notanumber` → stderr `invalid id`, exit 2 (no PG connection attempted; arg-parse path).

- [ ] **Step 2: Run with PG live**

Run:
```sh
KASTELLAN_PG_BIN_DIR='/Applications/Postgres 2.app/Contents/Versions/18/bin' \
  cargo test -p kastellan-core --test cli_memory_l3_e2e -- --nocapture 2>&1 | tail -20
```
Expected: all PASS.

- [ ] **Step 3: Commit**

```bash
git add core/tests/cli_memory_l3_e2e.rs
git commit -m "test(e2e): kastellan-cli memory l3 list/remove"
```

---

## Task 14: `scheduler_inner_loop_e2e` payload pin + full verification

**Files:**
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (the mid-tier `agent/plan.formulate` payload assertion — add an `l3_skill` key check)

- [ ] **Step 1: Add the `l3_skill` assertion** to the existing mid-tier audit-payload gate test in `core/tests/scheduler_inner_loop_e2e.rs` (search the test that asserts on `l1_insight` / `recalled_memory_ids` in the `agent/plan.formulate` payload). Add:

```rust
    // l3_skill: present as explicit null when the plan didn't emit one.
    assert!(formulate_payload.as_object().unwrap().contains_key("l3_skill"));
    assert_eq!(formulate_payload["l3_skill"], serde_json::Value::Null);
```
If the test's scripted plan does set a skill, assert the compact `{name, step_count, param_count}` shape instead.

- [ ] **Step 2: Run the affected e2e**

Run:
```sh
KASTELLAN_PG_BIN_DIR='/Applications/Postgres 2.app/Contents/Versions/18/bin' \
  cargo test -p kastellan-core --test scheduler_inner_loop_e2e -- --nocapture 2>&1 | tail -15
```
Expected: PASS.

- [ ] **Step 3: Full workspace test (no PG — skip-as-pass posture)**

Run: `cargo test --workspace 2>&1 | grep -E "test result:|FAILED" | grep -v "0 passed; 0 failed" | tail -40`
Expected: zero `FAILED`; aggregate passed should be **1157 + ~27 new** ≈ 1184 (exact number depends on final test count). Cross-check no pre-existing test regressed.

- [ ] **Step 4: Full workspace test WITH PG live**

Run:
```sh
KASTELLAN_PG_BIN_DIR='/Applications/Postgres 2.app/Contents/Versions/18/bin' \
  cargo test --workspace 2>&1 | grep -E "test result:|FAILED" | grep -v "0 passed; 0 failed" | tail -50
```
Expected: zero `FAILED`; the new `memory_l3_crystallise_e2e` (7) + `cli_memory_l3_e2e` (4) run for real. (Pre-existing `embedding_recall_e2e` + `gliner_relex_e2e` PG races may flake identically to `main` — confirm they match the baseline, not new.)

- [ ] **Step 5: Clippy gate (workspace, `-D warnings`)**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -8`
Expected: exit 0, no warnings.

- [ ] **Step 6: Doc-link check (core crate)**

Run: `RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc -p kastellan-core --no-deps --document-private-items 2>&1 | grep -c "unresolved link"`
Expected: the SAME count as `main` (no NEW broken links from the new module's doc-comments). If a new one appears, fix the offending `[\`Type\`]` link (qualify the path) before committing.

- [ ] **Step 7: Commit**

```bash
git add core/tests/scheduler_inner_loop_e2e.rs
git commit -m "test(e2e): pin l3_skill key in scheduler_inner_loop_e2e plan.formulate payload"
```

---

## Final step: docs + PR

- [ ] **Update `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md`** per the end-of-session checklist (record the L3 writer slice, the new test count, the audit-row contract, and tick the ROADMAP "L3 skill crystallisation" item from spec → shipped-writer). Commit.
- [ ] **Open the PR** from `feat/l3-skill-crystallisation` once CI is green.

---

## Self-review (plan vs spec)

**Spec coverage:**
- New module `l3_crystallise.rs` — Tasks 2-4. ✓
- `L3SkillCandidate`/`L3Param`/`L3TemplateStep` + `Plan.l3_skill` + `completion_skill()` — Task 1. ✓
- `agent_planner.md` update — Task 11. ✓
- `InnerLoopResult.terminal_l3_skill` + Completed-arm capture + `dispatch_count >= 1` grounding gate — Task 6. ✓
- `build_plan_formulate_payload` compact `l3_skill` key — Task 7. ✓
- `drain_lane` hook + `write_l3_crystallised_row` — Task 8. ✓
- `ACTION_L3_CRYSTALLISED` / `ACTION_L3_REMOVED` + `build_l3_write_payload` — Task 5. ✓
- `cli_audit::l3_remove_and_audit` — Task 9. ✓
- `memory l3 {list, remove}` CLI — Task 10. ✓
- Validation rules (caps, closed-world placeholders, reserved tags) — Task 2. ✓
- Dedup via canonical-SHA — Task 3. ✓
- Provenance `L3Source::AgentRaised` only in `drain_lane` — Tasks 3/8. ✓
- `trust: "untrusted"` metadata — Task 3. ✓
- Audit-row contract (3 rows) — Tasks 5/8/9 + e2e Tasks 12/13. ✓
- Test budget ~25-32 — Tasks 1-13 (≈5 types + ~17 module + ~5 audit/cli pins + 7 DB e2e + 4 CLI e2e + 1 inner-loop e2e). ✓
- No new `db/` helper — confirmed (Task 4 reuses existing `insert_memory_at_layer`/`delete_memory_at_layer`/`load_layer`). ✓

**Type consistency:** `L3SkillCandidate`/`L3Param`/`L3TemplateStep` (Task 1) used identically in Tasks 2,3,7,12. `L3Source::AgentRaised { task_id }` (Task 2) used in 3,5,8. `L3WriteOutcome::{Inserted,SkippedDuplicate}` (Task 2) matched in 5,8. `crystallise_l3`/`validate_l3_skill`/`compute_template_sha256`/`canonical_json`/`build_l3_metadata`/`list_l3`/`remove_l3` names consistent across 2-4,8-10,12. `build_l3_write_payload(outcome, source, skill_name, body_sha256)` signature identical in 5 and 8. `ACTION_L3_CRYSTALLISED`/`ACTION_L3_REMOVED` consistent in 5,8,9. ✓

**Placeholder scan:** no TBD/TODO; the only "copy the harness" references (Tasks 12/13) point at exact precedent files with concrete fixtures + assertions — appropriate for e2e harness reuse rather than reproducing 200+ lines of bring-up scaffolding. ✓
