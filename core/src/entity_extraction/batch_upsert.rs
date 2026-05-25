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
}
