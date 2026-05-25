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
