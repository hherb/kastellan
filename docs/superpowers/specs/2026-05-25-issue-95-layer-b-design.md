# Issue #95 — entity-upsert Layer B (full-batch unnest + per-row attribution fallback)

**Date:** 2026-05-25
**Issue:** [#95](https://github.com/hherb/kastellan/issues/95)
**Predecessor:** PR #94 (Layer A, merged at `3ab94f6` on 2026-05-20)
**Author/operator:** Horst Herb
**Status:** Spec — ready for implementation plan.

---

## Background

PR #94 (Layer A, Issue #90) collapsed each per-entity upsert from a two-round-trip
shape (`INSERT ... DO NOTHING` then follow-up `SELECT id` on conflict) into a
single round-trip via `INSERT ... ON CONFLICT DO UPDATE SET name_norm =
entities.name_norm RETURNING id, (xmax = 0) AS inserted`. Per-entity cost is now
exactly 1 RT; the `xmax = 0` discriminator distinguishes fresh-insert from
conflict-hit without a second query.

Layer A deliberately stopped at per-entity. The brainstorming pass that produced
PR #94 framed the deferral as a trade against per-row error attribution: a
full-batch `unnest` upsert collapses N row-level failures into one batch failure
with row indices the caller must map back, losing the diagnostic value of
"entity #5 with kind='person' tripped the FK on `entities_kind_fk`."

The trigger conditions Issue #95 listed for picking up Layer B
(observation-phase per-extract entity counts routinely above ~20, production
tracing showing the upsert as a measurable latency contributor, attribution
diagnostic value re-evaluated lower) have not fired. This spec proceeds anyway
with a design that **preserves the attribution opportunity** the deferral was
worried about, rather than overriding the original cost/benefit calculus. The
shape: **batch-first happy path (1 RT), per-row fallback on constraint
violation (re-run as today's Layer A loop with diagnostic error wrapping)**.

The empirical observation that re-frames the trade-off: **today's Layer A
loop has weak per-row attribution too**. Each per-entity `.map_err(|e|
kastellan_db::DbError::Query(format!("upsert entity: {e}")))` wraps the sqlx
error without identifying the failing entity. The fallback path is therefore
an opportunity to *add* per-row attribution where Layer A had none, not just
preserve parity.

## Goals

1. Collapse N entity upserts into 1 round-trip in the steady-state happy path.
2. Collapse N triple inserts into 1 round-trip in the steady-state happy path.
3. On constraint violation (Postgres SQLSTATE class `23`), transparently fall
   back to a per-row loop that wraps each error with `kind` + `name_norm`
   (entities) or `(src_id, dst_id, kind)` (relations) so the operator can
   identify the failing row from the error message alone.
4. Preserve every Layer A invariant byte-identically:
   - `UpsertOutcome { entity_ids, n_entities_upserted_new, n_relations_inserted }`
     public-API shape.
   - `EntityExtractionError` enum unchanged (still wraps `kastellan_db::DbError`
     and `String` for client errors).
   - 8-key `build_extract_entities_payload` audit-row contract.
   - `quarantine`-column preservation via the no-op `SET name_norm =
     entities.name_norm` self-assignment — operator-approved entities must
     survive re-extraction.
   - `name_norm` derivation via the existing
     `kastellan_db::entity_name::normalize_entity_name` helper.
5. Net-improve atomicity on the happy path: a single INSERT statement is
   atomically all-or-nothing, whereas Layer A's per-row loop allows partial
   commits on failure (one row commits before the next one fails). Layer B's
   happy path is strictly stronger; the fallback path matches Layer A's
   pseudo-atomicity.

## Non-goals

- **No new migration.** Schema is unchanged.
- **No public-API change.** `upsert_entities_and_relations` keeps the same
  signature, return type, and error type. Callers in `gliner_relex.rs` and
  test harnesses are not modified.
- **No audit-payload change.** The 8-key contract is wire-frozen by
  observation-phase queries.
- **No explicit transaction wrapping** in the fallback. Layer A does not wrap;
  Layer B should not silently introduce a behavioral difference on the failure
  path. Adding a BEGIN/COMMIT around the fallback would be a separate,
  broader change.
- **No retry loop.** The fallback runs once; first failing row aborts. Same
  posture as today's Layer A.
- **No `entities.attrs` or `relations.attrs` plumbing.** Both columns still
  receive `'{}'::jsonb` literal — same as Layer A.
- **No CLI surface.** Layer B is invisible to operators except through error
  message quality.

## Architecture

### Module shape

A new sibling file `core/src/entity_extraction/batch_upsert.rs` holds the new
implementation plus the Layer A per-row loop (as the fallback path). The
existing `core/src/entity_extraction/gliner_relex.rs` keeps the public function
name and signature; its body becomes a single-line delegate.

```
core/src/entity_extraction/
├── mod.rs                  (unchanged — trait, error type, ExtractResponse types)
├── gliner_relex.rs         (delegate only for upsert_entities_and_relations; UpsertOutcome stays here)
└── batch_upsert.rs         (NEW — Layer B impl + Layer A fallback + pure helpers + unit tests)
```

Rationale: `gliner_relex.rs` is already at ~289 LOC; inlining ~200 LOC of
Layer B would push it past the 500-LOC soft cap. The sibling-file split also
unlocks unit-testing the pure helpers (`dedup_entity_inputs`,
`build_entity_unnest_arrays`, `is_constraint_violation`, error formatters)
without a PG dependency. `UpsertOutcome` stays in `gliner_relex.rs` because
it is the public-API surface tied to the extractor's call site — moving it
would force an irrelevant import dance for every caller.

### Public surface in batch_upsert.rs

One exported async function with the same signature as today's
`upsert_entities_and_relations`:

```rust
pub async fn upsert_entities_and_relations(
    pool: &PgPool,
    merged: &ExtractResponse,
) -> Result<UpsertOutcome, EntityExtractionError>
```

Implementation body:

```rust
match try_batch_upsert(pool, merged).await {
    Ok(outcome) => Ok(outcome),
    Err(err) if is_constraint_violation(&err) => {
        // Fall back to per-row attribution path.
        per_row_upsert(pool, merged).await
    }
    Err(err) => Err(EntityExtractionError::Db(
        DbError::Query(format!("batch upsert: {err}"))
    )),
}
```

Both `try_batch_upsert` and `per_row_upsert` are module-private (`async fn`)
and return `Result<UpsertOutcome, sqlx::Error>` so the dispatch site can
classify the error before wrapping.

### Pure helpers (all module-private; unit-tested without DB)

```rust
struct DedupedEntity<'a> {
    label: &'a str,
    text: &'a str,
    name_norm: String,
}

/// Deduplicate input entities on (label, name_norm). Returns unique entries
/// in first-seen order; the position in the returned Vec is the batch index
/// referenced in fallback error messages.
fn dedup_entity_inputs<'a>(entities: &'a [Entity]) -> Vec<DedupedEntity<'a>>;

/// Build the four parallel arrays the unnest SQL expects. Lengths are equal
/// to `deduped.len()`. The quarantine array is uniformly TRUE (new rows land
/// quarantined; ON CONFLICT no-op preserves the operator's prior approval).
fn build_entity_unnest_arrays<'a>(
    deduped: &'a [DedupedEntity<'a>],
) -> (Vec<&'a str>, Vec<&'a str>, Vec<String>, Vec<bool>);

/// True iff err is sqlx::Error::Database with SQLSTATE class 23
/// (constraint violation family — 23502 NOT NULL, 23503 FK, 23505 UNIQUE,
/// 23514 CHECK). Other error kinds (network, timeout, decode) don't benefit
/// from per-row retry and should propagate verbatim.
fn is_constraint_violation(err: &sqlx::Error) -> bool;

/// Per-row error message format for entity fallback path:
///   `"upsert entity (kind='person', name_norm='dr smith'): <sqlx err>"`
/// Uses name_norm (NFC + lowercase + whitespace-collapsed) rather than the
/// raw user-supplied name to reduce PII leakage into error logs.
fn format_per_row_entity_error(kind: &str, name_norm: &str, err: &sqlx::Error) -> String;

/// Per-row error message format for relation fallback path:
///   `"insert relation (src=42, dst=43, kind='treats'): <sqlx err>"`
fn format_per_row_relation_error(src_id: i64, dst_id: i64, kind: &str, err: &sqlx::Error) -> String;
```

## Happy-path SQL

### Entities batch (1 RT)

```sql
INSERT INTO entities (kind, name, name_norm, quarantine)
SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::bool[])
ON CONFLICT (kind, name_norm) DO UPDATE
  SET name_norm = entities.name_norm
RETURNING kind, name_norm, id, (xmax = 0) AS inserted
```

Returns N rows in arbitrary order (RETURNING does not guarantee input
ordering). Rust-side builds a `HashMap<(String, String), (i64, bool)>` from
the result rows, then walks `merged.entities` in original input order to
populate `entity_ids: Vec<i64>`. Same-key duplicates in input resolve to the
same id (matches today's Layer A behavior: each duplicate's per-row upsert
hits the same ON CONFLICT row and returns the same id).

`n_entities_upserted_new` increments once per unique `(kind, name_norm)` whose
batch result carried `inserted = true`. (A duplicate in input does not double-
count because we only count from the deduped batch result map.)

### Relations batch (1 RT, if non-empty)

The triple pre-loop on the Rust side stays unchanged: walk each triple, look
up `(head.r#type, normalize(head.text))` and `(tail.r#type, normalize(tail.text))`
in `by_key`, skip triples where either endpoint is unknown. Surviving triples
become the input to the batch.

```sql
WITH input(src_id, dst_id, kind) AS (
    SELECT * FROM unnest($1::bigint[], $2::bigint[], $3::text[])
)
INSERT INTO relations (src_id, dst_id, kind, attrs)
SELECT i.src_id, i.dst_id, i.kind, '{}'::jsonb
FROM input i
WHERE NOT EXISTS (
    SELECT 1 FROM relations r
    WHERE r.src_id = i.src_id AND r.dst_id = i.dst_id AND r.kind = i.kind
)
RETURNING id
```

`n_relations_inserted` = the number of returned ids. The `WHERE NOT EXISTS`
subquery preserves Layer A's application-level dedup (the `relations` table
has no UNIQUE constraint by design — multi-edges with different `created_at`
timestamps are intentional, per the comment in migration `0001_init.sql`).

If the surviving triple list is empty (every triple referenced an unknown
entity), skip the SQL entirely and set `n_relations_inserted = 0`.

## Fallback path

Trigger: `is_constraint_violation(&err)` returns true.

Behavior: re-run the upsert as a per-row loop matching Layer A's exact SQL,
but with each `.map_err(...)` wrapping the underlying sqlx error via
`format_per_row_entity_error` (or the relation analogue). The first row that
fails aborts the fallback loop and returns the diagnostic error.

```rust
async fn per_row_upsert(pool: &PgPool, merged: &ExtractResponse)
    -> Result<UpsertOutcome, EntityExtractionError>
{
    // Per-entity loop with diagnostic error wrapping.
    let mut entity_ids = Vec::with_capacity(merged.entities.len());
    let mut n_new: u32 = 0;
    for ent in &merged.entities {
        let name_norm = normalize_entity_name(&ent.text);
        let (id, inserted): (i64, bool) = sqlx::query_as(
            "INSERT INTO entities (kind, name, name_norm, quarantine) \
             VALUES ($1, $2, $3, TRUE) \
             ON CONFLICT (kind, name_norm) DO UPDATE \
               SET name_norm = entities.name_norm \
             RETURNING id, (xmax = 0) AS inserted",
        )
        .bind(&ent.label)
        .bind(&ent.text)
        .bind(&name_norm)
        .fetch_one(pool)
        .await
        .map_err(|e| DbError::Query(format_per_row_entity_error(&ent.label, &name_norm, &e)))?;
        if inserted { n_new += 1; }
        entity_ids.push(id);
    }

    // Per-relation loop with diagnostic error wrapping.
    // ... (same pattern as Layer A but wrapped via format_per_row_relation_error)
}
```

Note: in the fallback, prior rows (that succeeded in the failed batch attempt
and were rolled back when the batch statement aborted) will be re-inserted via
ON CONFLICT no-op. This is harmless but slightly wasteful — acceptable because
the fallback is the exceptional path, not the steady state.

## Test plan

### Pure unit tests (no DB, in `batch_upsert.rs::tests`)

1. **`dedup_entity_inputs_removes_same_key_duplicates_preserves_first_seen_order`** —
   input `[Alpha#person, alpha#person, Beta#person]` → output length 2,
   `[Alpha#person, Beta#person]` (the lowercase `alpha` drops out; the original
   `Alpha` text survives because it was seen first).
2. **`dedup_entity_inputs_distinct_kinds_with_same_name_norm_are_distinct`** —
   input `[Smith#person, Smith#organization]` → output length 2 (different kinds
   are different keys).
3. **`dedup_entity_inputs_returns_empty_for_empty_input`** — pin the empty
   case.
4. **`build_entity_unnest_arrays_emits_parallel_arrays_of_equal_length`** —
   N=3 deduped input → 4 arrays each of length 3; quarantine array all true.
5. **`build_entity_unnest_arrays_handles_empty_input`** — N=0 → 4 empty
   arrays.
6. **`is_constraint_violation_true_for_23xxx_codes`** — fabricate sqlx
   `DatabaseError` instances for 23502, 23503, 23505, 23514; all return true.
7. **`is_constraint_violation_false_for_22xxx_data_exception`** — fabricated
   22001 (string-data-right-truncation) returns false.
8. **`is_constraint_violation_false_for_non_database_errors`** —
   `sqlx::Error::RowNotFound`, `sqlx::Error::PoolTimedOut`, etc. all return
   false.
9. **`format_per_row_entity_error_uses_name_norm_not_raw_name`** — assert
   the format string contains `name_norm='dr smith'` not `name='Dr Smith'`.
10. **`format_per_row_relation_error_contains_src_dst_kind`** — assert the
    format string carries all three identifiers.

(Test count for the fabrication-of-sqlx-error tests may require a thin local
wrapper around `sqlx::error::DatabaseError`; if too invasive, fold into a
single `tests/error_classification.rs` integration test that triggers each
error class against a real PG. Decide during TDD.)

### Integration tests (real PG, in `core/tests/entity_extraction_e2e.rs`)

All existing Layer A tests in this file (`upsert_creates_quarantined_entities`,
`upsert_is_idempotent_on_rerun`, `upsert_dedup_works_with_case_variants`,
`upsert_preserves_operator_unquarantine_decision`,
`upsert_counts_new_inserts_correctly_in_mixed_batch`) must continue to pass
byte-equivalently. They are the regression pin for the public-API contract.

**New Layer B integration tests:**

11. **`upsert_batch_happy_path_returns_same_outcome_shape_as_layer_a`** —
    N=5 mixed-pre-existing batch; assert `UpsertOutcome` field-by-field equality
    with what Layer A would produce (computed by running a reference call before
    the test entity set is seeded).
12. **`upsert_batch_preserves_entity_id_order_for_unique_inputs`** —
    `[Alpha, Beta, Gamma]` → `entity_ids` in that order (verified by re-querying
    the names by id).
13. **`upsert_batch_dedup_input_returns_same_id_for_duplicates`** —
    `[Alpha, alpha, Beta]` → `entity_ids = [id_a, id_a, id_b]`,
    `n_entities_upserted_new = 2`. Covers the Rust-side dedup + map-lookup
    re-walk.
14. **`upsert_batch_falls_back_to_per_row_on_entity_kind_fk_violation`** —
    upsert with a kind that doesn't exist in `entity_kinds` → assert the error
    is `EntityExtractionError::Db` and its message contains
    `kind='<missing-kind>'` and `name_norm=` substrings. This is the
    attribution improvement pin.
15. **`upsert_batch_falls_back_to_per_row_on_relation_kind_fk_violation`** —
    same shape but the triple's `relation` is a kind missing from
    `relation_kinds`. Assert error message contains `kind='<missing>'`.
16. **`upsert_batch_preserves_operator_unquarantine_decision`** — exact
    mirror of the Layer A test (`upsert_preserves_operator_unquarantine_decision`)
    but with N=3 in the batch including the operator-approved entity. The
    test ensures the load-bearing `SET name_norm = entities.name_norm` no-op
    survives the unnest path.
17. **`upsert_batch_skips_triples_referencing_unknown_entities`** —
    triple input includes an entity not in `merged.entities`; assert the
    batch silently skips it (no error) and `n_relations_inserted` reflects
    only the surviving triples.

### Net test delta

+10 unit + +7 integration = **+17 expected** (1023 → 1040 on macOS,
998 → 1015 on Linux DGX assuming all new tests are cross-platform — they
are; only the `[SKIP]`-on-no-PG check varies per host).

## Risks and mitigations

1. **sqlx array binding may need parameter-type annotation.** Postgres
   `text[]` from a Rust `&[&str]` may bind cleanly via sqlx's `Vec<&str>` →
   `text[]` codec or may need an explicit `::text[]` cast in the SQL. The
   SQL above uses `unnest($1::text[], ...)` which both makes the parameter
   type explicit at the query level and works for sqlx's binding logic.
   Confirm during TDD; adjust the parameter passing form if binding errors
   appear.
2. **`xmax = 0` discriminator in batched-upsert** — needs verification that
   the discriminator still functions per-row in the batch context. Postgres
   docs confirm `xmax` is set per-tuple, so this should work; the
   `upsert_batch_dedup_input_returns_same_id_for_duplicates` test pins the
   expected count behavior end-to-end.
3. **Empty input early-return** — both batch SQL paths must skip cleanly
   when `merged.entities` or the triple input is empty (no `unnest` over
   an empty array). The pure helper unit tests `*_handles_empty_input`
   pin this; the integration test `upsert_batch_skips_triples_referencing_unknown_entities`
   covers it end-to-end.
4. **Race against operator vocabulary changes** — between the entity batch
   succeeding and the relation batch starting, an operator could
   `kastellan-cli relations kinds remove <kind>` and trip a relation-kind FK
   violation. The fallback path handles this cleanly — operator gets a
   diagnostic naming the kind. Same race exists today in Layer A; not a
   regression.
5. **PII in error messages** — the chosen format uses `name_norm`
   (normalized) rather than the raw user-supplied name. This is the
   minor mitigation the design picked over (richer but-leakier) raw-name
   format. Acceptable for a fallback path that fires only on constraint
   violations.

## Acceptance criteria

- `cargo test --workspace` on macOS passes (expected count: 1040, +17 over
  the 1023 baseline at main `e93997e`).
- Every existing test in `core/tests/entity_extraction_e2e.rs` continues to
  pass byte-equivalently.
- `core/src/entity_extraction/gliner_relex.rs` is at or under 500 LOC after
  the delegate refactor (currently ~289 LOC; the delegate change removes ~115
  LOC of upsert body and adds ~5 LOC of delegate call).
- `core/src/entity_extraction/batch_upsert.rs` is at or under 500 LOC.
- `build_extract_entities_payload` 8-key contract assertion in
  `core/src/scheduler/audit.rs::tests` continues to pass.
- No new clippy warnings.

## Out of scope (future work)

- Transactional wrapping of the fallback path (would close the partial-commit
  window on the fallback; cleaner DB state on failure). Separate slice.
- Pre-validation of `kind` values against `entity_kinds` / `relation_kinds`
  caches before sending the batch (would catch most FK violations without a
  DB round trip; reduces fallback frequency). Separate slice, depends on
  exposing those caches to the extractor.
- Replacing `'{}'::jsonb` literal with a real `attrs` plumbing path (no
  caller wants this today; speculative).
- Migration to consolidate the trigger conditions Issue #95 listed into
  observation-phase telemetry (`pg_stat_statements` snapshot, per-call
  latency histogram). Separate, broader observability slice.
