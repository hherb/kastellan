//! Memory-write-time entity auto-linker.
//!
//! Compose-op: extract entities from the body of a freshly-written
//! memory, insert `(memory_id, entity_id)` rows into `memory_entities`,
//! and emit a 6-key `memory_linker/entity_link` audit row. The memory
//! row must already be committed when this is called (failure here does
//! NOT roll back the memory write — the caller's posture is
//! degrade-and-warn).
//!
//! ## Why this is a free function, not a trait method
//!
//! See `docs/superpowers/specs/2026-05-19-memory-entity-link-design.md`
//! §2: keeping the `EntityExtractor` trait DB-agnostic and `PgPool`-free
//! is load-bearing for unit tests and future non-Postgres backends.

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::PgPool;

use crate::entity_extraction::{
    EntityExtractionError, EntityExtractor, EntitySeeds, SeedSource,
};
use hhagent_db::{audit, memories::link_memory_to_entities, DbError};

/// What the auto-linker did, for caller telemetry. Returned on success
/// only; on failure the caller receives [`LinkError`] and decides
/// whether to count it as a degrade.
#[derive(Clone, Debug)]
pub struct LinkOutcome {
    /// Post-`ON CONFLICT DO NOTHING` row count from
    /// [`hhagent_db::memories::link_memory_to_entities`]. May be smaller
    /// than `seeds.ids.len()` when some entities were already linked to
    /// this memory (re-run idempotency path).
    pub n_entities_linked: u64,
    /// Forwarded for caller-side telemetry. The audit row uses
    /// `seeds.ids.len()` as the separate `n_seeds` payload key so
    /// observation-phase SQL sees both bucket counts.
    pub seeds: EntitySeeds,
}

/// Error kinds for the auto-linker.
#[derive(thiserror::Error, Debug)]
pub enum LinkError {
    #[error("entity extraction failed: {0}")]
    Extract(#[from] EntityExtractionError),
    #[error("db error: {0}")]
    Db(#[from] DbError),
}

/// Extract entities from `body` and link them to `memory_id`.
///
/// **Posture: caller-handles-failure.** A `LinkError::Extract` or
/// `LinkError::Db` MUST NOT be treated as a memory-write failure
/// by the caller — the memory row is already committed. Production
/// callers log the error at WARN, increment a degrade counter, and
/// continue. The audit row is written EVEN on failure (with
/// `n_entities_linked = 0` and `seed_source = "none"`) so the
/// observation phase sees every link attempt.
///
/// `layer_label` is a stringly-typed identifier of the calling layer
/// (`"L0"`, `"L1"`, future `"L2"`/`"L3"`/`"L4"`). It goes straight into
/// the audit payload's `layer` key. Stringly avoids a circular dep on
/// `hhagent_db::memories::MemoryLayer` from this module.
///
/// The function calls `extract` unconditionally; the NoOp-extractor
/// case is a path optimisation (empty `seeds.ids` short-circuits at the
/// fast-path in `link_memory_to_entities`) rather than a branch.
pub async fn link_memory_entities(
    extractor: &dyn EntityExtractor,
    pool: &PgPool,
    memory_id: i64,
    layer_label: &'static str,
    body: &str,
) -> Result<LinkOutcome, LinkError> {
    let extract_result = extractor.extract(body).await;

    let (seeds, n_linked) = match extract_result {
        Ok(seeds) => {
            // ON CONFLICT DO NOTHING in link_memory_to_entities makes
            // this idempotent on re-runs; empty seeds short-circuit at
            // the existing fast-path so the NoOp extractor case is
            // essentially free (no SQL issued).
            let n = link_memory_to_entities(pool, memory_id, &seeds.ids).await?;
            (seeds, n)
        }
        Err(e) => {
            // Audit the failed attempt; the audit insert is best-effort
            // (its own error is logged but doesn't shadow the primary
            // extract error). We then propagate the extract error so
            // the caller's `Err` arm runs (warn-log + degrade-counter).
            let payload = build_entity_link_payload(
                memory_id,
                layer_label,
                /* n_entities_linked */ 0,
                /* n_seeds */ 0,
                SeedSource::None,
                None,
            );
            if let Err(audit_err) = audit::insert(
                pool,
                "memory_linker",
                "entity_link",
                payload,
            )
            .await
            {
                tracing::warn!(
                    error = %audit_err, memory_id,
                    "memory_linker degraded-path audit row failed"
                );
            }
            return Err(LinkError::from(e));
        }
    };

    // Success-path audit row.
    let payload = build_entity_link_payload(
        memory_id,
        layer_label,
        n_linked,
        seeds.ids.len() as u64,
        seeds.source,
        seeds.model_version.as_deref(),
    );
    // Best-effort: an audit-insert failure here doesn't roll back the
    // already-committed link rows. Log + continue.
    if let Err(e) = audit::insert(pool, "memory_linker", "entity_link", payload).await {
        tracing::warn!(error = %e, memory_id, "memory_linker audit row failed");
    }

    Ok(LinkOutcome {
        n_entities_linked: n_linked,
        seeds,
    })
}

/// Pure builder: 6 keys, BTreeMap-ordered (matches the convention from
/// `scheduler::audit::build_*_payload`). Unit-tested directly so a
/// future accidental extra/missing key trips the regression pin.
pub(crate) fn build_entity_link_payload(
    memory_id: i64,
    layer_label: &str,
    n_entities_linked: u64,
    n_seeds: u64,
    seed_source: SeedSource,
    model_version: Option<&str>,
) -> Value {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    map.insert("memory_id".to_string(), Value::from(memory_id));
    map.insert("layer".to_string(), Value::from(layer_label.to_string()));
    map.insert(
        "n_entities_linked".to_string(),
        Value::from(n_entities_linked),
    );
    map.insert("n_seeds".to_string(), Value::from(n_seeds));
    map.insert(
        "seed_source".to_string(),
        serde_json::to_value(seed_source).expect("snake_case-serializable"),
    );
    map.insert(
        "model_version".to_string(),
        model_version.map(Value::from).unwrap_or(Value::Null),
    );
    Value::Object(map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_extraction::SeedSource;

    /// The audit-row payload has exactly 6 keys. Future additions must
    /// touch this test so observation-phase consumers can be informed.
    #[test]
    fn build_payload_keyset_is_exactly_six() {
        let payload =
            build_entity_link_payload(42, "L0", 3, 5, SeedSource::GlinerRelex, Some("multi-v1.0"));
        let obj = payload.as_object().expect("payload is an object");
        let keys: Vec<&String> = obj.keys().collect();
        assert_eq!(
            keys.len(),
            6,
            "expected exactly 6 keys, got {keys:?}",
        );
        // Spelled-out keyset so a renamed key is loud.
        for expected in &[
            "layer",
            "memory_id",
            "model_version",
            "n_entities_linked",
            "n_seeds",
            "seed_source",
        ] {
            assert!(obj.contains_key(*expected), "missing {expected}");
        }
    }

    #[test]
    fn build_payload_with_model_version_carries_string_value() {
        let payload =
            build_entity_link_payload(1, "L1", 2, 2, SeedSource::GlinerRelex, Some("multi-v1.0"));
        assert_eq!(payload["model_version"], Value::from("multi-v1.0"));
        assert_eq!(payload["layer"], Value::from("L1"));
        assert_eq!(payload["memory_id"], Value::from(1));
        assert_eq!(payload["n_entities_linked"], Value::from(2u64));
        assert_eq!(payload["n_seeds"], Value::from(2u64));
    }

    #[test]
    fn build_payload_without_model_version_emits_json_null() {
        let payload = build_entity_link_payload(1, "L0", 0, 0, SeedSource::None, None);
        assert_eq!(payload["model_version"], Value::Null);
        assert_eq!(payload["seed_source"], Value::from("none"));
    }

    #[test]
    fn build_payload_serializes_seed_source_as_snake_case() {
        let gliner = build_entity_link_payload(1, "L0", 0, 0, SeedSource::GlinerRelex, None);
        assert_eq!(gliner["seed_source"], Value::from("gliner_relex"));
        let none = build_entity_link_payload(1, "L0", 0, 0, SeedSource::None, None);
        assert_eq!(none["seed_source"], Value::from("none"));
    }

    #[test]
    fn link_error_extract_variant_carries_source() {
        let underlying = EntityExtractionError::Client("scripted".into());
        let wrapped: LinkError = underlying.into();
        match wrapped {
            LinkError::Extract(e) => {
                // Format the underlying error to prove it round-trips.
                let s = format!("{e}");
                assert!(s.contains("scripted"), "got: {s}");
            }
            _ => panic!("expected LinkError::Extract"),
        }
    }

    #[test]
    fn link_error_db_variant_carries_source() {
        let underlying = DbError::Query("scripted db error".into());
        let wrapped: LinkError = underlying.into();
        match wrapped {
            LinkError::Db(e) => {
                let s = format!("{e}");
                assert!(s.contains("scripted db error"), "got: {s}");
            }
            _ => panic!("expected LinkError::Db"),
        }
    }
}
