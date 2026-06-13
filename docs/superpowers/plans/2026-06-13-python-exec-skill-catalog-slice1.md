# python-exec skill catalog — slice 1 (crystallise + approval + operator CLI) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the agent crystallise a verbatim Python snippet it ran into a named, persisted layer-3 skill, and let the operator read + approve + pin it — mirroring the templated L3 arc, with the payload being opaque Python source instead of a tool-call template.

**Architecture:** A Python skill is a layer-3 `memories` row with `metadata.kind = "python"` and `metadata.python = {name, description, code}`, deduped by a canonical SHA-256 over the candidate. Crystallisation reuses the exact `drain_lane` grounding-gate hook the templated `l3_skill` uses; the approval gate is a *pure* `evaluate_python_approval` (structural caps + `secret://` scan — NO live-registry/tool-existence check, which is moot for opaque code); the operator CLI gains a `show` command (read the source before approving) and kind-aware `list`/`approve`/`pin`. **No invocation, no surfacing, no agent-autonomous run in this slice** — those are slice 2.

**Tech Stack:** Rust (workspace crate `kastellan-core` + `kastellan-db`), `sqlx`/PgPool, `serde_json`, `sha2`. Tests with `cargo test`; live-PG tests skip-as-pass without `KASTELLAN_PG_BIN_DIR`.

**Spec:** `docs/superpowers/specs/2026-06-13-python-exec-skill-catalog-design.md`

**Precedent files to read before starting** (this plan mirrors them):
- `core/src/memory/l3_crystallise.rs` (the templated writer)
- `core/src/memory/l3_approval.rs` (`SkillTrust`, `RejectReason`, `ApprovalDecision`)
- `core/src/scheduler/runner.rs:405-457` (`write_l3_crystallised_row`) + `:297-303` (drain_lane hook)
- `core/src/scheduler/inner_loop.rs:240-268` (`finish!` macro), `:493-512` (terminal capture), `:127-145` (`InnerLoopResult`)
- `core/src/bin/kastellan-cli/memory_l3/{trust.rs,shared.rs,list.rs}` + `memory_l3.rs` (dispatch)

**Conventions for every task:** TDD (write the failing test first), each file stays under 500 LOC, run `source "$HOME/.cargo/env"` once per shell before `cargo`. Commit after each task. **Stage only the files the task names — never `git add -A`** (an untracked `docs/essay-medium-draft.md` and `.claude/scheduled_tasks.lock` must stay out).

---

## File map

| File | Create / Modify | Responsibility |
| ---- | --------------- | -------------- |
| `core/src/cassandra/types.rs` | Modify | Add `PythonSkillCandidate` struct; add `Plan.python_skill` field + `Plan::completion_python_skill()`. |
| `core/src/memory/l3py_crystallise.rs` | Create | Pure validate + canonical SHA + secret scan; DB `crystallise_python_skill` + metadata builder + `list`/`fetch` helpers. |
| `core/src/memory/l3py_approval.rs` | Create | Pure `evaluate_python_approval`; reuse `SkillTrust`/`ApprovalDecision`; add `RejectReason::CodeSecretRef`. |
| `core/src/memory/l3_approval.rs` | Modify | Add the `CodeSecretRef { offset, found }` arm to the shared `RejectReason` enum + its `Display`. |
| `core/src/memory/mod.rs` | Modify | `pub mod l3py_crystallise; pub mod l3py_approval;` |
| `core/src/scheduler/inner_loop.rs` | Modify | `InnerLoopResult.terminal_python_skill`; extend `finish!`; capture under the grounding gate. |
| `core/src/scheduler/inner_loop/tests.rs` | Modify | Add `terminal_python_skill: None` to the one `InnerLoopResult` literal. |
| `core/src/scheduler/runner.rs` | Modify | `write_python_skill_crystallised_row` + drain_lane hook + `terminal_python_skill: None` in `failed_result`. |
| `core/src/bin/kastellan-cli/memory_l3/list.rs` | Modify | Kind-aware name lookup + a `KIND` column. |
| `core/src/bin/kastellan-cli/memory_l3/show.rs` | Create | New `memory l3 show <id>` — print full source (python) or pretty template (templated). |
| `core/src/bin/kastellan-cli/memory_l3/trust.rs` | Modify | Kind branch in `approve`/`pin`: python → `evaluate_python_approval` (no registry). |
| `core/src/bin/kastellan-cli/memory_l3.rs` | Modify | Register `show` in the dispatcher + usage strings. |

---

## Task 1: `PythonSkillCandidate` type + `Plan.python_skill` + completion gate

**Files:**
- Modify: `core/src/cassandra/types.rs` (add struct near `L3SkillCandidate:122`; add field to `Plan:177`; add method in `impl Plan` near `completion_skill:290`)
- Test: `core/src/cassandra/types.rs` tests module (`#[cfg(test)] mod tests;` is at `:336` → the tests live in `core/src/cassandra/types/tests.rs`)

- [ ] **Step 1: Write the failing tests** in `core/src/cassandra/types/tests.rs` (append at end of the file):

```rust
#[test]
fn python_skill_candidate_serde_roundtrips() {
    let c = PythonSkillCandidate {
        name: "sum_csv_column".into(),
        description: "Sum a numeric column of a CSV on stdin".into(),
        code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
    };
    let json = serde_json::to_string(&c).unwrap();
    let back: PythonSkillCandidate = serde_json::from_str(&json).unwrap();
    assert_eq!(c, back);
}

#[test]
fn completion_python_skill_some_only_on_terminal_with_candidate() {
    let cand = PythonSkillCandidate {
        name: "noop".into(),
        description: "d".into(),
        code: "pass\n".into(),
    };
    // Terminal plan carrying a python_skill → Some.
    let mut p = Plan {
        context: String::new(),
        decision: DECISION_TERMINAL.into(),
        rationale: String::new(),
        steps: vec![],
        result: Some(serde_json::json!({"kind":"text","body":"done"})),
        data_ceiling: DataClass::Public,
        refused: None,
        floor_request: None,
        l1_insight: None,
        l3_skill: None,
        python_skill: Some(cand.clone()),
        invoke_skill: None,
    };
    assert_eq!(p.completion_python_skill(), Some(&cand));
    // Non-terminal (has steps) → None even with a candidate.
    p.decision = "continue".into();
    p.steps = vec![];
    p.result = None;
    assert_eq!(p.completion_python_skill(), None);
}
```

Note: import `PythonSkillCandidate`, `DECISION_TERMINAL`, `DataClass` at the top of `tests.rs` if not already (`use super::*;` is typically present — verify the existing tests' import line and follow it).

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib cassandra::types 2>&1 | tail -20`
Expected: compile error — `PythonSkillCandidate` not found, `Plan` has no field `python_skill`, no method `completion_python_skill`.

- [ ] **Step 3: Add the struct** in `core/src/cassandra/types.rs` immediately after `L3SkillCandidate` (after line 127):

```rust
/// Agent-emitted candidate for a Python *skill* (L3, `kind = "python"`).
/// Unlike [`L3SkillCandidate`] (a parameterised tool-call template), this
/// is a **verbatim** Python snippet the agent ran this task. It carries no
/// parameters and is stored + later executed byte-for-byte unchanged — the
/// SHA-256 of the canonical `{name, description, code}` JSON is both the
/// dedup key and the approval binding.
///
/// Validation rules + caps live in [`crate::memory::l3py_crystallise`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PythonSkillCandidate {
    pub name: String,
    pub description: String,
    pub code: String,
}
```

- [ ] **Step 4: Add the `Plan` field.** In the `Plan` struct (after the `invoke_skill` field at line 241, i.e. as the last field), add:

```rust
    /// Agent-raised **Python** skill candidate (verbatim code). Honoured
    /// on the same gate as [`Plan::l3_skill`]: terminal + `Outcome::Completed`
    /// + `dispatch_count >= 1`. Captured into
    /// `InnerLoopResult.terminal_python_skill`; written to `MemoryLayer::Skill`
    /// (`kind = "python"`, `trust = "untrusted"`) by `runner::drain_lane`.
    /// Round-trips with `skip_serializing_if = Option::is_none` so non-emitting
    /// plans stay byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python_skill: Option<PythonSkillCandidate>,
```

- [ ] **Step 5: Add the completion gate method** inside `impl Plan`, right after `completion_skill` (after line 296):

```rust
    /// Returns `Some(candidate)` iff this plan would produce
    /// `Outcome::Completed` AND carries a `python_skill`. The inner loop
    /// ANDs in the `dispatch_count >= 1` grounding gate at the call site.
    /// Mirrors [`Plan::completion_skill`].
    pub fn completion_python_skill(&self) -> Option<&PythonSkillCandidate> {
        if self.is_terminal() {
            self.python_skill.as_ref()
        } else {
            None
        }
    }
```

- [ ] **Step 6: Find and fix every other `Plan { ... }` struct literal.** Adding a non-`Option`-defaulted-out field is fine for serde (it has `#[serde(default)]`), but struct literals in code/tests must name it.

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core 2>&1 | grep -A2 "missing field \`python_skill\`" | head -40`
For each reported `Plan { ... }` literal, add `python_skill: None,` (alongside the existing `l3_skill: None,`). Expect a handful across `cassandra/`, `scheduler/`, and test helpers.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib cassandra::types 2>&1 | tail -20`
Expected: PASS (including `python_skill_candidate_serde_roundtrips` + `completion_python_skill_some_only_on_terminal_with_candidate`).

- [ ] **Step 8: Commit**

```bash
git add core/src/cassandra/types.rs core/src/cassandra/types/tests.rs
# plus any files Step 6 touched to add `python_skill: None`:
git add -p core/src/cassandra core/src/scheduler   # review each hunk; stage only python_skill:None additions
git commit -m "feat(types): PythonSkillCandidate + Plan.python_skill crystallisation candidate"
```

---

## Task 2: `l3py_crystallise.rs` — pure validate + canonical SHA + secret scan

**Files:**
- Create: `core/src/memory/l3py_crystallise.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod l3py_crystallise;` after line 51)
- Test: inline `#[cfg(test)] mod tests` at the bottom of the new file

- [ ] **Step 1: Create `core/src/memory/l3py_crystallise.rs`** with the pure layer + constants (DB layer comes in Task 3). The validation differs from `l3_crystallise` in one key way: **code may contain newlines and tabs** (it is multi-line source); it rejects only NUL bytes. Name + description reuse the L3 caps/guards.

```rust
//! Writer for **Python** L3 skills (`metadata.kind = "python"`). Mirrors
//! [`crate::memory::l3_crystallise`] one payload over: where the templated
//! writer stores a parameterised tool-call template, this stores a *verbatim*
//! Python snippet the agent ran. One caller: `runner::drain_lane` on
//! `Outcome::Completed` with `dispatch_count >= 1` (the grounding gate).
//!
//! Crystallised Python skills land `trust:"untrusted"` and are inert until an
//! operator approves them. There is NO invocation path in this slice.
//!
//! See `docs/superpowers/specs/2026-06-13-python-exec-skill-catalog-design.md`.

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::cassandra::types::PythonSkillCandidate;
use crate::memory::l3_crystallise::L3Source; // reuse the provenance enum
use kastellan_db::memories::{insert_memory_at_layer, MemoryLayer};

/// Max bytes for the skill `name` (a stable identifier). Mirrors L3.
pub const PY_MAX_NAME_BYTES: usize = 64;
/// Max bytes for the skill `description` (becomes the memory `body`).
pub const PY_MAX_DESC_BYTES: usize = 512;
/// Max bytes for the verbatim Python `code`. Well under the worker's 256 KiB
/// `python.exec` code limit; a catalog skill is a small reusable snippet.
pub const PY_CODE_CAP: usize = 64 * 1024;

/// Error kinds the Python-skill writer can produce. Mirrors
/// [`crate::memory::l3_crystallise::L3Error`].
#[derive(Debug, thiserror::Error)]
pub enum PyError {
    #[error("python skill validation failed: {0}")]
    Validation(String),
    #[error("python skill db error: {0}")]
    Db(#[from] kastellan_db::DbError),
}

/// Outcome of a single `crystallise_python_skill` call. Mirrors
/// [`crate::memory::l3_crystallise::L3WriteOutcome`].
#[derive(Clone, Debug)]
pub enum PyWriteOutcome {
    Inserted { memory_id: i64 },
    SkippedDuplicate { memory_id: i64 },
}

impl PyWriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            PyWriteOutcome::Inserted { memory_id }
            | PyWriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}

/// `true` iff `s` is a strict snake_case identifier (`[a-z][a-z0-9_]*`).
/// Local copy (mirrors `l3_crystallise::is_snake_ident`).
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Validate a Python-skill candidate. Returns a normalised candidate
/// (trimmed name + description; **code is NOT trimmed** — leading/trailing
/// whitespace can be significant in Python) on success.
///
/// Key difference from `validate_l3_skill`: `code` is multi-line source, so
/// newlines and tabs are allowed; only a NUL byte is rejected (it cannot
/// occur in legitimate source and would truncate a C string downstream).
pub fn validate_python_skill(c: &PythonSkillCandidate) -> Result<PythonSkillCandidate, PyError> {
    // --- name ---
    let name = c.name.trim();
    if name.is_empty() {
        return Err(PyError::Validation("name is empty after trim".into()));
    }
    if name.len() > PY_MAX_NAME_BYTES {
        return Err(PyError::Validation(format!(
            "name exceeds {PY_MAX_NAME_BYTES} bytes ({})",
            name.len()
        )));
    }
    if !is_snake_ident(name) {
        return Err(PyError::Validation(format!(
            "name '{name}' is not snake_case ([a-z][a-z0-9_]*)"
        )));
    }

    // --- description (same guards as L3: no newline/control, capped) ---
    let description = c.description.trim();
    if description.is_empty() {
        return Err(PyError::Validation("description is empty after trim".into()));
    }
    if description.contains('\n') || description.contains('\r') {
        return Err(PyError::Validation("description contains newline".into()));
    }
    if description.bytes().any(|b| b < 0x20) {
        return Err(PyError::Validation("description contains control character".into()));
    }
    if description.len() > PY_MAX_DESC_BYTES {
        return Err(PyError::Validation(format!(
            "description exceeds {PY_MAX_DESC_BYTES} bytes ({})",
            description.len()
        )));
    }

    // --- code (verbatim; multi-line allowed; reject empty / NUL / over-cap) ---
    if c.code.is_empty() {
        return Err(PyError::Validation("code is empty".into()));
    }
    if c.code.len() > PY_CODE_CAP {
        return Err(PyError::Validation(format!(
            "code exceeds {PY_CODE_CAP} bytes ({})",
            c.code.len()
        )));
    }
    if c.code.bytes().any(|b| b == 0) {
        return Err(PyError::Validation("code contains a NUL byte".into()));
    }
    // Defensive: a skill must not bake in a secret reference (mirrors the
    // approval gate; rejecting at crystallise too means it never even lands).
    if c.code.contains(crate::secrets::REF_PREFIX) {
        return Err(PyError::Validation(format!(
            "code contains a '{}' secret reference (skills must not carry baked-in secrets)",
            crate::secrets::REF_PREFIX
        )));
    }

    Ok(PythonSkillCandidate {
        name: name.to_string(),
        description: description.to_string(),
        code: c.code.clone(),
    })
}

/// Deterministic JSON string for a candidate (top-level keys sorted; the
/// struct is flat so no recursive sort is needed). Load-bearing for dedup.
pub fn canonical_json(c: &PythonSkillCandidate) -> String {
    // serde_json::Map preserves insertion order; build it in sorted-key order.
    let mut map = serde_json::Map::new();
    map.insert("code".into(), serde_json::Value::String(c.code.clone()));
    map.insert("description".into(), serde_json::Value::String(c.description.clone()));
    map.insert("name".into(), serde_json::Value::String(c.name.clone()));
    serde_json::to_string(&serde_json::Value::Object(map)).expect("canonical serialise")
}

/// SHA-256 over the canonical candidate, lowercase 64-char hex.
pub fn compute_python_sha256(c: &PythonSkillCandidate) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(c).as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "sum_stdin".into(),
            description: "Sum integers from stdin".into(),
            code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
        }
    }

    #[test]
    fn validate_accepts_clean_multiline_code() {
        let v = validate_python_skill(&valid()).expect("clean skill validates");
        // code is preserved verbatim (newlines kept, NOT trimmed).
        assert!(v.code.contains('\n'));
        assert!(v.code.ends_with('\n'));
    }

    #[test]
    fn validate_rejects_non_snake_name() {
        let mut c = valid();
        c.name = "SumStdin".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_empty_code() {
        let mut c = valid();
        c.code = String::new();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_oversized_code() {
        let mut c = valid();
        c.code = "x".repeat(PY_CODE_CAP + 1);
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_nul_in_code() {
        let mut c = valid();
        c.code = "print(1)\u{0}".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_secret_ref_in_code() {
        let mut c = valid();
        c.code = "token = 'secret://abc12345'\nprint(token)\n".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_newline_in_description() {
        let mut c = valid();
        c.description = "line one\nline two".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn sha_is_deterministic_and_field_sensitive() {
        let a = compute_python_sha256(&valid());
        assert_eq!(a, compute_python_sha256(&valid()), "deterministic");
        assert_eq!(a.len(), 64);
        let mut c = valid();
        c.code = "print(2)\n".into();
        assert_ne!(a, compute_python_sha256(&c), "sensitive to code");
    }
}
```

- [ ] **Step 2: Register the module.** In `core/src/memory/mod.rs`, after `pub mod l3_crystallise;` (line 51), add:

```rust
pub mod l3py_crystallise;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib memory::l3py_crystallise 2>&1 | tail -25`
Expected: PASS (8 tests).

- [ ] **Step 4: Commit**

```bash
git add core/src/memory/l3py_crystallise.rs core/src/memory/mod.rs
git commit -m "feat(memory): l3py_crystallise pure layer — validate + canonical SHA for Python skills"
```

---

## Task 3: `l3py_crystallise.rs` — metadata builder + `crystallise_python_skill` (DB)

**Files:**
- Modify: `core/src/memory/l3py_crystallise.rs` (add the DB layer; tests are PG-gated)
- Test: a new PG-gated integration test file `core/tests/python_skill_crystallise_e2e.rs`

- [ ] **Step 1: Add the metadata builder + writer** to `core/src/memory/l3py_crystallise.rs`, after `compute_python_sha256` (before the `#[cfg(test)]` module). This mirrors `l3_crystallise::build_l3_metadata` + `crystallise_l3` but writes `kind: "python"` and a `python` object instead of `template`.

```rust
/// Build the `metadata` JSONB for a new Python-skill row. Schema:
/// `{source, task_id, trust, kind, body_sha256, created_at, python}`.
/// `python` is the full normalised candidate. `kind: "python"` is the
/// discriminator the CLI + (slice-2) surfacing branch on; absent ⇒ templated.
pub(crate) fn build_python_skill_metadata(
    source: &L3Source,
    candidate: &PythonSkillCandidate,
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
    obj.insert("kind".into(), serde_json::Value::String("python".into()));
    obj.insert("body_sha256".into(), serde_json::Value::String(body_sha256.into()));
    obj.insert("created_at".into(), serde_json::Value::String(created_at_rfc3339.into()));
    obj.insert(
        "python".into(),
        serde_json::to_value(candidate).expect("candidate serialises"),
    );
    serde_json::Value::Object(obj)
}

/// Crystallise a single Python skill. Validates, computes the canonical
/// SHA-256, EXISTS-checks against `layer = 3` rows by
/// `metadata->>'body_sha256'`, inserts on miss with `body = description`,
/// `kind: "python"`, `trust: "untrusted"`. Idempotent on the code SHA.
///
/// The `body_sha256` EXISTS-check is shared with the templated writer (both
/// hash into the same key), so a Python skill and a templated skill can never
/// collide unless their canonical digests coincide — cryptographically absent.
pub async fn crystallise_python_skill(
    pool: &PgPool,
    candidate: &PythonSkillCandidate,
    source: L3Source,
) -> Result<PyWriteOutcome, PyError> {
    let normalised = validate_python_skill(candidate)?;
    let body_sha256 = compute_python_sha256(&normalised);

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
        PyError::Db(kastellan_db::DbError::Query(format!(
            "crystallise_python_skill EXISTS-check body_sha256={body_sha256}: {e}"
        )))
    })?;

    if let Some(existing_id) = existing {
        return Ok(PyWriteOutcome::SkippedDuplicate { memory_id: existing_id });
    }

    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("rfc3339 format");
    let metadata = build_python_skill_metadata(&source, &normalised, &body_sha256, &created_at);

    let new_id = insert_memory_at_layer(
        pool,
        &normalised.description, // body = the human description
        &metadata,
        None, // no embedding for L3 v1
        MemoryLayer::Skill,
    )
    .await?;

    Ok(PyWriteOutcome::Inserted { memory_id: new_id })
}
```

- [ ] **Step 2: Write the PG-gated test** `core/tests/python_skill_crystallise_e2e.rs`. Mirror an existing PG-gated test's skip-as-pass harness — read `core/tests/` for the `tests_common`/`KASTELLAN_PG_BIN_DIR` skip pattern an existing memory test uses (e.g. how `memory_recall_e2e.rs` or a `*_e2e.rs` brings up PG) and copy that prologue exactly. The assertions:

```rust
// Prologue: bring up PG via tests-common, run migrations, get a PgPool `pool`.
// (Copy the exact skip-as-pass bring-up from an existing core/tests/*_e2e.rs.)

use kastellan_core::cassandra::types::PythonSkillCandidate;
use kastellan_core::memory::l3_crystallise::L3Source;
use kastellan_core::memory::l3py_crystallise::{crystallise_python_skill, PyWriteOutcome};

fn cand() -> PythonSkillCandidate {
    PythonSkillCandidate {
        name: "sum_stdin".into(),
        description: "Sum integers from stdin".into(),
        code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
    }
}

// inside the #[tokio::test] body, after `pool` is ready:
let out = crystallise_python_skill(&pool, &cand(), L3Source::AgentRaised { task_id: 1 })
    .await
    .expect("crystallise ok");
let id = match out {
    PyWriteOutcome::Inserted { memory_id } => memory_id,
    other => panic!("expected Inserted, got {other:?}"),
};

// Re-crystallising identical code dedups to the same row.
let again = crystallise_python_skill(&pool, &cand(), L3Source::AgentRaised { task_id: 2 })
    .await
    .expect("re-crystallise ok");
assert!(matches!(again, PyWriteOutcome::SkippedDuplicate { memory_id } if memory_id == id));

// The stored row is kind=python, trust=untrusted, with the verbatim code.
let row: (serde_json::Value, String) =
    sqlx::query_as("SELECT metadata, body FROM memories WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
assert_eq!(row.0.get("kind").and_then(|v| v.as_str()), Some("python"));
assert_eq!(row.0.get("trust").and_then(|v| v.as_str()), Some("untrusted"));
assert_eq!(
    row.0.get("python").and_then(|p| p.get("code")).and_then(|v| v.as_str()),
    Some(cand().code.as_str())
);
assert_eq!(row.1, cand().description);
```

- [ ] **Step 3: Run the test (skip-as-pass without PG, real with PG).**

Run (skip path): `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test python_skill_crystallise_e2e 2>&1 | tail -15`
Expected: compiles; either PASS-with-`[SKIP]` (no `KASTELLAN_PG_BIN_DIR`) or PASS with live PG (see the memory note for the Postgres.app bin-dir override to run it for real).

- [ ] **Step 4: Commit**

```bash
git add core/src/memory/l3py_crystallise.rs core/tests/python_skill_crystallise_e2e.rs
git commit -m "feat(memory): crystallise_python_skill DB writer + dedup (PG-gated e2e)"
```

---

## Task 4: crystallise wiring — inner_loop capture + runner writer + drain_lane hook

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (`InnerLoopResult` field, `finish!` macro, terminal capture)
- Modify: `core/src/scheduler/inner_loop/tests.rs` (the one `InnerLoopResult` literal at `:233`)
- Modify: `core/src/scheduler/runner.rs` (`failed_result` literal at `:550`, drain_lane hook at `:297-303`, new writer fn)
- Test: extend `core/src/scheduler/inner_loop/tests.rs`

- [ ] **Step 1: Write the failing inner-loop test** in `core/src/scheduler/inner_loop/tests.rs`. Find an existing test that drives a terminal plan carrying an `l3_skill` and asserts `terminal_l3_skill` is captured (search for `terminal_l3_skill`); clone it, set `python_skill: Some(...)` instead, and assert `result.terminal_python_skill == Some(...)`. Skeleton (adapt the harness to the existing test's fixtures):

```rust
#[tokio::test]
async fn terminal_python_skill_captured_under_grounding_gate() {
    // Reuse the same fake formulator/dispatcher harness the existing
    // `terminal_l3_skill_*` test uses: a plan that dispatches >= 1 step then
    // a terminal plan carrying `python_skill`.
    let cand = crate::cassandra::types::PythonSkillCandidate {
        name: "noop".into(), description: "d".into(), code: "pass\n".into(),
    };
    // ... build the queued plans so dispatch_count >= 1 then terminal w/ python_skill ...
    // let result = run_to_terminal(...).await.unwrap();
    assert_eq!(result.terminal_python_skill.as_ref(), Some(&cand));
}
```

- [ ] **Step 2: Run it to verify it fails to compile**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib scheduler::inner_loop 2>&1 | tail -20`
Expected: compile error — `InnerLoopResult` has no field `terminal_python_skill`.

- [ ] **Step 3: Add the `InnerLoopResult` field.** In `core/src/scheduler/inner_loop.rs` after `terminal_l3_skill` (line 144):

```rust
    /// `python_skill` from the terminal plan, captured only when the inner
    /// loop reaches `Outcome::Completed` AND `dispatch_count >= 1` (the same
    /// grounding gate as `terminal_l3_skill`). The lane runner writes one
    /// `action='l3.crystallised'` (`kind: "python"`) audit row if `Some`.
    pub terminal_python_skill: Option<crate::cassandra::types::PythonSkillCandidate>,
```

- [ ] **Step 4: Extend the `finish!` macro** (lines 254-267) to take a 4th positional argument, keeping the 3-arg and 1-arg forms working:

```rust
    macro_rules! finish {
        ($outcome:expr, $insight:expr, $skill:expr, $pyskill:expr) => {
            Ok(InnerLoopResult {
                outcome: $outcome,
                plan_count: ctx.plan_count,
                dispatch_count,
                terminal_l1_insight: $insight,
                terminal_l3_skill: $skill,
                terminal_python_skill: $pyskill,
            })
        };
        // 3-arg form (existing call sites): python skill None.
        ($outcome:expr, $insight:expr, $skill:expr) => {
            finish!($outcome, $insight, $skill, None)
        };
        // Convenience form for all non-Completed arms: all None.
        ($outcome:expr) => {
            finish!($outcome, None, None, None)
        };
    }
```

- [ ] **Step 5: Capture the Python skill at the terminal check.** In the terminal block (lines 494-511), after `captured_l3_skill` is computed, add:

```rust
            let captured_python_skill: Option<crate::cassandra::types::PythonSkillCandidate> =
                if dispatch_count >= 1 && !invoke_used {
                    plan.completion_python_skill().cloned()
                } else {
                    None
                };
            return finish!(
                Outcome::Completed(result),
                captured_l1_insight,
                captured_l3_skill,
                captured_python_skill
            );
```

(Replace the existing 3-arg `finish!(Outcome::Completed(result), captured_l1_insight, captured_l3_skill)` call with this 4-arg form.)

- [ ] **Step 6: Fix the other `InnerLoopResult` literals.**
  - `core/src/scheduler/runner.rs:550` (`failed_result`): add `terminal_python_skill: None,` next to `terminal_l3_skill: None,`.
  - `core/src/scheduler/inner_loop/tests.rs:233`: add `terminal_python_skill: None,`.

- [ ] **Step 7: Add the runner writer + drain_lane hook.** In `core/src/scheduler/runner.rs`, after the L3 hook (line 303) inside `drain_lane`:

```rust
        // Agent-raised Python-skill crystallisation. Same best-effort posture
        // as the L1/L3 hooks; Some only on Outcome::Completed + dispatch_count>=1.
        if let Some(skill) = result.terminal_python_skill.as_ref() {
            write_python_skill_crystallised_row(pool, claimed.id, skill).await;
        }
```

Then add the writer function after `write_l3_crystallised_row` (after line 457). It reuses `build_l3_write_payload` (the crystallise audit shape) and adds a `kind: "python"` field so the audit tail can distinguish it:

```rust
/// Crystallise the agent-raised Python skill + emit one `actor='scheduler'
/// action='l3.crystallised'` audit row carrying `kind: "python"`. Best-effort
/// (validation/DB errors logged at WARN and swallowed), mirroring
/// [`write_l3_crystallised_row`].
async fn write_python_skill_crystallised_row(
    pool: &PgPool,
    task_id: i64,
    skill: &crate::cassandra::types::PythonSkillCandidate,
) {
    use crate::memory::l3_crystallise::L3Source;
    use crate::memory::l3py_crystallise::{
        compute_python_sha256, crystallise_python_skill, validate_python_skill, PyError,
    };

    let source = L3Source::AgentRaised { task_id };
    let outcome = match crystallise_python_skill(pool, skill, source.clone()).await {
        Ok(o) => o,
        Err(PyError::Validation(msg)) => {
            tracing::warn!(task_id, error = %msg,
                "agent-raised python skill rejected on validation (skipping audit row)");
            return;
        }
        Err(PyError::Db(e)) => {
            tracing::warn!(task_id, error = %e,
                "agent-raised python skill DB error (skipping audit row)");
            return;
        }
    };

    let normalised = match validate_python_skill(skill) {
        Ok(n) => n,
        Err(_) => return, // unreachable: crystallise validated above
    };
    let body_sha256 = compute_python_sha256(&normalised);

    // Reuse the L3 crystallise payload shape; the PyWriteOutcome maps 1:1 to
    // the L3WriteOutcome arms the builder expects.
    let l3_outcome = match outcome {
        crate::memory::l3py_crystallise::PyWriteOutcome::Inserted { memory_id } => {
            crate::memory::l3_crystallise::L3WriteOutcome::Inserted { memory_id }
        }
        crate::memory::l3py_crystallise::PyWriteOutcome::SkippedDuplicate { memory_id } => {
            crate::memory::l3_crystallise::L3WriteOutcome::SkippedDuplicate { memory_id }
        }
    };
    let mut payload =
        build_l3_write_payload(&l3_outcome, &source, &normalised.name, &body_sha256);
    if let serde_json::Value::Object(ref mut m) = payload {
        m.insert("kind".into(), serde_json::Value::String("python".into()));
    }

    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_CRYSTALLISED, payload,
    ).await {
        tracing::warn!(task_id, error = %e,
            "audit insert for scheduler python l3.crystallised row failed (best-effort)");
    }
}
```

Confirm `build_l3_write_payload`, `ACTION_L3_CRYSTALLISED`, and `SCHEDULER_AUDIT_ACTOR` are already imported at the top of `runner.rs` (the L3 writer uses them — line 25 shows `ACTION_L3_CRYSTALLISED, ... SCHEDULER_AUDIT_ACTOR`; `build_l3_write_payload` is imported alongside the other `build_l3_*` — verify and add to the `use` if missing).

- [ ] **Step 8: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib scheduler 2>&1 | tail -25`
Expected: PASS, including `terminal_python_skill_captured_under_grounding_gate`.

- [ ] **Step 9: Commit**

```bash
git add core/src/scheduler/inner_loop.rs core/src/scheduler/inner_loop/tests.rs core/src/scheduler/runner.rs
git commit -m "feat(scheduler): crystallise agent-raised Python skills on grounded completion"
```

---

## Task 5: `l3py_approval.rs` — pure approval gate

**Files:**
- Modify: `core/src/memory/l3_approval.rs` (add the `CodeSecretRef` arm to `RejectReason` + `Display`)
- Create: `core/src/memory/l3py_approval.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod l3py_approval;`)
- Test: inline `#[cfg(test)] mod tests` in the new file

- [ ] **Step 1: Add the `CodeSecretRef` variant** to `RejectReason` in `core/src/memory/l3_approval.rs` (after `SecretRefPresent`, line 105):

```rust
    /// A Python skill's source embeds a `secret://` reference at `offset`
    /// bytes. The opaque-code analogue of [`RejectReason::SecretRefPresent`]
    /// (which keys on a step index a Python skill has none of).
    CodeSecretRef { offset: usize, found: String },
```

And its `Display` arm (in the `match self` block around line 116):

```rust
            RejectReason::CodeSecretRef { offset, found } => write!(
                f,
                "code embeds a secret reference '{found}' at byte offset {offset} \
                 (skills must not carry baked-in secrets)"
            ),
```

- [ ] **Step 2: Create `core/src/memory/l3py_approval.rs`:**

```rust
//! Operator approval gate for crystallised **Python** L3 skills. The
//! opaque-code analogue of [`crate::memory::l3_approval::evaluate_approval`]:
//! it re-runs structural validation + a `secret://` scan, but has **no
//! live-registry / tool-existence check** — a Python skill dispatches no
//! tools (its entire capability ceiling is the python-exec jail), so there is
//! nothing to check against the registry. The human reading the source via
//! `memory l3 show` is the real gate; this is the machine-checkable floor.
//!
//! See `docs/superpowers/specs/2026-06-13-python-exec-skill-catalog-design.md`.

use crate::cassandra::types::PythonSkillCandidate;
use crate::memory::l3_approval::{ApprovalDecision, RejectReason};
use crate::memory::l3py_crystallise::validate_python_skill;

/// Decide whether a stored Python skill may be promoted to `UserApproved`.
/// **PURE** — no I/O, no registry dependency. Collects ALL reasons:
/// 1. structural re-validation (short-circuits — a malformed skill yields
///    exactly one `StructuralInvalid`);
/// 2. every `secret://` occurrence in the source (one `CodeSecretRef` each).
///
/// Note: `validate_python_skill` already rejects `secret://` in code, so a
/// stored row that passed crystallisation will not trip (2); the re-scan is
/// defense-in-depth against a hand-edited SQL row and keeps the gate honest.
pub fn evaluate_python_approval(candidate: &PythonSkillCandidate) -> ApprovalDecision {
    let candidate = match validate_python_skill(candidate) {
        Ok(norm) => norm,
        Err(e) => {
            return ApprovalDecision::Reject {
                reasons: vec![RejectReason::StructuralInvalid(e.to_string())],
            }
        }
    };

    let mut reasons = Vec::new();
    let prefix = crate::secrets::REF_PREFIX;
    let mut search_from = 0;
    while let Some(rel) = candidate.code[search_from..].find(prefix) {
        let offset = search_from + rel;
        // Capture the ref token up to the next whitespace/quote for the message.
        let tail = &candidate.code[offset..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        reasons.push(RejectReason::CodeSecretRef {
            offset,
            found: tail[..end].to_string(),
        });
        search_from = offset + prefix.len();
    }

    if reasons.is_empty() {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Reject { reasons }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "sum_stdin".into(),
            description: "Sum integers from stdin".into(),
            code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
        }
    }

    #[test]
    fn approves_clean_skill() {
        assert_eq!(evaluate_python_approval(&clean()), ApprovalDecision::Approve);
    }

    #[test]
    fn rejects_structurally_invalid_skill() {
        let mut c = clean();
        c.name = "Bad Name".into();
        match evaluate_python_approval(&c) {
            ApprovalDecision::Reject { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(matches!(reasons[0], RejectReason::StructuralInvalid(_)));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn rejects_skill_with_secret_ref_in_code() {
        // Construct the candidate directly (bypassing validate, which also
        // rejects this) to prove the gate's own re-scan catches a hand-edited
        // row. validate runs FIRST in evaluate_python_approval and will emit
        // StructuralInvalid here — assert the rejection mentions the secret.
        let mut c = clean();
        c.code = "tok = 'secret://abc12345'\n".into();
        match evaluate_python_approval(&c) {
            ApprovalDecision::Reject { reasons } => {
                assert!(reasons.iter().any(|r| r.to_string().contains("secret")));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }
}
```

Note on the third test: `validate_python_skill` rejects `secret://` in code, so `evaluate_python_approval` short-circuits at the structural check and the `CodeSecretRef` loop is the belt-and-suspenders path for rows that bypassed validation (direct SQL). The assertion is on the rejection mentioning "secret" either way, which holds.

- [ ] **Step 3: Register the module** in `core/src/memory/mod.rs`, after `pub mod l3_approval;`:

```rust
pub mod l3py_approval;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib "memory::l3py_approval" 2>&1 | tail -20`
Expected: PASS (3 tests). Also re-run the L3 approval tests to confirm the enum change didn't break them: `cargo test -p kastellan-core --lib "memory::l3_approval" 2>&1 | tail -10` → PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/memory/l3_approval.rs core/src/memory/l3py_approval.rs core/src/memory/mod.rs
git commit -m "feat(memory): evaluate_python_approval pure gate + RejectReason::CodeSecretRef"
```

---

## Task 6: CLI — kind-aware `list` + new `show`

**Files:**
- Modify: `core/src/bin/kastellan-cli/memory_l3/list.rs`
- Create: `core/src/bin/kastellan-cli/memory_l3/show.rs`
- Modify: `core/src/bin/kastellan-cli/memory_l3.rs` (register `show`)

There is no automated test harness for the CLI binary's stdout; verify these by reading the diff + a manual smoke run against a live DB (operator step). Keep changes minimal and obviously correct.

- [ ] **Step 1: Make `list` kind-aware.** In `core/src/bin/kastellan-cli/memory_l3/list.rs`, replace the header + per-row block (lines 31-41) with a version that reads the name from `metadata.python.name` OR `metadata.template.name`, and prints a `KIND` column:

```rust
    println!(
        "{:<8}  {:<24}  {:<10}  {:<10}  NAME / DESCRIPTION",
        "ID", "CREATED_AT", "TRUST", "KIND"
    );
    for r in rows {
        let trust = kastellan_core::memory::l3_approval::SkillTrust::from_metadata_str(
            r.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
        )
        .as_str();
        // `kind` absent ⇒ templated (back-compat); "python" for code skills.
        let kind = r.metadata.get("kind").and_then(|v| v.as_str()).unwrap_or("templated");
        let name = r
            .metadata
            .get("python").and_then(|p| p.get("name")).and_then(|v| v.as_str())
            .or_else(|| r.metadata.get("template").and_then(|t| t.get("name")).and_then(|v| v.as_str()))
            .unwrap_or("?");
        println!("{:<8}  {:<24}  {:<10}  {:<10}  {} — {}", r.id, r.created_at, trust, kind, name, r.body);
    }
```

- [ ] **Step 2: Create `core/src/bin/kastellan-cli/memory_l3/show.rs`** — print the full source (python) or pretty template (templated). Reuse the `load_skill_row` prologue from `shared.rs`:

```rust
//! `memory l3 show <id>` — print the full payload of a crystallised skill so
//! the operator can READ it before approving. For a Python skill that is the
//! verbatim source (the human read IS the approval gate); for a templated
//! skill it is the pretty-printed step template.

use std::process::ExitCode;

use super::shared::load_skill_row;

pub(super) async fn memory_l3_show(args: &[String]) -> ExitCode {
    let (_, row) = match load_skill_row(args, "show").await {
        Ok(x) => x,
        Err(code) => return code,
    };
    let kind = row.metadata.get("kind").and_then(|v| v.as_str()).unwrap_or("templated");
    let trust = kastellan_core::memory::l3_approval::SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    )
    .as_str();
    println!("# skill #{} (kind={kind}, trust={trust})", row.id);
    println!("# description: {}", row.body);
    match kind {
        "python" => match row.metadata.get("python").and_then(|p| p.get("code")).and_then(|v| v.as_str()) {
            Some(code) => {
                println!("--- code ---");
                print!("{code}");
                if !code.ends_with('\n') {
                    println!();
                }
            }
            None => {
                eprintln!("memory l3 show: id={} has kind=python but no python.code", row.id);
                return ExitCode::from(1);
            }
        },
        _ => match row.metadata.get("template") {
            Some(t) => {
                println!("--- template ---");
                println!("{}", serde_json::to_string_pretty(t).unwrap_or_else(|_| t.to_string()));
            }
            None => {
                eprintln!("memory l3 show: id={} has no template", row.id);
                return ExitCode::from(1);
            }
        },
    }
    ExitCode::from(0)
}
```

- [ ] **Step 3: Register `show`** in `core/src/bin/kastellan-cli/memory_l3.rs`:
  - Add `mod show;` (after `mod shared;`, line 26).
  - Add `use show::memory_l3_show;` (after the other `use ...::memory_l3_*;`).
  - Add the dispatch arm (after the `"list"` arm at line 41): `"show"    => with_runtime("memory l3", memory_l3_show(&args[1..])),`
  - Update the two usage strings (lines 37 + 48) to include `show`: `<list|show|approve|pin|revoke|remove|run>`.

- [ ] **Step 4: Build to verify it compiles**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bin kastellan-cli 2>&1 | tail -15`
Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add core/src/bin/kastellan-cli/memory_l3/list.rs core/src/bin/kastellan-cli/memory_l3/show.rs core/src/bin/kastellan-cli/memory_l3.rs
git commit -m "feat(cli): kind-aware 'memory l3 list' + new 'memory l3 show' (read source before approving)"
```

---

## Task 7: CLI — kind-aware `approve` / `pin`

**Files:**
- Modify: `core/src/bin/kastellan-cli/memory_l3/trust.rs`

The templated `approve`/`pin` parse `metadata.template` and re-gate against the registry snapshot (`decide_against_registry`). A Python skill has no template and no tools, so it must instead parse `metadata.python` and gate with `evaluate_python_approval` (no registry). Branch on `metadata.kind` at the top of each handler.

- [ ] **Step 1: Add a kind helper + the python approve branch** in `core/src/bin/kastellan-cli/memory_l3/trust.rs`. At the top of `memory_l3_approve` (after `load_skill_row` returns `row`, before the template parse at line 32), insert a kind check that dispatches python skills to a new helper:

```rust
    // Python skills gate WITHOUT a registry snapshot (no tools to verify);
    // templated skills keep the registry re-gate below.
    if row.metadata.get("kind").and_then(|v| v.as_str()) == Some("python") {
        return approve_python_skill(&pool, &row).await;
    }
```

Then add the helper (new fn in the same file). It mirrors the templated approve body but uses `evaluate_python_approval` and an empty `tools` list for the audit:

```rust
use kastellan_core::cassandra::types::PythonSkillCandidate;
use kastellan_core::memory::l3py_approval::evaluate_python_approval;

/// Approve a Python skill: parse `metadata.python`, run the pure
/// `evaluate_python_approval` gate (no registry), flip trust on Approve.
async fn approve_python_skill(pool: &sqlx::PgPool, row: &kastellan_db::memories::Memory) -> ExitCode {
    let id = row.id;
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());
    let cand: PythonSkillCandidate = match row
        .metadata.get("python").cloned().and_then(|p| serde_json::from_value(p).ok())
    {
        Some(c) => c,
        None => {
            let reasons = vec!["stored row has kind=python but no parseable 'python'".to_string()];
            let _ = l3_approve_rejected_audit(pool, id, None, body_sha256, &reasons).await;
            eprintln!("memory l3 approve: id={id} has no parseable python payload; not approved");
            return ExitCode::from(1);
        }
    };
    let skill_name = cand.name.clone();
    match evaluate_python_approval(&cand) {
        ApprovalDecision::Approve => {
            let sha = body_sha256.unwrap_or("");
            // Python skills reference no tools — empty tools list in the audit.
            let tools: Vec<String> = Vec::new();
            if let Err(e) = l3_approve_and_audit(pool, id, &skill_name, sha, &tools).await {
                eprintln!("memory l3 approve: {e}");
                return ExitCode::from(1);
            }
            println!("approved python skill '{skill_name}' (#{id}) → trust=user_approved");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_approve_rejected_audit(pool, id, Some(&skill_name), body_sha256, &rendered).await;
            eprintln!("approval REJECTED for python skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}
```

- [ ] **Step 2: Add the python pin branch.** At the top of `memory_l3_pin` (after `load_skill_row`, before the ladder guard at line 98), insert:

```rust
    if row.metadata.get("kind").and_then(|v| v.as_str()) == Some("python") {
        return pin_python_skill(&pool, &row).await;
    }
```

And the helper (mirrors the templated pin: ladder-guard `user_approved` first, then re-gate with `evaluate_python_approval`):

```rust
async fn pin_python_skill(pool: &sqlx::PgPool, row: &kastellan_db::memories::Memory) -> ExitCode {
    let id = row.id;
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());
    let current = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    if current != SkillTrust::UserApproved {
        let reasons = vec![format!(
            "skill must be user_approved before pinning (current: {})",
            current.as_str()
        )];
        let _ = l3_pin_rejected_audit(pool, id, None, &reasons).await;
        eprintln!("memory l3 pin: id={id} is '{}', not user_approved; approve it first", current.as_str());
        return ExitCode::from(1);
    }
    let cand: PythonSkillCandidate = match row
        .metadata.get("python").cloned().and_then(|p| serde_json::from_value(p).ok())
    {
        Some(c) => c,
        None => {
            let reasons = vec!["stored row has kind=python but no parseable 'python'".to_string()];
            let _ = l3_pin_rejected_audit(pool, id, None, &reasons).await;
            eprintln!("memory l3 pin: id={id} has no parseable python payload; not pinned");
            return ExitCode::from(1);
        }
    };
    let skill_name = cand.name.clone();
    match evaluate_python_approval(&cand) {
        ApprovalDecision::Approve => {
            let sha = body_sha256.unwrap_or("");
            if let Err(e) = l3_pin_and_audit(pool, id, &skill_name, sha).await {
                eprintln!("memory l3 pin: {e}");
                return ExitCode::from(1);
            }
            println!("pinned python skill '{skill_name}' (#{id}) → trust=pinned (agent-autonomously invocable)");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_pin_rejected_audit(pool, id, Some(&skill_name), &rendered).await;
            eprintln!("pin REJECTED for python skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}
```

(`revoke` and `remove` operate only on the trust marker / row deletion and are already kind-agnostic — leave them unchanged.)

- [ ] **Step 3: Build to verify it compiles**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bin kastellan-cli 2>&1 | tail -15`
Expected: clean build. (Confirm the new `use` lines don't duplicate existing imports — `SkillTrust`, `ApprovalDecision`, and the `l3_*_audit` helpers are already imported at the top of `trust.rs`; add only `PythonSkillCandidate` + `evaluate_python_approval`.)

- [ ] **Step 4: Commit**

```bash
git add core/src/bin/kastellan-cli/memory_l3/trust.rs
git commit -m "feat(cli): kind-aware approve/pin — Python skills gate via evaluate_python_approval (no registry)"
```

---

## Task 8: Workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Full workspace build + test (skip-as-pass posture on the Mac)**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -25`
Expected: all green (the new e2e skip-as-passes without `KASTELLAN_PG_BIN_DIR`). Note the pass/fail/ignored counts; they should be the prior baseline plus the new unit tests.

- [ ] **Step 2: Clippy with the project gate**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -25`
Expected: clean (no warnings). Fix any `clippy::*` lints inline (the new code should already follow the surrounding idioms).

- [ ] **Step 3: Live-PG run of the crystallise e2e (optional, real verification).** Use the Postgres.app v18 bin-dir override from the memory note:

Run: `source "$HOME/.cargo/env" && KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" cargo test -p kastellan-core --test python_skill_crystallise_e2e -- --nocapture 2>&1 | tail -25`
Expected: the dedup + kind=python + verbatim-code assertions PASS against live PG (no `[SKIP]`).

- [ ] **Step 4: Commit any clippy fixes** (if Step 2 required changes)

```bash
git add -p   # stage only the clippy-fix hunks
git commit -m "chore: clippy -D warnings clean for the Python skill catalog slice 1"
```

---

## Out of scope (slice 2 — next plan)

Per the slicing decision (mirror the L3 PR sequence): **invocation** (`l3py_invoke.rs` pure gate with SHA-drift refuse + operator path dispatching one `python.exec` step + the daemon `l3_run` Python branch, fail-closed when python-exec is not enabled), **surfacing** (`l3_surface.rs` kind-aware projection — name/description/invocable, code never shown), **agent-autonomous invoke** (`Plan.invoke_skill` resolving a pinned Python skill by name), and the **`cli_memory_l3py_run_daemon_e2e`** end-to-end test all land in slice 2. The `body_sha256` re-hash binding at invoke time (the TOCTOU close for code) is a slice-2 control.

---

## Self-review notes

- **Spec coverage (slice-1 portion):** PythonSkillCandidate (Task 1) ✓; layer-3 + `kind` storage (Task 3 metadata builder) ✓; canonical SHA dedup (Tasks 2/3) ✓; agent-authored crystallise via `Plan.python_skill` + grounding gate (Tasks 1/4) ✓; pure approval = structural + secret scan, no registry (Task 5) ✓; `memory l3 show` + kind-aware list/approve/pin (Tasks 6/7) ✓; TDD unit + PG-gated e2e (throughout) ✓. Invocation/surfacing/agent-door/SHA-binding are explicitly slice 2.
- **Type consistency:** `PythonSkillCandidate{name,description,code}`, `PyError{Validation,Db}`, `PyWriteOutcome{Inserted,SkippedDuplicate}`, `crystallise_python_skill`, `evaluate_python_approval`, `RejectReason::CodeSecretRef{offset,found}`, `InnerLoopResult.terminal_python_skill`, `Plan.python_skill`, `Plan::completion_python_skill` are used identically across tasks. `L3Source` and `build_l3_write_payload` are reused unchanged.
- **No placeholders:** every code step shows complete code; the two spots that say "copy the existing harness" (the PG bring-up prologue in Task 3, and the inner-loop fake-formulator harness in Task 4) point at a specific existing test to mirror because that boilerplate is environment-specific and must match the in-tree pattern exactly — the assertions and new logic are fully spelled out.
