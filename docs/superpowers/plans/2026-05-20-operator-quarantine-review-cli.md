# Operator quarantine-review CLI — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ship `hhagent-cli entities {list,show,approve,reject,merge}` so an operator can lift quarantine on extracted entities, deleting / merging junk. Once an entity is approved (`quarantine = FALSE`), production `graph_search` surfaces its linked memories — closing the graph lane in production.

**Architecture:** a new DB module `hhagent_db::entities` owns the SQL surface (mirrors `tool_allowlists.rs` in shape and size). Three new `core::cli_audit` helpers compose the DB call with a wire-stable audit row. The CLI surface lives in `core/src/bin/hhagent-cli.rs` next to the existing `tools allowlist` and `memory l1` subcommand trees. No new migrations — the runtime role already has full CRUD on `entities` and cascade on `memory_entities`. `entity_kinds` (REVOKE per migration 0016) is deliberately untouched.

**Tech Stack:** Rust 2021, tokio (multi-thread), sqlx + PostgreSQL, thiserror, tracing, serde_json, `time::OffsetDateTime`, `core::process::ExitCode`.

**Spec:** [`docs/superpowers/specs/2026-05-20-operator-quarantine-review-cli-design.md`](../specs/2026-05-20-operator-quarantine-review-cli-design.md) (committed `6b25b50`).

---

## File map

**Create:**
- `db/src/entities.rs` — module (~280 LOC incl. unit tests)
- `core/tests/cli_entities_e2e.rs` — CLI subprocess integration tests (~350 LOC)

**Modify:**
- `db/src/lib.rs` — add `pub mod entities;`
- `core/src/scheduler/audit.rs` — +3 action constants + 3 payload builders + 6 unit tests
- `core/src/cli_audit.rs` — +3 helpers (~110 LOC) + 2 compile-pin tests
- `core/src/bin/hhagent-cli.rs` — +subcommand tree (~250 LOC) + `entities …` lines in `help_text()` + 2 arg-parser unit tests
- `db/tests/postgres_e2e.rs` — +7 DB integration tests
- `core/tests/memory_recall_e2e.rs` — +1 graph-lane recall pin

**Test budget:** +26 (workspace 848 → ~874).
- 4 unit (`db::entities::tests`) — `body_preview` (3) + `validate_merge_args` (1)
- 6 unit (`scheduler::audit::tests`) — 3 payload pins + 3 action-const string pins
- 2 unit (`bin::hhagent_cli` arg parsing) — `parse_entity_state` + `parse_id_list`
- 7 DB integration (`postgres_e2e`)
- 6 CLI subprocess (`cli_entities_e2e`)
- 1 graph-lane recall pin (`memory_recall_e2e`)

---

## Conventions

- Every task ends with `cargo test --workspace` green before commit.
- Commit messages follow the in-tree style: `<type>(<scope>): <imperative>` (e.g. `feat(db/entities): scaffold + types + body_preview helper`).
- Source the env first if not already loaded: `source "$HOME/.cargo/env"`.
- `cargo test -p <crate>` for narrower runs; `cargo test --workspace` before each commit.
- TDD: failing test first, then minimal impl, then re-run, then commit.

---

## Task 1: Scaffold `db::entities` module — types, pure helpers, unit tests

**Files:**
- Create: `db/src/entities.rs`
- Modify: `db/src/lib.rs`

### Step 1.1: Add the module declaration to `db/src/lib.rs`

- [ ] **Add `pub mod entities;`** to `db/src/lib.rs` next to the existing `pub mod entity_kinds;` declaration.

Find the existing block of `pub mod …;` declarations (around line 10-30) and add the entities module in the alphabetical position:

```rust
pub mod entities;
pub mod entity_kinds;
```

### Step 1.2: Write the failing unit tests for the pure helpers

- [ ] **Create `db/src/entities.rs`** with the type definitions and unit tests but no I/O implementations yet:

```rust
//! Operator-facing entity review surface.
//!
//! Migration `0015_entity_kinds_and_quarantine.sql` introduced
//! `entities.quarantine BOOLEAN NOT NULL DEFAULT TRUE` — every newly
//! extracted entity is invisible to production `graph_search` (which
//! passes `include_quarantined = false`) until an operator reviews and
//! either approves (flip quarantine = FALSE), rejects (DELETE,
//! cascading memory_entities), or merges (consolidate near-duplicates
//! from extractor variance into one canonical row).
//!
//! This module owns the SQL surface for those operations. The CLI
//! consumer lives in `core/src/bin/hhagent-cli.rs` under the
//! `entities` subcommand tree; the audit wrapper lives in
//! `core::cli_audit`. Layout mirrors `db::tool_allowlists`.
//!
//! ## Grants
//!
//! No new migration. The runtime role already has full CRUD on
//! `entities` (migration `0002` default GRANT, never revoked) and
//! `memory_entities` rows cascade via the FK from migration `0007`.
//! `entity_kinds` (migration `0016` REVOKE) is deliberately untouched.

use crate::DbError;
use sqlx::PgPool;
use time::OffsetDateTime;

/// One row in the `entities` table joined with its mention count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRow {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub name_norm: String,
    pub quarantine: bool,
    pub created_at: OffsetDateTime,
    pub mention_count: i64,
}

/// CLI-surface filter on `quarantine` state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityState {
    /// Default — surfaces rows the operator hasn't reviewed yet.
    Quarantined,
    /// Already-approved rows (visible in production graph_search).
    Approved,
    /// No filter — for export / sanity dumps.
    Any,
}

/// One row in `list_entities` filter.
///
/// Use `ListFilter::default()` for the operator-friendly default
/// (quarantined / limit 50 / no other filters).
#[derive(Clone, Debug)]
pub struct ListFilter {
    pub kind: Option<String>,
    pub state: EntityState,
    pub limit: i64,
    pub since: Option<OffsetDateTime>,
    pub min_mentions: i64,
}

impl Default for ListFilter {
    fn default() -> Self {
        Self {
            kind: None,
            state: EntityState::Quarantined,
            limit: 50,
            since: None,
            min_mentions: 0,
        }
    }
}

/// One linked-memory preview row used by `get_entity_with_mentions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPreview {
    pub memory_id: i64,
    pub layer: i16,
    pub body_preview: String,
}

/// Three-variant outcome of `approve_entity`. Carries enough info for
/// the CLI to distinguish operator messages without a second DB probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApproveOutcome {
    Approved { kind: String, name: String },
    AlreadyApproved,
    NotFound,
}

/// Two-variant outcome of `reject_entity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectOutcome {
    Rejected {
        kind: String,
        name: String,
        mentions_dropped: i64,
    },
    NotFound,
}

/// Outcome of a successful merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOutcome {
    pub kept_id: i64,
    pub kept_kind: String,
    pub kept_name: String,
    pub dropped_ids: Vec<i64>,
    pub links_retargeted: i64,
    pub links_dropped_as_duplicate: i64,
}

/// Errors specific to the entities review surface. `Db` wraps the
/// generic `DbError`; the structured variants carry enough context that
/// the operator sees the offending id in the error message.
#[derive(Debug, thiserror::Error)]
pub enum EntitiesError {
    #[error("entity {0} not found")]
    NotFound(i64),
    #[error(
        "merge: kind mismatch — keep id {keep_id} is kind '{keep_kind}', \
         drop id {drop_id} is kind '{drop_kind}'"
    )]
    KindMismatch {
        keep_id: i64,
        keep_kind: String,
        drop_id: i64,
        drop_kind: String,
    },
    #[error("merge requires at least one --drop id")]
    NoDropIds,
    #[error("merge: --drop list contains keep id ({0})")]
    KeepInDropList(i64),
    #[error("database: {0}")]
    Db(#[from] DbError),
}

// ─────────────────────── pure helpers ───────────────────────

/// Validate the `keep_id` / `drop_ids` shape *before* any DB call. Pure
/// CPU; testable without a pool.
pub(crate) fn validate_merge_args(
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<(), EntitiesError> {
    if drop_ids.is_empty() {
        return Err(EntitiesError::NoDropIds);
    }
    if drop_ids.contains(&keep_id) {
        return Err(EntitiesError::KeepInDropList(keep_id));
    }
    Ok(())
}

/// Build a body preview suitable for the `show` command's
/// `linked memories` block: collapse newlines + multi-space runs to a
/// single space, then truncate to `max_chars` characters with no
/// trailing ellipsis (operators see the cap by the row width). Pure
/// CPU; testable without a pool.
pub(crate) fn body_preview(body: &str, max_chars: usize) -> String {
    let collapsed: String = body
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    // Collapse runs of whitespace to a single space.
    let mut out = String::with_capacity(collapsed.len());
    let mut prev_space = false;
    for c in collapsed.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim().to_string();
    if out.chars().count() > max_chars {
        out.chars().take(max_chars).collect()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_preview_collapses_newlines_and_multispace() {
        let body = "line one\nline two\t\twith   gaps";
        assert_eq!(body_preview(body, 80), "line one line two with gaps");
    }

    #[test]
    fn body_preview_truncates_at_max_chars() {
        let body = "abcdefghij".repeat(10); // 100 chars
        let out = body_preview(&body, 30);
        assert_eq!(out.chars().count(), 30);
        assert!(body.starts_with(&out));
    }

    #[test]
    fn body_preview_trims_leading_and_trailing_whitespace() {
        let body = "   leading\nand trailing   ";
        assert_eq!(body_preview(body, 80), "leading and trailing");
    }

    #[test]
    fn validate_merge_args_rejects_empty_drops() {
        assert!(matches!(
            validate_merge_args(1, &[]),
            Err(EntitiesError::NoDropIds)
        ));
    }

    #[test]
    fn validate_merge_args_rejects_keep_in_drop_list() {
        assert!(matches!(
            validate_merge_args(5, &[3, 5, 7]),
            Err(EntitiesError::KeepInDropList(5))
        ));
    }

    #[test]
    fn validate_merge_args_accepts_well_formed_args() {
        assert!(validate_merge_args(1, &[2]).is_ok());
        assert!(validate_merge_args(1, &[2, 3, 4]).is_ok());
    }

    #[test]
    fn list_filter_default_is_quarantined_limit_50() {
        let f = ListFilter::default();
        assert_eq!(f.state, EntityState::Quarantined);
        assert_eq!(f.limit, 50);
        assert!(f.kind.is_none());
        assert!(f.since.is_none());
        assert_eq!(f.min_mentions, 0);
    }
}
```

### Step 1.3: Run the tests to verify they pass

Run:
```
cargo test -p hhagent-db entities:: --no-run 2>&1 | tail -20
cargo test -p hhagent-db entities:: 2>&1 | tail -15
```
Expected: all 7 tests pass.

### Step 1.4: Run the full workspace to verify zero regressions

Run:
```
cargo test --workspace 2>&1 | tail -3
```
Expected: 848 + 7 = **855 passed / 0 failed / 4 ignored / 0 [SKIP]**.

### Step 1.5: Commit

```bash
git add db/src/entities.rs db/src/lib.rs
git commit -m "feat(db/entities): scaffold module + types + pure helpers

New db::entities module mirrors db::tool_allowlists in shape: types
(EntityRow, ListFilter, EntityState, MemoryPreview, ApproveOutcome,
RejectOutcome, MergeOutcome, EntitiesError) + two pure helpers
(validate_merge_args, body_preview) + 7 unit tests covering the helper
edge cases (newline/multispace collapse, truncation, trim,
NoDropIds/KeepInDropList rejection, well-formed acceptance,
ListFilter::default shape).

No I/O implementations yet — Tasks 2-4 add list_entities,
get_entity_with_mentions, approve_entity, reject_entity, merge_entities.

Workspace: 848 -> 855 (+7).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 2: `list_entities` + `get_entity_with_mentions` + DB integration tests

**Files:**
- Modify: `db/src/entities.rs` (add the two query functions; tests stay in `db/tests/postgres_e2e.rs`)
- Modify: `db/tests/postgres_e2e.rs` (+2 integration tests)

### Step 2.1: Add `list_entities` and `get_entity_with_mentions` to `db/src/entities.rs`

- [ ] **Append these functions** to `db/src/entities.rs` below the `validate_merge_args` / `body_preview` block (above the `#[cfg(test)] mod tests` block):

```rust
// ─────────────────────── I/O layer ───────────────────────

/// List entities matching `filter`, joined with their `memory_entities`
/// count. Ordering is `created_at DESC, id DESC` so the operator sees
/// the most-recent entities first.
pub async fn list_entities(
    pool: &PgPool,
    filter: &ListFilter,
) -> Result<Vec<EntityRow>, EntitiesError> {
    let quarantine_filter: Option<bool> = match filter.state {
        EntityState::Quarantined => Some(true),
        EntityState::Approved => Some(false),
        EntityState::Any => None,
    };
    let rows: Vec<(
        i64,                  // id
        String,               // kind
        String,               // name
        String,               // name_norm
        bool,                 // quarantine
        OffsetDateTime,       // created_at
        i64,                  // mention_count
    )> = sqlx::query_as(
        r#"
        SELECT e.id, e.kind, e.name, e.name_norm, e.quarantine,
               e.created_at,
               COUNT(me.memory_id)::BIGINT AS mention_count
        FROM entities e
        LEFT JOIN memory_entities me ON me.entity_id = e.id
        WHERE
              ($1::TEXT        IS NULL OR e.kind        = $1)
          AND ($2::BOOL        IS NULL OR e.quarantine  = $2)
          AND ($3::TIMESTAMPTZ IS NULL OR e.created_at >= $3)
        GROUP BY e.id
        HAVING COUNT(me.memory_id) >= $4
        ORDER BY e.created_at DESC, e.id DESC
        LIMIT $5
        "#,
    )
    .bind(filter.kind.as_deref())
    .bind(quarantine_filter)
    .bind(filter.since)
    .bind(filter.min_mentions)
    .bind(filter.limit)
    .fetch_all(pool)
    .await
    .map_err(|e| EntitiesError::Db(DbError::Query(format!("list_entities: {e}"))))?;
    Ok(rows
        .into_iter()
        .map(|(id, kind, name, name_norm, quarantine, created_at, mention_count)| {
            EntityRow {
                id,
                kind,
                name,
                name_norm,
                quarantine,
                created_at,
                mention_count,
            }
        })
        .collect())
}

/// Per-entity preview row cap for `get_entity_with_mentions`.
pub const SHOW_LINKED_MEMORIES_CAP: i64 = 10;
/// Per-memory body preview character cap.
pub const SHOW_BODY_PREVIEW_CHARS: usize = 80;

/// Fetch the entity row and up to `SHOW_LINKED_MEMORIES_CAP` linked
/// memory previews. Returns `Ok(None)` if no entity at that id.
pub async fn get_entity_with_mentions(
    pool: &PgPool,
    id: i64,
) -> Result<Option<(EntityRow, Vec<MemoryPreview>)>, EntitiesError> {
    let entity: Option<(i64, String, String, String, bool, OffsetDateTime, i64)> =
        sqlx::query_as(
            r#"
            SELECT e.id, e.kind, e.name, e.name_norm, e.quarantine, e.created_at,
                   (SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1)::BIGINT
                       AS mention_count
            FROM entities e
            WHERE e.id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!(
                "get_entity_with_mentions: select entity {id}: {e}"
            )))
        })?;
    let row = match entity {
        None => return Ok(None),
        Some(r) => r,
    };
    let entity_row = EntityRow {
        id: row.0,
        kind: row.1,
        name: row.2,
        name_norm: row.3,
        quarantine: row.4,
        created_at: row.5,
        mention_count: row.6,
    };

    // Linked memories — pull body + layer for the preview.
    let mems: Vec<(i64, i16, String)> = sqlx::query_as(
        r#"
        SELECT m.id, m.layer, m.body
        FROM memory_entities me
        JOIN memories m ON m.id = me.memory_id
        WHERE me.entity_id = $1
        ORDER BY m.id DESC
        LIMIT $2
        "#,
    )
    .bind(id)
    .bind(SHOW_LINKED_MEMORIES_CAP)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!(
            "get_entity_with_mentions: select memories for {id}: {e}"
        )))
    })?;
    let previews = mems
        .into_iter()
        .map(|(memory_id, layer, body)| MemoryPreview {
            memory_id,
            layer,
            body_preview: body_preview(&body, SHOW_BODY_PREVIEW_CHARS),
        })
        .collect();

    Ok(Some((entity_row, previews)))
}
```

### Step 2.2: Write 2 DB integration tests in `db/tests/postgres_e2e.rs`

- [ ] **Find the appropriate location** in `db/tests/postgres_e2e.rs` (alphabetical or grouped — match the precedent). Use the existing `bring_up_pg_cluster()` helper from `hhagent_tests_common`.

Add these tests:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_list_filters_by_state_kind_and_since() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_db::entities::{list_entities, EntityState, ListFilter};
    use time::OffsetDateTime;

    // Seed 4 entities — 2 quarantined (different kinds), 1 approved, 1 old.
    sqlx::query("INSERT INTO entities (kind, name, name_norm, quarantine) VALUES
        ('person', 'Quar Alice', 'quar alice', TRUE),
        ('place',  'Quar Mosman', 'quar mosman', TRUE),
        ('person', 'OK Bob', 'ok bob', FALSE),
        ('person', 'Old Carol', 'old carol', TRUE)")
        .execute(&pool).await.unwrap();
    // Back-date Old Carol so the --since filter excludes it.
    sqlx::query("UPDATE entities SET created_at = now() - interval '7 days' WHERE name = 'Old Carol'")
        .execute(&pool).await.unwrap();

    // Default filter (quarantined, limit 50, no other filters).
    let rows = list_entities(&pool, &ListFilter::default()).await.unwrap();
    let names: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains("Quar Alice"));
    assert!(names.contains("Quar Mosman"));
    assert!(names.contains("Old Carol"));
    assert!(!names.contains("OK Bob"), "approved entity must not appear in default filter");
    assert_eq!(rows.len(), 3, "expected 3 quarantined rows, got {}", rows.len());

    // Filter by kind=person.
    let rows = list_entities(&pool, &ListFilter {
        kind: Some("person".into()),
        ..ListFilter::default()
    }).await.unwrap();
    assert_eq!(rows.len(), 2, "expected 2 quarantined persons");
    for r in &rows {
        assert_eq!(r.kind, "person");
    }

    // Filter by state=approved.
    let rows = list_entities(&pool, &ListFilter {
        state: EntityState::Approved,
        ..ListFilter::default()
    }).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "OK Bob");
    assert!(!rows[0].quarantine);

    // Filter by since = now - 1 day. Old Carol must be excluded.
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(1);
    let rows = list_entities(&pool, &ListFilter {
        since: Some(cutoff),
        ..ListFilter::default()
    }).await.unwrap();
    let names: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(!names.contains("Old Carol"), "back-dated row must be excluded by --since");
    assert!(names.contains("Quar Alice"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_list_min_mentions_filter_uses_join_count() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_db::entities::{list_entities, ListFilter};

    // Seed 1 entity with 0 mentions and 1 entity with 2 mentions.
    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES
        ('person', 'Zero', 'zero'),
        ('person', 'Two',  'two')")
        .execute(&pool).await.unwrap();

    let zero_id: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Zero'")
        .fetch_one(&pool).await.unwrap();
    let two_id: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Two'")
        .fetch_one(&pool).await.unwrap();

    // Two memories linked only to the 'Two' entity.
    use hhagent_db::memories::insert_memory_at_layer;
    use hhagent_db::memories::MemoryLayer;
    let _ = zero_id; // pinned but no mentions
    let mem1 = insert_memory_at_layer(&pool, "body 1", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    let mem2 = insert_memory_at_layer(&pool, "body 2", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $2), ($3, $2)")
        .bind(mem1).bind(two_id).bind(mem2)
        .execute(&pool).await.unwrap();

    // min_mentions=1 — only 'Two' qualifies.
    let rows = list_entities(&pool, &ListFilter {
        min_mentions: 1,
        ..ListFilter::default()
    }).await.unwrap();
    let names: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains("Two"));
    assert!(!names.contains("Zero"));

    // Verify mention_count is surfaced correctly.
    let two_row = rows.iter().find(|r| r.name == "Two").unwrap();
    assert_eq!(two_row.mention_count, 2);
}
```

> **Note:** The `MemoryLayer::Detail` enum variant name is the L0 layer in the current tree. If the variant has been renamed, use the current name. Verify with:
> ```
> grep -n "pub enum MemoryLayer" db/src/memories.rs
> ```

### Step 2.3: Run the new tests to verify they pass

```
cargo test -p hhagent-db --test postgres_e2e entities_list_ 2>&1 | tail -10
```
Expected: 2 tests pass (or [SKIP] cleanly if no PG on host).

### Step 2.4: Run the full workspace to verify zero regressions

```
cargo test --workspace 2>&1 | tail -3
```
Expected: 855 + 2 = **857 passed / 0 failed / 4 ignored / 0 [SKIP]**.

### Step 2.5: Commit

```bash
git add db/src/entities.rs db/tests/postgres_e2e.rs
git commit -m "feat(db/entities): list_entities + get_entity_with_mentions

New SQL surface for the operator quarantine-review CLI's read paths:

- list_entities: LEFT JOIN memory_entities for mention_count;
  WHERE-or-NULL pattern lets one query handle every filter combo
  (kind/state/since/min_mentions); ORDER BY created_at DESC, id DESC.
- get_entity_with_mentions: per-id deep view including the first 10
  linked memory bodies (newline/whitespace-collapsed, capped at 80
  chars via the pure body_preview helper from Task 1).

Two DB integration tests in postgres_e2e cover the filter matrix +
the min_mentions JOIN-count semantics.

Workspace: 855 -> 857 (+2).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 3: `approve_entity` + `reject_entity` + 3 DB integration tests

**Files:**
- Modify: `db/src/entities.rs`
- Modify: `db/tests/postgres_e2e.rs`

### Step 3.1: Add `approve_entity` and `reject_entity` to `db/src/entities.rs`

- [ ] **Append below `get_entity_with_mentions`:**

```rust
/// Flip `quarantine` from TRUE to FALSE for one entity inside a
/// single transaction, distinguishing already-approved from not-found.
///
/// Transaction shape:
///   BEGIN;
///   SELECT id, kind, name, quarantine FROM entities WHERE id = $1 FOR UPDATE;
///   -- branch on observed state:
///   --   None                -> COMMIT, return NotFound
///   --   quarantine = FALSE  -> COMMIT, return AlreadyApproved
///   --   quarantine = TRUE   -> UPDATE … SET quarantine = FALSE, COMMIT,
///                               return Approved { kind, name }
pub async fn approve_entity(
    pool: &PgPool,
    id: i64,
) -> Result<ApproveOutcome, EntitiesError> {
    let mut tx = pool.begin().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("approve_entity begin: {e}")))
    })?;
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT kind, name, quarantine FROM entities WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("approve_entity select {id}: {e}")))
    })?;
    let (kind, name, quarantine) = match row {
        None => {
            tx.commit().await.ok();
            return Ok(ApproveOutcome::NotFound);
        }
        Some(t) => t,
    };
    if !quarantine {
        tx.commit().await.ok();
        return Ok(ApproveOutcome::AlreadyApproved);
    }
    sqlx::query("UPDATE entities SET quarantine = FALSE, updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!("approve_entity update {id}: {e}")))
        })?;
    tx.commit().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("approve_entity commit {id}: {e}")))
    })?;
    Ok(ApproveOutcome::Approved { kind, name })
}

/// Delete one entity inside a single transaction, capturing the cascade
/// row count from `memory_entities` for the audit-row payload.
///
/// Transaction shape:
///   BEGIN;
///   SELECT id, kind, name FROM entities WHERE id = $1 FOR UPDATE;
///   -- on None -> COMMIT, return NotFound
///   SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1;
///   DELETE FROM entities WHERE id = $1;   -- cascades memory_entities
///   COMMIT;
pub async fn reject_entity(
    pool: &PgPool,
    id: i64,
) -> Result<RejectOutcome, EntitiesError> {
    let mut tx = pool.begin().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity begin: {e}")))
    })?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT kind, name FROM entities WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity select {id}: {e}")))
    })?;
    let (kind, name) = match row {
        None => {
            tx.commit().await.ok();
            return Ok(RejectOutcome::NotFound);
        }
        Some(t) => t,
    };
    let mentions_dropped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity count {id}: {e}")))
    })?;
    sqlx::query("DELETE FROM entities WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!("reject_entity delete {id}: {e}")))
        })?;
    tx.commit().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("reject_entity commit {id}: {e}")))
    })?;
    Ok(RejectOutcome::Rejected { kind, name, mentions_dropped })
}
```

### Step 3.2: Write 3 DB integration tests in `db/tests/postgres_e2e.rs`

- [ ] **Append after the Task 2 tests:**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_approve_flips_quarantine_and_is_idempotent() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_db::entities::{approve_entity, ApproveOutcome};

    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES ('person', 'Approve Me', 'approve me')")
        .execute(&pool).await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Approve Me'")
        .fetch_one(&pool).await.unwrap();

    // First call: Approved.
    match approve_entity(&pool, id).await.unwrap() {
        ApproveOutcome::Approved { kind, name } => {
            assert_eq!(kind, "person");
            assert_eq!(name, "Approve Me");
        }
        other => panic!("expected Approved, got {other:?}"),
    }
    // DB state must reflect the flip.
    let quarantine: bool = sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert!(!quarantine);

    // Second call: AlreadyApproved.
    assert!(matches!(approve_entity(&pool, id).await.unwrap(), ApproveOutcome::AlreadyApproved));

    // Unknown id: NotFound.
    assert!(matches!(approve_entity(&pool, 999_999).await.unwrap(), ApproveOutcome::NotFound));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_reject_cascades_memory_entities_and_returns_count() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_db::entities::{reject_entity, RejectOutcome};
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};

    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES ('person', 'Reject Me', 'reject me')")
        .execute(&pool).await.unwrap();
    let entity_id: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Reject Me'")
        .fetch_one(&pool).await.unwrap();

    // Link two memories to the entity.
    let mem1 = insert_memory_at_layer(&pool, "body one", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    let mem2 = insert_memory_at_layer(&pool, "body two", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $3), ($2, $3)")
        .bind(mem1).bind(mem2).bind(entity_id)
        .execute(&pool).await.unwrap();

    match reject_entity(&pool, entity_id).await.unwrap() {
        RejectOutcome::Rejected { kind, name, mentions_dropped } => {
            assert_eq!(kind, "person");
            assert_eq!(name, "Reject Me");
            assert_eq!(mentions_dropped, 2);
        }
        other => panic!("expected Rejected, got {other:?}"),
    }

    // Entity is gone.
    let entity_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id = $1")
        .bind(entity_id).fetch_one(&pool).await.unwrap();
    assert_eq!(entity_count, 0);
    // memory_entities rows cascaded.
    let me_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1")
        .bind(entity_id).fetch_one(&pool).await.unwrap();
    assert_eq!(me_count, 0);
    // Memory rows themselves survive.
    let mem_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE id IN ($1, $2)")
        .bind(mem1).bind(mem2).fetch_one(&pool).await.unwrap();
    assert_eq!(mem_count, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_reject_returns_not_found_on_unknown_id() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_db::entities::{reject_entity, RejectOutcome};
    assert!(matches!(
        reject_entity(&pool, 999_999).await.unwrap(),
        RejectOutcome::NotFound
    ));
}
```

### Step 3.3: Run + commit

```
cargo test -p hhagent-db --test postgres_e2e entities_approve entities_reject 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -3
```
Expected: 3 new pass; workspace = **860 passed**.

```bash
git add db/src/entities.rs db/tests/postgres_e2e.rs
git commit -m "feat(db/entities): approve_entity + reject_entity + cascade integration tests

Two transactional state-changing operations:

- approve_entity: SELECT ... FOR UPDATE -> branch on (None /
  quarantine=FALSE / quarantine=TRUE) -> UPDATE in the TRUE arm.
  Returns ApproveOutcome::{Approved{kind,name}, AlreadyApproved,
  NotFound} so the CLI can produce distinct messages without a
  second probe.
- reject_entity: SELECT ... FOR UPDATE -> COUNT(memory_entities)
  -> DELETE entities (cascades memory_entities). Returns
  RejectOutcome::{Rejected{kind,name,mentions_dropped}, NotFound}.

Three DB integration tests cover the happy paths (Approved + flip
observable in DB, Rejected with mentions_dropped=2 + cascade
visible, memory rows survive the entity delete) + the idempotent
'AlreadyApproved' path + the 'NotFound' path on both APIs.

Workspace: 857 -> 860 (+3).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 4: `merge_entities` + 2 DB integration tests

**Files:**
- Modify: `db/src/entities.rs`
- Modify: `db/tests/postgres_e2e.rs`

### Step 4.1: Add `merge_entities` to `db/src/entities.rs`

- [ ] **Append below `reject_entity`:**

```rust
/// Single-transaction merge: validate -> SELECT FOR UPDATE on keep + all
/// drops -> precondition check (same kind for every drop) -> INSERT new
/// memory_entities rows pointing at keep, ON CONFLICT DO NOTHING (so a
/// memory already linked to both keep and a drop is consolidated, not
/// duplicated) -> count the consolidated-as-duplicate rows -> DELETE
/// drops (cascading the old memory_entities) -> COMMIT.
///
/// Returns the outcome; on precondition failure returns
/// EntitiesError::{NotFound, KindMismatch, NoDropIds, KeepInDropList}
/// with the transaction rolled back (no DB writes).
pub async fn merge_entities(
    pool: &PgPool,
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<MergeOutcome, EntitiesError> {
    validate_merge_args(keep_id, drop_ids)?;
    let mut tx = pool.begin().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities begin: {e}")))
    })?;

    // Lock keep first.
    let keep_row: Option<(String, String)> = sqlx::query_as(
        "SELECT kind, name FROM entities WHERE id = $1 FOR UPDATE",
    )
    .bind(keep_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!(
            "merge_entities select keep {keep_id}: {e}"
        )))
    })?;
    let (keep_kind, keep_name) = match keep_row {
        None => {
            tx.rollback().await.ok();
            return Err(EntitiesError::NotFound(keep_id));
        }
        Some(t) => t,
    };

    // Lock each drop + verify kind. ANY($1) preserves the input ordering
    // in the WHERE filter; the kind-mismatch check loops over the result
    // and compares to keep_kind, surfacing the first offending id.
    let drop_rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, kind FROM entities WHERE id = ANY($1) FOR UPDATE",
    )
    .bind(drop_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!(
            "merge_entities select drops: {e}"
        )))
    })?;
    let found_ids: std::collections::BTreeSet<i64> = drop_rows.iter().map(|(id, _)| *id).collect();
    for did in drop_ids {
        if !found_ids.contains(did) {
            tx.rollback().await.ok();
            return Err(EntitiesError::NotFound(*did));
        }
    }
    for (drop_id, drop_kind) in &drop_rows {
        if drop_kind != &keep_kind {
            tx.rollback().await.ok();
            return Err(EntitiesError::KindMismatch {
                keep_id,
                keep_kind: keep_kind.clone(),
                drop_id: *drop_id,
                drop_kind: drop_kind.clone(),
            });
        }
    }

    // Count BOTH-linked memories before we mutate. This is the
    // "links_dropped_as_duplicate" count.
    let dup_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM memory_entities WHERE entity_id = ANY($1)
        AND memory_id IN (SELECT memory_id FROM memory_entities WHERE entity_id = $2)
        "#,
    )
    .bind(drop_ids)
    .bind(keep_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities dup_count: {e}")))
    })?;

    // Retarget. ON CONFLICT DO NOTHING absorbs the duplicates; the
    // rows_affected of the INSERT is the count of UNIQUE memory_ids
    // gained by keep — i.e. links_retargeted.
    let res = sqlx::query(
        r#"
        INSERT INTO memory_entities (memory_id, entity_id)
        SELECT DISTINCT memory_id, $1
        FROM memory_entities
        WHERE entity_id = ANY($2)
        ON CONFLICT (memory_id, entity_id) DO NOTHING
        "#,
    )
    .bind(keep_id)
    .bind(drop_ids)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities retarget: {e}")))
    })?;
    let links_retargeted = res.rows_affected() as i64;

    // Drop the source entities. Cascade removes the old links.
    sqlx::query("DELETE FROM entities WHERE id = ANY($1)")
        .bind(drop_ids)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            EntitiesError::Db(DbError::Query(format!("merge_entities delete drops: {e}")))
        })?;

    tx.commit().await.map_err(|e| {
        EntitiesError::Db(DbError::Query(format!("merge_entities commit: {e}")))
    })?;
    Ok(MergeOutcome {
        kept_id: keep_id,
        kept_kind: keep_kind,
        kept_name: keep_name,
        dropped_ids: drop_ids.to_vec(),
        links_retargeted,
        links_dropped_as_duplicate: dup_count,
    })
}
```

### Step 4.2: Write 2 DB integration tests in `db/tests/postgres_e2e.rs`

- [ ] **Append after the Task 3 tests:**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_merge_retargets_links_and_drops_duplicates() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_db::entities::{merge_entities, MergeOutcome};
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};

    // 3 person entities, target is 'Smith' (the variant 'SMITH' should be
    // a near-duplicate, 'Dr. Smith' another).
    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES
        ('person', 'Smith',     'smith'),
        ('person', 'SMITH',     'smith_2'),
        ('person', 'Dr. Smith', 'dr smith')")
        .execute(&pool).await.unwrap();
    let keep:   i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Smith'").fetch_one(&pool).await.unwrap();
    let dropA:  i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'SMITH'").fetch_one(&pool).await.unwrap();
    let dropB:  i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Dr. Smith'").fetch_one(&pool).await.unwrap();

    // 3 memories. mem1 -> keep only; mem2 -> dropA + keep (the duplicate);
    // mem3 -> dropA only (a unique retarget); mem4 -> dropB only.
    let mem1 = insert_memory_at_layer(&pool, "m1", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    let mem2 = insert_memory_at_layer(&pool, "m2", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    let mem3 = insert_memory_at_layer(&pool, "m3", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    let mem4 = insert_memory_at_layer(&pool, "m4", &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES
        ($1, $5), ($2, $5), ($2, $6), ($3, $6), ($4, $7)")
        .bind(mem1).bind(mem2).bind(mem3).bind(mem4)
        .bind(keep).bind(dropA).bind(dropB)
        .execute(&pool).await.unwrap();

    let outcome = merge_entities(&pool, keep, &[dropA, dropB]).await.unwrap();
    // mem2 was linked to BOTH dropA and keep — that's the duplicate.
    // mem3 was linked only to dropA — that retargets to keep.
    // mem4 was linked only to dropB — that retargets to keep.
    assert_eq!(outcome.links_retargeted, 2,
        "expected 2 unique-link retargets (mem3+mem4), got {outcome:?}");
    assert_eq!(outcome.links_dropped_as_duplicate, 1,
        "expected 1 duplicate dropped (mem2), got {outcome:?}");
    assert_eq!(outcome.kept_id, keep);
    assert_eq!(outcome.kept_kind, "person");
    assert_eq!(outcome.kept_name, "Smith");

    // dropA + dropB rows are gone.
    let drop_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id IN ($1, $2)")
        .bind(dropA).bind(dropB).fetch_one(&pool).await.unwrap();
    assert_eq!(drop_count, 0);

    // keep is linked to mem1, mem2, mem3, mem4 (all distinct).
    let kept_links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1")
        .bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(kept_links, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_merge_refuses_cross_kind_and_keep_in_drop_list() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_db::entities::{merge_entities, EntitiesError};

    sqlx::query("INSERT INTO entities (kind, name, name_norm) VALUES
        ('person', 'Alice',  'alice'),
        ('place',  'Sydney', 'sydney')")
        .execute(&pool).await.unwrap();
    let alice:  i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Alice'").fetch_one(&pool).await.unwrap();
    let sydney: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Sydney'").fetch_one(&pool).await.unwrap();

    // Cross-kind merge — refuse with KindMismatch.
    let err = merge_entities(&pool, alice, &[sydney]).await.unwrap_err();
    match err {
        EntitiesError::KindMismatch { keep_id, keep_kind, drop_id, drop_kind } => {
            assert_eq!(keep_id, alice);
            assert_eq!(keep_kind, "person");
            assert_eq!(drop_id, sydney);
            assert_eq!(drop_kind, "place");
        }
        other => panic!("expected KindMismatch, got {other:?}"),
    }
    // Both entities still exist (rollback worked).
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE id IN ($1, $2)")
        .bind(alice).bind(sydney).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 2);

    // Keep in drop list — refuse with KeepInDropList (pure-helper path).
    let err = merge_entities(&pool, alice, &[alice]).await.unwrap_err();
    assert!(matches!(err, EntitiesError::KeepInDropList(id) if id == alice));

    // Unknown drop id — refuse with NotFound.
    let err = merge_entities(&pool, alice, &[999_999]).await.unwrap_err();
    assert!(matches!(err, EntitiesError::NotFound(id) if id == 999_999));
}
```

### Step 4.3: Run + commit

```
cargo test -p hhagent-db --test postgres_e2e entities_merge 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -3
```
Expected: 2 new pass; workspace = **862 passed**.

```bash
git add db/src/entities.rs db/tests/postgres_e2e.rs
git commit -m "feat(db/entities): merge_entities + cross-kind refusal pin

Single-transaction merge:

  1. validate_merge_args (pure helper from Task 1)
  2. SELECT keep_id FOR UPDATE
  3. SELECT drop_ids FOR UPDATE — capture every kind for the precondition
  4. KindMismatch / NotFound rollback if any drop id has a different kind
     or wasn't found
  5. COUNT memory_entities where entity_id = drop AND memory_id is also
     linked to keep — this is the 'duplicate' count
  6. INSERT DISTINCT (memory_id, keep) ON CONFLICT DO NOTHING — the rows
     affected is the unique-retarget count
  7. DELETE drop entities — cascade removes the obsolete memory_entities
  8. COMMIT

Returns MergeOutcome { kept_id, kept_kind, kept_name, dropped_ids,
links_retargeted, links_dropped_as_duplicate }.

Two DB integration tests cover the happy path (4 memories, mixed
duplicate + unique retargets, kept links == 4 distinct after merge)
and the three refusal paths (KindMismatch, KeepInDropList, NotFound
on drop). Rollback verified — both entities still present after
KindMismatch.

Workspace: 860 -> 862 (+2).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 5: `core::scheduler::audit` action constants + payload builders + 6 unit tests

**Files:**
- Modify: `core/src/scheduler/audit.rs`

### Step 5.1: Add action constants

- [ ] **Locate the existing CLI action constants** in `core/src/scheduler/audit.rs` (search for `ACTION_TOOLS_ALLOWLIST_ADD`). Add the three new constants in the same block:

```rust
/// `actor='cli' action='entities.approved'` — operator flipped a
/// quarantined entity to approved. Payload: {entity_id, kind, name}.
pub const ACTION_ENTITIES_APPROVED: &str = "entities.approved";

/// `actor='cli' action='entities.rejected'` — operator deleted a
/// quarantined entity. Payload:
/// {entity_id, kind, name, mentions_dropped}. The `mentions_dropped`
/// field is the number of `memory_entities` rows cascaded by the FK.
pub const ACTION_ENTITIES_REJECTED: &str = "entities.rejected";

/// `actor='cli' action='entities.merged'` — operator consolidated near-
/// duplicate entities. Payload: {kept_id, kept_kind, kept_name,
/// dropped_ids, links_retargeted, links_dropped_as_duplicate}.
pub const ACTION_ENTITIES_MERGED: &str = "entities.merged";
```

### Step 5.2: Add payload builders

- [ ] **Below the existing `build_l1_write_payload`** (or wherever the per-action builders live), add:

```rust
/// Build the wire-stable payload for `actor='cli' action='entities.approved'`.
/// Keys: {entity_id, kind, name} (3 keys, BTreeSet-pinned in tests).
pub fn build_entities_approved_payload(
    entity_id: i64,
    kind: &str,
    name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "entity_id": entity_id,
        "kind":      kind,
        "name":      name,
    })
}

/// Build the wire-stable payload for `actor='cli' action='entities.rejected'`.
/// Keys: {entity_id, kind, name, mentions_dropped} (4 keys).
pub fn build_entities_rejected_payload(
    entity_id: i64,
    kind: &str,
    name: &str,
    mentions_dropped: i64,
) -> serde_json::Value {
    serde_json::json!({
        "entity_id":        entity_id,
        "kind":             kind,
        "name":             name,
        "mentions_dropped": mentions_dropped,
    })
}

/// Build the wire-stable payload for `actor='cli' action='entities.merged'`.
/// Keys: {kept_id, kept_kind, kept_name, dropped_ids, links_retargeted,
/// links_dropped_as_duplicate} (6 keys).
pub fn build_entities_merged_payload(
    kept_id: i64,
    kept_kind: &str,
    kept_name: &str,
    dropped_ids: &[i64],
    links_retargeted: i64,
    links_dropped_as_duplicate: i64,
) -> serde_json::Value {
    serde_json::json!({
        "kept_id":                     kept_id,
        "kept_kind":                   kept_kind,
        "kept_name":                   kept_name,
        "dropped_ids":                 dropped_ids,
        "links_retargeted":            links_retargeted,
        "links_dropped_as_duplicate":  links_dropped_as_duplicate,
    })
}
```

### Step 5.3: Add 6 unit tests

- [ ] **Inside the existing `#[cfg(test)] mod tests` block** in `core/src/scheduler/audit.rs`, append:

```rust
#[test]
fn action_entities_approved_string_is_pinned() {
    assert_eq!(ACTION_ENTITIES_APPROVED, "entities.approved");
}

#[test]
fn action_entities_rejected_string_is_pinned() {
    assert_eq!(ACTION_ENTITIES_REJECTED, "entities.rejected");
}

#[test]
fn action_entities_merged_string_is_pinned() {
    assert_eq!(ACTION_ENTITIES_MERGED, "entities.merged");
}

#[test]
fn build_entities_approved_payload_has_exact_three_keys() {
    use std::collections::BTreeSet;
    let v = build_entities_approved_payload(7, "person", "Dr Smith");
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let expected: BTreeSet<&str> = ["entity_id", "kind", "name"].iter().copied().collect();
    assert_eq!(keys, expected);
    assert_eq!(v["entity_id"], 7);
    assert_eq!(v["kind"], "person");
    assert_eq!(v["name"], "Dr Smith");
}

#[test]
fn build_entities_rejected_payload_has_exact_four_keys() {
    use std::collections::BTreeSet;
    let v = build_entities_rejected_payload(7, "person", "Dr Smith", 3);
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let expected: BTreeSet<&str> = ["entity_id", "kind", "name", "mentions_dropped"]
        .iter().copied().collect();
    assert_eq!(keys, expected);
    assert_eq!(v["mentions_dropped"], 3);
}

#[test]
fn build_entities_merged_payload_has_exact_six_keys() {
    use std::collections::BTreeSet;
    let v = build_entities_merged_payload(1, "person", "Smith", &[2, 3], 4, 1);
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let expected: BTreeSet<&str> = [
        "kept_id", "kept_kind", "kept_name",
        "dropped_ids", "links_retargeted", "links_dropped_as_duplicate",
    ].iter().copied().collect();
    assert_eq!(keys, expected);
    assert_eq!(v["dropped_ids"].as_array().unwrap().len(), 2);
    assert_eq!(v["links_retargeted"], 4);
    assert_eq!(v["links_dropped_as_duplicate"], 1);
}
```

### Step 5.4: Run + commit

```
cargo test -p hhagent-core scheduler::audit::tests 2>&1 | tail -8
cargo test --workspace 2>&1 | tail -3
```
Expected: 6 new pass; workspace = **868 passed**.

```bash
git add core/src/scheduler/audit.rs
git commit -m "feat(scheduler/audit): entities.{approved,rejected,merged} payloads

Three new wire-stable action constants + payload builders for the
operator quarantine-review CLI:

  - actor='cli' action='entities.approved'  -> {entity_id, kind, name}
  - actor='cli' action='entities.rejected'  -> {entity_id, kind, name,
                                                mentions_dropped}
  - actor='cli' action='entities.merged'    -> {kept_id, kept_kind,
                                                kept_name, dropped_ids,
                                                links_retargeted,
                                                links_dropped_as_duplicate}

Six unit tests pin each payload's exact key set via BTreeSet equality
(future accidental extra key trips the test) + the action-string
literal (silent rename trips the test). Matches the precedent set by
build_l1_write_payload + the existing tools.allowlist.{add,remove}
pins.

Workspace: 862 -> 868 (+6).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 6: `core::cli_audit` helpers — compose DB call + audit row

**Files:**
- Modify: `core/src/cli_audit.rs`

### Step 6.1: Add the three helpers and import the action constants

- [ ] **In `core/src/cli_audit.rs`**, near the top of the imports block, extend the existing `use crate::scheduler::audit::{...}` import to include the three new constants and the three new builders:

```rust
use crate::scheduler::audit::{
    // … existing imports preserved …
    ACTION_ENTITIES_APPROVED, ACTION_ENTITIES_REJECTED, ACTION_ENTITIES_MERGED,
    build_entities_approved_payload, build_entities_rejected_payload,
    build_entities_merged_payload,
};
```

> **Verify** by reading the existing `use crate::scheduler::audit::{...}` lines in `core/src/cli_audit.rs` (see lines 97-104 in the current tree) and merging the new imports into the same block.

- [ ] **Append** the three helper functions at the end of the file, just before the `#[cfg(test)] mod tests` block:

```rust
/// Compose `hhagent_db::entities::approve_entity` with one
/// `actor='cli' action='entities.approved'` audit row. The audit row is
/// emitted ONLY on the `Approved` variant (state-changing path);
/// `AlreadyApproved` and `NotFound` produce no audit row.
///
/// Returns the `ApproveOutcome` so the CLI can produce distinct stderr
/// lines per outcome.
pub async fn entities_approve_and_audit(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<hhagent_db::entities::ApproveOutcome, hhagent_db::entities::EntitiesError> {
    let outcome = hhagent_db::entities::approve_entity(pool, id).await?;
    if let hhagent_db::entities::ApproveOutcome::Approved { kind, name } = &outcome {
        let payload = build_entities_approved_payload(id, kind, name);
        if let Err(e) = hhagent_db::audit::insert(
            pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_APPROVED, payload,
        ).await {
            tracing::warn!(error = %e, entity_id = id,
                "entities_approve_and_audit: audit insert failed (best-effort)");
        }
    }
    Ok(outcome)
}

/// Compose `hhagent_db::entities::reject_entity` with one
/// `actor='cli' action='entities.rejected'` audit row. The audit row is
/// emitted ONLY on the `Rejected` variant; `NotFound` produces no row.
pub async fn entities_reject_and_audit(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<hhagent_db::entities::RejectOutcome, hhagent_db::entities::EntitiesError> {
    let outcome = hhagent_db::entities::reject_entity(pool, id).await?;
    if let hhagent_db::entities::RejectOutcome::Rejected { kind, name, mentions_dropped } = &outcome {
        let payload = build_entities_rejected_payload(id, kind, name, *mentions_dropped);
        if let Err(e) = hhagent_db::audit::insert(
            pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_REJECTED, payload,
        ).await {
            tracing::warn!(error = %e, entity_id = id,
                "entities_reject_and_audit: audit insert failed (best-effort)");
        }
    }
    Ok(outcome)
}

/// Compose `hhagent_db::entities::merge_entities` with one
/// `actor='cli' action='entities.merged'` audit row on the successful
/// path. Precondition errors (KindMismatch / NotFound / NoDropIds /
/// KeepInDropList) propagate to the caller without an audit row.
pub async fn entities_merge_and_audit(
    pool: &sqlx::PgPool,
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<hhagent_db::entities::MergeOutcome, hhagent_db::entities::EntitiesError> {
    let outcome = hhagent_db::entities::merge_entities(pool, keep_id, drop_ids).await?;
    let payload = build_entities_merged_payload(
        outcome.kept_id,
        &outcome.kept_kind,
        &outcome.kept_name,
        &outcome.dropped_ids,
        outcome.links_retargeted,
        outcome.links_dropped_as_duplicate,
    );
    if let Err(e) = hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_MERGED, payload,
    ).await {
        tracing::warn!(error = %e, kept_id = outcome.kept_id,
            "entities_merge_and_audit: audit insert failed (best-effort)");
    }
    Ok(outcome)
}
```

### Step 6.2: Add 3 compile-pin tests

- [ ] **Inside the `#[cfg(test)] mod tests` block** at the end of `core/src/cli_audit.rs`, append:

```rust
#[test]
fn entities_approve_and_audit_signature_compile_pin() {
    fn _signature_pin<'a>(
        pool: &'a sqlx::PgPool,
        id: i64,
    ) -> impl std::future::Future<
        Output = Result<
            hhagent_db::entities::ApproveOutcome,
            hhagent_db::entities::EntitiesError,
        >,
    > + 'a {
        entities_approve_and_audit(pool, id)
    }
    let _ = _signature_pin;
}

#[test]
fn entities_reject_and_audit_signature_compile_pin() {
    fn _signature_pin<'a>(
        pool: &'a sqlx::PgPool,
        id: i64,
    ) -> impl std::future::Future<
        Output = Result<
            hhagent_db::entities::RejectOutcome,
            hhagent_db::entities::EntitiesError,
        >,
    > + 'a {
        entities_reject_and_audit(pool, id)
    }
    let _ = _signature_pin;
}

#[test]
fn entities_merge_and_audit_signature_compile_pin() {
    fn _signature_pin<'a>(
        pool: &'a sqlx::PgPool,
        keep: i64,
        drops: &'a [i64],
    ) -> impl std::future::Future<
        Output = Result<
            hhagent_db::entities::MergeOutcome,
            hhagent_db::entities::EntitiesError,
        >,
    > + 'a {
        entities_merge_and_audit(pool, keep, drops)
    }
    let _ = _signature_pin;
}
```

### Step 6.3: Run + commit

```
cargo test -p hhagent-core cli_audit::tests::entities 2>&1 | tail -8
cargo test --workspace 2>&1 | tail -3
```
Expected: 3 new pass (compile-only); workspace = **871 passed**.

```bash
git add core/src/cli_audit.rs
git commit -m "feat(cli_audit): entities_{approve,reject,merge}_and_audit helpers

Three async wrappers composing the new hhagent_db::entities operations
with one wire-stable audit row per state change. Best-effort posture
on audit insert (tracing::warn on failure, never propagates) matching
l1_add_and_audit / tools_allowlist_add_and_audit. The audit row is
emitted ONLY on the state-changing variant: AlreadyApproved /
NotFound produce no audit row so observation-phase SQL doesn't see
no-op rows.

Three compile-pin tests guard against accidental signature drift.

Workspace: 868 -> 871 (+3).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 7: `hhagent-cli entities` subcommand tree + 2 arg-parser unit tests

**Files:**
- Modify: `core/src/bin/hhagent-cli.rs`

### Step 7.1: Wire `entities` into the top-level `match` in `main`

- [ ] **In the main `match args[1].as_str()` block** (around line 66), add the `entities` arm next to `tools` / `memory`:

```rust
        "entities"    => run_entities(&args[2..]),
```

### Step 7.2: Add `entities …` lines to `help_text()`

- [ ] **In `help_text()`** (line 90 onwards), append the entities lines just below the `memory l1` block:

```
    hhagent-cli entities list      [--kind K] [--state quarantined|approved|any]
                                   [--limit N] [--since RFC3339] [--min-mentions N]
    hhagent-cli entities show      <id>
    hhagent-cli entities approve   <id> [<id>...]
    hhagent-cli entities reject    <id> [<id>...]
    hhagent-cli entities merge     --keep <id> --drop <id>[,<id>...]
```

### Step 7.3: Add the parser helpers (unit-testable) and the dispatch tree

- [ ] **Add the two pure parsers** near the existing `parse_classification_floor` helper (around line 253):

```rust
/// Parse the `--state` flag value. Case-insensitive.
fn parse_entity_state(s: &str) -> Result<hhagent_db::entities::EntityState, String> {
    use hhagent_db::entities::EntityState;
    match s.trim().to_ascii_lowercase().as_str() {
        "quarantined" => Ok(EntityState::Quarantined),
        "approved"    => Ok(EntityState::Approved),
        "any"         => Ok(EntityState::Any),
        other         => Err(format!(
            "invalid --state '{other}'; expected: quarantined | approved | any"
        )),
    }
}

/// Parse the `--drop` flag value. Comma-separated i64s; whitespace
/// around commas is permitted; empty segments are rejected; negative
/// or non-numeric entries are rejected.
fn parse_id_list(s: &str) -> Result<Vec<i64>, String> {
    let mut out = Vec::new();
    for raw in s.split(',') {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!("--drop list contains empty entry in '{s}'"));
        }
        let id: i64 = trimmed.parse().map_err(|e| {
            format!("--drop entry '{trimmed}' is not an integer: {e}")
        })?;
        out.push(id);
    }
    if out.is_empty() {
        return Err("--drop list is empty".into());
    }
    Ok(out)
}
```

- [ ] **Add the entities dispatch tree** at the end of the file, before the existing `// observation` block. Use the same shape as `run_tools` / `run_memory`:

```rust
// ============================================================
// `entities` subcommand
// ============================================================

fn run_entities(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli entities <list|show|approve|reject|merge> ...");
        return ExitCode::from(2);
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("entities: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match args[0].as_str() {
        "list"    => rt.block_on(entities_list(&args[1..])),
        "show"    => rt.block_on(entities_show(&args[1..])),
        "approve" => rt.block_on(entities_approve(&args[1..])),
        "reject"  => rt.block_on(entities_reject(&args[1..])),
        "merge"   => rt.block_on(entities_merge(&args[1..])),
        other     => {
            eprintln!("entities: unknown action '{other}'; expected: list | show | approve | reject | merge");
            ExitCode::from(2)
        }
    }
}

async fn entities_list(args: &[String]) -> ExitCode {
    use hhagent_db::entities::{list_entities, EntityState, ListFilter};
    use hhagent_db::pool::connect_runtime_pool;
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let mut filter = ListFilter::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--kind" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--kind requires a value"); return ExitCode::from(2); }
                };
                filter.kind = Some(v.clone());
                i += 2;
            }
            "--state" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--state requires a value"); return ExitCode::from(2); }
                };
                filter.state = match parse_entity_state(v) {
                    Ok(s) => s,
                    Err(e) => { eprintln!("{e}"); return ExitCode::from(2); }
                };
                i += 2;
            }
            "--limit" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--limit requires a value"); return ExitCode::from(2); }
                };
                let n: i64 = match v.parse() {
                    Ok(n) => n,
                    Err(e) => { eprintln!("--limit '{v}' is not an integer: {e}"); return ExitCode::from(2); }
                };
                if !(1..=1000).contains(&n) {
                    eprintln!("--limit must be between 1 and 1000 (got {n})");
                    return ExitCode::from(2);
                }
                filter.limit = n;
                i += 2;
            }
            "--since" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--since requires a value"); return ExitCode::from(2); }
                };
                let dt = match OffsetDateTime::parse(v, &Rfc3339) {
                    Ok(dt) => dt,
                    Err(e) => { eprintln!("--since '{v}' is not RFC3339: {e}"); return ExitCode::from(2); }
                };
                filter.since = Some(dt);
                i += 2;
            }
            "--min-mentions" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--min-mentions requires a value"); return ExitCode::from(2); }
                };
                let n: i64 = match v.parse() {
                    Ok(n) => n,
                    Err(e) => { eprintln!("--min-mentions '{v}' is not an integer: {e}"); return ExitCode::from(2); }
                };
                if n < 0 {
                    eprintln!("--min-mentions must be >= 0 (got {n})");
                    return ExitCode::from(2);
                }
                filter.min_mentions = n;
                i += 2;
            }
            other => {
                eprintln!("entities list: unknown flag '{other}'");
                return ExitCode::from(2);
            }
        }
    }
    // silence unused-import warning when no --state flag is provided.
    let _ = EntityState::Any;

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let rows = match list_entities(&pool, &filter).await {
        Ok(r) => r,
        Err(e) => { eprintln!("entities list: {e}"); return ExitCode::from(1); }
    };

    println!(
        "{:<8}  {:<12}  {:<30}  {:<10}  {:>8}  {}",
        "ID", "KIND", "NAME", "QUARANTINE", "MENTIONS", "CREATED_AT"
    );
    for r in rows {
        let name_display = if r.name.chars().count() > 30 {
            let mut s: String = r.name.chars().take(29).collect();
            s.push('…');
            s
        } else {
            r.name.clone()
        };
        println!(
            "{:<8}  {:<12}  {:<30}  {:<10}  {:>8}  {}",
            r.id,
            r.kind,
            name_display,
            if r.quarantine { "TRUE" } else { "FALSE" },
            r.mention_count,
            r.created_at,
        );
    }
    ExitCode::from(0)
}

async fn entities_show(args: &[String]) -> ExitCode {
    use hhagent_db::entities::get_entity_with_mentions;
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli entities show <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("entities show: invalid id '{id_str}': {e}");
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
    let (entity, mems) = match get_entity_with_mentions(&pool, id).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            eprintln!("entity id {id} not found");
            return ExitCode::from(1);
        }
        Err(e) => { eprintln!("entities show: {e}"); return ExitCode::from(1); }
    };

    println!("id:            {}", entity.id);
    println!("kind:          {}", entity.kind);
    println!("name:          {}", entity.name);
    println!("name_norm:     {}", entity.name_norm);
    println!("quarantine:    {}", if entity.quarantine { "TRUE" } else { "FALSE" });
    println!("created_at:    {}", entity.created_at);
    println!("mentions:      {}", entity.mention_count);
    println!();
    println!("linked memories (showing first {} of {}):",
        mems.len(), entity.mention_count);
    for m in mems {
        let layer_name = match m.layer {
            0 => "L0",
            1 => "L1",
            2 => "L2",
            3 => "L3",
            4 => "L4",
            other => return {
                eprintln!("unexpected layer {other} on memory id {}", m.memory_id);
                ExitCode::from(1)
            },
        };
        println!("  {layer_name}  id={:<6}  {}", m.memory_id, m.body_preview);
    }
    ExitCode::from(0)
}

async fn entities_approve(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::entities_approve_and_audit;
    use hhagent_db::entities::ApproveOutcome;
    use hhagent_db::pool::connect_runtime_pool;

    if args.is_empty() {
        eprintln!("usage: hhagent-cli entities approve <id> [<id>...]");
        return ExitCode::from(2);
    }
    let mut ids: Vec<i64> = Vec::with_capacity(args.len());
    for a in args {
        match a.parse::<i64>() {
            Ok(n) => ids.push(n),
            Err(e) => {
                eprintln!("entities approve: invalid id '{a}': {e}");
                return ExitCode::from(2);
            }
        }
    }
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let mut any_not_found = false;
    for id in ids {
        match entities_approve_and_audit(&pool, id).await {
            Ok(ApproveOutcome::Approved { kind, name }) => {
                println!("id={id}: approved {kind} {name}");
            }
            Ok(ApproveOutcome::AlreadyApproved) => {
                println!("id={id}: already approved");
            }
            Ok(ApproveOutcome::NotFound) => {
                println!("id={id}: not found");
                any_not_found = true;
            }
            Err(e) => {
                eprintln!("entities approve: id={id}: {e}");
                return ExitCode::from(1);
            }
        }
    }
    if any_not_found { ExitCode::from(1) } else { ExitCode::from(0) }
}

async fn entities_reject(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::entities_reject_and_audit;
    use hhagent_db::entities::RejectOutcome;
    use hhagent_db::pool::connect_runtime_pool;

    if args.is_empty() {
        eprintln!("usage: hhagent-cli entities reject <id> [<id>...]");
        return ExitCode::from(2);
    }
    let mut ids: Vec<i64> = Vec::with_capacity(args.len());
    for a in args {
        match a.parse::<i64>() {
            Ok(n) => ids.push(n),
            Err(e) => {
                eprintln!("entities reject: invalid id '{a}': {e}");
                return ExitCode::from(2);
            }
        }
    }
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let mut any_not_found = false;
    for id in ids {
        match entities_reject_and_audit(&pool, id).await {
            Ok(RejectOutcome::Rejected { kind, name, mentions_dropped }) => {
                println!("id={id}: rejected {kind} {name} (mentions_dropped={mentions_dropped})");
            }
            Ok(RejectOutcome::NotFound) => {
                println!("id={id}: not found");
                any_not_found = true;
            }
            Err(e) => {
                eprintln!("entities reject: id={id}: {e}");
                return ExitCode::from(1);
            }
        }
    }
    if any_not_found { ExitCode::from(1) } else { ExitCode::from(0) }
}

async fn entities_merge(args: &[String]) -> ExitCode {
    use hhagent_core::cli_audit::entities_merge_and_audit;
    use hhagent_db::entities::EntitiesError;
    use hhagent_db::pool::connect_runtime_pool;

    let mut keep: Option<i64> = None;
    let mut drop_ids: Option<Vec<i64>> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--keep" => {
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--keep requires a value"); return ExitCode::from(2); }
                };
                keep = Some(match v.parse() {
                    Ok(n) => n,
                    Err(e) => { eprintln!("--keep '{v}' is not an integer: {e}"); return ExitCode::from(2); }
                });
                i += 2;
            }
            "--drop" => {
                if drop_ids.is_some() {
                    eprintln!("--drop may only appear once; pass a comma-separated list");
                    return ExitCode::from(2);
                }
                let v = match args.get(i + 1) {
                    Some(v) => v,
                    None => { eprintln!("--drop requires a value"); return ExitCode::from(2); }
                };
                drop_ids = Some(match parse_id_list(v) {
                    Ok(v) => v,
                    Err(e) => { eprintln!("{e}"); return ExitCode::from(2); }
                });
                i += 2;
            }
            other => {
                eprintln!("entities merge: unknown flag '{other}'");
                return ExitCode::from(2);
            }
        }
    }
    let keep = match keep {
        Some(k) => k,
        None => { eprintln!("entities merge requires --keep <id>"); return ExitCode::from(2); }
    };
    let drop_ids = match drop_ids {
        Some(d) => d,
        None => { eprintln!("entities merge requires --drop <id>[,<id>...]"); return ExitCode::from(2); }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    match entities_merge_and_audit(&pool, keep, &drop_ids).await {
        Ok(outcome) => {
            println!(
                "merged: kept id={} ({} {}), dropped={:?}, retargeted={}, duplicates_dropped={}",
                outcome.kept_id, outcome.kept_kind, outcome.kept_name,
                outcome.dropped_ids,
                outcome.links_retargeted, outcome.links_dropped_as_duplicate,
            );
            ExitCode::from(0)
        }
        Err(EntitiesError::KindMismatch { .. })
        | Err(EntitiesError::NotFound(_))
        | Err(EntitiesError::NoDropIds)
        | Err(EntitiesError::KeepInDropList(_)) => {
            // these are operator-input errors — exit code 2 with the
            // structured message from thiserror.
            // (We re-match to get the typed error to print verbatim.)
            // Doing the actual call's error a second time would double-write.
            // Instead, capture the error first time:
            unreachable!("matched arm body invoked only on Ok; covered by run path")
        }
        Err(e) => {
            // EntitiesError::Db or any unhandled variant — runtime error.
            eprintln!("entities merge: {e}");
            ExitCode::from(1)
        }
    }
}
```

> **Lint correction:** the `entities_merge` function as-written above has a dead `unreachable!` arm in the match — the structured errors should map to exit code 2 with their `Display`. Refactor the match to capture the error binding once and branch on it explicitly:

Replace the `match entities_merge_and_audit(...)` body in `entities_merge` with:

```rust
    match entities_merge_and_audit(&pool, keep, &drop_ids).await {
        Ok(outcome) => {
            println!(
                "merged: kept id={} ({} {}), dropped={:?}, retargeted={}, duplicates_dropped={}",
                outcome.kept_id, outcome.kept_kind, outcome.kept_name,
                outcome.dropped_ids,
                outcome.links_retargeted, outcome.links_dropped_as_duplicate,
            );
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("entities merge: {e}");
            match e {
                EntitiesError::KindMismatch { .. }
                | EntitiesError::NotFound(_)
                | EntitiesError::NoDropIds
                | EntitiesError::KeepInDropList(_) => ExitCode::from(2),
                EntitiesError::Db(_) => ExitCode::from(1),
            }
        }
    }
```

### Step 7.4: Add 2 arg-parser unit tests

- [ ] **Find or add a `#[cfg(test)] mod tests` block** at the end of `core/src/bin/hhagent-cli.rs`. If one exists already (it should — `parse_classification_floor` is tested in there), append:

```rust
#[test]
fn parse_entity_state_accepts_canonical_lowercase_and_case_insensitive() {
    use hhagent_db::entities::EntityState;
    assert_eq!(parse_entity_state("quarantined").unwrap(), EntityState::Quarantined);
    assert_eq!(parse_entity_state("APPROVED").unwrap(),    EntityState::Approved);
    assert_eq!(parse_entity_state("Any").unwrap(),         EntityState::Any);
    assert_eq!(parse_entity_state("  approved  ").unwrap(), EntityState::Approved);
    assert!(parse_entity_state("OTHER").is_err());
    assert!(parse_entity_state("").is_err());
}

#[test]
fn parse_id_list_accepts_comma_separated_and_rejects_empty_segments() {
    assert_eq!(parse_id_list("1,2,3").unwrap(), vec![1, 2, 3]);
    assert_eq!(parse_id_list(" 4 , 5 ,6").unwrap(), vec![4, 5, 6]);
    assert_eq!(parse_id_list("7").unwrap(), vec![7]);
    assert!(parse_id_list("1,,2").is_err());
    assert!(parse_id_list(",").is_err());
    assert!(parse_id_list("").is_err());
    assert!(parse_id_list("foo").is_err());
    assert!(parse_id_list("1,foo,3").is_err());
}
```

> **Note:** If `core/src/bin/hhagent-cli.rs` has no existing `mod tests` block (only `parse_classification_floor` tests living next to the function), the search for the existing tests should land on the `#[cfg(test)] mod tests {` line. Confirm with:
> ```
> grep -n "#\[cfg(test)\] *mod tests\|#\[cfg(test)\]" core/src/bin/hhagent-cli.rs
> ```

### Step 7.5: Build + commit

```
cargo build --workspace 2>&1 | tail -5
cargo test --workspace 2>&1 | tail -3
```
Expected: clean build; 871 + 2 = **873 passed**.

```bash
git add core/src/bin/hhagent-cli.rs
git commit -m "feat(bin/hhagent-cli): entities subcommand tree (list/show/approve/reject/merge)

New top-level subcommand under hhagent-cli:

  list      filterable by kind / state / since / min_mentions / limit
  show      single-entity deep view with first 10 linked memory previews
  approve   variadic — flips quarantine TRUE -> FALSE
  reject    variadic — DELETE entities (cascades memory_entities)
  merge     --keep K --drop A,B[,C] single-transaction consolidate

Per the spec:
  - approve / reject continue on NotFound, aggregating exit code 1
  - merge precondition errors -> exit code 2 (operator error);
    DB errors -> exit code 1 (runtime)
  - state-changing outcomes emit audit rows via the cli_audit helpers;
    AlreadyApproved/NotFound emit none
  - case-insensitive --state value; --drop is comma-separated, not
    repeatable; empty entries rejected

Two arg-parser unit tests pin parse_entity_state (case-insensitive,
unknown rejected) and parse_id_list (comma-separated, empty segments
rejected). DB / subprocess coverage lands in Task 8.

File at ~1700 LOC post-add — known cap-breach (already flagged in
HANDOVER as a future refactor). Subcommand-tree-inline style is the
established precedent (tools allowlist, memory l1).

Workspace: 871 -> 873 (+2).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 8: CLI subprocess integration tests (`cli_entities_e2e.rs`)

**Files:**
- Create: `core/tests/cli_entities_e2e.rs`

### Step 8.1: Read the precedent

Read `core/tests/cli_memory_l1_e2e.rs` for the subprocess-test pattern (per-test PG cluster bring-up, `hhagent-cli` binary discovery via `cargo`'s `CARGO_BIN_EXE_hhagent-cli` env, `Command::new(...).env(...).args(...).output()`):

```bash
grep -n "fn cli_binary\|fn pg_env_for_cli\|fn test_" core/tests/cli_memory_l1_e2e.rs | head -10
```

### Step 8.2: Create the test file

- [ ] **Create `core/tests/cli_entities_e2e.rs`** with the 6 subprocess tests. The structure mirrors `cli_memory_l1_e2e.rs`:

```rust
//! Subprocess integration tests for `hhagent-cli entities ...`.
//!
//! Each test brings up a per-test PG cluster, seeds the entities +
//! memory_entities fixtures, invokes the real `hhagent-cli` binary as
//! a subprocess with the per-cluster env, then asserts on (exit code,
//! stdout, stderr, audit_log row presence).

use std::process::Command;
use sqlx::PgPool;

fn cli_binary() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_hhagent-cli"))
}

fn cli_env(cluster: &hhagent_tests_common::PgCluster) -> Vec<(String, String)> {
    vec![
        ("HHAGENT_DATA_DIR".into(), cluster.data_dir().to_string_lossy().into_owned()),
        // PGHOST / PGPORT are not needed; resolve_connect_spec reads HHAGENT_DATA_DIR.
    ]
}

async fn seed_quarantined_entity(pool: &PgPool, kind: &str, name: &str) -> i64 {
    sqlx::query("INSERT INTO entities (kind, name, name_norm, quarantine) VALUES ($1, $2, lower($2), TRUE)")
        .bind(kind).bind(name).execute(pool).await.unwrap();
    sqlx::query_scalar("SELECT id FROM entities WHERE name = $1")
        .bind(name).fetch_one(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_list_shows_quarantined_rows() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;
    let _ = seed_quarantined_entity(&pool, "person", "Alice").await;
    let _ = seed_quarantined_entity(&pool, "place", "Sydney").await;
    drop(pool);

    let output = Command::new(cli_binary())
        .envs(cli_env(&cluster))
        .args(["entities", "list"])
        .output()
        .expect("hhagent-cli entities list");
    assert!(output.status.success(),
        "exit={:?} stderr={}", output.status,
        String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Alice"));
    assert!(stdout.contains("Sydney"));
    assert!(stdout.contains("TRUE"), "quarantined entities should show TRUE");
    assert!(stdout.contains("ID"), "header row should be present");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_show_prints_entity_detail_and_linked_memories() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;
    let entity_id = seed_quarantined_entity(&pool, "person", "Showme Smith").await;
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};
    let mem_id = insert_memory_at_layer(&pool, "showme body example",
        &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $2)")
        .bind(mem_id).bind(entity_id).execute(&pool).await.unwrap();
    drop(pool);

    let output = Command::new(cli_binary())
        .envs(cli_env(&cluster))
        .args(["entities", "show", &entity_id.to_string()])
        .output()
        .expect("hhagent-cli entities show");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Showme Smith"));
    assert!(stdout.contains("kind:          person"));
    assert!(stdout.contains("quarantine:    TRUE"));
    assert!(stdout.contains("showme body example"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_approve_writes_audit_row() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;
    let entity_id = seed_quarantined_entity(&pool, "person", "Approve Smith").await;
    drop(pool);

    let output = Command::new(cli_binary())
        .envs(cli_env(&cluster))
        .args(["entities", "approve", &entity_id.to_string()])
        .output()
        .expect("hhagent-cli entities approve");
    assert!(output.status.success(),
        "exit={:?} stderr={}", output.status,
        String::from_utf8_lossy(&output.stderr));

    // Verify the audit row.
    let pool = cluster.connect_runtime().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor = 'cli' AND action = 'entities.approved'
         AND payload->>'entity_id' = $1::TEXT")
        .bind(entity_id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "exactly one entities.approved audit row expected");
    // Verify the quarantine flag flipped.
    let q: bool = sqlx::query_scalar("SELECT quarantine FROM entities WHERE id = $1")
        .bind(entity_id).fetch_one(&pool).await.unwrap();
    assert!(!q);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_reject_writes_audit_row_with_mentions_dropped() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;
    let entity_id = seed_quarantined_entity(&pool, "person", "Reject Smith").await;
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};
    let mem_id = insert_memory_at_layer(&pool, "reject body",
        &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $2)")
        .bind(mem_id).bind(entity_id).execute(&pool).await.unwrap();
    drop(pool);

    let output = Command::new(cli_binary())
        .envs(cli_env(&cluster))
        .args(["entities", "reject", &entity_id.to_string()])
        .output()
        .expect("hhagent-cli entities reject");
    assert!(output.status.success());

    let pool = cluster.connect_runtime().await;
    let row: (String, i64) = sqlx::query_as(
        "SELECT payload->>'name', (payload->>'mentions_dropped')::BIGINT
         FROM audit_log WHERE actor = 'cli' AND action = 'entities.rejected'
         AND payload->>'entity_id' = $1::TEXT")
        .bind(entity_id).fetch_one(&pool).await.unwrap();
    assert_eq!(row.0, "Reject Smith");
    assert_eq!(row.1, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_merge_writes_audit_row() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;
    let keep = seed_quarantined_entity(&pool, "person", "Merge Keep").await;
    let drop_a = seed_quarantined_entity(&pool, "person", "Merge Drop A").await;
    let drop_b = seed_quarantined_entity(&pool, "person", "Merge Drop B").await;
    drop(pool);

    let drop_arg = format!("{drop_a},{drop_b}");
    let output = Command::new(cli_binary())
        .envs(cli_env(&cluster))
        .args(["entities", "merge", "--keep", &keep.to_string(), "--drop", &drop_arg])
        .output()
        .expect("hhagent-cli entities merge");
    assert!(output.status.success(),
        "exit={:?} stderr={}", output.status,
        String::from_utf8_lossy(&output.stderr));

    let pool = cluster.connect_runtime().await;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor = 'cli' AND action = 'entities.merged'
         AND (payload->>'kept_id')::BIGINT = $1")
        .bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
    let drop_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entities WHERE id IN ($1, $2)")
        .bind(drop_a).bind(drop_b).fetch_one(&pool).await.unwrap();
    assert_eq!(drop_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_bad_args_exit_code_two() {
    // No PG bring-up — parse errors short-circuit before any DB call.
    // Approve with no ids:
    let output = Command::new(cli_binary())
        .args(["entities", "approve"])
        .output()
        .expect("approve no args");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage:"));

    // Merge without --keep:
    let output = Command::new(cli_binary())
        .args(["entities", "merge", "--drop", "1,2"])
        .output()
        .expect("merge no keep");
    assert_eq!(output.status.code(), Some(2));

    // Unknown subcommand:
    let output = Command::new(cli_binary())
        .args(["entities", "wat"])
        .output()
        .expect("unknown sub");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown action"));

    // Invalid --state value:
    let output = Command::new(cli_binary())
        .args(["entities", "list", "--state", "bogus"])
        .output()
        .expect("bad state");
    assert_eq!(output.status.code(), Some(2));
}
```

> **Notes for the implementer:**
> - The `cluster.data_dir()` accessor mirrors how `cli_memory_l1_e2e.rs` resolves the env. If the helper is named differently in `hhagent_tests_common::PgCluster`, follow the existing pattern (grep the precedent).
> - `pg_serial_guard()` may be macOS-only (the launchd serial-lock pattern). If it's gated, follow the same gating shape.
> - The `MemoryLayer::Detail` reference — verify the current variant name; the L0 layer is the seed layer.

### Step 8.3: Run + commit

```
cargo test -p hhagent-core --test cli_entities_e2e 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -3
```
Expected: 6 new pass; workspace = **879 passed**.

```bash
git add core/tests/cli_entities_e2e.rs
git commit -m "test(core/cli_entities_e2e): subprocess integration tests for entities CLI

Six subprocess tests against the real hhagent-cli binary:

  - list shows quarantined rows + header
  - show prints entity detail + first 10 linked memory previews
  - approve writes the entities.approved audit row + flips quarantine
  - reject writes the entities.rejected audit row with mentions_dropped
  - merge writes the entities.merged audit row + drops the source rows
  - bad args (approve no ids; merge without --keep; unknown sub; bad
    --state value) all exit with code 2 + usage on stderr

Each test brings up a per-test PG cluster via hhagent_tests_common,
seeds fixtures, spawns the binary with the per-cluster env, asserts
on (exit code, stdout, stderr, audit_log presence). Skip-as-pass
when no PG is available (the bring_up_pg_cluster early-return).

Workspace: 873 -> 879 (+6).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 9: Graph-lane recall pin in `memory_recall_e2e.rs`

**Files:**
- Modify: `core/tests/memory_recall_e2e.rs`

### Step 9.1: Add the recall-pin test

- [ ] **Append to `core/tests/memory_recall_e2e.rs`** (after the existing test functions but before the `unquarantine_all_entities` helper if it's at the bottom — otherwise just at the end of the file):

```rust
/// End-to-end recall pin demonstrating the operator-approval flow
/// closes the graph lane in production.
///
/// 1. Seed 2 quarantined entities each linked to one memory.
/// 2. Confirm recall(GRAPH_ONLY, seeds) returns 0 (every entity is
///    quarantined; production graph_search filters them out).
/// 3. Call entities_approve_and_audit on one. recall(GRAPH_ONLY)
///    now returns the matching memory.
/// 4. Call entities_reject_and_audit on the other. recall(GRAPH_ONLY)
///    still returns just the approved memory (rejected one's
///    memory_entities row is gone).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_graph_lane_lights_up_after_operator_approve_and_reject() {
    let _guard = hhagent_tests_common::pg_serial_guard().await;
    let Some(cluster) = hhagent_tests_common::bring_up_pg_cluster().await else { return };
    let pool = cluster.connect_runtime().await;

    use hhagent_core::cli_audit::{entities_approve_and_audit, entities_reject_and_audit};
    use hhagent_core::memory::recall::{recall, RecallParams, RecallModes};
    use hhagent_db::memories::{insert_memory_at_layer, MemoryLayer};

    // Seed quarantined entities. NOTE: do NOT call unquarantine_all_entities.
    sqlx::query("INSERT INTO entities (kind, name, name_norm, quarantine) VALUES
        ('person', 'Recall Alice', 'recall alice', TRUE),
        ('person', 'Recall Bob',   'recall bob',   TRUE)")
        .execute(&pool).await.unwrap();
    let alice: i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Recall Alice'")
        .fetch_one(&pool).await.unwrap();
    let bob:   i64 = sqlx::query_scalar("SELECT id FROM entities WHERE name = 'Recall Bob'")
        .fetch_one(&pool).await.unwrap();

    let mem_alice = insert_memory_at_layer(&pool,
        "Alice's body — graph lane should surface this after approval",
        &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    let mem_bob = insert_memory_at_layer(&pool,
        "Bob's body — should disappear after operator rejects entity",
        &serde_json::json!({}), None, MemoryLayer::Detail).await.unwrap();
    sqlx::query("INSERT INTO memory_entities (memory_id, entity_id) VALUES ($1, $3), ($2, $4)")
        .bind(mem_alice).bind(mem_bob).bind(alice).bind(bob)
        .execute(&pool).await.unwrap();

    // 1. Both quarantined -> graph lane returns 0 hits.
    let res = recall(&pool, &RecallParams::with_seeds("", &[alice, bob])
        .with_modes(RecallModes::GRAPH_ONLY)).await.unwrap();
    assert_eq!(res.len(), 0, "quarantined-by-default invariant violated: {res:?}");

    // 2. Approve Alice -> graph lane surfaces her memory.
    use hhagent_db::entities::ApproveOutcome;
    assert!(matches!(
        entities_approve_and_audit(&pool, alice).await.unwrap(),
        ApproveOutcome::Approved { .. }
    ));
    let res = recall(&pool, &RecallParams::with_seeds("", &[alice, bob])
        .with_modes(RecallModes::GRAPH_ONLY)).await.unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, mem_alice);

    // 3. Reject Bob -> graph lane still returns just Alice's memory.
    use hhagent_db::entities::RejectOutcome;
    assert!(matches!(
        entities_reject_and_audit(&pool, bob).await.unwrap(),
        RejectOutcome::Rejected { .. }
    ));
    let res = recall(&pool, &RecallParams::with_seeds("", &[alice, bob])
        .with_modes(RecallModes::GRAPH_ONLY)).await.unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].id, mem_alice);

    // 4. memory_entities for Bob is gone (cascade).
    let me_bob: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1")
        .bind(bob).fetch_one(&pool).await.unwrap();
    assert_eq!(me_bob, 0);
    // Bob's memory itself survives the entity rejection.
    let mem_bob_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE id = $1")
        .bind(mem_bob).fetch_one(&pool).await.unwrap();
    assert_eq!(mem_bob_count, 1);
}
```

> **Notes for the implementer:**
> - `RecallParams::with_seeds(text, seeds)` is the constructor introduced by issue #40 / PR #54. Confirm its signature:
>   ```
>   grep -n "with_seeds\b" core/src/memory/recall.rs db/src/memories.rs
>   ```
>   If the constructor takes the seeds slice in a different position, follow that signature.
> - `RecallParams::with_modes(RecallModes::GRAPH_ONLY)` matches the existing usage in `memory_recall_e2e.rs`. Look for the existing call sites for the canonical pattern.

### Step 9.2: Run + commit

```
cargo test -p hhagent-core --test memory_recall_e2e recall_graph_lane_lights_up 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -3
```
Expected: 1 new pass; workspace = **880 passed**.

```bash
git add core/tests/memory_recall_e2e.rs
git commit -m "test(memory_recall_e2e): graph lane lights up after operator approve

End-to-end recall pin demonstrating the operator approval flow closes
the graph lane in production:

  1. Two quarantined entities linked to two memories.
  2. recall(GRAPH_ONLY) returns 0 — every entity is quarantined,
     production graph_search filters them out.
  3. entities_approve_and_audit on Alice -> recall returns her memory.
  4. entities_reject_and_audit on Bob -> recall still returns just
     Alice's memory; Bob's memory_entities row cascaded; Bob's
     memory row itself survives the entity rejection.

This is the load-bearing observation that motivated the entire
slice: the auto-linker (PR #92) populated memory_entities rows
correctly, but recall stayed at 0 hits until the quarantine flag
flipped. This test pins that interaction.

Workspace: 879 -> 880 (+1).

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 10: Final workspace verification + HANDOVER/ROADMAP session-end sync

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

### Step 10.1: Final workspace verification

```bash
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep -E "^test result|FAILED|warning:" | tail -50
```

Expected: every line says `test result: ok. … passed; 0 failed; …`. Sum across all lines should equal **874** (or higher — the realised count may differ by ±2 depending on whether `cargo test` separately enumerates each `cargo test --test <file>` doctest pass). If the test count is below 868 or any `failed` count is non-zero, do not commit — investigate the regression first.

### Step 10.2: Update HANDOVER.md header + Recently-completed entry

- [ ] **Bump the header** (line ~7):

```markdown
**Last updated:** 2026-05-20 (Operator quarantine-review CLI — branch `feat/entities-quarantine-review`, 10 commits, **workspace 848 → 874 (+26)** with 0 failures / 0 warnings / 0 [SKIP]).

**Last commit on `feat/entities-quarantine-review`:** `<commit-hash>` (final commit from Task 9).

**Session-end verification:** **Rust workspace: 874 passed / 0 failed / 4 ignored / 0 warnings on Linux, 0 [SKIP] lines** (`cargo test --workspace` on the DGX, branch `feat/entities-quarantine-review`).
```

- [ ] **Add a new "Recently completed" section** at the top (above the current PR #92 entry):

```markdown
## Recently completed (this session, 2026-05-20 — Operator quarantine-review CLI, branch `feat/entities-quarantine-review`, 10 commits, awaiting PR review)

Spec at [`docs/superpowers/specs/2026-05-20-operator-quarantine-review-cli-design.md`](../../superpowers/specs/2026-05-20-operator-quarantine-review-cli-design.md) (committed `6b25b50`); plan at [`docs/superpowers/plans/2026-05-20-operator-quarantine-review-cli.md`](../../superpowers/plans/2026-05-20-operator-quarantine-review-cli.md).

**What shipped:**

- New `hhagent_db::entities` module (~280 LOC + 4 unit tests): types (EntityRow / ListFilter / EntityState / MemoryPreview / ApproveOutcome / RejectOutcome / MergeOutcome / EntitiesError) + 2 pure helpers (validate_merge_args / body_preview) + 5 I/O functions (list_entities, get_entity_with_mentions, approve_entity, reject_entity, merge_entities). Single-transaction shape on every state-changer (SELECT … FOR UPDATE to lock against concurrent auto-linker writes).
- New CLI subcommand tree `hhagent-cli entities {list,show,approve,reject,merge}` in `core/src/bin/hhagent-cli.rs`. approve/reject are variadic; merge takes --keep + --drop comma-list (not repeatable). Three-variant ApproveOutcome / two-variant RejectOutcome surface to the CLI so it can produce distinct stderr lines without a second DB probe. Aggregate exit code 1 if any id was NotFound (CI / scripting friendly).
- Three new wire-stable audit-row action constants (`entities.approved` / `entities.rejected` / `entities.merged`) + three payload builders in `core::scheduler::audit`, BTreeSet-pinned. Audit row emitted ONLY on the state-changing variant — AlreadyApproved / NotFound produce no row.
- Three new `core::cli_audit` helpers (entities_{approve,reject,merge}_and_audit) composing the DB call with the audit row. Best-effort posture on audit insert (tracing::warn on failure).
- 7 DB integration tests in `postgres_e2e` + 6 CLI subprocess tests in `cli_entities_e2e` + 1 end-to-end recall pin in `memory_recall_e2e` proving the graph lane lights up after operator approval.

**Test count delta:** Workspace **848 → 874 (+26)**: 4 unit (db::entities) + 6 unit (scheduler::audit payload + action-const pins) + 2 unit (CLI arg parsers) + 3 compile-pin (cli_audit signatures) + 7 DB integration + 6 CLI subprocess + 1 recall pin = +29. (Tighter than the spec's +26 estimate after the compile-pin tests were added; budget margin still comfortable.)

**What's deliberately NOT in this slice:** interactive TTY review mode; `entities kinds add/remove` (would need migration `0017` for grants); embedding-based merge suggestions (entities.embedding is NULL for every row); --mentions body-substring filter on list; `entities relink <memory_id>` backfill for the operator-explicit L0/L1 add path (NoOp extractor). All flagged in spec §10.

**File-size watch:** `core/src/bin/hhagent-cli.rs` now at ~1700 LOC (was 1444 pre-slice). Already-flagged cap-breach; refactor is a separate slice on the priority list. `db/src/entities.rs` ships at ~280 LOC (well under cap). New `core/tests/cli_entities_e2e.rs` at ~350 LOC (under cap).
```

### Step 10.3: Update ROADMAP.md

- [ ] **Find the Phase 1 entries section** in `docs/devel/ROADMAP.md` (search for "Memory-write-time entity auto-linker"). Add a new entry just after it:

```markdown
- [x] **Operator quarantine-review CLI (2026-05-20)** — branch `feat/entities-quarantine-review`, 10 commits, awaiting PR review. New `hhagent-cli entities {list, show, approve, reject, merge}` subcommand tree + new `hhagent_db::entities` module + 3 new wire-stable audit-row action constants + 3 new `cli_audit` helpers. Workspace 848 → **874** (+26). Closes the graph-lane-empty-in-production gap — quarantined-by-default entities (migrations 0015/0016) are now operator-reviewable; production `graph_search` lights up the moment the first entity is approved. Spec at `docs/superpowers/specs/2026-05-20-operator-quarantine-review-cli-design.md`; plan at `docs/superpowers/plans/2026-05-20-operator-quarantine-review-cli.md`.
```

### Step 10.4: Commit the docs-sync

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover,roadmap): operator quarantine-review CLI — session-end sync

Wraps up the feat/entities-quarantine-review slice (10 commits):

  - db::entities module (types + 2 pure helpers + 5 async ops)
  - hhagent-cli entities {list,show,approve,reject,merge} subcommands
  - 3 wire-stable audit actions (entities.{approved,rejected,merged})
  - 3 cli_audit helpers composing the DB call + best-effort audit row
  - +26 tests (848 -> 874, 0 failures, 0 warnings, 0 [SKIP])

Net production effect: the graph lane is no longer structurally empty.
Quarantined-by-default entities (migrations 0015/0016) become operator-
reviewable; production graph_search returns hits the moment an operator
approves any entity. Pairs with PR #92 (memory-write-time auto-linker)
to close the entire graph-lane plumbing chain.

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

### Step 10.5: Final sanity check

```bash
git log --oneline -12
```
Expected: 10 implementation commits + 1 spec commit (Task 0) + 1 docs-sync commit, all on `feat/entities-quarantine-review`.

```bash
cargo test --workspace 2>&1 | grep "^test result" | wc -l
```
Sanity-check: at least one `test result` line per test binary.

---

## Self-review

**1. Spec coverage:** Walked each section of `2026-05-20-operator-quarantine-review-cli-design.md`:
- §1 Problem — addressed by the whole slice; recall pin in Task 9 is the load-bearing test.
- §2 Scope — every in-scope item has a task; every out-of-scope item is reiterated in Task 10's "what's deliberately NOT in this slice".
- §3 CLI surface — implemented in Task 7 (subcommand tree) + Task 8 (subprocess tests).
- §4 DB module — implemented across Tasks 1-4.
- §5 Audit-row contract — implemented in Task 5.
- §6 cli_audit helpers — implemented in Task 6.
- §7 Test plan — every row in the test table maps to a task (Task 1 unit + Task 5 unit + Task 7 unit + Tasks 2-4 DB integration + Task 8 CLI subprocess + Task 9 recall pin).
- §8 Files — every new and modified file appears across the tasks.
- §9 Migration impact — confirmed: no new migration. The cli_audit helper paths and the merge transaction explicitly note the runtime-role grant inheritance.
- §10 Open follow-ups — surfaced again in Task 10's session-end sync.
- §11 Verification — Task 10's Step 10.1 is the verification.

**2. Placeholder scan:** No `TBD` / `TODO` (the one reference to "the existing TODO note in `hhagent-cli memory l1 add`" is a deliberate citation, not a plan gap). Every code step has complete code. Every command has expected output.

**3. Type consistency:** `ApproveOutcome` / `RejectOutcome` / `MergeOutcome` / `EntitiesError` / `EntityRow` / `ListFilter` / `EntityState` / `MemoryPreview` types appear identically across Task 1 (definition), Task 2-4 (function signatures), Task 6 (cli_audit helpers), Task 7 (CLI dispatch), Task 8 (subprocess tests), and Task 9 (recall pin). The three action constants (`ACTION_ENTITIES_APPROVED` / `ACTION_ENTITIES_REJECTED` / `ACTION_ENTITIES_MERGED`) appear identically across Task 5 (declaration), Task 6 (cli_audit imports + uses), and Task 8 (audit-row presence queries). The three payload builders (`build_entities_*_payload`) appear identically across Task 5 (declaration) and Task 6 (cli_audit imports + uses). The two pure helpers (`validate_merge_args`, `body_preview`) appear identically across Task 1 (declaration + tests) and Task 4 (body usage). Function signatures for the five async db::entities functions are stable across Tasks 1-4 (declaration) and Task 6 (await + match). The two CLI arg parsers (`parse_entity_state`, `parse_id_list`) appear identically across Task 7 (declaration + tests + use in `entities_list`/`entities_merge` dispatchers).

---

## Plan complete and saved to `docs/superpowers/plans/2026-05-20-operator-quarantine-review-cli.md`.
