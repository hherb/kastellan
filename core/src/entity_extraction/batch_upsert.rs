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

/// Deduplicate input entities on `(label, name_norm)`. Returns:
///   - `Vec<DedupedEntity>`: unique entries in first-seen order (so the
///     display form of the first occurrence wins, matching the per-row
///     upsert's first-writer-wins on `entities.name`).
///   - `Vec<String>`: `name_norm` per ORIGINAL input position (parallel
///     to `entities`). The dispatcher uses this for its post-upsert
///     re-walk so it doesn't have to normalize each input a second time.
///
/// Required because PostgreSQL's `INSERT ... ON CONFLICT DO UPDATE`
/// rejects duplicate conflict targets within a single statement with
/// `cardinality_violation: ON CONFLICT DO UPDATE command cannot affect
/// row a second time`. Deduping at the Rust layer keeps the SQL simple
/// and matches the per-row loop's observable behaviour (same id returned
/// for duplicate inputs).
pub(crate) fn dedup_entity_inputs<'a>(
    entities: &'a [Entity],
) -> (Vec<DedupedEntity<'a>>, Vec<String>) {
    let mut seen = std::collections::HashSet::<(String, String)>::new();
    let mut deduped = Vec::with_capacity(entities.len());
    let mut name_norms_by_input = Vec::with_capacity(entities.len());
    for ent in entities {
        let name_norm = normalize_entity_name(&ent.text);
        name_norms_by_input.push(name_norm.clone());
        let key = (ent.label.clone(), name_norm.clone());
        if seen.insert(key) {
            deduped.push(DedupedEntity {
                label: &ent.label,
                text: &ent.text,
                name_norm,
            });
        }
    }
    (deduped, name_norms_by_input)
}

/// Select the **newly-inserted** entities from an upsert result map, as
/// `(id, kind, name)` borrows ready for the forward embed loop.
///
/// Walks `deduped` (so the result is in first-seen input order, matching
/// the rest of the module), looks each entry up in `upsert_map` by its
/// `(kind, name_norm)` key, and keeps only rows the upsert actually created
/// (`inserted == true`, the `xmax = 0` discriminator). Conflict-hit rows are
/// dropped — an existing entity keeps whatever embedding it had, and a still-
/// NULL existing row stays the backfill's job (`kastellan-cli entities
/// reembed`), mirroring the L1 #324(forward)/#325(backfill) split.
///
/// `kind` is the input label and `name` is the input display text — the same
/// `(kind, name)` the backfill reads back from `entities.{kind,name}`, so
/// `entity_embedding_text(kind, name)` yields an identical string on either
/// path. Pure; no I/O. Unit-tested in `batch_upsert/tests.rs`.
pub(crate) fn select_new_entities<'a>(
    deduped: &'a [DedupedEntity<'a>],
    upsert_map: &HashMap<(String, String), (i64, bool)>,
) -> Vec<(i64, &'a str, &'a str)> {
    deduped
        .iter()
        .filter_map(|d| {
            let key = (d.label.to_string(), d.name_norm.clone());
            match upsert_map.get(&key) {
                Some((id, true)) => Some((*id, d.label, d.text)),
                _ => None,
            }
        })
        .collect()
}

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
///
/// PostgreSQL SQLSTATE codes are always 5 characters; the length guard
/// keeps truncated/short codes from accidentally classifying.
pub(crate) fn is_constraint_violation_code(code: &str) -> bool {
    code.len() == 5 && code.starts_with("23")
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

use crate::entity_extraction::EntityExtractionError;
use crate::memory::embedder::Embedder;
use crate::memory::entity_embedding_text;
use crate::workers::gliner_relex::ExtractResponse;
use kastellan_db::entity_embedding::set_entity_embedding;
use kastellan_db::DbError;
use sqlx::PgPool;
use std::collections::HashMap;

/// Forward embed-on-insert: embed each newly-inserted entity through the
/// shared [`entity_embedding_text`] chokepoint and write the vector via the
/// guarded [`set_entity_embedding`] updater (the same writer the backfill
/// uses, so an on-insert vector is byte-identical to a backfilled one).
///
/// **Degrade-and-warn per row** — mirrors
/// [`crate::memory::reembed_entities_null`] and
/// [`crate::memory::l1_promote::promote_l1`]: an embed `None` (the
/// [`crate::memory::RouterEmbedder`] already logged the WARN), a lost
/// `embedding IS NULL` race (`Ok(false)` — a concurrent backfill embedded the
/// row first; not an error, no WARN), or a write `Err` (WARN) all skip that
/// row and continue. The loop **never** returns an error: a flaky embedder
/// must not block the entity/relation write the caller is performing.
async fn embed_new_entities(
    pool: &PgPool,
    embedder: &dyn Embedder,
    new_entities: &[(i64, &str, &str)],
) {
    for &(id, kind, name) in new_entities {
        let text = entity_embedding_text(kind, name);
        // Embed declined/failed (`None`) → the RouterEmbedder already logged
        // the WARN, or a NoOpEmbedder intentionally skips; the row stays NULL.
        if let Some(vector) = embedder.embed_for_storage(&text).await {
            match set_entity_embedding(pool, id, &vector).await {
                // Embedded, or a concurrent backfill won the IS-NULL race
                // (Ok(false)) — both leave the row with a valid embedding.
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "kastellan::entity_extraction",
                        entity_id = id,
                        error = %e,
                        "entity embed-on-insert: write failed; row left NULL (backfill will catch it)",
                    );
                }
            }
        }
    }
}

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
///
/// Commit semantics: each per-row statement auto-commits — earlier rows
/// in the deduped vec are persisted before a later row's failure
/// surfaces. Matches Layer A's per-row loop. Idempotent re-runs are
/// safe because the SQL is ON CONFLICT DO UPDATE on `(kind, name_norm)`.
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
    embedder: &dyn Embedder,
) -> Result<crate::entity_extraction::gliner_relex::UpsertOutcome, EntityExtractionError> {
    // Phase 1: entity upsert with fallback.
    // dedup_entity_inputs returns the deduped vec PLUS a Vec<String> of
    // name_norms parallel to merged.entities — re-used by the post-upsert
    // re-walk below so we don't normalize each input a second time.
    let (deduped, name_norms_by_input) = dedup_entity_inputs(&merged.entities);
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
    for (ent, name_norm) in merged.entities.iter().zip(name_norms_by_input.iter()) {
        let key = (ent.label.clone(), name_norm.clone());
        let (id, inserted) = upsert_map
            .get(&key)
            .copied()
            .expect("dedup invariant: every input entity is in the upsert_map");
        entity_ids.push(id);
        if inserted && counted_new.insert(key) {
            n_new += 1;
        }
    }

    // Forward embed-on-insert: embed the entities this upsert just CREATED so
    // they are immediately visible to the entity-similarity recall lane
    // without waiting for an `entities reembed` backfill. Run here — after the
    // entities are committed, before the relations phase — so a newly-created
    // row still gets embedded even if the relations phase below errors.
    // Degrade-and-warn; never fails the upsert. Conflict-hit rows are skipped
    // (still the backfill's job); a NoOpEmbedder makes this a no-op loop.
    let new_entities = select_new_entities(&deduped, &upsert_map);
    embed_new_entities(pool, embedder, &new_entities).await;

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

    Ok(crate::entity_extraction::gliner_relex::UpsertOutcome {
        entity_ids,
        n_entities_upserted_new: n_new,
        n_relations_inserted,
    })
}

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
///
/// Commit semantics: each per-row INSERT auto-commits — earlier rows
/// in the resolved vec are persisted before a later row's failure
/// surfaces. Re-runs are safe because the SQL is WHERE NOT EXISTS
/// (application-level dedup on `(src_id, dst_id, kind)`). Entities
/// inserted in phase 1 are not rolled back when phase 2 fails — phases
/// are independent (matches the pre-Layer-B per-row loop).
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

#[cfg(test)]
mod tests;
