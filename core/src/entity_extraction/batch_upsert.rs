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
use hhagent_db::normalize_entity_name;

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
use crate::workers::gliner_relex::ExtractResponse;
use hhagent_db::DbError;
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
}
