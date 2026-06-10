# Operator quarantine-review CLI — design spec

**Date:** 2026-05-20
**Branch:** `feat/entities-quarantine-review`
**Predecessor specs:**
- [`2026-05-19-entity-extraction-v2-gliner-relex-design.md`](2026-05-19-entity-extraction-v2-gliner-relex-design.md) — introduced `entities.quarantine BOOLEAN DEFAULT TRUE` (migration `0015`) and `graph_search`'s `include_quarantined: bool` flag.
- [`2026-05-19-memory-entity-link-design.md`](2026-05-19-memory-entity-link-design.md) — wired the memory-write-time auto-linker, populating `memory_entities` rows for newly-extracted entities.

## 1. Problem

The graph lane of `recall()` is structurally complete (semantic + lexical + graph fused via RRF). Migrations `0015` and `0016` introduced **quarantine-by-default** semantics: every newly-extracted entity ships with `quarantine = TRUE`, and production callers of `graph_search` pass `include_quarantined = false`. The memory-write-time auto-linker (PR #92, merged at `d58ecc9`) populates `memory_entities` rows correctly for those quarantined entities — but those rows are invisible in production recall because the join filters out quarantined entities.

Net effect today: `recall(GRAPH_ONLY, seeds)` returns 0 rows in production for the entire entity vocabulary, even though the data is in the database.

The unblocker is an operator CLI that surfaces the quarantined entities and lets the operator approve (un-quarantine), reject (delete), or merge (consolidate near-duplicates from extractor variance).

## 2. Scope

In scope:
- A new top-level subcommand tree `kastellan-cli entities` with five actions: `list`, `show`, `approve`, `reject`, `merge`.
- A new DB module `kastellan_db::entities` carrying pure helpers + I/O for the four operations the CLI needs.
- Three new audit-row wire actions (`entities.approved` / `entities.rejected` / `entities.merged`).
- Three new `core::cli_audit::*_and_audit` helpers composing the DB call + audit emission.
- DB and subprocess integration tests covering the happy paths, idempotency, cascade behaviour, and one end-to-end recall pin that demonstrates the graph lane lights up after approval.

Deliberately NOT in scope:
- **Interactive TTY mode** (no curses-style paginated walk-through). The list + batch approve/reject flow covers the bulk-review case. A future interactive mode can wrap the same DB primitives.
- **Kind-vocabulary edits** (`entities kinds add/remove`). Migration `0016` deliberately revoked runtime writes on `entity_kinds`; if/when needed, that becomes a separate elevated CLI path.
- **Embedding-based merge suggestions**. `entities.embedding` is `NULL` for every row; populating it is its own slice.
- **Filter-by-mentions-body-content** (`--mentions <substring>`). Solvable with raw `psql` today; CLI add only when an operator asks.
- **Undo / unmerge**. `deleted_memories` only journals memories; entities have no journal. If an operator regrets a merge, they re-add the entity manually.

## 3. CLI surface

```
kastellan-cli entities list      [--kind <K>] [--state quarantined|approved|any]
                               [--limit N] [--since <RFC3339>] [--min-mentions N]
kastellan-cli entities show      <id>
kastellan-cli entities approve   <id>...
kastellan-cli entities reject    <id>...
kastellan-cli entities merge     --keep <id> --drop <id>[,<id>...]
```

Conventions match the existing `tools allowlist` / `memory l1` subcommand precedents: fixed-width columnar output, `eprintln!` + `ExitCode::from(2)` for arg errors, `ExitCode::from(1)` for runtime errors, `--flag value` style (no `--flag=value`).

### 3.1 `list` semantics

Defaults: `--state quarantined --limit 50`. The default is the most common operator review path. The flag values are case-insensitive at the CLI surface but normalise to a closed `EntityState` enum internally.

Filters:
- `--kind <K>` — exact match on `entities.kind` (FK-validated). Unknown kind → exit 2.
- `--state quarantined|approved|any` — maps to `WHERE quarantine = TRUE`, `WHERE quarantine = FALSE`, or no filter.
- `--limit N` — `1 ≤ N ≤ 1000`. Out-of-range → exit 2.
- `--since <RFC3339>` — filters `entities.created_at >= $`. Parse via `time::OffsetDateTime::parse(_, &Rfc3339)`. Bad format → exit 2.
- `--min-mentions N` — filters on the per-row count of joined `memory_entities` rows. `N >= 0`. Default 0 (no filter).

Output (fixed-width columns):

```
ID       KIND          NAME                    QUARANTINE  MENTIONS  CREATED_AT
123      person        Dr Smith                TRUE        2         2026-05-19T10:01:00Z
...
```

`NAME` is left-truncated to 30 chars with an ellipsis if it overflows; this prevents column drift on the rare long entity name.

### 3.2 `show <id>` semantics

One-entity deep view. Output:

```
id:            123
kind:          person
name:          Dr Smith
name_norm:     dr smith
quarantine:    TRUE
created_at:    2026-05-19T10:01:00Z
mentions:      2
attrs:         {}

linked memories (showing first 10):
  L0  id=42   Dr Smith treats asthma in Mosman.
  L1  id=15   Insight derived from the same body...
```

`linked memories` lists memory id + layer + first 80 chars of the body (stripped of newlines, single-space-collapsed). Hard cap of 10 rows even if the entity has more mentions — `--limit` flag is deliberately not added; the goal is to help the operator decide approve vs reject, not to enumerate every linked row. If the operator needs more, they can query `memory_entities` directly.

The entity_id is **not** found → exit 1 with `entity id <N> not found`.

### 3.3 `approve <id>...` semantics

Variadic — one or more space-separated ids. Each id is processed sequentially in its own DB call (not transactionally batched) so a parse error on id #5 doesn't roll back the four already-approved ids before it.

Three distinct outcomes per id (carried out of the DB layer as a `pub enum ApproveOutcome { Approved { kind, name }, AlreadyApproved, NotFound }` so the CLI can produce distinct stderr lines without a second probe):

- `Approved { kind, name }` — row was flipped quarantine TRUE → FALSE. CLI prints `id=<N>: approved <kind> <name>`. One audit row emitted.
- `AlreadyApproved` — row exists with quarantine already FALSE. CLI prints `id=<N>: already approved`. **No audit row** (no state change).
- `NotFound` — no row at this id. CLI prints `id=<N>: not found`. **No audit row**.

The CLI continues processing remaining ids after a `NotFound`. The aggregate exit code is **1** if any id resulted in `NotFound`, else **0**. This mirrors `memory l1 remove`'s "id not present is benign" posture but with non-zero exit aggregation for CI / scripting.

### 3.4 `reject <id>...` semantics

Same variadic shape as `approve`. Two outcomes per id, surfaced as `pub enum RejectOutcome { Rejected { kind, name, mentions_dropped }, NotFound }`:

- `Rejected { kind, name, mentions_dropped }` — row was deleted; cascade dropped `mentions_dropped` rows from `memory_entities`. CLI prints `id=<N>: rejected <kind> <name> (mentions_dropped=<M>)`. One audit row emitted.
- `NotFound` — no row at this id. CLI prints `id=<N>: not found`. **No audit row**. Aggregate exit code follows the same "1 if any NotFound" rule as `approve`.

Effect: `DELETE FROM entities WHERE id = $1`. The FK constraint on `memory_entities (entity_id) ON DELETE CASCADE` (migration `0007`) drops every joined row automatically. The memory rows themselves are untouched.

`mentions_dropped` is surfaced by selecting `COUNT(*)` from `memory_entities WHERE entity_id = $1` **inside the same transaction**, **before** the DELETE. (PostgreSQL doesn't expose cascade row counts in the `DELETE` row count.)

The transaction:

```sql
BEGIN;
SELECT kind, name FROM entities WHERE id = $1 FOR UPDATE;   -- locks the row
SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1;
DELETE FROM entities WHERE id = $1;                          -- cascades
COMMIT;
```

The audit row is written **outside** this transaction (the audit table runs under runtime-role grants on a separate session). The two-write inconsistency window is acceptable per the existing audit-best-effort posture in `cli_audit.rs`: a crash between transaction commit and audit insert loses the audit signal but leaves the DB consistent.

### 3.5 `merge --keep <id> --drop <id>[,<id>...]` semantics

Argument shape: `--keep` takes exactly one i64. `--drop` takes one comma-separated list of i64 (e.g. `--drop 12,13,17`). The `--drop` flag is **not** repeatable — providing it twice errors out. Whitespace around commas in the list is permitted; empty entries are rejected (e.g. `--drop 1,,2` → exit 2).

Single transaction:

```sql
BEGIN;
SELECT id, kind, name FROM entities WHERE id = $keep FOR UPDATE;
SELECT id, kind FROM entities WHERE id = ANY($drop) FOR UPDATE;
-- precondition check: every drop_id has the same kind as keep_id
-- (returns an error reason if not; no DB writes happen)

-- Re-link memory_entities. INSERT … ON CONFLICT DO NOTHING handles the
-- case where the same memory was already linked to both the keep and a
-- drop id — that link is "consolidated" rather than duplicated.
INSERT INTO memory_entities (memory_id, entity_id)
SELECT memory_id, $keep FROM memory_entities WHERE entity_id = ANY($drop)
ON CONFLICT (memory_id, entity_id) DO NOTHING;
-- row count tells us how many distinct memories the keep entity gained.

-- Count duplicates dropped (memories linked to BOTH keep and a drop):
SELECT COUNT(*) FROM memory_entities WHERE entity_id = ANY($drop)
  AND memory_id IN (SELECT memory_id FROM memory_entities WHERE entity_id = $keep);
-- diff between this count and the cascade count is the "links_retargeted".

-- Drop the dropped entities. Cascade removes the old memory_entities rows.
DELETE FROM entities WHERE id = ANY($drop);
COMMIT;
```

Outcomes:
- **Preconditions failure** — drop ids include a kind ≠ keep's kind, or any id is not found → returns `DbError::Query` with structured message naming the offending id and kind. Exit 2 (operator error). No DB writes happen because the transaction rolls back.
- **All drops already merged** — the INSERT … ON CONFLICT … added 0 rows; everything was duplicated. Exit 0; output `merged: kept <K>, dropped <ids>, no new links retargeted (all duplicates)`.
- **Happy path** — `links_retargeted = N`, `links_dropped_as_duplicate = M`. Exit 0; output a one-line summary; one audit row.

Why this shape:
- Single transaction so a crash mid-merge leaves the entities + memory_entities tables internally consistent (either both old and new entities exist with all rows intact, or only the keep entity exists with consolidated links).
- `FOR UPDATE` on every targeted row locks against concurrent writes from a parallel CLI invocation or an in-flight auto-linker call. The auto-linker calls `link_memory_to_entities` which `INSERT … ON CONFLICT`s into `memory_entities`; if the auto-linker tries to insert a link to one of the drop entities while merge is running, it blocks on the row lock and retries against the keep entity post-commit (the drop entity will be gone, the auto-linker will need to re-resolve). This is a benign race — the auto-linker's caller (the L0/L1 writer) is degrade-and-warn on `link_memory_entities` errors.

Cross-kind merge refusal pins the operator's mental model: merging `person:Dr Smith` into `place:Smith Street` is almost certainly a mistake, not a typo. The error message names both kinds so the operator can re-check.

## 4. New DB module — `kastellan_db::entities`

File: `db/src/entities.rs` (NEW, ~270 LOC including tests).

### 4.1 Types

```rust
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityState { Quarantined, Approved, Any }

#[derive(Clone, Debug)]
pub struct ListFilter {
    pub kind: Option<String>,
    pub state: EntityState,
    pub limit: i64,
    pub since: Option<OffsetDateTime>,
    pub min_mentions: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPreview {
    pub memory_id: i64,
    pub layer: i16,
    pub body_preview: String,    // first 80 chars, newlines collapsed
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOutcome {
    pub kept_id: i64,
    pub kept_kind: String,
    pub kept_name: String,
    pub dropped_ids: Vec<i64>,
    pub links_retargeted: i64,
    pub links_dropped_as_duplicate: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum EntitiesError {
    #[error("entity {0} not found")]
    NotFound(i64),
    #[error("kind mismatch: keep id {keep_id} is kind '{keep_kind}', drop id {drop_id} is kind '{drop_kind}'")]
    KindMismatch { keep_id: i64, keep_kind: String, drop_id: i64, drop_kind: String },
    #[error("merge requires at least one --drop id")]
    NoDropIds,
    #[error("merge: --drop list contains keep id ({0})")]
    KeepInDropList(i64),
    #[error("database: {0}")]
    Db(#[from] DbError),
}
```

### 4.2 Functions

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApproveOutcome {
    Approved { kind: String, name: String },
    AlreadyApproved,
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectOutcome {
    Rejected { kind: String, name: String, mentions_dropped: i64 },
    NotFound,
}

pub async fn list_entities(pool: &PgPool, filter: &ListFilter)
    -> Result<Vec<EntityRow>, EntitiesError>;

pub async fn get_entity_with_mentions(pool: &PgPool, id: i64)
    -> Result<Option<(EntityRow, Vec<MemoryPreview>)>, EntitiesError>;

/// Distinguishes Approved (flipped TRUE → FALSE) / AlreadyApproved /
/// NotFound. Single SELECT … FOR UPDATE → UPDATE inside one transaction
/// so the quarantine-state observation and the flip are atomic against
/// concurrent approvals.
pub async fn approve_entity(pool: &PgPool, id: i64)
    -> Result<ApproveOutcome, EntitiesError>;

/// Distinguishes Rejected / NotFound. Single transaction wrapping the
/// SELECT … FOR UPDATE → COUNT(memory_entities) → DELETE sequence so
/// the cascade row count is stable against concurrent inserts from
/// the auto-linker (which would otherwise be racing on memory_entities).
pub async fn reject_entity(pool: &PgPool, id: i64)
    -> Result<RejectOutcome, EntitiesError>;

/// Single-transaction merge. Returns the outcome on success; returns
/// EntitiesError::{NotFound, KindMismatch, NoDropIds, KeepInDropList} on
/// precondition failure (DB rolled back, no writes).
pub async fn merge_entities(pool: &PgPool, keep_id: i64, drop_ids: &[i64])
    -> Result<MergeOutcome, EntitiesError>;
```

Pure helpers (private to the module):

```rust
fn validate_merge_args(keep_id: i64, drop_ids: &[i64]) -> Result<(), EntitiesError>;
fn body_preview(body: &str, max_chars: usize) -> String;   // newlines → space, truncate
```

`body_preview` is `pub(crate)` (testable directly without a DB roundtrip).

### 4.3 SQL shape pins (load-bearing)

The `list_entities` query joins `memory_entities` for the count. We use a LEFT JOIN + GROUP BY rather than a subquery so PostgreSQL can use the existing `memory_entities_entity_id_idx` index (migration `0007`):

```sql
SELECT e.id, e.kind, e.name, e.name_norm, e.quarantine, e.created_at,
       COUNT(me.memory_id)::BIGINT AS mention_count
FROM entities e
LEFT JOIN memory_entities me ON me.entity_id = e.id
WHERE
  ($1::TEXT     IS NULL OR e.kind = $1)
  AND ($2::BOOL IS NULL OR e.quarantine = $2)
  AND ($3::TIMESTAMPTZ IS NULL OR e.created_at >= $3)
GROUP BY e.id
HAVING COUNT(me.memory_id) >= $4
ORDER BY e.created_at DESC, e.id DESC
LIMIT $5;
```

The `($N::TYPE IS NULL OR …)` pattern lets us bind a single SQL string for every filter combination without dynamic SQL composition (which would also work but adds a small `.push_str` choreography that's easy to get wrong).

## 5. Audit-row contract

Three new action constants live in `core::scheduler::audit` next to the existing CLI actions:

```rust
pub const ACTION_ENTITIES_APPROVED: &str = "entities.approved";
pub const ACTION_ENTITIES_REJECTED: &str = "entities.rejected";
pub const ACTION_ENTITIES_MERGED:   &str = "entities.merged";
```

Wire-stable payload shapes:

| Actor | Action | Payload keys (BTreeSet-pinned) | When |
|---|---|---|---|
| `cli` | `entities.approved` | `{entity_id, kind, name}` (3) | One per id that flipped quarantine TRUE → FALSE. Not written on "already approved" or "not found". |
| `cli` | `entities.rejected` | `{entity_id, kind, name, mentions_dropped}` (4) | One per id that was successfully deleted. Not written on "not found". |
| `cli` | `entities.merged` | `{kept_id, kept_kind, kept_name, dropped_ids, links_retargeted, links_dropped_as_duplicate}` (6) | One per successful `merge` call. Not written on precondition failure. |

Where:
- `kind` / `name` reflect the pre-DELETE state for `entities.rejected`.
- `dropped_ids` is a JSON array of integers; `links_retargeted` and `links_dropped_as_duplicate` are integers.

Each shape gets a `build_entities_*_payload` pure helper next to the existing `build_l1_write_payload` in `core::scheduler::audit`, and each helper has a `BTreeSet` key-pin test mirroring the existing L1 / step-failure precedents.

## 6. `core::cli_audit` helpers

```rust
pub async fn entities_approve_and_audit(
    pool: &PgPool,
    id: i64,
) -> Result<ApproveOutcome, EntitiesError>;

pub async fn entities_reject_and_audit(
    pool: &PgPool,
    id: i64,
) -> Result<RejectOutcome, EntitiesError>;

pub async fn entities_merge_and_audit(
    pool: &PgPool,
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<MergeOutcome, EntitiesError>;
```

Each helper:
1. Calls the corresponding `kastellan_db::entities::*` function.
2. **Only on the state-changing outcome variant** (`Approved`, `Rejected`, or successful `merge`), builds the payload via the `core::scheduler::audit::build_entities_*_payload` helper and calls `kastellan_db::audit::insert(pool, "cli", ACTION_ENTITIES_*, payload)`. `AlreadyApproved` / `NotFound` outcomes produce no audit row (no state was changed).
3. Best-effort posture on audit insert: a failure is logged at `tracing::warn!` and swallowed; the helper still returns the operation outcome.

This matches the existing `l1_add_and_audit` / `tools_allowlist_add_and_audit` posture exactly, with the additional refinement that the audit-row emit is gated on the state-change variant so observation-phase SQL never sees a `cli/entities.approved` row for an entity that was already approved.

## 7. Test plan

| Tier | What's pinned | Count |
|---|---|---|
| Unit (`db::entities::tests`) | `body_preview`: newlines-to-space + multi-space collapse + truncation; `validate_merge_args`: NoDropIds, KeepInDropList; `ListFilter` Default has sane values | 4 |
| Unit (`scheduler::audit::tests`) | Each `build_entities_*_payload` is BTreeSet-pinned to its exact key set (3 tests, one per action); action-const string-literal stability (3 tests against silent rename) | 6 |
| Unit (`cli` arg parsing) | `parse_entity_state("quarantined"|"approved"|"any"|"OTHER")`; `parse_id_list` accepts `1,2,3` + rejects empty + rejects non-digit | 2 |
| DB integration (`postgres_e2e`) | `list_entities` default-filter returns quarantined only; `list_entities` honours `kind`/`since`/`min_mentions` filters; `approve_entity` returns `Approved` first call + `AlreadyApproved` second call + `NotFound` on unknown id; `reject_entity` returns `Rejected{mentions_dropped}` and cascades `memory_entities` while leaving memory rows intact + `NotFound` on unknown id; `merge_entities` happy path + idempotent on already-linked + cross-kind precondition refusal + keep-in-drop refusal | 7 |
| CLI subprocess (`cli_entities_e2e`) | `entities list` happy path; `entities show <id>` happy path; `entities approve <id>` writes audit row; `entities reject <id>` writes audit row; `entities merge --keep K --drop A,B` writes audit row; bad args produce exit code 2 with usage on stderr | 6 |
| Recall pin (`memory_recall_e2e`) | Seed 2 quarantined entities linked to 2 memories. `recall(GRAPH_ONLY, seeds)` returns 0 hits. Call `entities_approve_and_audit` for one. `recall(GRAPH_ONLY, seeds)` now returns 1. Call `entities_reject_and_audit` for the other. `recall(GRAPH_ONLY, seeds)` still returns the approved one (rejected one's memory_entities row is gone). | 1 |
| **Total** | | **+26** |

Budget: 848 → ~874. Margin against the proposal's +18 estimate is comfortable; the plan can refine exact per-task counts.

## 8. Files (NEW + MODIFIED)

**NEW:**
- `db/src/entities.rs` (~280 LOC incl. tests)
- `core/tests/cli_entities_e2e.rs` (~350 LOC)

**MODIFIED:**
- `db/src/lib.rs` — add `pub mod entities;`
- `core/src/cli_audit.rs` — +3 helpers (~110 LOC)
- `core/src/scheduler/audit.rs` — +3 action constants + 3 payload builders + stability tests
- `core/src/bin/kastellan-cli.rs` — +subcommand tree (~250 LOC); also add `entities …` to `help_text()`
- `db/tests/postgres_e2e.rs` — +7 tests
- `core/tests/memory_recall_e2e.rs` — +1 recall-pin scenario

**File-size watch:**
- `kastellan-cli.rs` already at **1444 LOC** (a known 500-LOC cap-breach flagged in HANDOVER's "Open follow-up surfaces"). This slice adds ~250 → ~1700 LOC. The split-into-modules refactor is a separate slice already on the priority list; this slice deliberately does not attempt it.
- `db/src/memories.rs` already at **949 LOC**; this slice doesn't touch it.
- New `db/src/entities.rs` ships under cap at ~280 LOC.

## 9. Migration impact

**No new migrations.**

- The runtime role already has full CRUD on `entities` (default `GRANT ALL` from migration `0002`, never revoked on this table). `UPDATE entities SET quarantine = FALSE` runs under the existing pool's runtime role.
- `memory_entities` cascade DELETE: covered by migration `0007`'s `ON DELETE CASCADE`. The CLI doesn't issue raw `DELETE FROM memory_entities` — it relies on the cascade.
- `entity_kinds` REVOKE from migration `0016` is **not** in this slice's blast radius. The CLI never INSERTs / UPDATEs / DELETEs on `entity_kinds`.

If a future slice introduces a `kinds` subcommand, that one will need migration `0017`-style grants for the specific actions, isolated by a dedicated role or kept as an operator-elevated path.

## 10. Open follow-ups (post-slice)

Filed against this slice's exit but explicitly out of scope for the present slice:

1. **`entities relink <memory_id>` subcommand** — operator-driven backfill for the auto-linker's "operator-explicit L1 add" gap (NoOp extractor) and any pre-extractor memory rows. Pairs with the existing TODO note in `kastellan-cli memory l1 add`.
2. **Interactive review mode** — terminal UI iterating quarantined entities one-by-one, accepting `a/r/m/q` keystrokes. The DB primitives shipped here would be the building blocks.
3. **Embedding-based merge suggestions** — once `entities.embedding` is populated, `entities suggest-merges <id>` would surface near-duplicates by cosine similarity.
4. **Per-entity provenance** — `entities.created_by_extractor` / `entities.created_at_audit_id` would let the CLI surface "this entity was extracted by Gemma 4 on 2026-05-19; the source memory body is X". Today's `mention_count` query is a weak proxy.

## 11. Verification

- `cargo test --workspace` post-slice: 848 → ~872 passed, 0 failed, 0 [SKIP] on Linux.
- Smoke test (operator-runnable, post-deploy): after seeding ≥1 memory through the L0 or L1 path with `KASTELLAN_GLINER_RELEX_ENABLE=1` and verifying `SELECT COUNT(*) FROM entities WHERE quarantine = TRUE > 0`, run `kastellan-cli entities list`. Approve a known entity. Submit a follow-up task referencing that entity. `agent/plan.formulate` audit-row should now show `graph_seed_count >= 1` (Slice F key from the v2 extractor's payload).
- Threat-model invariant unchanged: this slice introduces no new sandboxed code paths and no new egress endpoints. The CLI runs as the operator's OS user, talks to the same runtime PG pool the daemon uses.
