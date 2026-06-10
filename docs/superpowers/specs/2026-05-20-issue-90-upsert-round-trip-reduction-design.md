# Issue #90 — `upsert_entities_and_relations` round-trip reduction (Layer A)

**Status:** Design approved 2026-05-20.
**Filed:** GitHub issue [#90](https://github.com/hherb/kastellan/issues/90) on 2026-05-19 during the post-merge code-review pass of PR #91 (entity extraction v2).
**Scope chosen:** Layer A only (`xmax = 0` discriminator). Layer B (`unnest` full-batch) explicitly deferred.

## Background

Today [`core/src/entity_extraction/gliner_relex.rs::upsert_entities_and_relations`](../../../core/src/entity_extraction/gliner_relex.rs) does a two-statement per-entity upsert: `INSERT … ON CONFLICT (kind, name_norm) DO NOTHING RETURNING id`, then a follow-up `SELECT id FROM entities WHERE kind = $1 AND name_norm = $2` whenever the first statement hit conflict. For every pre-existing entity (the steady-state hot path once the corpus warms up) this is two round-trips.

At v2's typical 5–20 entities per `formulate_plan` call on local UDS Postgres the absolute latency is microseconds — not an active perf problem — but the shape is a needless waste, and the `xmax = 0` trick is non-obvious enough that doing it via its own commit + test gives the audit log a clean before/after.

## Goal (Layer A only)

Eliminate the conditional follow-up `SELECT` so every entity costs **exactly one** round-trip whether it's new or pre-existing. The per-entity loop body stays — error attribution remains per-entity (one bad row doesn't poison the whole batch). Public types, function signatures, audit-row contract, and `UpsertOutcome` are unchanged.

## Non-goals

- **No full-batch `unnest` upsert.** Layer B is explicitly out of scope (per operator choice during brainstorming). The risk of losing per-entity error attribution outweighs the marginal RT savings at v2 scale. Layer B can come back as a separate slice if the corpus grows.
- **No triple batching.** Triples still loop one-by-one with `INSERT … SELECT WHERE NOT EXISTS`. Triples have a different shape (missing-endpoint `continue` mid-loop) and aren't the issue's primary concern.
- **No audit-row contract change.** `build_extract_entities_payload` keeps its 8-key shape and signature byte-for-byte.
- **No public-API change on `UpsertOutcome`.** Same 3 fields (`entity_ids`, `n_entities_upserted_new`, `n_relations_inserted`), same types.
- **No migration.** Schema is untouched.

## The SQL change

```sql
INSERT INTO entities (kind, name, name_norm, quarantine)
VALUES ($1, $2, $3, TRUE)
ON CONFLICT (kind, name_norm) DO UPDATE
  SET name_norm = entities.name_norm
RETURNING id, (xmax = 0) AS inserted
```

Two things are load-bearing here, each documented inline at the call site:

1. **`SET name_norm = entities.name_norm` is a no-op self-assignment.** Standard Postgres idiom for "force `RETURNING` to fire on conflict without changing the row's logical state." Critically it does **not** touch `quarantine` — if the operator has already approved an entity (`quarantine = FALSE` per the operator quarantine-review CLI shipped via PR #93), re-encountering that entity during extraction must not silently re-quarantine it.
2. **`xmax = 0` is the canonical inserted/existed discriminator.** A freshly-inserted row has `xmax = 0` (no future-deleting transaction); a row hit on conflict has the conflict transaction's xid as its `xmax`. Returning the boolean directly avoids the conditional `SELECT` round-trip.

### Side-effects of the no-op `UPDATE`

PostgreSQL treats the `DO UPDATE` arm as a real update for MVCC purposes — `xmin` advances and a new tuple version is written even though the column values are identical. Implications:

- **No trigger fires** on `entities` (verified — only `memories` has an `AFTER DELETE` trigger via migration 0008).
- **Vacuum pressure** increases proportionally to re-extraction frequency. At v2 volume (≈3 plan iterations per task × ≈10 entities = ≈30 row-version-bumps per task) this is negligible; autovacuum's default thresholds absorb it without operator action.
- **Lock shape** changes from a brief `RowExclusiveLock` on `INSERT` (which `DO NOTHING` releases immediately on conflict) to a write lock held to commit. Per-row contention is fine because the entities table has no application-level concurrent writers (only the GLiNER-Relex extractor writes via this path; the operator quarantine-review CLI uses `UPDATE` / `DELETE` against the same row keys but is operator-paced).

Neither concern is load-bearing at v2's scale; both are documented in code so a future maintainer doesn't have to re-derive them.

## Rust changes

`core/src/entity_extraction/gliner_relex.rs::upsert_entities_and_relations` (lines 172-274), the entity loop only:

**Before** (lines 183-217 of the function, ~35 LOC):

```rust
for ent in &merged.entities {
    let name_norm = normalize_entity_name(&ent.text);
    let inserted_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ($1, $2, $3, TRUE) \
         ON CONFLICT (kind, name_norm) DO NOTHING \
         RETURNING id",
    )
    .bind(&ent.label).bind(&ent.text).bind(&name_norm)
    .fetch_optional(pool).await
    .map_err(|e| kastellan_db::DbError::Query(format!("upsert entity: {e}")))?;

    let id = match inserted_id {
        Some(id) => { n_new += 1; id }
        None => {
            sqlx::query_scalar(
                "SELECT id FROM entities WHERE kind = $1 AND name_norm = $2",
            )
            .bind(&ent.label).bind(&name_norm)
            .fetch_one(pool).await
            .map_err(|e| kastellan_db::DbError::Query(format!("resolve entity id: {e}")))?
        }
    };
    entity_ids.push(id);
}
```

**After** (~22 LOC):

```rust
for ent in &merged.entities {
    let name_norm = normalize_entity_name(&ent.text);
    // ON CONFLICT DO UPDATE SET name_norm = entities.name_norm is the standard
    // Postgres idiom for "force RETURNING on conflict without mutating the row's
    // logical state." The self-assignment is critical — it preserves the row's
    // `quarantine` column so operator approval (per the quarantine-review CLI)
    // is never silently re-quarantined. `xmax = 0` is the canonical discriminator
    // for inserted vs. existed (fresh rows have xmax=0; conflict-hit rows
    // carry the conflict transaction's xid).
    let row: (i64, bool) = sqlx::query_as(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ($1, $2, $3, TRUE) \
         ON CONFLICT (kind, name_norm) DO UPDATE \
           SET name_norm = entities.name_norm \
         RETURNING id, (xmax = 0) AS inserted",
    )
    .bind(&ent.label).bind(&ent.text).bind(&name_norm)
    .fetch_one(pool).await
    .map_err(|e| kastellan_db::DbError::Query(format!("upsert entity: {e}")))?;
    if row.1 { n_new += 1; }
    entity_ids.push(row.0);
}
```

Net delta: ~13 LOC dropped, one round-trip per pre-existing entity dropped. Triples block (lines 230-267) is unchanged.

## Tests

Tests added in TDD order. Integration tests live in [`core/tests/entity_extraction_e2e.rs`](../../../core/tests/entity_extraction_e2e.rs) alongside the existing `upsert_*` tests.

The new code path is intentionally observably indistinguishable from the old via `UpsertOutcome`'s shape (preserving the public contract is the point). The existing `upsert_is_idempotent_on_rerun` already pins the conflict-arm counter behaviour and id-stability across reruns — Layer A makes that test exercise the new SQL automatically, so it provides full regression coverage of the "existing row → counter = 0 + same id returned" path without needing a duplicate test.

The NEW tests target two cases the existing suite doesn't cover:

**New (2):**

1. **`upsert_preserves_operator_unquarantine_decision`** — bug-of-omission regression pin. Seed one entity (lands quarantined per the default), manually `UPDATE entities SET quarantine = FALSE WHERE id = $1` (simulating an operator approval via `kastellan-cli entities approve`), then call `upsert_entities_and_relations` with that same `(kind, name_norm)`. Assert: post-call, the row's `quarantine` column is still `FALSE`. Catches a future edit that changes the no-op `SET` to clobber `quarantine` — the load-bearing guarantee that makes the operator quarantine-review CLI's approvals survive re-extraction.

2. **`upsert_counts_new_inserts_correctly_in_mixed_batch`** — mixed-batch counter pin. Seed one entity. Call `upsert_entities_and_relations` with two entities in the same call: the pre-existing one + one fresh. Assert `entity_ids.len() == 2`, `n_entities_upserted_new == 1` (only the fresh one). Catches an accumulator bug where the `xmax = 0` discriminator is read but the per-iteration counter increment goes wrong. The existing tests cover all-new (`upsert_creates_quarantined_entities`, `upsert_dedup_works_with_case_variants`) and all-existing (`upsert_is_idempotent_on_rerun`); the mixed case is the gap.

**Preserved unchanged (all four existing `upsert_*` tests stay green byte-for-byte and now exercise the new SQL):**

- `upsert_creates_quarantined_entities` — all-new path
- `upsert_is_idempotent_on_rerun` — all-existing path (also pins id stability across rerun)
- `upsert_dedup_works_with_case_variants` — case-norm dedup
- `extractor_extract_writes_summary_audit_row` — audit-row contract

Plus all 6 existing mock-tier and real-model tier extractor tests in the same file. The audit-row payload key-set test in `core/src/scheduler/audit.rs::tests_extract_entities::extract_entities_payload_has_exactly_8_keys` also stays green — the payload contract is unchanged.

**Test budget delta: 881 → 883 (+2).**

## Verification

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test entity_extraction_e2e      # the touched suite
cargo test --workspace                                       # the regression net
```

Both commands return green with the post-change test counts above.

## What this slice deliberately does NOT do

- **No full-batch `unnest` upsert** (Layer B from the issue). Defer until v2 scale justifies it.
- **No triple-loop batching.** Same reasoning.
- **No audit-row payload change.** `n_entities_upserted_new` remains the only "new-insert" counter; the meaning is identical (count of `RETURNING inserted = TRUE` rows).
- **No public-API change.** `UpsertOutcome`'s three fields, `upsert_entities_and_relations`'s signature, and `EntityExtractor::extract`'s return shape are all unchanged.
- **No migration.** Schema is untouched.
- **No `xmin`/`xmax`-aware consumer code.** The boolean is consumed inside the function and discarded.

## Open follow-ups (not in this slice)

- Layer B (full-batch `unnest`) — reopens with a smaller scope once the entity-extraction corpus grows or observation-phase SQL surfaces RT cost as material.
- Triple batching — same story; deferred until measurement warrants.
- `ON CONFLICT DO NOTHING` callers elsewhere in the codebase that could benefit from the same `xmax` idiom: `db/src/memories.rs::link_memory_to_entities` is the natural cross-reference, but it batches via `unnest` already so the discriminator question doesn't apply the same way.

## File-size watch

`core/src/entity_extraction/gliner_relex.rs` shrinks from 518 LOC to ≈505 LOC. Still over the 500-LOC soft cap by a sliver; a structural split (lifting the `#[cfg(test)] mod tests` block) is the same natural split candidate flagged in the v2 entry of HANDOVER's "Working state". No new urgency from this slice.
