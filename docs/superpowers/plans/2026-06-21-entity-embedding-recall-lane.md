# Entity-embedding backfill + entity-similarity recall lane — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Populate `entities.embedding` via a backfill CLI and consume it through a fourth recall lane that surfaces memories linked to query-similar entities.

**Architecture:** Bottom-up across four layers — three new `db` SQL helpers in a dedicated `entity_embedding.rs` module; a shared `ReembedReport` lifted into `core/src/memory/reembed.rs`; a `core` backfill (`entity_reembed.rs`) mirroring the L1 backfill; a new `entity` lane wired into `recall`; and a `kastellan-cli entities reembed` action. Reuses the existing `Embedder` seam and RRF fusion unchanged.

**Tech Stack:** Rust, sqlx (runtime query strings), pgvector (`vector(256)`, cosine `<=>`), tokio, async-trait.

**Spec:** `docs/superpowers/specs/2026-06-21-entity-embedding-recall-lane-design.md`

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new dependencies are introduced by this plan.
- **Cross-platform Linux + macOS.** All code here is pure-Rust and OS-agnostic (no sandbox/seccomp/Landlock). No migration.
- **`EMBEDDING_DIM = 256`** (Matryoshka). Never hardcode 1024/768. Vectors flow through the existing `truncate_to_embedding_dim`/`check_embedding_dim` chokepoints — do not re-truncate.
- **`SandboxPolicy.fs_*` / sandbox** untouched. **DGX not required** as an acceptance gate — macOS live PG 18 exercises the same pgvector SQL.
- **Keep files under 500 LOC.** New modules are sized well under cap.
- **TDD, frequent commits, pure functions in reusable modules.** Inline docs understandable to a junior contributor are mandatory.
- **Build/test prelude (every command):** `source "$HOME/.cargo/env"` first.
- **Subagent commits stage specific files** — `git add <listed files>`, never `git add -A` (untracked drafts + `.claude/*.lock` must stay out).

---

### Task 1: `db` — entity-embedding scan + guarded write helpers

**Files:**
- Create: `db/src/entity_embedding.rs`
- Modify: `db/src/lib.rs` (add `pub mod entity_embedding;` next to the other `pub mod` lines, ~line 33)
- Modify: `db/src/memories.rs` (bump two private helpers to `pub(crate)` so the new module reuses the exact chokepoints)

**Interfaces:**
- Consumes: `crate::DbError`; `crate::memories::{check_embedding_dim, limit_as_i64, vector_literal, EMBEDDING_DIM}`.
- Produces:
  - `kastellan_db::entity_embedding::load_unembedded_entities(executor) -> Result<Vec<(i64, String, String)>, DbError>` — `(id, kind, name)` for every `embedding IS NULL` entity, id-ordered.
  - `kastellan_db::entity_embedding::set_entity_embedding(executor, id: i64, embedding: &[f32]) -> Result<bool, DbError>` — guarded UPDATE; `true` iff one row written.

- [ ] **Step 1: Bump the two memories helpers to `pub(crate)`**

In `db/src/memories.rs`, change the two signatures (currently bare `fn`) so the new sibling module can reuse them:

```rust
// was: fn check_embedding_dim(label: &str, v: &[f32]) -> Result<(), DbError> {
pub(crate) fn check_embedding_dim(label: &str, v: &[f32]) -> Result<(), DbError> {
```

```rust
// was: fn limit_as_i64(k: usize) -> i64 {
pub(crate) fn limit_as_i64(k: usize) -> i64 {
```

(`vector_literal` and `EMBEDDING_DIM` are already `pub`.)

- [ ] **Step 2: Register the new module**

In `db/src/lib.rs`, add the module declaration alphabetically among the existing `pub mod` block (after `pub mod entity_name;`, before `pub mod graph;`):

```rust
pub mod entity_embedding;
```

- [ ] **Step 3: Write the failing unit test**

Create `db/src/entity_embedding.rs` with only the module docs + the test module:

```rust
//! Read/write helpers for the `entities.embedding` column — the
//! entity-embedding **backfill** scan + guarded updater, and the
//! entity-similarity recall lane (issue: entity-embedding recall lane).
//!
//! Co-located here (rather than in the over-cap `entities.rs` /
//! `memories/search.rs`) so all three entity-embedding SQL helpers share
//! one focused, testable module. Every helper reuses the same dimension
//! chokepoint (`check_embedding_dim`) and `vector(256)` literal encoder
//! (`vector_literal`) the memories lane uses, so a backfilled entity vector
//! is byte-identical to what a future forward path would store.

use sqlx::Row;

use crate::memories::{check_embedding_dim, limit_as_i64, vector_literal, EMBEDDING_DIM};
use crate::DbError;

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_entity_embedding` rejects a wrong-dimension vector *before* any
    /// I/O — the dim contract is a hard gate, not a degrade case. A lazy
    /// (never-connected) pool proves no round-trip happens on the reject path.
    #[tokio::test]
    async fn set_entity_embedding_rejects_wrong_dim() {
        let pool = sqlx::PgPool::connect_lazy(
            "postgres://invalid:invalid@127.0.0.1:1/nonexistent",
        )
        .expect("lazy pool");
        // EMBEDDING_DIM - 1 components: too short, must be rejected up front.
        let short = vec![0.0f32; EMBEDDING_DIM - 1];
        let err = set_entity_embedding(&pool, 1, &short).await;
        assert!(err.is_err(), "wrong-dim vector must be rejected before I/O");
    }
}
```

- [ ] **Step 4: Run the test to verify it fails (compile error — fn not defined)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-db --lib entity_embedding 2>&1 | tail -20`
Expected: FAIL — `cannot find function set_entity_embedding`.

- [ ] **Step 5: Implement `load_unembedded_entities` + `set_entity_embedding`**

Insert above the `#[cfg(test)]` block in `db/src/entity_embedding.rs`:

```rust
/// Scan every entity whose `embedding IS NULL`, returning `(id, kind, name)`
/// in ascending-id (stable, resumable) order.
///
/// Returns **all** NULL-embedding entities regardless of `quarantine`:
/// embedding is independent of review state (a quarantined entity may later
/// be approved, and we must not re-embed on approve), and embedding a
/// quarantined row leaks nothing — the recall lane filters quarantined rows
/// at query time. The caller composes the embed text from `(kind, name)`.
pub async fn load_unembedded_entities<'e, E>(
    executor: E,
) -> Result<Vec<(i64, String, String)>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT id, kind, name \
         FROM entities \
         WHERE embedding IS NULL \
         ORDER BY id",
    )
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("load_unembedded_entities: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: i64 = r
            .try_get(0)
            .map_err(|e| DbError::Query(format!("decode entity.id: {e}")))?;
        let kind: String = r
            .try_get(1)
            .map_err(|e| DbError::Query(format!("decode entity.kind: {e}")))?;
        let name: String = r
            .try_get(2)
            .map_err(|e| DbError::Query(format!("decode entity.name: {e}")))?;
        out.push((id, kind, name));
    }
    Ok(out)
}

/// Write `embedding` for entity `id`, but **only if it is still NULL**.
///
/// The `embedding IS NULL` guard makes the write idempotent + race-safe: a
/// row embedded concurrently (by a parallel backfill, or a future forward
/// path) no-ops and returns `false`. Returns `true` iff exactly one row was
/// updated. Dimension-checked before the write — a wrong-width vector is a
/// hard `DbError`, never silently stored. Byte-for-byte mirror of
/// `memories::set_embedding`.
pub async fn set_entity_embedding<'e, E>(
    executor: E,
    id: i64,
    embedding: &[f32],
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    check_embedding_dim("set_entity_embedding", embedding)?;

    let lit = vector_literal(embedding);
    let res = sqlx::query(
        "UPDATE entities \
         SET embedding = $1::vector \
         WHERE id = $2 AND embedding IS NULL",
    )
    .bind(lit)
    .bind(id)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("set_entity_embedding id={id}: {e}")))?;
    Ok(res.rows_affected() == 1)
}
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-db --lib entity_embedding 2>&1 | tail -20`
Expected: PASS — `set_entity_embedding_rejects_wrong_dim` green.

- [ ] **Step 7: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-db --all-targets -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add db/src/entity_embedding.rs db/src/lib.rs db/src/memories.rs
git commit -m "feat(db): entity-embedding scan + guarded write helpers

New db::entity_embedding module: load_unembedded_entities (id,kind,name
scan of NULL-embedding rows) + set_entity_embedding (guarded, race-safe
UPDATE). Reuses the memories dim-check + vector(256) literal chokepoints
(bumped to pub(crate)).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `db` — `entity_similarity_search` lane query

**Files:**
- Modify: `db/src/entity_embedding.rs`

**Interfaces:**
- Consumes: `crate::memories::{check_embedding_dim, limit_as_i64, vector_literal}`; tables `entities`, `memory_entities`.
- Produces: `kastellan_db::entity_embedding::entity_similarity_search(executor, query_embedding: &[f32], entity_fanout: i64, k: usize, include_quarantined: bool) -> Result<Vec<i64>, DbError>` — up to `k` memory ids, best-first.

- [ ] **Step 1: Write the failing unit test**

Add to the `tests` module in `db/src/entity_embedding.rs`:

```rust
    /// `k == 0` is a fast-path no-op: returns empty without issuing SQL, so a
    /// lazy pool never connects. (The behaviour against real rows is covered
    /// by `core/tests/entity_reembed_e2e.rs`.)
    #[tokio::test]
    async fn entity_similarity_search_k_zero_is_empty_no_sql() {
        let pool = sqlx::PgPool::connect_lazy(
            "postgres://invalid:invalid@127.0.0.1:1/nonexistent",
        )
        .expect("lazy pool");
        let q = vec![0.0f32; EMBEDDING_DIM];
        let out = entity_similarity_search(&pool, &q, 64, 0, false)
            .await
            .expect("k==0 returns Ok(empty) with no round-trip");
        assert!(out.is_empty());
    }

    /// A wrong-dimension query embedding is rejected before any I/O (mirrors
    /// the semantic lane's hard dim contract).
    #[tokio::test]
    async fn entity_similarity_search_rejects_wrong_dim() {
        let pool = sqlx::PgPool::connect_lazy(
            "postgres://invalid:invalid@127.0.0.1:1/nonexistent",
        )
        .expect("lazy pool");
        let short = vec![0.0f32; EMBEDDING_DIM - 1];
        assert!(
            entity_similarity_search(&pool, &short, 64, 10, false).await.is_err(),
            "wrong-dim query embedding must be rejected before I/O"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails (fn not defined)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-db --lib entity_embedding 2>&1 | tail -20`
Expected: FAIL — `cannot find function entity_similarity_search`.

- [ ] **Step 3: Implement `entity_similarity_search`**

Add to `db/src/entity_embedding.rs` (above the `#[cfg(test)]` block):

```rust
/// Entity-similarity recall lane: the memories linked to the entities nearest
/// the query embedding.
///
/// Two stages in one statement: (1) the `entity_fanout` entities with the
/// smallest cosine distance (`<=>`) to `query_embedding`, restricted to
/// embedded, non-quarantined rows (unless `include_quarantined`); (2) the
/// memories linked to those entities via `memory_entities`, ranked by each
/// memory's *closest* matching entity (`MIN(dist)`), id-tiebroken for stable
/// order, capped at `k`.
///
/// `include_quarantined = false` is the production posture — it preserves the
/// invariant that operator-unreviewed entities never surface memories into
/// recall (mirrors `memories::graph_search`). The operator CLI may pass
/// `true`.
///
/// `k == 0` → empty, no SQL. `query_embedding.len()` must equal
/// `EMBEDDING_DIM` (hard `DbError`, not a degrade case). An empty result
/// (no embedded/approved entities yet) is normal — the lane simply
/// contributes nothing to fusion.
pub async fn entity_similarity_search<'e, E>(
    executor: E,
    query_embedding: &[f32],
    entity_fanout: i64,
    k: usize,
    include_quarantined: bool,
) -> Result<Vec<i64>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if k == 0 {
        return Ok(Vec::new());
    }
    check_embedding_dim("entity query", query_embedding)?;

    let lit = vector_literal(query_embedding);
    let rows = sqlx::query(
        "SELECT me.memory_id \
         FROM ( \
             SELECT id, embedding <=> $1::vector AS dist \
             FROM entities \
             WHERE embedding IS NOT NULL \
               AND ($4 OR quarantine = FALSE) \
             ORDER BY dist \
             LIMIT $2 \
         ) top_e \
         JOIN memory_entities me ON me.entity_id = top_e.id \
         GROUP BY me.memory_id \
         ORDER BY MIN(top_e.dist) ASC, me.memory_id ASC \
         LIMIT $3",
    )
    .bind(lit)
    .bind(entity_fanout)
    .bind(limit_as_i64(k))
    .bind(include_quarantined)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("entity_similarity_search: {e}")))?;

    rows.into_iter()
        .map(|r| {
            r.try_get::<i64, _>(0)
                .map_err(|e| DbError::Query(format!("decode memory_id: {e}")))
        })
        .collect()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-db --lib entity_embedding 2>&1 | tail -20`
Expected: PASS — all three `entity_embedding` unit tests green.

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-db --all-targets -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add db/src/entity_embedding.rs
git commit -m "feat(db): entity_similarity_search recall-lane query

Top-N entities nearest the query embedding (cosine <=>), then their
linked memories ranked by closest matching entity. Quarantine-filtered
by default; mirrors graph_search's include_quarantined seam.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `core` — lift the shared `ReembedReport` into `memory/reembed.rs`

**Files:**
- Create: `core/src/memory/reembed.rs`
- Modify: `core/src/memory/l1_reembed.rs` (delete the moved items + their tests, import from `super::reembed`)
- Modify: `core/src/memory/mod.rs` (add `pub mod reembed;`, re-point the re-export)

**Interfaces:**
- Produces: `kastellan_core::memory::reembed::{ReembedReport, format_reembed_report, reembed_batch_failed}` — re-exported from the `memory` facade so the existing public paths `kastellan_core::memory::{ReembedReport, format_reembed_report, reembed_batch_failed}` are unchanged.
- Consumes (l1_reembed, entity_reembed): the same three items via `crate::memory::reembed::*`.

This is a pure refactor — no behaviour change. Tests move with the code.

- [ ] **Step 1: Create `core/src/memory/reembed.rs` with the moved items + tests**

```rust
//! Shared outcome type + pure helpers for the embedding **backfill**
//! workflows (`l1_reembed`, `entity_reembed`). One report shape — scanned /
//! embedded / skipped — describes both, so it lives here rather than in
//! either backfill module.

/// Outcome of a backfill batch.
///
/// Invariant: `embedded + skipped == scanned`. `scanned` is the number of
/// NULL-embedding rows the scan found; `embedded` actually wrote a vector;
/// `skipped` covers every row that did not get embedded (embed declined/
/// failed, a concurrent write won the `IS NULL` guard, or a per-row write
/// error) — none of which fail the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReembedReport {
    /// NULL-embedding rows found by the scan.
    pub scanned: usize,
    /// Rows whose embedding was written this run.
    pub embedded: usize,
    /// Rows scanned but not embedded (degrade-and-warn; not batch failures).
    pub skipped: usize,
}

/// True when a batch found NULL-embedding rows to embed but embedded **none**
/// — `scanned > 0 && embedded == 0`. Equivalent to "every scanned row was
/// skipped" (since `embedded + skipped == scanned`): a total failure,
/// typically an unreachable embed endpoint.
///
/// Distinguished from the idempotent no-op (`scanned == 0`), which is *not* a
/// failure. The CLI maps this to a non-zero exit code so a scripted
/// `reembed && next-step` chain does not treat a wholly-failed backfill as
/// success; the backfill loops use it to emit an aggregate WARN.
pub fn reembed_batch_failed(report: &ReembedReport) -> bool {
    report.scanned > 0 && report.embedded == 0
}

/// Render a [`ReembedReport`] as the one-line operator summary
/// `scanned=<n> embedded=<n> skipped=<n>`. Pure — the CLI prints this to
/// stdout; keeping it a function (not an inline `println!`) makes the exact
/// wording test-pinnable and reusable across backfills.
pub fn format_reembed_report(report: &ReembedReport) -> String {
    format!(
        "scanned={} embedded={} skipped={}",
        report.scanned, report.embedded, report.skipped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report's documented invariant holds for a hand-built value.
    #[test]
    fn report_parts_sum_to_scanned() {
        let r = ReembedReport { scanned: 5, embedded: 3, skipped: 2 };
        assert_eq!(r.embedded + r.skipped, r.scanned);
    }

    /// The operator-facing one-line summary is stable and greppable.
    #[test]
    fn format_reembed_report_is_stable_one_line() {
        let r = ReembedReport { scanned: 7, embedded: 5, skipped: 2 };
        assert_eq!(format_reembed_report(&r), "scanned=7 embedded=5 skipped=2");
    }

    /// The empty backfill (nothing to do) renders all-zeros, not a blank line.
    #[test]
    fn format_reembed_report_empty_batch() {
        let r = ReembedReport { scanned: 0, embedded: 0, skipped: 0 };
        assert_eq!(format_reembed_report(&r), "scanned=0 embedded=0 skipped=0");
    }

    /// The idempotent no-op (nothing scanned) is **not** a failure.
    #[test]
    fn reembed_batch_failed_false_for_empty_scan() {
        let r = ReembedReport { scanned: 0, embedded: 0, skipped: 0 };
        assert!(!reembed_batch_failed(&r));
    }

    /// Any embedded row means progress — not a failure, even with some skips.
    #[test]
    fn reembed_batch_failed_false_when_any_embedded() {
        let all = ReembedReport { scanned: 3, embedded: 3, skipped: 0 };
        let partial = ReembedReport { scanned: 5, embedded: 3, skipped: 2 };
        assert!(!reembed_batch_failed(&all));
        assert!(!reembed_batch_failed(&partial));
    }

    /// Rows scanned but none embedded (every row skipped) is the total-failure
    /// signal the CLI maps to a non-zero exit code.
    #[test]
    fn reembed_batch_failed_true_when_all_skipped() {
        let r = ReembedReport { scanned: 4, embedded: 0, skipped: 4 };
        assert!(reembed_batch_failed(&r));
    }
}
```

- [ ] **Step 2: Delete the moved items from `core/src/memory/l1_reembed.rs`**

Remove from `l1_reembed.rs`: the `ReembedReport` struct definition (and its doc comment), the `reembed_batch_failed` fn (and doc), the `format_reembed_report` fn (and doc), and the four moved unit tests in its `mod tests` block (`report_parts_sum_to_scanned`, `format_reembed_report_is_stable_one_line`, `format_reembed_report_empty_batch`, `reembed_batch_failed_false_for_empty_scan`, `reembed_batch_failed_false_when_any_embedded`, `reembed_batch_failed_true_when_all_skipped`). **Keep** `reembed_l1_null` and its `reembed_l1_null_signature_compile_pin` test.

Then replace the top-of-file `use` for db items so the report comes from the new module. Change:

```rust
use kastellan_db::memories::{load_unembedded_at_layer, set_embedding, MemoryLayer};
use kastellan_db::DbError;
use sqlx::PgPool;

use crate::memory::embedder::Embedder;
```

to:

```rust
use kastellan_db::memories::{load_unembedded_at_layer, set_embedding, MemoryLayer};
use kastellan_db::DbError;
use sqlx::PgPool;

use crate::memory::embedder::Embedder;
use crate::memory::reembed::{reembed_batch_failed, ReembedReport};
```

(`reembed_l1_null`'s body already calls `reembed_batch_failed(&report)` and returns `ReembedReport` — now resolved via the import. `format_reembed_report` is not referenced inside `l1_reembed.rs`.)

- [ ] **Step 3: Update `core/src/memory/mod.rs` module decl + re-export**

Add the module declaration (next to the other backfill modules, after `pub mod l1_reembed;`):

```rust
pub mod reembed;
```

Change the existing facade re-export line:

```rust
pub use l1_reembed::{format_reembed_report, reembed_batch_failed, reembed_l1_null, ReembedReport};
```

to:

```rust
pub use l1_reembed::reembed_l1_null;
pub use reembed::{format_reembed_report, reembed_batch_failed, ReembedReport};
```

- [ ] **Step 4: Run the moved tests + the l1 pin to verify green (no behaviour change)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib "memory::reembed" "memory::l1_reembed" 2>&1 | tail -25`
Expected: PASS — the six report tests now under `memory::reembed`, plus `l1_reembed`'s signature pin; no test lost.

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add core/src/memory/reembed.rs core/src/memory/l1_reembed.rs core/src/memory/mod.rs
git commit -m "refactor(memory): lift shared ReembedReport into memory/reembed.rs

Pure move so the entity backfill can reuse the report type + the
batch-failed/format helpers. Public paths kastellan_core::memory::{
ReembedReport, format_reembed_report, reembed_batch_failed} unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `core` — entity backfill (`entity_embedding_text` + `reembed_entities_null`)

**Files:**
- Create: `core/src/memory/entity_reembed.rs`
- Modify: `core/src/memory/mod.rs` (add `pub mod entity_reembed;` + facade re-export)

**Interfaces:**
- Consumes: `kastellan_db::entity_embedding::{load_unembedded_entities, set_entity_embedding}` (Task 1); `crate::memory::embedder::Embedder`; `crate::memory::reembed::{ReembedReport, reembed_batch_failed}` (Task 3).
- Produces:
  - `kastellan_core::memory::entity_reembed::entity_embedding_text(kind: &str, name: &str) -> String`
  - `kastellan_core::memory::entity_reembed::reembed_entities_null(pool: &PgPool, embedder: &dyn Embedder) -> Result<ReembedReport, DbError>`

- [ ] **Step 1: Write the failing unit tests**

Create `core/src/memory/entity_reembed.rs`:

```rust
//! Entity-embedding **backfill** — `kastellan-cli entities reembed`.
//!
//! Every `entities.embedding` is NULL today (no write path populates it).
//! [`reembed_entities_null`] scans those rows and embeds each through the
//! injected [`Embedder`] chokepoint — the CLI injects a
//! [`crate::memory::RouterEmbedder`], so a backfilled entity vector is
//! Matryoshka-truncated to `EMBEDDING_DIM`, unit-norm, with an
//! `action='embed'` audit row per call, exactly like the L1 path.
//!
//! ## What gets embedded
//!
//! [`entity_embedding_text`] composes the string fed to the embedder:
//! `"<kind>: <name>"` (e.g. `"person: Horst Herb"`). The `kind` prefix gives
//! the embedder type context and disambiguates same-named entities of
//! different kinds. It is the single source of truth for entity embed text,
//! so a future forward (embed-on-insert) path embeds identically.
//!
//! ## Safety / idempotency
//!
//! Safe to re-run: the scan only returns `embedding IS NULL` rows and the
//! write ([`set_entity_embedding`]) re-asserts `embedding IS NULL`, so a row
//! embedded by a prior run or a concurrent writer no-ops. A per-row embed
//! failure **skips that row** (degrade-and-warn) rather than failing the
//! batch — mirrors [`crate::memory::l1_reembed::reembed_l1_null`].

use kastellan_db::entity_embedding::{load_unembedded_entities, set_entity_embedding};
use kastellan_db::DbError;
use sqlx::PgPool;

use crate::memory::embedder::Embedder;
use crate::memory::reembed::{reembed_batch_failed, ReembedReport};

#[cfg(test)]
mod tests {
    use super::*;

    /// The embed text is `"<kind>: <name>"` — the exact contract a future
    /// forward path must match so backfilled + on-insert vectors agree.
    #[test]
    fn entity_embedding_text_is_kind_colon_name() {
        assert_eq!(entity_embedding_text("person", "Horst Herb"), "person: Horst Herb");
    }

    /// Empty kind still produces a deterministic, non-panicking string.
    #[test]
    fn entity_embedding_text_handles_empty_kind() {
        assert_eq!(entity_embedding_text("", "x"), ": x");
    }

    /// Unicode names pass through unchanged (no normalization here — that is
    /// the extractor's job; this is purely the embed-text shape).
    #[test]
    fn entity_embedding_text_passes_through_unicode() {
        assert_eq!(entity_embedding_text("place", "München"), "place: München");
    }

    /// Compile-pin the public signature (mirrors the `reembed_l1_null` pin):
    /// `&PgPool` + `&dyn Embedder` → `Result<ReembedReport, DbError>`. The
    /// behaviour is exercised by `core/tests/entity_reembed_e2e.rs`.
    #[allow(dead_code)]
    fn reembed_entities_null_signature_compile_pin() {
        fn _assert<'a>(
            pool: &'a PgPool,
            embedder: &'a dyn Embedder,
        ) -> impl std::future::Future<Output = Result<ReembedReport, DbError>> + 'a {
            reembed_entities_null(pool, embedder)
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (fn not defined)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib "memory::entity_reembed" 2>&1 | tail -20`
Expected: FAIL — `cannot find function entity_embedding_text` (plus the module isn't declared yet; do Step 3's mod.rs edit first if the crate won't compile to run the test — see note).

> Note: declaring the module (Step 4) is required for the file to compile at all. If you prefer strict red-green, add `pub mod entity_reembed;` to `mod.rs` first (without the re-export), confirm the FAIL is "fn not defined", then implement.

- [ ] **Step 3: Implement `entity_embedding_text` + `reembed_entities_null`**

Insert above the `#[cfg(test)]` block in `core/src/memory/entity_reembed.rs`:

```rust
/// Compose the text embedded for an entity: `"<kind>: <name>"`. Pure; the
/// single source of truth for entity embed text (see module docs).
pub fn entity_embedding_text(kind: &str, name: &str) -> String {
    format!("{kind}: {name}")
}

/// Embed every entity whose `embedding IS NULL`, writing each vector back
/// through the guarded [`set_entity_embedding`] updater.
///
/// Per-row degrade-and-warn: a `None` from the embedder (transient failure or
/// an intentional skip — the [`crate::memory::RouterEmbedder`] logs the
/// WARN), a lost race on the `IS NULL` guard, or a write error all count as
/// `skipped` and the loop continues. The only `Err` returned is a failure of
/// the **initial scan** ([`load_unembedded_entities`]) — there is nothing to
/// back-fill if we cannot even read the work-list.
pub async fn reembed_entities_null(
    pool: &PgPool,
    embedder: &dyn Embedder,
) -> Result<ReembedReport, DbError> {
    let rows = load_unembedded_entities(pool).await?;
    let scanned = rows.len();
    let mut embedded = 0usize;
    let mut skipped = 0usize;

    for (id, kind, name) in rows {
        let text = entity_embedding_text(&kind, &name);
        match embedder.embed_for_storage(&text).await {
            Some(vector) => match set_entity_embedding(pool, id, &vector).await {
                Ok(true) => embedded += 1,
                // The `IS NULL` guard no-op'd: embedded concurrently or the
                // row vanished between scan and update. Not an error.
                Ok(false) => {
                    tracing::warn!(
                        target: "kastellan::memory",
                        entity_id = id,
                        "entity reembed: row no longer NULL at update time; skipped"
                    );
                    skipped += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "kastellan::memory",
                        entity_id = id,
                        error = %e,
                        "entity reembed: embedding write failed; row left NULL, skipped"
                    );
                    skipped += 1;
                }
            },
            // Embed declined/failed (the RouterEmbedder logged the WARN).
            None => skipped += 1,
        }
    }

    let report = ReembedReport { scanned, embedded, skipped };

    // Aggregate signal: rows were scanned but none embedded — typically an
    // unreachable embed endpoint. The per-row `None` path can't WARN
    // generically, so surface it at the batch level.
    if reembed_batch_failed(&report) {
        tracing::warn!(
            target: "kastellan::memory",
            scanned = report.scanned,
            skipped = report.skipped,
            "entity reembed: all scanned rows skipped, none embedded — embed endpoint may be unreachable"
        );
    }

    Ok(report)
}
```

- [ ] **Step 4: Declare the module + facade re-export in `core/src/memory/mod.rs`**

Add the module declaration (after `pub mod entity_link;` or near the other backfill modules):

```rust
pub mod entity_reembed;
```

Add to the facade re-exports (next to the `reembed` re-export from Task 3):

```rust
pub use entity_reembed::{entity_embedding_text, reembed_entities_null};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib "memory::entity_reembed" 2>&1 | tail -20`
Expected: PASS — the three `entity_embedding_text` tests green; the signature pin compiles.

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add core/src/memory/entity_reembed.rs core/src/memory/mod.rs
git commit -m "feat(memory): entity-embedding backfill (reembed_entities_null)

Scans NULL-embedding entities, embeds '<kind>: <name>' through the
injected Embedder, writes back via the guarded set_entity_embedding.
Per-row degrade-and-warn, reuses the shared ReembedReport. Pure
entity_embedding_text is the single source of truth for embed text.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `core` — wire the `entity` recall lane

**Files:**
- Modify: `core/src/memory/recall.rs` (RecallModes field + presets + const + lane block + all-skipped error message)
- Modify: `core/src/memory/recall/tests.rs` (update preset/constructor assertions, add new pins)

**Interfaces:**
- Consumes: `kastellan_db::entity_embedding::entity_similarity_search` (Task 2); existing `EMBEDDING_DIM`, `RRF_K_CONSTANT`, `reciprocal_rank_fusion`.
- Produces: `RecallModes { semantic, lexical, graph, entity }`; `RecallModes::{ALL, ENTITY_ONLY, SEMANTIC_LEXICAL_ENTITY}`; `ENTITY_SIMILARITY_FANOUT: i64`. `RecallParams::new` now enables the entity lane.

- [ ] **Step 1: Add the `entity` field + presets + const (write the failing test first)**

Add to `core/src/memory/recall/tests.rs` (near the existing mode pins):

```rust
/// `RecallModes::ALL` now enables the fourth (entity-similarity) lane. If a
/// future fifth lane lands without updating `ALL`, this trips loudly.
#[allow(clippy::assertions_on_constants)]
#[test]
fn recall_modes_all_includes_entity() {
    assert!(RecallModes::ALL.entity);
    assert!(RecallModes::ALL.graph);
    assert!(RecallModes::ALL.semantic);
    assert!(RecallModes::ALL.lexical);
}

/// `RecallModes::ENTITY_ONLY` exact shape pin.
#[test]
fn recall_modes_entity_only_is_only_entity() {
    let m = RecallModes::ENTITY_ONLY;
    assert!(m.entity);
    assert!(!m.semantic);
    assert!(!m.lexical);
    assert!(!m.graph);
}

/// Pin `ENTITY_SIMILARITY_FANOUT = 64` so a future tune is an explicit PR.
#[test]
fn entity_similarity_fanout_is_sixty_four() {
    assert_eq!(ENTITY_SIMILARITY_FANOUT, 64);
}
```

- [ ] **Step 2: Run to verify it fails (no `entity` field / no consts)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib "memory::recall" 2>&1 | tail -20`
Expected: FAIL — `no field 'entity'` / `no associated item ENTITY_ONLY` / `cannot find value ENTITY_SIMILARITY_FANOUT`.

- [ ] **Step 3: Add the field to `RecallModes` + update every preset**

In `core/src/memory/recall.rs`, add the field to the struct (after `graph`):

```rust
    /// Run the entity-similarity lane: embed-nearest entities (via
    /// [`kastellan_db::entity_embedding::entity_similarity_search`]) →
    /// their linked memories. Requires `query_embedding` (the same input the
    /// semantic lane uses); needs no seeds.
    pub entity: bool,
```

Update the existing associated consts to set `entity` explicitly:

```rust
    pub const ALL: RecallModes = RecallModes {
        semantic: true,
        lexical: true,
        graph: true,
        entity: true,
    };

    pub const SEMANTIC_ONLY: RecallModes = RecallModes {
        semantic: true,
        lexical: false,
        graph: false,
        entity: false,
    };

    pub const LEXICAL_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: true,
        graph: false,
        entity: false,
    };

    pub const GRAPH_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: false,
        graph: true,
        entity: false,
    };

    pub const SEMANTIC_AND_LEXICAL: RecallModes = RecallModes {
        semantic: true,
        lexical: true,
        graph: false,
        entity: false,
    };
```

Add two new presets after `SEMANTIC_AND_LEXICAL`:

```rust
    /// Semantic + lexical + entity, graph off — the no-seeds default used by
    /// [`RecallParams::new`]. The entity lane needs only `query_embedding`
    /// (which `new` supplies), so it runs even without graph seeds; it is most
    /// valuable here, where the graph lane is off.
    pub const SEMANTIC_LEXICAL_ENTITY: RecallModes = RecallModes {
        semantic: true,
        lexical: true,
        graph: false,
        entity: true,
    };

    /// Run only the entity-similarity lane.
    pub const ENTITY_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: false,
        graph: false,
        entity: true,
    };
```

Add the fan-out constant near `GRAPH_FANOUT_CAP_PER_SEED`:

```rust
/// How many nearest entities the entity-similarity lane considers before
/// joining to their memories. Bounded + generous, analogous to
/// [`GRAPH_FANOUT_CAP_PER_SEED`]: a query close to many entities still pulls a
/// finite candidate set. Tuning is a follow-up if measurement shows it matters.
pub const ENTITY_SIMILARITY_FANOUT: i64 = 64;
```

- [ ] **Step 4: Point `RecallParams::new` at the new default + add the lane block**

In `RecallParams::new`, change the `modes` field:

```rust
            modes: RecallModes::SEMANTIC_LEXICAL_ENTITY,
```

(Leave `with_seeds` on `RecallModes::ALL`.)

In `recall`, after the `if params.modes.graph { … }` block and before the `if lane_lists.is_empty()` guard, add the entity lane (mirrors the semantic lane's input handling):

```rust
    if params.modes.entity {
        any_enabled = true;
        match params.query_embedding {
            Some(emb) if emb.len() == EMBEDDING_DIM => {
                lane_lists.push(
                    kastellan_db::entity_embedding::entity_similarity_search(
                        pool,
                        emb,
                        ENTITY_SIMILARITY_FANOUT,
                        lane_k,
                        false,
                    )
                    .await?,
                );
            }
            Some(_) => {
                return Err(DbError::Query(format!(
                    "entity lane: embedding dim must be {EMBEDDING_DIM}"
                )));
            }
            None => {
                tracing::warn!(
                    target: "kastellan::memory",
                    "entity lane requested but query_embedding is None; skipping"
                );
            }
        }
    }
```

Update the all-skipped error message to mention the entity lane. Change the format string in the `if lane_lists.is_empty()` block to:

```rust
        return Err(DbError::Query(format!(
            "recall: no lanes ran (any_enabled={any_enabled}); \
             at least one enabled lane must have its required input — \
             semantic needs query_embedding, lexical needs non-empty query_text, \
             graph needs non-empty seed_entity_ids, entity needs query_embedding"
        )));
```

- [ ] **Step 5: Fix the existing constructor/preset test that asserted the old default**

In `core/src/memory/recall/tests.rs`, update `recall_params_new_default_is_semantic_and_lexical_no_seeds` (it asserted `new()` uses `SEMANTIC_AND_LEXICAL`). Replace its body assertions with:

```rust
#[test]
fn recall_params_new_default_enables_entity_no_graph_no_seeds() {
    let emb: Vec<f32> = vec![0.0; super::EMBEDDING_DIM];
    let params = RecallParams::new("query text", &emb);
    assert!(params.seed_entity_ids.is_none());
    assert_eq!(params.modes, RecallModes::SEMANTIC_LEXICAL_ENTITY);
    // The entity lane is on (no seeds needed); graph stays OFF — re-enabling
    // graph in new() would warn-and-skip on every no-seed call. #40 pin.
    assert!(params.modes.entity);
    assert!(params.modes.semantic);
    assert!(params.modes.lexical);
    assert!(!params.modes.graph);
}
```

(Leave `recall_modes_semantic_and_lexical_is_two_text_lanes`, `recall_modes_all_includes_graph`, and `recall_params_with_seeds_enables_all_three_lanes` as-is — `ALL` still has graph+semantic+lexical true, and `SEMANTIC_AND_LEXICAL` still has entity false. Optionally extend `recall_params_with_seeds_enables_all_three_lanes` with `assert!(params.modes.entity);` since `ALL` now enables it.)

- [ ] **Step 6: Run the recall unit suite to verify green**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib "memory::recall" 2>&1 | tail -25`
Expected: PASS — new entity pins green; updated default-constructor test green; no regressions.

- [ ] **Step 7: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add core/src/memory/recall.rs core/src/memory/recall/tests.rs
git commit -m "feat(memory): entity-similarity recall lane

Fourth lane: query embedding -> nearest entities -> their linked
memories, RRF-fused. RecallModes gains entity; ALL + the new no-seeds
default (SEMANTIC_LEXICAL_ENTITY, used by RecallParams::new) enable it,
so the lane runs on the common cli_ask path. Quarantine-filtered in
production. ENTITY_SIMILARITY_FANOUT=64.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: `core` — live PG e2e (`entity_reembed_e2e`)

**Files:**
- Create: `core/tests/entity_reembed_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_core::memory::entity_reembed::reembed_entities_null`; `kastellan_core::memory::reembed::ReembedReport`; `kastellan_db::entity_embedding::{entity_similarity_search, load_unembedded_entities}`; `kastellan_db::memories::{insert_memory, link_memory_to_entities, EMBEDDING_DIM}`; `kastellan_db::graph::PgGraph` (`upsert_entity`); `kastellan_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix}`.

This mirrors `core/tests/memory_l1_reembed_e2e.rs` (read it for the cluster/skip/runtime boilerplate). Proves: (1) backfill populates an entity embedding and the lane then surfaces its linked memory; (2) idempotent re-run embeds nothing; (3) degrade-and-warn leaves the row NULL; (4) the lane excludes quarantined entities.

- [ ] **Step 1: Write the e2e file**

```rust
//! End-to-end DB integration coverage for the entity-embedding **backfill** +
//! the **entity-similarity recall lane**
//! ([`kastellan_core::memory::entity_reembed::reembed_entities_null`] +
//! [`kastellan_db::entity_embedding::entity_similarity_search`]).
//!
//! `entities.embedding` is NULL for every row today. The backfill embeds each
//! `"<kind>: <name>"` through the injected `Embedder`; the lane then finds the
//! memories linked to query-similar entities. Scenarios:
//!
//!   1. backfill populates a NULL entity embedding + the lane surfaces its
//!      linked memory;
//!   2. it is idempotent — a re-run embeds nothing;
//!   3. it degrades-and-warns — a failing embed leaves the entity NULL;
//!   4. the lane excludes quarantined entities (privacy invariant).
//!
//! Each scenario brings up its own per-test Postgres cluster. Skips silently
//! with `[SKIP]` lines on hosts without Postgres or a reachable supervisor.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

use kastellan_core::memory::embedder::Embedder;
use kastellan_core::memory::entity_reembed::reembed_entities_null;
use kastellan_core::memory::reembed::ReembedReport;
use kastellan_db::entity_embedding::{entity_similarity_search, load_unembedded_entities};
use kastellan_db::graph::{Graph, PgGraph};
use kastellan_db::memories::{insert_memory, link_memory_to_entities, EMBEDDING_DIM};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

/// Test embedder: counts calls, returns a fixed unit vector (or `None`).
struct FakeEmbedder {
    calls: AtomicUsize,
    out: Option<Vec<f32>>,
}
impl FakeEmbedder {
    fn returning(out: Option<Vec<f32>>) -> Self {
        Self { calls: AtomicUsize::new(0), out }
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed_for_storage(&self, _text: &str) -> Option<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.out.clone()
    }
}

/// A deterministic `EMBEDDING_DIM`-length unit vector: 1.0 in slot 0, else 0.
fn unit_vec_e0() -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    v[0] = 1.0;
    v
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

/// Un-quarantine every entity (migration 0015 quarantines new rows by
/// default; the lane filters quarantined rows in production).
async fn unquarantine_all(pool: &sqlx::PgPool) {
    sqlx::query("UPDATE entities SET quarantine = FALSE")
        .execute(pool)
        .await
        .expect("unquarantine all entities");
}

// ---------------------------------------------------------------------------
// Scenario 1 — backfill populates a NULL entity + the lane finds its memory
// ---------------------------------------------------------------------------

#[test]
fn reembed_populates_entity_and_lane_surfaces_linked_memory() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre1-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-1"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed a memory and an entity, link them. The entity has a NULL
        // embedding (nothing populates it yet).
        let mem_id = insert_memory(&pool, "a memory about alice", &serde_json::json!({}), None)
            .await
            .expect("insert memory");
        let graph = PgGraph::new(&pool);
        let alice_id = graph
            .upsert_entity("person", "alice", &serde_json::json!({}))
            .await
            .expect("upsert entity");
        link_memory_to_entities(&pool, mem_id, &[alice_id])
            .await
            .expect("link");
        unquarantine_all(&pool).await;

        // Pre-condition: the entity is unembedded, so the lane finds nothing.
        let before = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, false)
            .await
            .expect("lane before");
        assert!(before.is_empty(), "no embedded entity yet -> empty lane");

        // Backfill embeds the entity (FakeEmbedder returns the e0 unit vec).
        let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
        let report = reembed_entities_null(&pool, &embedder).await.expect("reembed");
        assert_eq!(report, ReembedReport { scanned: 1, embedded: 1, skipped: 0 });
        assert_eq!(embedder.call_count(), 1);

        // The lane now surfaces the linked memory for a query near the entity.
        let after = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, false)
            .await
            .expect("lane after");
        assert!(after.contains(&mem_id), "linked memory surfaces via the entity lane");

        // Nothing left unembedded.
        let remaining = load_unembedded_entities(&pool).await.expect("scan after");
        assert!(remaining.is_empty(), "no NULL-embedding entities remain");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 2 — idempotent re-run embeds nothing
// ---------------------------------------------------------------------------

#[test]
fn reembed_entities_is_idempotent_on_rerun() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre2-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-2"}),
        )
        .await
        .expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let graph = PgGraph::new(&pool);
        graph
            .upsert_entity("person", "bob", &serde_json::json!({}))
            .await
            .expect("upsert entity");

        let first = FakeEmbedder::returning(Some(unit_vec_e0()));
        let r1 = reembed_entities_null(&pool, &first).await.expect("reembed 1");
        assert_eq!(r1, ReembedReport { scanned: 1, embedded: 1, skipped: 0 });

        // Re-run: the row is no longer NULL, so it is not scanned — embedder
        // never called.
        let second = FakeEmbedder::returning(Some(unit_vec_e0()));
        let r2 = reembed_entities_null(&pool, &second).await.expect("reembed 2");
        assert_eq!(r2, ReembedReport { scanned: 0, embedded: 0, skipped: 0 });
        assert_eq!(second.call_count(), 0, "no double-embed on re-run");

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 3 — degrade-and-warn: a failing embed leaves the entity NULL
// ---------------------------------------------------------------------------

#[test]
fn reembed_entities_degrades_and_warns_leaving_null() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre3-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-3"}),
        )
        .await
        .expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let graph = PgGraph::new(&pool);
        let id = graph
            .upsert_entity("person", "carol", &serde_json::json!({}))
            .await
            .expect("upsert entity");

        // Embedder always returns None: the row is scanned but skipped.
        let embedder = FakeEmbedder::returning(None);
        let report = reembed_entities_null(&pool, &embedder)
            .await
            .expect("reembed degrades, not errors");
        assert_eq!(report, ReembedReport { scanned: 1, embedded: 0, skipped: 1 });
        assert_eq!(embedder.call_count(), 1);

        // The entity is still NULL — it remains in the unembedded scan.
        let remaining = load_unembedded_entities(&pool).await.expect("scan after");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, id);

        pool.close().await;
    });
}

// ---------------------------------------------------------------------------
// Scenario 4 — the lane excludes quarantined entities
// ---------------------------------------------------------------------------

#[test]
fn entity_lane_excludes_quarantined_entities() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "entre-d",
        "entre-l",
        &format!("kastellan-supervisor-test-pg-entre4-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "entity-reembed-4"}),
        )
        .await
        .expect("probe");
        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let mem_id = insert_memory(&pool, "a quarantined-entity memory", &serde_json::json!({}), None)
            .await
            .expect("insert memory");
        let graph = PgGraph::new(&pool);
        let dave_id = graph
            .upsert_entity("person", "dave", &serde_json::json!({}))
            .await
            .expect("upsert entity");
        link_memory_to_entities(&pool, mem_id, &[dave_id])
            .await
            .expect("link");
        // Deliberately DO NOT un-quarantine: dave stays quarantine = TRUE.

        // Backfill embeds the quarantined entity (backfill is review-blind).
        let embedder = FakeEmbedder::returning(Some(unit_vec_e0()));
        let report = reembed_entities_null(&pool, &embedder).await.expect("reembed");
        assert_eq!(report, ReembedReport { scanned: 1, embedded: 1, skipped: 0 });

        // Production lane (include_quarantined=false) must NOT surface it.
        let prod = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, false)
            .await
            .expect("lane prod");
        assert!(!prod.contains(&mem_id), "quarantined entity must not leak its memory");

        // The operator path (include_quarantined=true) DOES see it — proving
        // the row is embedded + linked, only the quarantine filter hid it.
        let op = entity_similarity_search(&pool, &unit_vec_e0(), 64, 10, true)
            .await
            .expect("lane operator");
        assert!(op.contains(&mem_id), "operator path surfaces the quarantined entity's memory");

        pool.close().await;
    });
}
```

- [ ] **Step 2: Run the e2e (live PG on this Mac)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --test entity_reembed_e2e -- --nocapture 2>&1 | tail -30`
Expected: 4 tests PASS against the per-test PG cluster (or `[SKIP]` lines if PG bin dir / supervisor unavailable — see the memory note on Postgres.app bin paths to point the suite at PG 18 if it skips).

> If it skips for lack of a PG bin dir: use the session-local-override pattern from the `postgres-app-bin-paths` memory to point `pg_bin_dir_or_skip` at `/Applications/Postgres 2.app/Contents/Versions/18/bin/` before re-running.

- [ ] **Step 3: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --test entity_reembed_e2e -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add core/tests/entity_reembed_e2e.rs
git commit -m "test(memory): live e2e for entity backfill + similarity lane

Backfill populates an entity embedding + the lane surfaces its linked
memory; idempotent re-run; degrade-and-warn; and the lane excludes
quarantined entities (operator path with include_quarantined=true proves
the row is embedded+linked, only the filter hid it).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: CLI — `kastellan-cli entities reembed`

**Files:**
- Modify: `core/src/bin/kastellan-cli/entities.rs` (add `reembed` arm + `entities_reembed` async fn + usage strings)

**Interfaces:**
- Consumes: `kastellan_core::memory::{format_reembed_report, reembed_batch_failed, reembed_entities_null, RouterEmbedder}`; `kastellan_llm_router::{Router, RouterConfig}`; `kastellan_db::pool::connect_runtime_pool`; the existing `crate::common::{resolve_connect_spec, with_runtime}`.

Mirrors `memory_l1_reembed` in `core/src/bin/kastellan-cli/memory_l1.rs`.

- [ ] **Step 1: Add the `reembed` dispatch arm + update usage strings**

In `run_entities`, add the arm and extend both usage strings to include `reembed`:

```rust
pub(crate) fn run_entities(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: kastellan-cli entities <list|show|approve|reject|merge|reembed|kinds> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "list"    => with_runtime("entities", entities_list(&args[1..])),
        "show"    => with_runtime("entities", entities_show(&args[1..])),
        "approve" => with_runtime("entities", entities_approve(&args[1..])),
        "reject"  => with_runtime("entities", entities_reject(&args[1..])),
        "merge"   => with_runtime("entities", entities_merge(&args[1..])),
        "reembed" => with_runtime("entities", entities_reembed(&args[1..])),
        "kinds"   => crate::entities_kinds::run(&args[1..]),
        other     => {
            eprintln!("entities: unknown action '{other}'; expected: list | show | approve | reject | merge | reembed | kinds");
            ExitCode::from(2)
        }
    }
}
```

- [ ] **Step 2: Add the `entities_reembed` async handler**

Add near the other `entities_*` handlers in the same file:

```rust
/// `entities reembed` — backfill `entities.embedding` for every entity whose
/// embedding is NULL, through the real `RouterEmbedder` (same config as the
/// daemon). Prints `scanned=/embedded=/skipped=`; exits non-zero when a batch
/// found rows but embedded none (e.g. an unreachable embed endpoint) so a
/// scripted `reembed && next-step` chain does not proceed. Takes no args.
async fn entities_reembed(args: &[String]) -> ExitCode {
    use std::sync::Arc;

    use kastellan_core::memory::{
        format_reembed_report, reembed_batch_failed, reembed_entities_null, RouterEmbedder,
    };
    use kastellan_db::pool::connect_runtime_pool;

    if !args.is_empty() {
        eprintln!("usage: kastellan-cli entities reembed");
        return ExitCode::from(2);
    }

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // Build the Router-backed embedder. `from_env` reads the host's
    // KASTELLAN_LLM_* config — run this with the same env the daemon uses so
    // backfilled vectors match on-insert ones.
    let router_cfg = match kastellan_llm_router::RouterConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("entities reembed: RouterConfig::from_env: {e}");
            return ExitCode::from(1);
        }
    };
    let router = match kastellan_llm_router::Router::new(router_cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("entities reembed: Router::new: {e}");
            return ExitCode::from(1);
        }
    };
    let embedder = RouterEmbedder::new(pool.clone(), router);

    match reembed_entities_null(&pool, &embedder).await {
        Ok(report) => {
            println!("{}", format_reembed_report(&report));
            // A batch that found rows but embedded none exits non-zero; the
            // idempotent no-op (scanned==0) exits 0.
            if reembed_batch_failed(&report) {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(e) => {
            eprintln!("entities reembed: {e}");
            ExitCode::from(1)
        }
    }
}
```

- [ ] **Step 3: Build + verify the dispatch (no-arg usage exits 2; unknown action unchanged)**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --bin kastellan-cli 2>&1 | tail -5 && ./target/debug/kastellan-cli entities reembed extra-arg; echo "exit=$?"`
Expected: build clean; `usage: kastellan-cli entities reembed` on stderr, `exit=2` (the no-DB arg-validation path runs before any pool connect).

- [ ] **Step 4: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --bin kastellan-cli -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add core/src/bin/kastellan-cli/entities.rs
git commit -m "feat(cli): kastellan-cli entities reembed

Backfills entities.embedding through the real RouterEmbedder; prints
scanned=/embedded=/skipped=, exits non-zero on a wholly-failed batch.
Mirrors 'memory l1 reembed'.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Full-workspace verification + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (header + Next TODO update — session-end convention)
- Modify: `docs/devel/ROADMAP.md` (mark the entity-embedding lane shipped)

- [ ] **Step 1: Full workspace test**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -30`
Expected: all green (live PG e2e tests run on this Mac; `[SKIP]` only for the documented gated suites). Investigate any failure before proceeding — note `cli_ask_e2e::ask_subprocess_fails_after_plan_iteration_cap` is a known pre-existing heavy-load flake (passes in isolation); re-run it alone if it trips: `cargo test -p kastellan-core --test cli_ask_e2e ask_subprocess_fails_after_plan_iteration_cap`.

- [ ] **Step 2: Full workspace clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 3: Census the touched files against the 500-LOC cap**

Run: `source "$HOME/.cargo/env" && wc -l db/src/entity_embedding.rs core/src/memory/{reembed,entity_reembed,recall}.rs core/src/memory/l1_reembed.rs core/src/bin/kastellan-cli/entities.rs`
Expected: every file ≤ 500 (or, for `entities.rs`/`recall.rs`, within the documented ≤27-over deferral — note the number in the handover if so).

- [ ] **Step 4: Update HANDOVER.md + ROADMAP.md (session-end convention)**

Per `CLAUDE.md` / HANDOVER's end-of-session checklist: move the entity-embedding lane into "Recently completed", write a fresh "Next TODO", note the **forward embed-on-insert path** + **ANN index** as the carried-forward follow-ups, and refresh the over-cap census. Keep both docs concise.

- [ ] **Step 5: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: entity-embedding backfill + similarity lane shipped

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 6: Push + open PR**

```bash
git push -u origin feat/entity-embedding-recall-lane
gh pr create --base main --title "feat(memory): entity-embedding backfill + entity-similarity recall lane" --body "$(cat <<'EOF'
## Summary
Populates `entities.embedding` (NULL everywhere today) via a backfill CLI and consumes it through a fourth recall lane that surfaces memories linked to query-similar entities. Mirrors the L1 embedding arc (#324/#325).

- `db::entity_embedding` — scan + guarded write + `entity_similarity_search` lane query
- `core::memory::entity_reembed::reembed_entities_null` (+ shared `ReembedReport` lifted to `memory/reembed.rs`)
- `recall` gains the `entity` lane (RRF-fused; on by default for the no-seeds path; quarantine-filtered in production)
- `kastellan-cli entities reembed`

## Scope / follow-ups
Backfill + lane only. Forward embed-on-insert path (in `batch_upsert`) and an ANN index on `entities.embedding` are deferred follow-ups (documented in the spec).

## Verification
`cargo test --workspace` green (incl. live-PG `entity_reembed_e2e` 4/0 on macOS PG 18); `cargo clippy --workspace --all-targets -D warnings` clean. Pure-Rust, no migration, no OS-gated code → DGX not required.

Spec: `docs/superpowers/specs/2026-06-21-entity-embedding-recall-lane-design.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

> If `git push` from the Mac times out (github ports firewalled), use the DGX relay from the `mac-github-push-blocked-relay-via-dgx` memory: `git format-patch origin/main..HEAD --stdout | ssh dgx 'cd ~/src/kastellan && git checkout -b feat/entity-embedding-recall-lane origin/main && git am && git push -u origin feat/entity-embedding-recall-lane'`, then `gh pr create` from the Mac.

---

## Self-Review

**Spec coverage:**
- Layer 1 (db `entity_embedding.rs`: `load_unembedded_entities`, `set_entity_embedding`, `entity_similarity_search`) → Tasks 1–2. ✓
- Layer 2 (shared report extraction + `entity_reembed`) → Tasks 3–4. ✓
- Layer 3 (recall `entity` lane, presets, fanout const, no-seeds default) → Task 5. ✓
- Layer 4 (CLI `entities reembed`) → Task 7. ✓
- Quarantine semantics (backfill embeds all; lane filters) → enforced in Task 2 SQL, proven in Task 6 Scenario 4. ✓
- Error handling (degrade-and-warn; dim hard-error; non-zero CLI exit) → Tasks 2, 4, 5, 7. ✓
- Testing matrix (db unit, core unit, e2e, recall unit) → Tasks 1, 2, 4, 5, 6. ✓
- No migration / no ANN index / no forward path / DGX-not-required → respected throughout; follow-ups noted in Tasks 6–8 + the PR body. ✓

**Placeholder scan:** No TBD/TODO/"handle appropriately"; every code step shows complete code; every command has an expected result. ✓

**Type consistency:** `ReembedReport` (re-exported via `memory::reembed`), `reembed_entities_null(&PgPool, &dyn Embedder) -> Result<ReembedReport, DbError>`, `entity_similarity_search(_, &[f32], i64, usize, bool) -> Result<Vec<i64>, DbError>`, `entity_embedding_text(&str,&str)->String`, `ENTITY_SIMILARITY_FANOUT: i64`, `RecallModes::{ALL, ENTITY_ONLY, SEMANTIC_LEXICAL_ENTITY}` are used identically across Tasks 1→7. ✓
