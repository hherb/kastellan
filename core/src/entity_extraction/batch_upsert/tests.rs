//! Pure unit tests for the `batch_upsert` module.
//!
//! Lifted from an inline `#[cfg(test)] mod tests` block in `batch_upsert.rs`
//! to keep the production file under the 500-LOC soft cap. The body is
//! byte-identical to what it was inline; `use super::*` still resolves to
//! the parent `batch_upsert` module per the Rust 2018 sibling-directory
//! module pattern. Integration tests that hit a real Postgres cluster live
//! in `core/tests/entity_extraction_e2e.rs`.

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
    let (deduped, name_norms_by_input) = dedup_entity_inputs(&input);
    assert_eq!(deduped.len(), 2);
    assert_eq!(deduped[0].text, "Alpha");
    assert_eq!(deduped[0].name_norm, "alpha");
    assert_eq!(deduped[1].text, "Beta");
    assert_eq!(deduped[1].name_norm, "beta");
    // Parallel name_norms vec carries one entry per ORIGINAL input,
    // even duplicates — the dispatcher's re-walk depends on this.
    assert_eq!(name_norms_by_input, vec!["alpha", "alpha", "beta"]);
}

#[test]
fn dedup_entity_inputs_distinct_kinds_with_same_name_norm_are_distinct() {
    // (kind, name_norm) is the dedup key — same name, different kinds
    // stay separate (`Smith` as person and `Smith` as organization).
    let input = vec![
        make_entity("Smith", "person"),
        make_entity("Smith", "organization"),
    ];
    let (deduped, _) = dedup_entity_inputs(&input);
    assert_eq!(deduped.len(), 2);
    assert_eq!(deduped[0].label, "person");
    assert_eq!(deduped[1].label, "organization");
}

#[test]
fn dedup_entity_inputs_returns_empty_for_empty_input() {
    // Empty input → empty output. No SQL will be issued downstream.
    let input: Vec<Entity> = Vec::new();
    let (deduped, name_norms_by_input) = dedup_entity_inputs(&input);
    assert!(deduped.is_empty());
    assert!(name_norms_by_input.is_empty());
}

#[test]
fn build_entity_unnest_arrays_emits_parallel_arrays_of_equal_length() {
    let input = vec![
        make_entity("Alpha", "person"),
        make_entity("Beta", "organization"),
        make_entity("Gamma", "person"),
    ];
    let (deduped, _) = dedup_entity_inputs(&input);
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
fn is_constraint_violation_code_false_for_wrong_length() {
    // SQLSTATE is always exactly 5 chars. A literal "23" prefix on a
    // 2- or 4-char string is not a valid SQLSTATE and must not
    // classify (defends against truncated/synthetic codes).
    for code in &["23", "230", "2300", "230000", "23X05X"] {
        assert!(
            !is_constraint_violation_code(code),
            "code {code:?} is not a valid 5-char SQLSTATE and should NOT classify"
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
