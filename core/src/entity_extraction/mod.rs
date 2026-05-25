//! Entity extraction: query-time NER for the recall graph lane.
//!
//! This module owns the `EntityExtractor` trait and its production
//! impl `GlinerRelexExtractor` (in `gliner_relex.rs`), plus the
//! `NoOpEntityExtractor` used when the gliner-relex worker isn't
//! configured.
//!
//! See `docs/superpowers/specs/2026-05-19-entity-extraction-v2-gliner-relex-design.md`
//! for the architecture rationale (single-pass joint NER+RE via the
//! gliner-relex worker; quarantine-on-upsert; Rust-side normalization
//! for case/whitespace/Unicode-insensitive dedup).

pub mod gliner_relex;
pub mod batch_upsert;

/// Canonical form for entity-name dedup. Re-exported from
/// `hhagent-db` so the v2 extractor and `PgGraph::upsert_entity`
/// share one normalization implementation — schema concern (the
/// `entities.name_norm` column) lives in the db crate; core just
/// re-exports for convenience at the call sites.
pub use hhagent_db::normalize_entity_name;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Telemetry: which extraction path produced the seeds. v2 collapses
/// v1's three-variant enum to two — the only production source is the
/// gliner-relex worker; v1's deterministic + LLM legs are gone.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedSource {
    /// At least one gliner-relex chunk dispatched and decoded
    /// successfully. The `EntitySeeds::ids` vector may still be empty —
    /// when the model recognised nothing in the input, the extractor
    /// returns `GlinerRelex` + empty ids rather than `None`. This is
    /// load-bearing for observation-phase SQL:
    ///   * `graph_seed_source='gliner_relex' AND graph_seed_count=0`
    ///     → the model ran and produced zero entities (interesting:
    ///     either low-signal input or a model-recall regression).
    ///   * `graph_seed_source='none'`
    ///     → the extractor never produced usable output (worker absent,
    ///     all chunks failed, DB error, NoOp).
    GlinerRelex,
    /// Extractor degraded (worker absent / every chunk failed / DB
    /// error) or wasn't configured. Graph lane proceeds with seeds=[].
    None,
}

/// What the extractor returns to `RouterAgent::formulate_plan`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntitySeeds {
    pub ids: Vec<i64>,
    pub source: SeedSource,
    /// Model version label (e.g. `"multi-v1.0"`). Populated only on
    /// non-degraded extractions; goes into the audit row.
    pub model_version: Option<String>,
}

impl EntitySeeds {
    /// Empty seeds with `SeedSource::None` — what every degrade path
    /// returns.
    pub fn empty() -> Self {
        Self { ids: Vec::new(), source: SeedSource::None, model_version: None }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EntityExtractionError {
    #[error("db error: {0}")]
    Db(#[from] hhagent_db::DbError),
    #[error("client error: {0}")]
    Client(String),
}

/// Async seam: extracts entity ids for the recall graph lane.
///
/// `RouterAgent::formulate_plan` invokes this BEFORE recall on every
/// plan iteration; failure is degrade-and-warn (the caller substitutes
/// `EntitySeeds::empty()` and continues).
#[async_trait]
pub trait EntityExtractor: Send + Sync {
    async fn extract(
        &self,
        query_text: &str,
    ) -> Result<EntitySeeds, EntityExtractionError>;
}

/// Used when the gliner-relex worker isn't configured (env var off,
/// weights missing, smoke-test posture). Returns empty seeds; the
/// single startup WARN line in `core/src/main.rs` is the only
/// operator signal. No audit row.
pub struct NoOpEntityExtractor;

impl NoOpEntityExtractor {
    pub fn new() -> Self { Self }
}

impl Default for NoOpEntityExtractor {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl EntityExtractor for NoOpEntityExtractor {
    async fn extract(&self, _: &str) -> Result<EntitySeeds, EntityExtractionError> {
        Ok(EntitySeeds::empty())
    }
}

/// Test-only impl: returns a fixed `EntitySeeds` regardless of input.
/// Used by unit tests that need `Arc<dyn EntityExtractor>` without
/// spinning up the real worker.
pub struct StaticEntityExtractor {
    seeds: EntitySeeds,
}

impl StaticEntityExtractor {
    pub fn new(seeds: EntitySeeds) -> Self { Self { seeds } }

    /// Convenience: scripted seeds with `SeedSource::GlinerRelex` +
    /// model version `"test"`.
    pub fn with_ids(ids: Vec<i64>) -> Self {
        Self {
            seeds: EntitySeeds {
                ids,
                source: SeedSource::GlinerRelex,
                model_version: Some("test".into()),
            },
        }
    }
}

#[async_trait]
impl EntityExtractor for StaticEntityExtractor {
    async fn extract(&self, _: &str) -> Result<EntitySeeds, EntityExtractionError> {
        Ok(self.seeds.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_basic_ascii() {
        assert_eq!(normalize_entity_name("Smith"), "smith");
        assert_eq!(normalize_entity_name("SMITH"), "smith");
        assert_eq!(normalize_entity_name("smith"), "smith");
    }

    #[test]
    fn normalize_trims_and_collapses_whitespace() {
        assert_eq!(normalize_entity_name("  Dr   Smith  "), "dr smith");
        assert_eq!(normalize_entity_name("Dr\tSmith"), "dr smith");
        assert_eq!(normalize_entity_name("Dr\n\nSmith"), "dr smith");
    }

    #[test]
    fn normalize_preserves_punctuation() {
        // Important: punctuation NOT stripped (U.S. vs US conflation risk).
        assert_eq!(normalize_entity_name("Dr. Smith"), "dr. smith");
        assert_ne!(
            normalize_entity_name("Dr. Smith"),
            normalize_entity_name("Dr Smith"),
            "punctuation must distinguish forms"
        );
    }

    #[test]
    fn normalize_applies_nfc_to_unicode() {
        // "café" composed (1 char é) vs decomposed (e + combining acute).
        let composed = "café";
        let decomposed = "cafe\u{0301}";
        assert_ne!(composed, decomposed, "raw inputs differ in NFC vs NFD");
        assert_eq!(
            normalize_entity_name(composed),
            normalize_entity_name(decomposed),
            "NFC normalization must collapse composition forms"
        );
    }

    #[test]
    fn normalize_empty_and_whitespace_only() {
        assert_eq!(normalize_entity_name(""), "");
        assert_eq!(normalize_entity_name("   "), "");
        assert_eq!(normalize_entity_name("\t\n"), "");
    }

    #[test]
    fn seed_source_serializes_to_snake_case() {
        let g = serde_json::to_value(SeedSource::GlinerRelex).unwrap();
        assert_eq!(g, serde_json::json!("gliner_relex"));
        let n = serde_json::to_value(SeedSource::None).unwrap();
        assert_eq!(n, serde_json::json!("none"));
    }

    #[test]
    fn seed_source_deserializes_from_snake_case() {
        let g: SeedSource = serde_json::from_value(serde_json::json!("gliner_relex")).unwrap();
        assert_eq!(g, SeedSource::GlinerRelex);
        let n: SeedSource = serde_json::from_value(serde_json::json!("none")).unwrap();
        assert_eq!(n, SeedSource::None);
    }

    #[test]
    fn entity_seeds_empty_has_none_source_and_no_ids() {
        let s = EntitySeeds::empty();
        assert!(s.ids.is_empty());
        assert_eq!(s.source, SeedSource::None);
        assert!(s.model_version.is_none());
    }

    #[tokio::test]
    async fn noop_entity_extractor_returns_empty() {
        let e = NoOpEntityExtractor::new();
        let s = e.extract("anything goes here").await.expect("noop should not fail");
        assert!(s.ids.is_empty());
        assert_eq!(s.source, SeedSource::None);
    }

    #[tokio::test]
    async fn static_entity_extractor_returns_scripted_seeds() {
        let e = StaticEntityExtractor::with_ids(vec![7, 13, 42]);
        let s = e.extract("any text").await.expect("static should not fail");
        assert_eq!(s.ids, vec![7, 13, 42]);
        assert_eq!(s.source, SeedSource::GlinerRelex);
        assert_eq!(s.model_version.as_deref(), Some("test"));
    }
}
