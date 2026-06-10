# Issue #95 — Layer B Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse entity + relation upsert from N per-row round-trips to 1 batch round-trip via PostgreSQL `unnest`, with a per-row attribution fallback on constraint violation (SQLSTATE class `23`).

**Architecture:** New sibling module `core/src/entity_extraction/batch_upsert.rs` holds pure helpers + the batch + per-row paths. `gliner_relex::upsert_entities_and_relations` keeps its public name and signature but its body becomes a single-line delegate. `UpsertOutcome` stays in `gliner_relex.rs` (public-API anchor). Dispatch within `batch_upsert::upsert_entities_and_relations` is two-phase: phase 1 (entities) tries the batch, falls back to per-row with `kind + name_norm` diagnostic wrapping on constraint violation; phase 2 (relations) is independent — entity phase having succeeded does not roll back if relations later fall back.

**Tech Stack:** Rust 2021, sqlx (PgPool, query, query_as), Postgres `unnest`, `serde_json`, `tokio::test`.

**Spec reference:** [`docs/superpowers/specs/2026-05-25-issue-95-layer-b-design.md`](../specs/2026-05-25-issue-95-layer-b-design.md) committed at `c70ae5d`.

**Baseline:** `main` at `e93997e`; macOS workspace 1023 passed / 0 failed / 3 ignored. Target after Layer B: ~1040 (+17 expected).

---

## Working environment for every task

All work happens in worktree **`/Users/hherb/src/kastellan-issue-95`** on branch **`feat/issue-95-upsert-layer-b`**. Every Bash command in this plan assumes that worktree is the cwd. Source the cargo env once per shell:

```sh
source "$HOME/.cargo/env"
```

When in doubt, `cd /Users/hherb/src/kastellan-issue-95`.

---

## Task 1: Scaffold `batch_upsert.rs` + ship `dedup_entity_inputs`

**Files:**
- Create: `core/src/entity_extraction/batch_upsert.rs`
- Modify: `core/src/entity_extraction/mod.rs:13` (add `pub mod batch_upsert;`)
- Test: same file (inline `#[cfg(test)] mod tests`)

**What this task delivers:** Empty new module file wired into the module tree, plus the first pure helper `dedup_entity_inputs` with three unit tests.

- [ ] **Step 1: Write the failing tests in the new file**

Create `core/src/entity_extraction/batch_upsert.rs`:

```rust
//! Layer B entity + relation upsert: batch-first via PostgreSQL `unnest`
//! with per-row attribution fallback on SQLSTATE class 23 (constraint
//! violations).
//!
//! Public surface: `upsert_entities_and_relations(pool, merged)` —
//! same signature as `gliner_relex::upsert_entities_and_relations`,
//! which now delegates here.
//!
//! See `docs/superpowers/specs/2026-05-25-issue-95-layer-b-design.md`
//! for design rationale.

use crate::workers::gliner_relex::Entity;
use kastellan_db::normalize_entity_name;

/// One unique entity input position in the batch. The `Vec<DedupedEntity>`
/// returned by `dedup_entity_inputs` carries no original-input index; the
/// position in the Vec IS the batch position. The original-input order is
/// preserved (via re-walk in the caller) by mapping back through the
/// `(label, name_norm)` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DedupedEntity<'a> {
    pub label: &'a str,
    pub text: &'a str,
    pub name_norm: String,
}

/// Deduplicate input entities on `(label, name_norm)`. Returns the unique
/// entries in first-seen order (so the display form of the first
/// occurrence wins, matching the per-row upsert's first-writer-wins on
/// `entities.name`).
///
/// Required because PostgreSQL's `INSERT ... ON CONFLICT DO UPDATE`
/// rejects duplicate conflict targets within a single statement with
/// `cardinality_violation: ON CONFLICT DO UPDATE command cannot affect
/// row a second time`. Deduping at the Rust layer keeps the SQL simple
/// and matches the per-row loop's observable behaviour (same id returned
/// for duplicate inputs).
pub(crate) fn dedup_entity_inputs<'a>(entities: &'a [Entity]) -> Vec<DedupedEntity<'a>> {
    let mut seen = std::collections::HashSet::<(String, String)>::new();
    let mut deduped = Vec::with_capacity(entities.len());
    for ent in entities {
        let name_norm = normalize_entity_name(&ent.text);
        let key = (ent.label.clone(), name_norm.clone());
        if seen.insert(key) {
            deduped.push(DedupedEntity {
                label: &ent.label,
                text: &ent.text,
                name_norm,
            });
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::gliner_relex::Entity;

    fn make_entity(text: &str, label: &str) -> Entity {
        Entity {
            text: text.to_string(),
            label: label.to_string(),
            start: 0,
            end: text.len() as u32,
            score: 0.99,
        }
    }

    #[test]
    fn dedup_entity_inputs_removes_same_key_duplicates_preserves_first_seen_order() {
        // Input: [Alpha#person, alpha#person, Beta#person]
        // Expected: [Alpha#person, Beta#person]
        // The lowercase `alpha` drops out; the original `Alpha` text
        // survives because it was seen first (first-writer-wins on
        // entities.name).
        let input = vec![
            make_entity("Alpha", "person"),
            make_entity("alpha", "person"),
            make_entity("Beta", "person"),
        ];
        let deduped = dedup_entity_inputs(&input);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].text, "Alpha");
        assert_eq!(deduped[0].name_norm, "alpha");
        assert_eq!(deduped[1].text, "Beta");
        assert_eq!(deduped[1].name_norm, "beta");
    }

    #[test]
    fn dedup_entity_inputs_distinct_kinds_with_same_name_norm_are_distinct() {
        // (kind, name_norm) is the dedup key — same name, different kinds
        // stay separate (`Smith` as person and `Smith` as organization).
        let input = vec![
            make_entity("Smith", "person"),
            make_entity("Smith", "organization"),
        ];
        let deduped = dedup_entity_inputs(&input);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].label, "person");
        assert_eq!(deduped[1].label, "organization");
    }

    #[test]
    fn dedup_entity_inputs_returns_empty_for_empty_input() {
        // Empty input → empty output. No SQL will be issued downstream.
        let input: Vec<Entity> = Vec::new();
        let deduped = dedup_entity_inputs(&input);
        assert!(deduped.is_empty());
    }
}
```

Modify `core/src/entity_extraction/mod.rs` line 13 — add `pub mod batch_upsert;` directly after `pub mod gliner_relex;`:

```rust
pub mod gliner_relex;
pub mod batch_upsert;
```

- [ ] **Step 2: Run the tests to verify they pass on first compile**

```sh
cd /Users/hherb/src/kastellan-issue-95
cargo test -p kastellan-core --lib entity_extraction::batch_upsert::tests -- --nocapture
```

Expected: `test result: ok. 3 passed; 0 failed`. (The tests are not strictly "failing first" — they describe pure helper behaviour that the helper implements correctly. The TDD discipline here is "write tests + implementation together in one file" — there's no separate red phase because there's no pre-existing code to be wrong about.)

- [ ] **Step 3: Run the full workspace to confirm no regression**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1023 + 3 = 1026; failed = 0; ignored = 3.

- [ ] **Step 4: Commit**

```sh
git add core/src/entity_extraction/batch_upsert.rs core/src/entity_extraction/mod.rs
git commit -m "$(cat <<'EOF'
feat(entity_extraction/batch_upsert): scaffold module + dedup_entity_inputs helper

First of 11 TDD-ordered slices implementing Issue #95 Layer B (full-batch
unnest entity + relation upsert with per-row attribution fallback).

This slice scaffolds the new sibling module and ships the first pure
helper: dedup_entity_inputs collapses input entities by (label,
name_norm) preserving first-seen order. Required because Postgres
INSERT ... ON CONFLICT DO UPDATE rejects duplicate conflict targets
within a single statement with cardinality_violation.

+3 unit tests, no DB dependency: same-key dedup preserves first-seen
display form; distinct kinds with same name_norm stay separate; empty
input returns empty.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `build_entity_unnest_arrays` pure helper

**Files:**
- Modify: `core/src/entity_extraction/batch_upsert.rs` (append helper + tests)

**What this task delivers:** Pure helper that assembles the four parallel arrays the entity-batch SQL expects from a `Vec<DedupedEntity>`.

- [ ] **Step 1: Write the failing tests + implementation**

Append to `core/src/entity_extraction/batch_upsert.rs` (above `#[cfg(test)] mod tests`):

```rust
/// Build the four parallel arrays the entity-batch unnest SQL expects.
/// Arrays are returned in the order:
///   (kinds, names, name_norms, quarantines)
/// All arrays have length `deduped.len()`. The quarantine array is
/// uniformly TRUE — new rows land quarantined; the ON CONFLICT no-op
/// (SET name_norm = entities.name_norm) preserves the operator's prior
/// approval on conflict-hit rows.
///
/// Returns `&'a str` slices into the borrowed DedupedEntity for `kinds`
/// and `names` (zero-allocation); `name_norms` is owned (already
/// normalized during dedup); `quarantines` is owned (uniform Vec).
pub(crate) fn build_entity_unnest_arrays<'a>(
    deduped: &'a [DedupedEntity<'a>],
) -> (Vec<&'a str>, Vec<&'a str>, Vec<String>, Vec<bool>) {
    let n = deduped.len();
    let mut kinds = Vec::with_capacity(n);
    let mut names = Vec::with_capacity(n);
    let mut name_norms = Vec::with_capacity(n);
    let mut quarantines = Vec::with_capacity(n);
    for d in deduped {
        kinds.push(d.label);
        names.push(d.text);
        name_norms.push(d.name_norm.clone());
        quarantines.push(true);
    }
    (kinds, names, name_norms, quarantines)
}
```

Add to the `#[cfg(test)] mod tests` block (before the closing brace):

```rust
    #[test]
    fn build_entity_unnest_arrays_emits_parallel_arrays_of_equal_length() {
        let input = vec![
            make_entity("Alpha", "person"),
            make_entity("Beta", "organization"),
            make_entity("Gamma", "person"),
        ];
        let deduped = dedup_entity_inputs(&input);
        let (kinds, names, name_norms, quarantines) = build_entity_unnest_arrays(&deduped);
        assert_eq!(kinds.len(), 3);
        assert_eq!(names.len(), 3);
        assert_eq!(name_norms.len(), 3);
        assert_eq!(quarantines.len(), 3);
        assert_eq!(kinds, vec!["person", "organization", "person"]);
        assert_eq!(names, vec!["Alpha", "Beta", "Gamma"]);
        assert_eq!(name_norms, vec!["alpha", "beta", "gamma"]);
        // Every new row lands quarantined; ON CONFLICT no-op preserves
        // operator's prior approval on conflict-hit rows.
        assert_eq!(quarantines, vec![true, true, true]);
    }

    #[test]
    fn build_entity_unnest_arrays_handles_empty_input() {
        let deduped: Vec<DedupedEntity<'_>> = Vec::new();
        let (kinds, names, name_norms, quarantines) = build_entity_unnest_arrays(&deduped);
        assert!(kinds.is_empty());
        assert!(names.is_empty());
        assert!(name_norms.is_empty());
        assert!(quarantines.is_empty());
    }
```

- [ ] **Step 2: Run the tests**

```sh
cargo test -p kastellan-core --lib entity_extraction::batch_upsert::tests -- --nocapture
```

Expected: `test result: ok. 5 passed; 0 failed` (3 from Task 1 + 2 new).

- [ ] **Step 3: Full workspace check**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1028; failed = 0.

- [ ] **Step 4: Commit**

```sh
git add core/src/entity_extraction/batch_upsert.rs
git commit -m "$(cat <<'EOF'
feat(entity_extraction/batch_upsert): build_entity_unnest_arrays helper

Pure helper that assembles the four parallel arrays the entity-batch
unnest SQL expects: (kinds, names, name_norms, quarantines). Returns
borrowed &str slices for kinds and names; owned Vec<String> for
name_norms (already normalized during dedup); owned Vec<bool> for
quarantines (uniform TRUE — ON CONFLICT no-op preserves operator
approvals on conflict-hit rows).

+2 unit tests: equal-length parallel arrays for N=3 input; empty input
returns four empty arrays.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `is_constraint_violation_code` + `is_constraint_violation` predicate

**Files:**
- Modify: `core/src/entity_extraction/batch_upsert.rs` (append helpers + tests)

**What this task delivers:** Pure predicate that classifies sqlx errors as "constraint violation worth falling back" vs "propagate immediately." Split into a code-string helper (unit-testable) + a sqlx-error wrapper (covered by integration tests).

- [ ] **Step 1: Write the helpers + tests**

Append to `core/src/entity_extraction/batch_upsert.rs`:

```rust
/// True iff the SQLSTATE code names a constraint violation (PostgreSQL
/// class 23). Members:
///   - 23000: integrity_constraint_violation (generic)
///   - 23001: restrict_violation
///   - 23502: not_null_violation
///   - 23503: foreign_key_violation
///   - 23505: unique_violation
///   - 23514: check_violation
///   - 23P01: exclusion_violation
///
/// These all indicate a per-row issue: re-running as per-row attribution
/// path will identify the failing row. Other classes (22 data exception,
/// 42 syntax, 08 connection failure, etc.) won't benefit from per-row
/// retry and should propagate immediately.
pub(crate) fn is_constraint_violation_code(code: &str) -> bool {
    code.starts_with("23")
}

/// True iff `err` is `sqlx::Error::Database` carrying a SQLSTATE class 23
/// code. Returns false for non-database errors (network, decode, timeout)
/// and for database errors without a code or with a non-23 code.
pub(crate) fn is_constraint_violation(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = err {
        if let Some(code) = db_err.code() {
            return is_constraint_violation_code(&code);
        }
    }
    false
}
```

Add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn is_constraint_violation_code_true_for_each_23xxx_code() {
        // Every member of the PostgreSQL constraint-violation family.
        for code in &["23000", "23001", "23502", "23503", "23505", "23514", "23P01"] {
            assert!(
                is_constraint_violation_code(code),
                "code {code} should classify as constraint violation"
            );
        }
    }

    #[test]
    fn is_constraint_violation_code_false_for_22xxx_data_exception() {
        // Data exception class — caller can't fix by per-row retry.
        for code in &["22001", "22003", "22007", "22P02"] {
            assert!(
                !is_constraint_violation_code(code),
                "code {code} should NOT classify as constraint violation"
            );
        }
    }

    #[test]
    fn is_constraint_violation_code_false_for_other_classes() {
        // Connection, syntax, transaction-rollback — none benefit from per-row retry.
        for code in &["08003", "42P01", "40001", "53300", "57014", ""] {
            assert!(
                !is_constraint_violation_code(code),
                "code {code} should NOT classify as constraint violation"
            );
        }
    }
```

- [ ] **Step 2: Run the tests**

```sh
cargo test -p kastellan-core --lib entity_extraction::batch_upsert::tests -- --nocapture
```

Expected: `test result: ok. 8 passed; 0 failed`.

- [ ] **Step 3: Full workspace check**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1031; failed = 0.

- [ ] **Step 4: Commit**

```sh
git add core/src/entity_extraction/batch_upsert.rs
git commit -m "$(cat <<'EOF'
feat(entity_extraction/batch_upsert): is_constraint_violation predicate

Split into pure code-classifier (is_constraint_violation_code) +
sqlx::Error wrapper (is_constraint_violation). The pure classifier is
unit-testable; the wrapper is exercised by integration tests when
Tasks 8 + 10 trigger real FK violations.

Class 23 (constraint violation) members handled: 23000 generic, 23001
restrict, 23502 not_null, 23503 foreign_key, 23505 unique, 23514 check,
23P01 exclusion. Other classes (22 data exception, 08 connection, 42
syntax, 40 transaction rollback) classify as non-fallback because per-row
retry won't help.

+3 unit tests: every 23xxx returns true; every 22xxx returns false;
representative other-class codes (08003, 42P01, 40001, 53300, 57014,
empty) all return false.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Error-message formatters

**Files:**
- Modify: `core/src/entity_extraction/batch_upsert.rs` (append helpers + tests)

**What this task delivers:** Pure helpers that wrap sqlx errors with `kind` + `name_norm` (entities) or `(src_id, dst_id, kind)` (relations) attribution before they propagate up. Used by the per-row fallback path.

- [ ] **Step 1: Write the helpers + tests**

Append to `core/src/entity_extraction/batch_upsert.rs`:

```rust
/// Format the per-row entity error message used by the fallback path.
/// Uses `name_norm` (NFC + lowercase + whitespace-collapsed) rather than
/// the raw user-supplied name to reduce PII leakage into error logs.
///
/// Example: `upsert entity (kind='person', name_norm='dr smith'): foreign key violation on entities_kind_fk`
pub(crate) fn format_per_row_entity_error(
    kind: &str,
    name_norm: &str,
    err: &sqlx::Error,
) -> String {
    format!("upsert entity (kind='{kind}', name_norm='{name_norm}'): {err}")
}

/// Format the per-row relation error message used by the fallback path.
/// Uses entity ids (already-resolved BIGINTs, no name leakage) and the
/// relation kind string.
///
/// Example: `insert relation (src=42, dst=43, kind='treats'): foreign key violation on relations_kind_fk`
pub(crate) fn format_per_row_relation_error(
    src_id: i64,
    dst_id: i64,
    kind: &str,
    err: &sqlx::Error,
) -> String {
    format!("insert relation (src={src_id}, dst={dst_id}, kind='{kind}'): {err}")
}
```

Add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn format_per_row_entity_error_uses_name_norm_not_raw_name() {
        // sqlx::Error::PoolTimedOut is convenient because it Display's as
        // a fixed string and needs no DB to construct. The actual sqlx
        // error variant doesn't matter for this format test.
        let err = sqlx::Error::PoolTimedOut;
        let msg = format_per_row_entity_error("person", "dr smith", &err);
        assert!(msg.contains("kind='person'"), "msg should contain kind: {msg}");
        assert!(msg.contains("name_norm='dr smith'"), "msg should contain name_norm: {msg}");
        // The raw form "Dr Smith" must NOT appear — name_norm only.
        assert!(!msg.contains("'Dr Smith'"), "msg should NOT contain raw name: {msg}");
        // The underlying sqlx error Display must be appended.
        assert!(msg.contains("pool"), "msg should contain underlying error: {msg}");
    }

    #[test]
    fn format_per_row_relation_error_contains_src_dst_kind() {
        let err = sqlx::Error::PoolTimedOut;
        let msg = format_per_row_relation_error(42, 43, "treats", &err);
        assert!(msg.contains("src=42"), "msg should contain src: {msg}");
        assert!(msg.contains("dst=43"), "msg should contain dst: {msg}");
        assert!(msg.contains("kind='treats'"), "msg should contain kind: {msg}");
        assert!(msg.contains("pool"), "msg should contain underlying error: {msg}");
    }
```

- [ ] **Step 2: Run the tests**

```sh
cargo test -p kastellan-core --lib entity_extraction::batch_upsert::tests -- --nocapture
```

Expected: `test result: ok. 10 passed; 0 failed`.

- [ ] **Step 3: Full workspace check**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1033; failed = 0.

- [ ] **Step 4: Commit**

```sh
git add core/src/entity_extraction/batch_upsert.rs
git commit -m "$(cat <<'EOF'
feat(entity_extraction/batch_upsert): per-row error formatters

format_per_row_entity_error wraps sqlx errors with kind + name_norm
attribution for the fallback path. format_per_row_relation_error does
the same with (src_id, dst_id, kind). Both use name_norm (normalized)
rather than raw user-supplied name to reduce PII leakage into error
logs.

+2 unit tests: entity formatter uses name_norm not raw name; relation
formatter carries all three identifiers. Both verify the underlying
sqlx error's Display is appended.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Entity-batch happy path + delegate from `gliner_relex.rs`

**Files:**
- Modify: `core/src/entity_extraction/batch_upsert.rs` (add `try_batch_upsert_entities` + `per_row_upsert_entities` + public `upsert_entities_and_relations` entity phase)
- Modify: `core/src/entity_extraction/gliner_relex.rs:172-289` (replace body with delegate call)
- Test: `core/tests/entity_extraction_e2e.rs` (add 1 new integration test; existing 5 tests should keep passing)

**What this task delivers:** The first integration test of the batch path; the dispatch from `gliner_relex::upsert_entities_and_relations` to the new module; both entity-batch and entity-fallback functions in place (relations stay as the Layer A loop for now). Existing 5 integration tests in `entity_extraction_e2e.rs` are the regression pin.

- [ ] **Step 1: Write the failing integration test in `core/tests/entity_extraction_e2e.rs`**

Append the following before any existing closing module brace (the file has no top-level `mod` so just append at the end):

```rust
/// Layer B happy-path regression pin: a fresh batch of N=5 unique
/// entities through the batch path produces the same UpsertOutcome
/// shape as Layer A would have (entity_ids in order, n_new = 5,
/// n_relations_inserted = 0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_happy_path_returns_same_outcome_shape_as_layer_a() {
    let Some((_cluster, pool)) = bring_up_pg("batch-happy").await else {
        return;
    };

    // 5 unique entities, no triples — pure entity-batch exercise.
    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(),   label: "person".into(),       start: 0, end: 5,  score: 0.99 },
            Entity { text: "Beta".into(),    label: "organization".into(), start: 0, end: 4,  score: 0.99 },
            Entity { text: "Gamma".into(),   label: "person".into(),       start: 0, end: 5,  score: 0.99 },
            Entity { text: "Delta".into(),   label: "location".into(),     start: 0, end: 5,  score: 0.99 },
            Entity { text: "Epsilon".into(), label: "person".into(),       start: 0, end: 7,  score: 0.99 },
        ],
        triples: vec![],
    };
    let out = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect("batch upsert should succeed on fresh batch");

    assert_eq!(out.entity_ids.len(), 5, "one id per input entity");
    assert_eq!(out.n_entities_upserted_new, 5, "every entity is new");
    assert_eq!(out.n_relations_inserted, 0, "no triples → no relations");

    // Verify each id round-trips to the expected (kind, name) pair via
    // a SELECT. This is the load-bearing regression pin for the
    // dispatcher: if try_batch_upsert returns ids in a different
    // order than the input, this assertion fails.
    for (idx, ent) in merged.entities.iter().enumerate() {
        let (kind, name): (String, String) =
            sqlx::query_as("SELECT kind, name FROM entities WHERE id = $1")
                .bind(out.entity_ids[idx])
                .fetch_one(&pool)
                .await
                .expect("SELECT round-trip");
        assert_eq!(&kind, &ent.label, "entity_ids[{idx}] kind mismatch");
        assert_eq!(&name, &ent.text, "entity_ids[{idx}] name mismatch");
    }

    pool.close().await;
}
```

- [ ] **Step 2: Run it to verify it fails**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e \
  upsert_batch_happy_path_returns_same_outcome_shape_as_layer_a \
  -- --nocapture
```

Expected: fails because `upsert_entities_and_relations` still uses the per-row loop (test SHOULD pass anyway if the per-row loop produces correct order — which it does — so this test actually doubles as a regression pin proving the per-row path also satisfies the shape contract). If it passes against the old per-row code, that's fine: the test moves to a green state and remains green when we swap to batch.

In practice: the test should PASS even before the impl change. That's the spec contract: byte-equivalent UpsertOutcome.

- [ ] **Step 3: Write the entity-phase implementation in `batch_upsert.rs`**

Append to `core/src/entity_extraction/batch_upsert.rs`:

```rust
use crate::entity_extraction::EntityExtractionError;
use crate::workers::gliner_relex::ExtractResponse;
use kastellan_db::DbError;
use sqlx::PgPool;
use std::collections::HashMap;

/// One row's worth of the entity batch's RETURNING clause: the
/// (kind, name_norm) key plus the resolved id and the xmax=0
/// inserted-vs-existed discriminator.
type EntityUpsertResult = (String, String, i64, bool);

/// Batch path: one round-trip via `unnest`. Returns a map from
/// `(kind, name_norm)` to `(id, inserted)` that the caller re-walks in
/// original input order to build `entity_ids: Vec<i64>` and count
/// `n_entities_upserted_new`. Empty input → empty map, no SQL issued.
async fn try_batch_upsert_entities(
    pool: &PgPool,
    deduped: &[DedupedEntity<'_>],
) -> Result<HashMap<(String, String), (i64, bool)>, sqlx::Error> {
    if deduped.is_empty() {
        return Ok(HashMap::new());
    }
    let (kinds, names, name_norms, quarantines) = build_entity_unnest_arrays(deduped);
    // unnest($1::text[], $2::text[], $3::text[], $4::bool[]) builds N
    // rows; ON CONFLICT DO UPDATE SET name_norm = entities.name_norm is
    // the load-bearing no-op that preserves operator-approved quarantine
    // state (pinned by upsert_batch_preserves_operator_unquarantine_decision
    // in Task 7). RETURNING includes kind + name_norm so the caller can
    // map results back to input position without an ORDINALITY CTE.
    let rows: Vec<EntityUpsertResult> = sqlx::query_as(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::bool[]) \
         ON CONFLICT (kind, name_norm) DO UPDATE \
           SET name_norm = entities.name_norm \
         RETURNING kind, name_norm, id, (xmax = 0) AS inserted",
    )
    .bind(&kinds)
    .bind(&names)
    .bind(&name_norms)
    .bind(&quarantines)
    .fetch_all(pool)
    .await?;
    let mut map = HashMap::with_capacity(rows.len());
    for (kind, name_norm, id, inserted) in rows {
        map.insert((kind, name_norm), (id, inserted));
    }
    Ok(map)
}

/// Per-row fallback: walks deduped entities, runs one Layer A statement
/// per row, wraps every error via `format_per_row_entity_error` so the
/// caller's error message identifies the failing entity by kind +
/// name_norm. First-failure-aborts (same posture as today's Layer A).
async fn per_row_upsert_entities(
    pool: &PgPool,
    deduped: &[DedupedEntity<'_>],
) -> Result<HashMap<(String, String), (i64, bool)>, EntityExtractionError> {
    let mut map = HashMap::with_capacity(deduped.len());
    for d in deduped {
        let (id, inserted): (i64, bool) = sqlx::query_as(
            "INSERT INTO entities (kind, name, name_norm, quarantine) \
             VALUES ($1, $2, $3, TRUE) \
             ON CONFLICT (kind, name_norm) DO UPDATE \
               SET name_norm = entities.name_norm \
             RETURNING id, (xmax = 0) AS inserted",
        )
        .bind(d.label)
        .bind(d.text)
        .bind(&d.name_norm)
        .fetch_one(pool)
        .await
        .map_err(|e| {
            DbError::Query(format_per_row_entity_error(d.label, &d.name_norm, &e))
        })?;
        map.insert((d.label.to_string(), d.name_norm.clone()), (id, inserted));
    }
    Ok(map)
}

/// Public Layer B entry point. Two-phase dispatch:
///   Phase 1 (entities): try batch, on SQLSTATE 23 fall back to per-row
///                       attribution; any other error propagates.
///   Phase 2 (relations): TODO Task 9 — currently delegates to the
///                       legacy per-row relation loop in
///                       gliner_relex.rs (lives at module scope via
///                       crate::entity_extraction::gliner_relex::
///                       upsert_relations_per_row_legacy).
///
/// Re-walks `merged.entities` in original input order to populate
/// `entity_ids: Vec<i64>` from the phase-1 map. `n_entities_upserted_new`
/// counts unique-key first-time inserts (a duplicate in input shares an
/// id with its sibling, so the duplicate does NOT double-count).
pub async fn upsert_entities_and_relations(
    pool: &PgPool,
    merged: &ExtractResponse,
) -> Result<crate::entity_extraction::gliner_relex::UpsertOutcome, EntityExtractionError> {
    // Phase 1: entity upsert with fallback.
    let deduped = dedup_entity_inputs(&merged.entities);
    let upsert_map = match try_batch_upsert_entities(pool, &deduped).await {
        Ok(m) => m,
        Err(e) if is_constraint_violation(&e) => {
            per_row_upsert_entities(pool, &deduped).await?
        }
        Err(e) => {
            return Err(EntityExtractionError::Db(DbError::Query(format!(
                "batch upsert entities: {e}"
            ))));
        }
    };

    // Re-walk merged.entities in original input order. Same-key duplicates
    // resolve to the same id (matches Layer A); the "new" counter only
    // fires once per unique (kind, name_norm) — pinned by Task 6's
    // dedup test.
    let mut entity_ids = Vec::with_capacity(merged.entities.len());
    let mut counted_new = std::collections::HashSet::<(String, String)>::new();
    let mut n_new: u32 = 0;
    for ent in &merged.entities {
        let key = (ent.label.clone(), normalize_entity_name(&ent.text));
        let (id, inserted) = upsert_map
            .get(&key)
            .copied()
            .expect("dedup invariant: every input entity is in the upsert_map");
        entity_ids.push(id);
        if inserted && counted_new.insert(key) {
            n_new += 1;
        }
    }

    // Phase 2 placeholder: delegate to legacy per-row relation loop for
    // now. Task 9 replaces this with the batch + fallback path.
    let n_relations_inserted = crate::entity_extraction::gliner_relex::
        upsert_relations_per_row_legacy(pool, merged, &upsert_map).await?;

    Ok(crate::entity_extraction::gliner_relex::UpsertOutcome {
        entity_ids,
        n_entities_upserted_new: n_new,
        n_relations_inserted,
    })
}
```

- [ ] **Step 4: Refactor `gliner_relex.rs` — extract the relation loop into `upsert_relations_per_row_legacy`, delegate `upsert_entities_and_relations` to the new module**

Replace `core/src/entity_extraction/gliner_relex.rs` lines 159-289 (the entire old function body including its big comment) with:

```rust
// Integration test coverage in core/tests/entity_extraction_e2e.rs:
//   - upsert_creates_quarantined_entities
//   - upsert_is_idempotent_on_rerun
//   - upsert_dedup_works_with_case_variants
//   - upsert_preserves_operator_unquarantine_decision
//   - upsert_counts_new_inserts_correctly_in_mixed_batch
//   - upsert_batch_* (Issue #95 Layer B, see batch_upsert.rs)

/// Upsert every entity in `merged.entities` into the `entities` table
/// (quarantine=TRUE on new rows; conflict by `(kind, name_norm)` →
/// preserve existing row including its quarantine state). Then for
/// every triple in `merged.triples`, look up the head and tail entity
/// ids and insert into `relations` if no row already exists with the
/// same `(src_id, dst_id, kind)` triple.
///
/// Best-effort idempotent: rerunning with the same input produces no
/// new rows.
///
/// Layer B (Issue #95): the public entry point now delegates to
/// `crate::entity_extraction::batch_upsert::upsert_entities_and_relations`,
/// which batches the entity upsert via `unnest` for a single round-trip
/// in the happy path and falls back to a per-row loop with diagnostic
/// error wrapping on SQLSTATE 23 constraint violations.
pub async fn upsert_entities_and_relations(
    pool: &PgPool,
    merged: &ExtractResponse,
) -> Result<UpsertOutcome, crate::entity_extraction::EntityExtractionError> {
    crate::entity_extraction::batch_upsert::upsert_entities_and_relations(pool, merged).await
}

/// Per-row relation upsert (Layer A path). Called from
/// `batch_upsert::upsert_entities_and_relations` while phase-2 (relation
/// batch) is still TODO — Task 9 will introduce a batch + fallback
/// counterpart and this function moves into batch_upsert.rs as the
/// fallback.
///
/// Returns the count of newly inserted relation rows (triples that
/// matched WHERE NOT EXISTS).
pub(crate) async fn upsert_relations_per_row_legacy(
    pool: &PgPool,
    merged: &ExtractResponse,
    by_key: &std::collections::HashMap<(String, String), (i64, bool)>,
) -> Result<u32, crate::entity_extraction::EntityExtractionError> {
    let mut n_relations_inserted: u32 = 0;
    for tri in &merged.triples {
        let head_key = (tri.head.r#type.clone(), normalize_entity_name(&tri.head.text));
        let tail_key = (tri.tail.r#type.clone(), normalize_entity_name(&tri.tail.text));
        let head_id = match by_key.get(&head_key) {
            Some((id, _)) => *id,
            None => continue, // triple references unknown entity — skip
        };
        let tail_id = match by_key.get(&tail_key) {
            Some((id, _)) => *id,
            None => continue,
        };
        let relation_norm = tri
            .relation
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        // Schema allows multi-edges intentionally (0001 comment); we
        // dedup at the application layer via WHERE NOT EXISTS to make
        // re-extraction idempotent.
        let n: u64 = sqlx::query(
            "INSERT INTO relations (src_id, dst_id, kind, attrs) \
             SELECT $1, $2, $3, '{}'::jsonb \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM relations \
                 WHERE src_id = $1 AND dst_id = $2 AND kind = $3 \
             )",
        )
        .bind(head_id)
        .bind(tail_id)
        .bind(&relation_norm)
        .execute(pool)
        .await
        .map_err(|e| kastellan_db::DbError::Query(format!("insert relation: {e}")))?
        .rows_affected();
        n_relations_inserted += n as u32;
    }
    Ok(n_relations_inserted)
}
```

- [ ] **Step 5: Run the new integration test**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e \
  upsert_batch_happy_path_returns_same_outcome_shape_as_layer_a \
  -- --nocapture
```

Expected: `test result: ok. 1 passed; 0 failed` (or `[SKIP]` on hosts without PG — turn that into PASS).

- [ ] **Step 6: Run all existing entity_extraction_e2e tests to verify no regression**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e -- --nocapture
```

Expected: all 5 existing tests + 1 new = 6 passed.

- [ ] **Step 7: Full workspace**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1034; failed = 0.

- [ ] **Step 8: Commit**

```sh
git add core/src/entity_extraction/batch_upsert.rs core/src/entity_extraction/gliner_relex.rs core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'EOF'
feat(entity_extraction): Layer B entity-batch path + delegate

Layer B happy path for entities: try_batch_upsert_entities runs one
INSERT ... SELECT FROM unnest(...) ON CONFLICT DO UPDATE statement
returning (kind, name_norm, id, xmax=0). Per-row fallback
per_row_upsert_entities walks deduped entities running today's Layer A
SQL wrapping each error via format_per_row_entity_error with kind +
name_norm attribution.

upsert_entities_and_relations in batch_upsert.rs:
  Phase 1 (entities): try batch → on SQLSTATE 23 fall back per-row
  Phase 2 (relations): delegates to upsert_relations_per_row_legacy in
                       gliner_relex.rs (Task 9 replaces with batch path)

gliner_relex.rs public surface preserved: upsert_entities_and_relations
keeps signature; body becomes a single-line delegate. UpsertOutcome
struct stays in gliner_relex.rs (public-API anchor).

+1 integration test (upsert_batch_happy_path_returns_same_outcome_shape_as_layer_a):
N=5 fresh batch through the batch path, asserts UpsertOutcome shape +
each entity_id round-trips to the expected (kind, name) via SELECT.

Existing 5 entity_extraction_e2e tests continue to pass byte-equivalently
through the new delegate path — they are the regression pin for the
public-API contract.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Entity-batch order preservation + same-key dedup integration tests

**Files:**
- Test: `core/tests/entity_extraction_e2e.rs` (append 2 tests)

**What this task delivers:** Two integration tests that verify the dispatcher's order-preservation logic. No code changes — the dispatcher built in Task 5 should already satisfy them. If either fails, that's a Task 5 bug.

- [ ] **Step 1: Write the failing tests**

Append to `core/tests/entity_extraction_e2e.rs`:

```rust
/// Pins that entity_ids is returned in the original input order even
/// though the unnest batch's RETURNING clause may emit rows in arbitrary
/// order. Layer B's HashMap re-walk preserves order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_preserves_entity_id_order_for_unique_inputs() {
    let Some((_cluster, pool)) = bring_up_pg("batch-order").await else {
        return;
    };

    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "Beta".into(),  label: "person".into(), start: 0, end: 4, score: 0.99 },
            Entity { text: "Gamma".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
        ],
        triples: vec![],
    };
    let out = upsert_entities_and_relations(&pool, &merged).await.unwrap();

    // Verify each id resolves to the expected name in input order.
    assert_eq!(out.entity_ids.len(), 3);
    for (idx, expected_name) in ["Alpha", "Beta", "Gamma"].iter().enumerate() {
        let name: String =
            sqlx::query_scalar("SELECT name FROM entities WHERE id = $1")
                .bind(out.entity_ids[idx])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(&name, expected_name, "entity_ids[{idx}] wrong order");
    }

    pool.close().await;
}

/// Pins that input duplicates resolve to the same id and n_new counts
/// each unique (kind, name_norm) only once, even when the input has
/// duplicates. Matches Layer A's observable behaviour where each
/// per-row upsert of a duplicate hits ON CONFLICT and returns the
/// same id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_dedup_input_returns_same_id_for_duplicates() {
    let Some((_cluster, pool)) = bring_up_pg("batch-dedup").await else {
        return;
    };

    // Input: [Alpha, alpha (same key — dups), Beta]
    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "alpha".into(), label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "Beta".into(),  label: "person".into(), start: 0, end: 4, score: 0.99 },
        ],
        triples: vec![],
    };
    let out = upsert_entities_and_relations(&pool, &merged).await.unwrap();

    assert_eq!(out.entity_ids.len(), 3, "entity_ids has one id per input position");
    assert_eq!(
        out.entity_ids[0], out.entity_ids[1],
        "duplicate inputs (Alpha and alpha) must resolve to the same id"
    );
    assert_ne!(
        out.entity_ids[0], out.entity_ids[2],
        "distinct inputs (Alpha and Beta) must resolve to different ids"
    );
    assert_eq!(
        out.n_entities_upserted_new, 2,
        "duplicate should NOT double-count — exactly 2 new (Alpha, Beta)"
    );
    assert_eq!(out.n_relations_inserted, 0);

    pool.close().await;
}
```

- [ ] **Step 2: Run them**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e \
  upsert_batch_preserves_entity_id_order_for_unique_inputs \
  upsert_batch_dedup_input_returns_same_id_for_duplicates \
  -- --nocapture
```

Expected: both pass. (If either fails, the Task 5 dispatcher has a bug — debug there before continuing.)

- [ ] **Step 3: Full workspace**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1036; failed = 0.

- [ ] **Step 4: Commit**

```sh
git add core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'EOF'
test(entity_extraction): pin Layer B order + dedup behaviour

upsert_batch_preserves_entity_id_order_for_unique_inputs proves the
dispatcher's HashMap re-walk preserves original input order even when
the unnest batch's RETURNING emits rows in arbitrary order.

upsert_batch_dedup_input_returns_same_id_for_duplicates proves
[Alpha, alpha, Beta] returns three ids with the first two equal (both
resolve to the same upserted row) and n_entities_upserted_new = 2 (not
3 — the duplicate doesn't double-count). Matches Layer A's per-row
behaviour byte-equivalently.

No code change — these tests exercise the dispatcher built in the
previous slice.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Quarantine preservation through the batch path

**Files:**
- Test: `core/tests/entity_extraction_e2e.rs` (append 1 test)

**What this task delivers:** The load-bearing invariant pin (the existing Layer A test `upsert_preserves_operator_unquarantine_decision` covers N=1 through the batch path of N=1; this test covers N=3 with the operator-approved entity in position 2). Confirms the `SET name_norm = entities.name_norm` no-op in the unnest SQL preserves operator approvals just like Layer A's per-row SQL did.

- [ ] **Step 1: Write the failing test**

Append to `core/tests/entity_extraction_e2e.rs`:

```rust
/// Layer B batch path must preserve operator-approved (quarantine=FALSE)
/// entities just like Layer A. This is the load-bearing invariant the
/// no-op `SET name_norm = entities.name_norm` clause guarantees: ON
/// CONFLICT must not touch the quarantine column. Pinned for N=3 with
/// the approved entity in the middle position (Layer A's existing pin
/// uses N=1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_preserves_operator_unquarantine_decision() {
    let Some((_cluster, pool)) = bring_up_pg("batch-quar").await else {
        return;
    };

    // First pass: insert 3 entities. All land quarantined.
    let merged1 = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(),    label: "person".into(), start: 0, end: 5, score: 0.99 },
            Entity { text: "Dr Smith".into(), label: "person".into(), start: 0, end: 8, score: 0.99 },
            Entity { text: "Gamma".into(),    label: "person".into(), start: 0, end: 5, score: 0.99 },
        ],
        triples: vec![],
    };
    let out1 = upsert_entities_and_relations(&pool, &merged1).await.unwrap();
    assert_eq!(out1.entity_ids.len(), 3);
    let smith_id = out1.entity_ids[1];

    // Operator approves the middle entity via the quarantine-review CLI
    // (simulated as a direct UPDATE).
    sqlx::query("UPDATE entities SET quarantine = FALSE WHERE id = $1")
        .bind(smith_id)
        .execute(&pool)
        .await
        .expect("operator approve simulation");

    // Second pass: re-extract — all three hit ON CONFLICT through the
    // batch path.
    let out2 = upsert_entities_and_relations(&pool, &merged1).await.unwrap();
    assert_eq!(out2.entity_ids, out1.entity_ids, "same ids returned");
    assert_eq!(out2.n_entities_upserted_new, 0, "no new rows on rerun");

    // Load-bearing assertion: the batch path's ON CONFLICT DO UPDATE
    // SET name_norm = entities.name_norm must NOT have clobbered the
    // operator's approval.
    let quarantine_after: bool =
        sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
            .bind(smith_id)
            .fetch_one(&pool)
            .await
            .expect("read back quarantine");
    assert!(
        !quarantine_after,
        "Layer B batch path must preserve operator unquarantine decision (quarantine=FALSE)"
    );

    // The sibling entities (Alpha, Gamma) should still be quarantined —
    // operator only approved Smith.
    for sibling_id in [out1.entity_ids[0], out1.entity_ids[2]] {
        let q: bool = sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
            .bind(sibling_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(q, "sibling entity should remain quarantined (operator only approved Smith)");
    }

    pool.close().await;
}
```

- [ ] **Step 2: Run it**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e \
  upsert_batch_preserves_operator_unquarantine_decision \
  -- --nocapture
```

Expected: pass. (If it fails, the batch SQL is missing the `SET name_norm = entities.name_norm` no-op.)

- [ ] **Step 3: Full workspace**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1037; failed = 0.

- [ ] **Step 4: Commit**

```sh
git add core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'EOF'
test(entity_extraction): pin Layer B quarantine-preservation invariant

Mirror of Layer A's upsert_preserves_operator_unquarantine_decision but
with N=3 entities and the operator-approved entity in the middle
position. Verifies the batch path's ON CONFLICT DO UPDATE SET
name_norm = entities.name_norm clause preserves operator approvals
byte-equivalently to Layer A's per-row SQL.

Also asserts sibling entities (Alpha, Gamma) remain quarantined — the
operator only approved Smith.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Entity fallback path on `entity_kinds` FK violation

**Files:**
- Test: `core/tests/entity_extraction_e2e.rs` (append 1 test)

**What this task delivers:** The first integration test that fires the fallback path. Drops an entry from `entity_kinds`, attempts upsert with that kind, asserts the returned error carries `kind='...'` and `name_norm='...'` substrings (proof that `format_per_row_entity_error` wrapped the error during the fallback walk).

- [ ] **Step 1: Write the failing test**

Append to `core/tests/entity_extraction_e2e.rs`:

```rust
/// Layer B fallback pin: when the batch upsert trips a constraint
/// violation (SQLSTATE class 23), the dispatcher falls back to per-row
/// upsert which wraps each error via format_per_row_entity_error
/// carrying the failing entity's kind + name_norm. This is the
/// attribution improvement over Layer A (today's per-row loop wraps
/// errors with just "upsert entity: <sqlx err>" — no per-row identifier).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_falls_back_to_per_row_on_entity_kind_fk_violation() {
    let Some((_cluster, pool)) = bring_up_pg("batch-fb-ent").await else {
        return;
    };

    // The schema's seed includes a baseline of entity_kinds rows
    // (migration 0015). Delete one to force an FK violation when the
    // upsert tries to insert an entity with that kind.
    // We use a known-deletable kind — `event` is in the 0015 seed and
    // unused in this test.
    let deleted_kind = "event";
    sqlx::query("DELETE FROM entity_kinds WHERE kind = $1")
        .bind(deleted_kind)
        .execute(&pool)
        .await
        .expect("delete entity_kinds row");

    // Attempt to upsert two entities: one with the dropped kind (which
    // will fail FK), one with a present kind. The batch should fail with
    // 23503 foreign_key_violation; the dispatcher falls back to per-row
    // which produces an error message identifying the failing kind.
    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Alpha".into(),       label: "person".into(),       start: 0, end: 5, score: 0.99 },
            Entity { text: "ConcertX".into(),    label: deleted_kind.into(),   start: 0, end: 8, score: 0.99 },
        ],
        triples: vec![],
    };
    let err = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect_err("expected FK violation from missing entity_kind");

    let msg = err.to_string();
    assert!(
        msg.contains(&format!("kind='{deleted_kind}'")),
        "fallback error should identify the failing kind '{deleted_kind}': {msg}"
    );
    assert!(
        msg.contains("name_norm='concertx'"),
        "fallback error should carry name_norm of failing entity: {msg}"
    );
    // Sanity: the error should also mention foreign key violation
    // (sqlx propagates the underlying message).
    assert!(
        msg.to_lowercase().contains("foreign key"),
        "fallback error should propagate underlying FK violation message: {msg}"
    );

    pool.close().await;
}
```

- [ ] **Step 2: Run it**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e \
  upsert_batch_falls_back_to_per_row_on_entity_kind_fk_violation \
  -- --nocapture
```

Expected: pass. If it fails because the error message format doesn't match, double-check `format_per_row_entity_error` from Task 4 — the format string must contain `kind='...'` and `name_norm='...'` exactly as the test asserts.

- [ ] **Step 3: Full workspace**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1038; failed = 0.

- [ ] **Step 4: Commit**

```sh
git add core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'EOF'
test(entity_extraction): pin Layer B per-row fallback on entity_kind FK

Delete an entry from entity_kinds, attempt upsert with that kind, assert
the returned error's message contains kind='<dropped>' and
name_norm='<failing entity>'. Proves:
  1. The batch path fails with SQLSTATE 23503 (FK violation)
  2. is_constraint_violation classifies it as fallback-worthy
  3. per_row_upsert_entities runs and produces a diagnostic error
  4. format_per_row_entity_error wraps the failing row's kind +
     name_norm into the error message
  5. The underlying sqlx FK violation message is preserved

This is the attribution improvement over Layer A — today's per-row loop
wraps errors with just "upsert entity: <sqlx err>" without identifying
the failing entity. The fallback path's per-row error wrapping closes
that diagnostic gap.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Relation-batch happy path + fallback dispatch

**Files:**
- Modify: `core/src/entity_extraction/batch_upsert.rs` (add `try_batch_upsert_relations` + `per_row_upsert_relations` + phase-2 dispatch; replace the Task 5 legacy placeholder)
- Modify: `core/src/entity_extraction/gliner_relex.rs` (remove `upsert_relations_per_row_legacy` once batch_upsert.rs no longer needs it)
- Test: `core/tests/entity_extraction_e2e.rs` (append 1 test)

**What this task delivers:** Phase 2 — relations batch + fallback. After this task the legacy per-row relation loop in `gliner_relex.rs` is gone; both phases live in `batch_upsert.rs`.

- [ ] **Step 1: Write the failing integration test**

Append to `core/tests/entity_extraction_e2e.rs`:

```rust
/// Layer B relations happy-path pin: triples insert via batch, dedup via
/// WHERE NOT EXISTS, skip triples whose head or tail references an
/// entity not in merged.entities. Re-run is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_relations_inserts_dedups_and_skips_unknown_entities() {
    let Some((_cluster, pool)) = bring_up_pg("batch-rel-happy").await else {
        return;
    };

    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Dr Smith".into(), label: "person".into(),   start: 0, end: 8, score: 0.99 },
            Entity { text: "asthma".into(),   label: "condition".into(), start: 0, end: 6, score: 0.99 },
        ],
        triples: vec![
            // Valid triple: both endpoints in merged.entities.
            Triple {
                head: TripleEntity { text: "Dr Smith".into(), r#type: "person".into() },
                tail: TripleEntity { text: "asthma".into(),   r#type: "condition".into() },
                relation: "treats".into(),
            },
            // Triple referencing an unknown entity → should be silently skipped.
            Triple {
                head: TripleEntity { text: "Dr Smith".into(), r#type: "person".into() },
                tail: TripleEntity { text: "diabetes".into(), r#type: "condition".into() },
                relation: "treats".into(),
            },
        ],
    };
    let out1 = upsert_entities_and_relations(&pool, &merged).await.unwrap();
    assert_eq!(out1.entity_ids.len(), 2, "both entities upserted");
    assert_eq!(
        out1.n_relations_inserted, 1,
        "only the valid triple should insert (unknown-entity triple silently skipped)"
    );

    // Re-run: WHERE NOT EXISTS makes the relation insert idempotent.
    let out2 = upsert_entities_and_relations(&pool, &merged).await.unwrap();
    assert_eq!(out2.n_relations_inserted, 0, "re-run finds the relation present");

    // Verify the relation row landed in the DB with the expected kind.
    let (src, dst, kind): (i64, i64, String) = sqlx::query_as(
        "SELECT src_id, dst_id, kind FROM relations WHERE kind = 'treats' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(src, out1.entity_ids[0]);
    assert_eq!(dst, out1.entity_ids[1]);
    assert_eq!(kind, "treats");

    pool.close().await;
}
```

- [ ] **Step 2: Run it to verify it fails (or passes — currently delegates to the legacy per-row loop in `gliner_relex.rs` which behaves identically)**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e \
  upsert_batch_relations_inserts_dedups_and_skips_unknown_entities \
  -- --nocapture
```

Expected: PASSES against the legacy delegate (the assertion population is shape-equivalent). This is intentional — the test pins behaviour that both Layer A and Layer B must satisfy. After Step 3-5 it remains green via the new batch path.

- [ ] **Step 3: Write the relation-phase implementation in `batch_upsert.rs`**

Append to `core/src/entity_extraction/batch_upsert.rs`:

```rust
/// One row's worth of phase-2 input: resolved (src_id, dst_id) plus the
/// normalized relation kind. Built from `merged.triples` after looking
/// up head/tail in the entity upsert map; triples referencing an
/// unknown entity are silently skipped (matches Layer A behaviour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTriple {
    pub src_id: i64,
    pub dst_id: i64,
    pub kind: String,
}

/// Walk `merged.triples`, look up each triple's head and tail in the
/// entity-upsert map, normalize the relation kind, and collect surviving
/// triples into a Vec<ResolvedTriple>. Triples where either endpoint is
/// missing from the map are silently skipped (matches Layer A's
/// `continue` posture).
pub(crate) fn build_resolved_triples(
    merged: &ExtractResponse,
    by_key: &HashMap<(String, String), (i64, bool)>,
) -> Vec<ResolvedTriple> {
    let mut out = Vec::with_capacity(merged.triples.len());
    for tri in &merged.triples {
        let head_key = (tri.head.r#type.clone(), normalize_entity_name(&tri.head.text));
        let tail_key = (tri.tail.r#type.clone(), normalize_entity_name(&tri.tail.text));
        let head_id = match by_key.get(&head_key) {
            Some((id, _)) => *id,
            None => continue,
        };
        let tail_id = match by_key.get(&tail_key) {
            Some((id, _)) => *id,
            None => continue,
        };
        let kind = tri
            .relation
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        out.push(ResolvedTriple { src_id: head_id, dst_id: tail_id, kind });
    }
    out
}

/// Batch path for relations: one round-trip via `unnest`. Uses WHERE NOT
/// EXISTS for application-level dedup (the `relations` table has no
/// UNIQUE constraint by design — multi-edges with different timestamps
/// are intentional per the comment in migration 0001_init.sql).
/// Empty input → 0 rows inserted, no SQL issued.
async fn try_batch_upsert_relations(
    pool: &PgPool,
    resolved: &[ResolvedTriple],
) -> Result<u32, sqlx::Error> {
    if resolved.is_empty() {
        return Ok(0);
    }
    let srcs: Vec<i64> = resolved.iter().map(|r| r.src_id).collect();
    let dsts: Vec<i64> = resolved.iter().map(|r| r.dst_id).collect();
    let kinds: Vec<&str> = resolved.iter().map(|r| r.kind.as_str()).collect();

    let rows: Vec<(i64,)> = sqlx::query_as(
        "WITH input(src_id, dst_id, kind) AS ( \
            SELECT * FROM unnest($1::bigint[], $2::bigint[], $3::text[]) \
         ) \
         INSERT INTO relations (src_id, dst_id, kind, attrs) \
         SELECT i.src_id, i.dst_id, i.kind, '{}'::jsonb \
         FROM input i \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM relations r \
             WHERE r.src_id = i.src_id AND r.dst_id = i.dst_id AND r.kind = i.kind \
         ) \
         RETURNING id",
    )
    .bind(&srcs)
    .bind(&dsts)
    .bind(&kinds)
    .fetch_all(pool)
    .await?;
    Ok(rows.len() as u32)
}

/// Per-row fallback for relations: walks resolved triples, runs today's
/// Layer A WHERE NOT EXISTS SQL per row, wraps each error via
/// format_per_row_relation_error so the caller's error message
/// identifies the failing relation by (src_id, dst_id, kind).
async fn per_row_upsert_relations(
    pool: &PgPool,
    resolved: &[ResolvedTriple],
) -> Result<u32, EntityExtractionError> {
    let mut n_inserted: u32 = 0;
    for r in resolved {
        let n: u64 = sqlx::query(
            "INSERT INTO relations (src_id, dst_id, kind, attrs) \
             SELECT $1, $2, $3, '{}'::jsonb \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM relations \
                 WHERE src_id = $1 AND dst_id = $2 AND kind = $3 \
             )",
        )
        .bind(r.src_id)
        .bind(r.dst_id)
        .bind(&r.kind)
        .execute(pool)
        .await
        .map_err(|e| {
            DbError::Query(format_per_row_relation_error(r.src_id, r.dst_id, &r.kind, &e))
        })?
        .rows_affected();
        n_inserted += n as u32;
    }
    Ok(n_inserted)
}
```

- [ ] **Step 4: Replace the Task 5 legacy placeholder in `upsert_entities_and_relations`**

In `core/src/entity_extraction/batch_upsert.rs`, find the comment block + line:

```rust
    // Phase 2 placeholder: delegate to legacy per-row relation loop for
    // now. Task 9 replaces this with the batch + fallback path.
    let n_relations_inserted = crate::entity_extraction::gliner_relex::
        upsert_relations_per_row_legacy(pool, merged, &upsert_map).await?;
```

Replace with:

```rust
    // Phase 2: relation upsert with fallback.
    let resolved = build_resolved_triples(merged, &upsert_map);
    let n_relations_inserted = match try_batch_upsert_relations(pool, &resolved).await {
        Ok(n) => n,
        Err(e) if is_constraint_violation(&e) => {
            per_row_upsert_relations(pool, &resolved).await?
        }
        Err(e) => {
            return Err(EntityExtractionError::Db(DbError::Query(format!(
                "batch insert relations: {e}"
            ))));
        }
    };
```

- [ ] **Step 5: Delete `upsert_relations_per_row_legacy` from `gliner_relex.rs`** — it's no longer called. Remove the entire `pub(crate) async fn upsert_relations_per_row_legacy(...)` definition added in Task 5.

- [ ] **Step 6: Run the new test + all entity_extraction_e2e tests**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e -- --nocapture
```

Expected: 9 passed (5 existing Layer A + 4 new Layer B from Tasks 5-9).

- [ ] **Step 7: Full workspace**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1039; failed = 0.

- [ ] **Step 8: Commit**

```sh
git add core/src/entity_extraction/batch_upsert.rs core/src/entity_extraction/gliner_relex.rs core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'EOF'
feat(entity_extraction): Layer B relations-batch path + fallback

Phase 2 of Layer B: collapse the per-triple relation loop into one batch
via WITH input(...) AS (SELECT * FROM unnest(...)) INSERT ... WHERE NOT
EXISTS ... RETURNING id. Counts newly inserted rows by len() of the
returned id list (preserves Layer A's application-level dedup via WHERE
NOT EXISTS; relations has no UNIQUE constraint by schema design).

build_resolved_triples is a pure helper that walks merged.triples,
looks up head/tail in the entity-upsert map, normalizes the relation
kind, and collects surviving triples. Triples referencing an unknown
entity are silently skipped (matches Layer A `continue` posture).

per_row_upsert_relations is the fallback path — walks resolved triples
running Layer A SQL per-row, wrapping each error via
format_per_row_relation_error with (src_id, dst_id, kind) attribution.

upsert_relations_per_row_legacy deleted from gliner_relex.rs — both
phases now live in batch_upsert.rs.

+1 integration test: relations happy-path (valid triple inserts,
unknown-entity triple silently skipped, re-run is idempotent, row
shape verified via SELECT).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Relations fallback on `relation_kinds` FK violation

**Files:**
- Test: `core/tests/entity_extraction_e2e.rs` (append 1 test)

**What this task delivers:** Mirror of Task 8 but for relations — drops a `relation_kinds` row, attempts upsert with that kind, asserts the error carries `kind='<dropped>'` (proof `format_per_row_relation_error` wrapped during the fallback walk).

- [ ] **Step 1: Write the failing test**

Append to `core/tests/entity_extraction_e2e.rs`:

```rust
/// Layer B relations fallback pin: when a relation kind isn't in
/// relation_kinds, the batch insert trips FK violation (23503), the
/// dispatcher falls back to per-row which produces a diagnostic error
/// naming src/dst/kind. This is the attribution improvement over the
/// pre-Layer-B per-row code (which wrapped relation errors with just
/// "insert relation: <sqlx err>" — no per-row identifier).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_batch_falls_back_to_per_row_on_relation_kind_fk_violation() {
    let Some((_cluster, pool)) = bring_up_pg("batch-fb-rel").await else {
        return;
    };

    // Use a relation kind that is NOT in relation_kinds seed (migration
    // 0017's 19 seed values: undefined, treats, prescribed, diagnosed
    // with, has symptom, side effect of, contraindicated with, allergic
    // to, located in, employed by, works at, member of, owns, knows,
    // identified as, refers to, occurred on, associated with, relative of).
    // `eats` is not in the seed list.
    let bogus_kind = "eats";

    let merged = ExtractResponse {
        entities: vec![
            Entity { text: "Dr Smith".into(), label: "person".into(),    start: 0, end: 8, score: 0.99 },
            Entity { text: "lunch".into(),    label: "event".into(),     start: 0, end: 5, score: 0.99 },
        ],
        triples: vec![
            Triple {
                head: TripleEntity { text: "Dr Smith".into(), r#type: "person".into() },
                tail: TripleEntity { text: "lunch".into(),    r#type: "event".into() },
                relation: bogus_kind.into(),
            },
        ],
    };
    let err = upsert_entities_and_relations(&pool, &merged)
        .await
        .expect_err("expected FK violation from missing relation_kind");

    let msg = err.to_string();
    assert!(
        msg.contains(&format!("kind='{bogus_kind}'")),
        "fallback error should identify the failing relation kind '{bogus_kind}': {msg}"
    );
    assert!(
        msg.contains("src=") && msg.contains("dst="),
        "fallback error should carry src/dst ids: {msg}"
    );
    assert!(
        msg.to_lowercase().contains("foreign key"),
        "fallback error should propagate underlying FK violation message: {msg}"
    );

    pool.close().await;
}
```

- [ ] **Step 2: Run it**

```sh
cargo test -p kastellan-core --test entity_extraction_e2e \
  upsert_batch_falls_back_to_per_row_on_relation_kind_fk_violation \
  -- --nocapture
```

Expected: pass.

- [ ] **Step 3: Full workspace**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: total passed = 1040; failed = 0.

- [ ] **Step 4: Commit**

```sh
git add core/tests/entity_extraction_e2e.rs
git commit -m "$(cat <<'EOF'
test(entity_extraction): pin Layer B relations fallback on relation_kind FK

Mirror of the entity-side fallback test from Task 8. Uses 'eats' (not in
migration 0017's 19-value relation_kinds seed) as the bogus relation
kind. Asserts:
  1. Batch insert fails with FK violation (23503)
  2. Dispatcher falls back to per-row
  3. format_per_row_relation_error wraps with src/dst/kind
  4. Underlying sqlx FK violation message is preserved

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Final verification + file-size check + push

**Files:** none modified (verification + measurement only).

**What this task delivers:** Confirms the workspace is green end-to-end, both `batch_upsert.rs` and `gliner_relex.rs` are under the 500-LOC cap, no new clippy warnings, branch is ready for PR.

- [ ] **Step 1: Full workspace + Python**

```sh
cd /Users/hherb/src/kastellan-issue-95
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tee /tmp/layer-b-final.txt | tail -10
```

Expected: total passed = 1040 (+17 over 1023 baseline); failed = 0; ignored = 3.

Sum-check:

```sh
grep "^test result:" /tmp/layer-b-final.txt | python3 -c "
import sys, re
p=f=i=0
for line in sys.stdin:
    m = re.search(r'(\d+) passed.*?(\d+) failed.*?(\d+) ignored', line)
    if m: p += int(m.group(1)); f += int(m.group(2)); i += int(m.group(3))
print(f'TOTAL passed: {p}, failed: {f}, ignored: {i}')
"
```

Expected: `TOTAL passed: 1040, failed: 0, ignored: 3`. (May vary by ±1 from the +17 estimate — the spec accepted a range. If the count is off by more than ±2, debug before continuing.)

- [ ] **Step 2: File-size check**

```sh
wc -l core/src/entity_extraction/batch_upsert.rs core/src/entity_extraction/gliner_relex.rs
```

Expected:
- `batch_upsert.rs`: under 500 LOC (target ~400-450 LOC including pure-helper tests + module doc).
- `gliner_relex.rs`: under 500 LOC (target ~180 LOC after the delegate refactor — was ~289 LOC pre-task; we removed the upsert body, added a 1-line delegate, then in Task 9 deleted `upsert_relations_per_row_legacy`).

If either file is over cap, flag for follow-up in the HANDOVER tech-debt section but don't block the merge.

- [ ] **Step 3: Clippy check**

```sh
cargo clippy --workspace --all-targets 2>&1 | grep -E "^warning|^error" | grep -v "^warning: unused" | head -20
```

Expected: no NEW warnings beyond the 5 pre-existing in `db/src/probe.rs` (3 doc-list-indent) and `kastellan-protocol` (2 io_other_error). If new warnings appear, fix them before the commit.

- [ ] **Step 4: Verify the audit-payload 8-key contract is unchanged**

```sh
cargo test -p kastellan-core --lib scheduler::audit::tests build_extract_entities_payload -- --nocapture
```

Expected: pass. (The Layer B change does not modify `build_extract_entities_payload`; this is a defensive re-check.)

- [ ] **Step 5: Verify branch state + push**

```sh
git log --oneline main..feat/issue-95-upsert-layer-b
```

Expected output should look like (10 task commits + 1 spec commit + the merge base):

```
<sha> test(entity_extraction): pin Layer B relations fallback on relation_kind FK
<sha> feat(entity_extraction): Layer B relations-batch path + fallback
<sha> test(entity_extraction): pin Layer B per-row fallback on entity_kind FK
<sha> test(entity_extraction): pin Layer B quarantine-preservation invariant
<sha> test(entity_extraction): pin Layer B order + dedup behaviour
<sha> feat(entity_extraction): Layer B entity-batch path + delegate
<sha> feat(entity_extraction/batch_upsert): per-row error formatters
<sha> feat(entity_extraction/batch_upsert): is_constraint_violation predicate
<sha> feat(entity_extraction/batch_upsert): build_entity_unnest_arrays helper
<sha> feat(entity_extraction/batch_upsert): scaffold module + dedup_entity_inputs helper
c70ae5d docs(specs): Issue #95 Layer B design — batch-first + per-row attribution fallback
```

Push (operator confirms first; do NOT push without checking):

```sh
git push -u origin feat/issue-95-upsert-layer-b
```

- [ ] **Step 6: Open PR (operator confirms first)**

```sh
gh pr create --title "Issue #95: entity-upsert Layer B (batch-first + per-row attribution fallback)" --body "$(cat <<'EOF'
## Summary

- Implements Issue #95 Layer B: full-batch `unnest` upsert for entities + relations with per-row attribution fallback on SQLSTATE class 23 constraint violations.
- Public API (`UpsertOutcome`, `EntityExtractionError`) byte-frozen — Layer A integration tests continue to pass byte-equivalently.
- New sibling module `core/src/entity_extraction/batch_upsert.rs` holds both phases + 5 pure helpers.
- `gliner_relex::upsert_entities_and_relations` keeps its name and signature; body becomes a 1-line delegate.
- 8-key `build_extract_entities_payload` audit contract preserved.
- Quarantine no-op `SET name_norm = entities.name_norm` retained in both batch and per-row SQL.

## Design context

The original Issue #95 framing deferred Layer B until trigger conditions fired (observation-phase per-extract entity counts above ~20; production tracing showing the upsert as a latency hotspot; attribution diagnostic value re-evaluated lower). None of those have fired. This PR proceeds anyway with a design that **preserves the attribution opportunity** the deferral was worried about, rather than overriding the original cost/benefit calculus.

The empirical observation that re-frames the trade-off: today's Layer A loop has weak per-row attribution too (`format!("upsert entity: {e}")` doesn't identify which entity in a batch tripped a constraint). The fallback path is therefore an opportunity to **add** per-row attribution where Layer A had none — `kind='person', name_norm='dr smith'` carries the failing-row identifier into the error message.

## Test plan

- [x] +10 pure unit tests in `batch_upsert::tests` (no DB)
- [x] +7 integration tests in `entity_extraction_e2e.rs` (real PG)
- [x] All 5 existing Layer A integration tests continue to pass byte-equivalently (regression pin)
- [x] `cargo test --workspace` on macOS: 1040 / 0 / 3 (+17 over 1023 baseline)
- [x] `cargo clippy --workspace --all-targets`: no new warnings
- [x] `build_extract_entities_payload` 8-key audit contract unchanged

## Spec + plan

- Spec: [docs/superpowers/specs/2026-05-25-issue-95-layer-b-design.md](docs/superpowers/specs/2026-05-25-issue-95-layer-b-design.md)
- Plan: [docs/superpowers/plans/2026-05-25-issue-95-layer-b.md](docs/superpowers/plans/2026-05-25-issue-95-layer-b.md)

Closes #95.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

- [ ] **Step 7: HANDOVER + ROADMAP update** (separate doc-sync slice — handled in the parent task list, not here)

---

## Self-Review

**Spec coverage:**
- ✅ Architecture (sibling module + delegate) — Task 5
- ✅ Pure helpers (`dedup_entity_inputs`, `build_entity_unnest_arrays`, `is_constraint_violation`, error formatters) — Tasks 1-4
- ✅ Entity batch happy path + dispatch — Task 5
- ✅ Entity batch order + dedup behaviour — Task 6
- ✅ Quarantine preservation — Task 7
- ✅ Entity fallback — Task 8
- ✅ Relations batch + dispatch — Task 9
- ✅ Relations fallback — Task 10
- ✅ Workspace verification + file-size + clippy — Task 11
- ✅ All 5 existing Layer A tests covered by "regression pin" comment in Tasks 5 + 11

**Placeholder scan:** None found. Every step has exact code, exact commands, expected outcomes.

**Type consistency:**
- `DedupedEntity<'a>` introduced Task 1, consumed Tasks 2, 5, 9.
- `EntityUpsertResult` type alias in Task 5 used only inside `try_batch_upsert_entities`.
- `ResolvedTriple` introduced Task 9, consumed by `try_batch_upsert_relations` + `per_row_upsert_relations`.
- `HashMap<(String, String), (i64, bool)>` shape used consistently across Tasks 5 + 9.
- `UpsertOutcome` reference path: `crate::entity_extraction::gliner_relex::UpsertOutcome` throughout.
- Function names consistent: `try_batch_upsert_entities`, `per_row_upsert_entities`, `try_batch_upsert_relations`, `per_row_upsert_relations`.

**Test-count math (defensive recount):**
- Task 1: +3 unit (total: 1026)
- Task 2: +2 unit (1028)
- Task 3: +3 unit (1031)
- Task 4: +2 unit (1033)
- Task 5: +1 integration (1034)
- Task 6: +2 integration (1036)
- Task 7: +1 integration (1037)
- Task 8: +1 integration (1038)
- Task 9: +1 integration (1039)
- Task 10: +1 integration (1040)
- Task 11: 0 (verification only)

Total: +17 = 1040. Matches spec target. ✅

**Spec-to-task mapping:**
- Spec test #1 (`dedup_entity_inputs_removes_same_key_duplicates_preserves_first_seen_order`) → Task 1 ✅
- Spec test #2 (`dedup_entity_inputs_distinct_kinds_with_same_name_norm_are_distinct`) → Task 1 ✅
- Spec test #3 (`dedup_entity_inputs_returns_empty_for_empty_input`) → Task 1 ✅
- Spec test #4 (`build_entity_unnest_arrays_emits_parallel_arrays_of_equal_length`) → Task 2 ✅
- Spec test #5 (`build_entity_unnest_arrays_handles_empty_input`) → Task 2 ✅
- Spec test #6 (`is_constraint_violation_true_for_each_23xxx_code`) → Task 3 (as `is_constraint_violation_code_true_for_each_23xxx_code` — split into pure code helper) ✅
- Spec test #7 (`is_constraint_violation_false_for_22xxx_data_exception`) → Task 3 ✅
- Spec test #8 (`is_constraint_violation_false_for_non_database_errors`) → Task 3 (collapsed into `is_constraint_violation_code_false_for_other_classes` — covers 22xxx + 08/42/40/53/57 in one test) ✅
- Spec test #9 (`format_per_row_entity_error_uses_name_norm_not_raw_name`) → Task 4 ✅
- Spec test #10 (`format_per_row_relation_error_contains_src_dst_kind`) → Task 4 ✅
- Spec test #11 (`upsert_batch_happy_path_returns_same_outcome_shape_as_layer_a`) → Task 5 ✅
- Spec test #12 (`upsert_batch_preserves_entity_id_order_for_unique_inputs`) → Task 6 ✅
- Spec test #13 (`upsert_batch_dedup_input_returns_same_id_for_duplicates`) → Task 6 ✅
- Spec test #14 (`upsert_batch_falls_back_to_per_row_on_entity_kind_fk_violation`) → Task 8 ✅
- Spec test #15 (`upsert_batch_falls_back_to_per_row_on_relation_kind_fk_violation`) → Task 10 ✅
- Spec test #16 (`upsert_batch_preserves_operator_unquarantine_decision`) → Task 7 ✅
- Spec test #17 (`upsert_batch_skips_triples_referencing_unknown_entities`) → Task 9 (folded into `upsert_batch_relations_inserts_dedups_and_skips_unknown_entities` along with the happy path + idempotency assertions) ✅

All 17 spec tests covered. ✅

**Risk coverage:**
- Risk #1 (sqlx array binding): Task 5 explicitly uses the `db/src/memories.rs:442` pattern (`unnest($1::bigint[])` + `.bind(slice)`). If binding fails, Task 5 Step 2 surfaces it as a compile/runtime error before anything is committed.
- Risk #2 (`xmax = 0` under unnest): Pinned by Task 5 (happy path verifies counts) + Task 6 (dedup test verifies n_new behaviour).
- Risk #3 (empty input): Pinned by Tasks 1 + 2 unit tests for both helpers; Task 5's empty-input guard (`if deduped.is_empty()`); Task 9's empty-input guard.
- Risk #4 (race against operator vocabulary changes): Tasks 8 + 10 simulate by deleting kind rows mid-test.
- Risk #5 (PII in error messages): Task 4 unit test pins `name_norm` (lowercased) not raw name in the format string.

Plan complete and saved to `docs/superpowers/plans/2026-05-25-issue-95-layer-b.md`.
