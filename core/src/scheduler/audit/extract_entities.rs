//! Audit payload for the `extractor:gliner-relex` summary row.
//!
//! Split out of the parent [`super`] module (v2 Entity Extraction arc)
//! so the lifecycle/finalize/l1 audit helpers and this extraction
//! summary stay separately readable. The const + builder are
//! re-exported from the parent (`pub use extract_entities::…`) so the
//! public path `scheduler::audit::{ACTION_EXTRACT_ENTITIES,
//! build_extract_entities_payload}` is unchanged for callers in
//! `entity_extraction::gliner_relex` and `entity_extraction_e2e`.
//!
//! Pure function — no I/O, no clock — so the payload shape is
//! unit-testable without Postgres; the co-located tests below pin it.

/// Audit action for the `extractor:gliner-relex` summary row emitted
/// per `extractor.extract()` call (v2 Entity Extraction).
pub const ACTION_EXTRACT_ENTITIES: &str = "extract_entities";

/// Build the `extractor:gliner-relex` audit row payload. 10 keys.
///
/// `n_entities_out` / `n_triples_out` are what the worker returned;
/// `n_*_dropped_nul` how many of those the upsert chokepoint dropped for
/// carrying a NUL (#818) — a span the worker invented, since its input
/// held none — so the seeds the turn got are the difference.
// One parameter per payload key — a flat builder, so the arg-count
// heuristic is suppressed rather than bundled into a struct that would
// duplicate the key list.
#[allow(clippy::too_many_arguments)]
pub fn build_extract_entities_payload(
    n_chars_in: usize,
    n_chunks: usize,
    n_entities_out: usize,
    n_triples_out: usize,
    dropped_nul: crate::entity_extraction::gliner_relex::NulDropped,
    n_entities_upserted_new: u32,
    n_relations_inserted: u32,
    model_version: &str,
    latency_ms_total: u64,
) -> serde_json::Value {
    serde_json::json!({
        "n_chars_in":              n_chars_in,
        "n_chunks":                n_chunks,
        "n_entities_out":          n_entities_out,
        "n_triples_out":           n_triples_out,
        "n_entities_dropped_nul":  dropped_nul.entities,
        "n_triples_dropped_nul":   dropped_nul.triples,
        "n_entities_upserted_new": n_entities_upserted_new,
        "n_relations_inserted":    n_relations_inserted,
        "model_version":           model_version,
        "latency_ms_total":        latency_ms_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_entities_payload_has_exactly_10_keys() {
        let dropped = crate::entity_extraction::gliner_relex::NulDropped { entities: 1, triples: 3 };
        let p = build_extract_entities_payload(234, 1, 5, 2, dropped, 5, 2, "multi-v1.0", 142);
        let obj = p.as_object().expect("object");
        let keys: std::collections::BTreeSet<&String> = obj.keys().collect();
        let expected: std::collections::BTreeSet<String> = [
            "n_chars_in", "n_chunks", "n_entities_out", "n_triples_out",
            "n_entities_dropped_nul", "n_triples_dropped_nul", "n_entities_upserted_new", "n_relations_inserted",
            "model_version", "latency_ms_total",
        ].iter().map(|s| s.to_string()).collect();
        let expected_refs: std::collections::BTreeSet<&String> = expected.iter().collect();
        assert_eq!(keys, expected_refs, "10-key shape pin");
        assert_eq!((p["n_entities_dropped_nul"].clone(), p["n_triples_dropped_nul"].clone()), (1.into(), 3.into()));
    }

    #[test]
    fn action_extract_entities_is_snake_case() {
        assert_eq!(ACTION_EXTRACT_ENTITIES, "extract_entities");
    }
}
