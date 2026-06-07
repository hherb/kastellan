//! `GlinerRelexExtractor` — production EntityExtractor impl built on
//! the gliner-relex worker landed in PR #88.
//!
//! Per-call flow (composed across Tasks 7–11):
//!   1. Chunk the input text if it exceeds the worker's 8 KiB cap
//!      (`chunk_text`).
//!   2. Resolve current `entity_labels` via `db::entity_kinds::KindsCache`.
//!   3. Fire `Client::extract` per chunk (sequential — same warm worker).
//!   4. Merge per-chunk responses, dedup, re-anchor offsets
//!      (`merge_chunks`).
//!   5. Upsert entities + relations into PostgreSQL, quarantined by
//!      default (`upsert_entities_and_relations`).
//!   6. Emit `extractor:gliner-relex/extract_entities` summary audit
//!      row (`emit_extract_entities_audit`).
//!   7. Return `EntitySeeds`.

use crate::workers::gliner_relex::{Entity, ExtractResponse, ExtractRequest, Triple};

/// Maximum chunk size in bytes — sized below the worker's 8192-byte
/// cap with headroom for label-list overhead in the JSON envelope.
pub const CHUNK_SIZE_BYTES: usize = 7500;

/// Overlap between consecutive chunks in bytes. Ensures entities that
/// span a naive split boundary still appear in at least one chunk in
/// full.
pub const OVERLAP_BYTES: usize = 500;

/// One chunk of the input with its byte offset into the original text.
/// `text` is always valid UTF-8 (the splitter never cuts mid-codepoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub byte_offset: usize,
    pub text: String,
}

/// Split `text` into overlapping chunks of at most `chunk_size_bytes`,
/// each subsequent chunk starting `chunk_size_bytes - overlap_bytes`
/// later. Empty input → empty Vec; input under-cap → single chunk
/// with the whole text.
///
/// The splitter walks UTF-8 char boundaries and never returns a chunk
/// that splits a codepoint. If a single codepoint exceeds the chunk
/// size (impossible in practice — codepoints are at most 4 bytes), the
/// function returns the codepoint as a single chunk regardless of cap.
pub fn chunk_text(text: &str, chunk_size_bytes: usize, overlap_bytes: usize) -> Vec<TextChunk> {
    if text.is_empty() {
        return Vec::new();
    }
    assert!(
        chunk_size_bytes > overlap_bytes,
        "chunk_size_bytes must exceed overlap_bytes"
    );

    if text.len() <= chunk_size_bytes {
        return vec![TextChunk {
            byte_offset: 0,
            text: text.to_string(),
        }];
    }

    let stride = chunk_size_bytes - overlap_bytes;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        // Aim for `start + chunk_size_bytes` but back off to the
        // nearest char-boundary at-or-before that index.
        let mut end = (start + chunk_size_bytes).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1; // walk forward until we land on a boundary
        }
        // Same walk on `start` for safety, though our stride math keeps
        // it aligned in the common case.
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        chunks.push(TextChunk {
            byte_offset: start,
            text: text[start..end].to_string(),
        });
        if end == text.len() {
            break;
        }
        start += stride;
    }
    chunks
}

use crate::entity_extraction::normalize_entity_name;
use std::collections::HashSet;

/// Merge per-chunk extract responses into a single deduped response.
/// Entities are deduped by `(label, normalize_entity_name(text))` —
/// first occurrence's display form wins (matches the DB upsert's
/// first-writer-wins on `entities.name`). Triples are deduped by
/// `(head_norm, tail_norm, relation_norm)` — same first-wins
/// discipline. Entity offsets in the merged response are re-anchored
/// to the original text's byte position via `byte_offset`.
///
/// Inputs are `(byte_offset, response)` pairs. Returns one merged
/// response.
pub fn merge_chunks(chunk_responses: Vec<(usize, ExtractResponse)>) -> ExtractResponse {
    let mut entities: Vec<Entity> = Vec::new();
    let mut seen_entities: HashSet<(String, String)> = HashSet::new();
    let mut triples: Vec<Triple> = Vec::new();
    let mut seen_triples: HashSet<(String, String, String)> = HashSet::new();

    for (offset, resp) in chunk_responses {
        for ent in resp.entities {
            let key = (ent.label.clone(), normalize_entity_name(&ent.text));
            if !seen_entities.contains(&key) {
                seen_entities.insert(key);
                // Re-anchor start/end to the original-text byte position.
                let anchored = Entity {
                    text: ent.text,
                    label: ent.label,
                    start: ent.start.saturating_add(offset as u32),
                    end: ent.end.saturating_add(offset as u32),
                    score: ent.score,
                };
                entities.push(anchored);
            }
        }
        for tri in resp.triples {
            let key = (
                normalize_entity_name(&tri.head.text),
                normalize_entity_name(&tri.tail.text),
                tri.relation.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" "),
            );
            if !seen_triples.contains(&key) {
                seen_triples.insert(key);
                // Triples preserve their head/tail entity_idx as-is.
                // Consumers should not rely on entity_idx after merge
                // (it points into a chunk-local entity list, not the
                // merged list). The upsert path resolves head/tail by
                // text/label lookup anyway.
                triples.push(tri);
            }
        }
    }

    ExtractResponse { entities, triples }
}

use sqlx::PgPool;

/// Result of the upsert pass.
pub struct UpsertOutcome {
    /// IDs of every entity in the merged response, in original order
    /// (whether newly inserted or pre-existing). This is what the
    /// extractor returns to recall as the graph-lane seeds.
    pub entity_ids: Vec<i64>,
    /// Number of entity rows the upsert created (not counting
    /// ON CONFLICT hits).
    pub n_entities_upserted_new: u32,
    /// Number of relation rows the upsert created.
    pub n_relations_inserted: u32,
}

// Integration test coverage in core/tests/entity_extraction_e2e.rs:
//   - upsert_creates_quarantined_entities
//   - upsert_is_idempotent_on_rerun
//   - upsert_dedup_works_with_case_variants
//   - upsert_preserves_operator_unquarantine_decision
//   - upsert_counts_new_inserts_correctly_in_mixed_batch
//   - upsert_batch_* (Issue #95 Layer B, see batch_upsert.rs)

/// Upsert every entity in `merged.entities` into the `entities` table
/// (quarantine=TRUE on new rows; conflict by `(kind, name_norm)` →
/// preserve existing row including its quarantine state). Then for
/// every triple in `merged.triples`, look up the head and tail entity
/// ids and insert into `relations` if no row already exists with the
/// same `(src_id, dst_id, kind)` triple.
///
/// Best-effort idempotent: rerunning with the same input produces no
/// new rows.
///
/// Layer B (Issue #95): the public entry point now delegates to
/// `crate::entity_extraction::batch_upsert::upsert_entities_and_relations`,
/// which batches the entity upsert via `unnest` for a single round-trip
/// in the happy path and falls back to a per-row loop with diagnostic
/// error wrapping on SQLSTATE 23 constraint violations.
pub async fn upsert_entities_and_relations(
    pool: &PgPool,
    merged: &ExtractResponse,
) -> Result<UpsertOutcome, crate::entity_extraction::EntityExtractionError> {
    crate::entity_extraction::batch_upsert::upsert_entities_and_relations(pool, merged).await
}

use crate::entity_extraction::{EntityExtractor, EntityExtractionError, EntitySeeds, SeedSource};
use crate::workers::gliner_relex::Client;
use async_trait::async_trait;
use hhagent_db::entity_kinds::KindsCache;
use hhagent_db::relation_kinds::RelationKindsCache;
use std::sync::Arc;

/// Default thresholds (per spike correction #3 — model is noisy below 0.5).
pub const DEFAULT_THRESHOLD: f32 = 0.5;
pub const DEFAULT_RELATION_THRESHOLD: f32 = 0.5;

/// How the extractor resolves the relation-label vocabulary it passes
/// to the GLiNER worker on every chunk.
///
/// Production builds use `FromDb` so an operator extending the
/// `relation_kinds` table propagates automatically (subject to the
/// 60-second cache TTL). Tests inject a fixed list via `Override` to
/// keep behaviour deterministic without seeding the DB.
enum RelationLabelSource {
    /// Read live from the database via [`RelationKindsCache`]. The
    /// cache memoises for 60 s; operator-driven INSERTs propagate to
    /// the running daemon without an explicit invalidation step.
    FromDb(Arc<RelationKindsCache>),
    /// Hard-coded vocabulary supplied by the caller (typically a unit
    /// test). Bypasses the DB lookup entirely so tests can pin
    /// behaviour against a small, predictable label set without
    /// running migrations or seeding tables.
    Override(Vec<String>),
}

pub struct GlinerRelexExtractor {
    client: Client,
    pool: PgPool,
    kinds_cache: Arc<KindsCache>,
    relation_labels: RelationLabelSource,
}

impl GlinerRelexExtractor {
    /// Build the production extractor with both kind caches seeded
    /// empty. First `extract` call populates each cache via one
    /// `SELECT kind FROM <table>` query.
    pub fn new(client: Client, pool: PgPool) -> Self {
        Self {
            client,
            pool,
            kinds_cache: Arc::new(KindsCache::new()),
            relation_labels: RelationLabelSource::FromDb(Arc::new(RelationKindsCache::new())),
        }
    }

    /// Override the relation-label source with a fixed list. Used by
    /// unit + mock-tier tests that want determinism without seeding
    /// the `relation_kinds` table. Each call replaces the previous
    /// configuration (the field is not append-only).
    ///
    /// Production callers should leave this method un-called so the
    /// extractor reads the live operator-managed vocabulary via
    /// [`RelationKindsCache`].
    pub fn with_relation_labels(mut self, labels: Vec<String>) -> Self {
        self.relation_labels = RelationLabelSource::Override(labels);
        self
    }

    /// Resolve the relation-label list for the current call. Either
    /// returns the operator-managed list from the DB-backed cache or
    /// the test-supplied override.
    ///
    /// Cache-fetch failures on the production path *degrade-and-warn*
    /// rather than abort the whole extraction: an empty list switches
    /// the worker into entity-only mode for this call. Triples are
    /// dropped for the call but entity-anchored recall still works.
    /// The decision mirrors the existing `kinds_cache` failure
    /// handling earlier in [`extract`](Self::extract): the worker
    /// failing or the DB being briefly unavailable should not
    /// silently lose entity-extraction signal too.
    async fn resolve_relation_labels(&self) -> Vec<String> {
        match &self.relation_labels {
            // `Override` is the test-only seam: the caller supplies the
            // exact list passed to the worker. We do NOT apply
            // `strip_undefined_label` here — tests need to be able to
            // pin the worker's behaviour against arbitrary inputs
            // (including `undefined`, if a test ever wants to assert
            // how the worker reacts to it). Production callers must use
            // `FromDb`, where the filter is applied.
            RelationLabelSource::Override(labels) => labels.clone(),
            RelationLabelSource::FromDb(cache) => match cache.list_kinds(&self.pool).await {
                Ok(labels) => strip_undefined_label(labels),
                Err(e) => {
                    tracing::warn!(
                        target: "hhagent::entity_extraction",
                        error = %e,
                        "relation_kinds cache fetch failed; running entity-only for this call",
                    );
                    Vec::new()
                }
            },
        }
    }
}

/// Strip the `undefined` FK-fallback label out of a relation-kinds list
/// before handing it to the worker.
///
/// `undefined` is the `ON DELETE SET DEFAULT` target on the
/// `relations_kind_fk` FK introduced by migration `0017`. It exists so
/// that deleting a kind from `relation_kinds` does not orphan rows in
/// `relations`; it is not a label we want GLiNER to consider matching.
/// Passing it through would invite the model to emit triples with
/// `relation="undefined"`, which carry no semantic content and clutter
/// the graph.
///
/// Pure helper, deterministic, no I/O — extracted from the live cache
/// path so the filter contract is unit-testable without spinning up
/// Postgres.
pub(crate) fn strip_undefined_label(labels: Vec<String>) -> Vec<String> {
    labels.into_iter().filter(|k| k != "undefined").collect()
}

#[async_trait]
impl EntityExtractor for GlinerRelexExtractor {
    async fn extract(&self, query_text: &str) -> Result<EntitySeeds, EntityExtractionError> {
        let started = std::time::Instant::now();
        let chunks = chunk_text(query_text, CHUNK_SIZE_BYTES, OVERLAP_BYTES);
        if chunks.is_empty() {
            // Empty input — return None source, no audit row.
            return Ok(EntitySeeds::empty());
        }

        let labels = self.kinds_cache.list_kinds(&self.pool).await?;
        let relation_labels = self.resolve_relation_labels().await;
        let mut chunk_responses: Vec<(usize, ExtractResponse)> = Vec::new();

        for chunk in &chunks {
            let req = ExtractRequest {
                text: chunk.text.clone(),
                entity_labels: labels.clone(),
                relation_labels: relation_labels.clone(),
                threshold: Some(DEFAULT_THRESHOLD),
                relation_threshold: Some(DEFAULT_RELATION_THRESHOLD),
                max_entities: None,
            };
            match self.client.extract(req).await {
                Ok(resp) => chunk_responses.push((chunk.byte_offset, resp)),
                Err(e) => {
                    tracing::warn!(
                        target: "hhagent::entity_extraction",
                        error = %e,
                        chunk_offset = chunk.byte_offset,
                        "client.extract failed; degrading chunk",
                    );
                }
            }
        }

        if chunk_responses.is_empty() {
            // All chunks failed.
            return Ok(EntitySeeds::empty());
        }

        let n_chunks = chunk_responses.len();
        let merged = merge_chunks(chunk_responses);
        let outcome = upsert_entities_and_relations(&self.pool, &merged).await?;
        let latency_ms_total = started.elapsed().as_millis() as u64;

        // Emit summary audit row — best-effort, WARN on failure.
        let payload = crate::scheduler::audit::build_extract_entities_payload(
            query_text.len(),
            n_chunks,
            merged.entities.len(),
            merged.triples.len(),
            outcome.n_entities_upserted_new,
            outcome.n_relations_inserted,
            "multi-v1.0",
            latency_ms_total,
        );
        if let Err(e) = hhagent_db::audit::insert(
            &self.pool,
            "extractor:gliner-relex",
            crate::scheduler::audit::ACTION_EXTRACT_ENTITIES,
            payload,
        ).await {
            tracing::warn!(
                target: "hhagent::entity_extraction",
                error = %e,
                "extract_entities audit row insert failed; not propagating",
            );
        }

        Ok(EntitySeeds {
            ids: outcome.entity_ids,
            source: SeedSource::GlinerRelex,
            model_version: Some("multi-v1.0".into()),
        })
    }
}

#[cfg(test)]
mod tests;
