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
/// Upsert every entity in `merged.entities` into the `entities` table
/// (quarantine=TRUE on new rows; conflict by `(kind, name_norm)` →
/// preserve existing row including its quarantine state). Then for
/// every triple in `merged.triples`, look up the head and tail entity
/// ids and insert into `relations` if no row already exists with the
/// same `(src_id, dst_id, kind)` triple.
///
/// Best-effort idempotent: rerunning with the same input produces no
/// new rows.
pub async fn upsert_entities_and_relations(
    pool: &PgPool,
    merged: &ExtractResponse,
) -> Result<UpsertOutcome, crate::entity_extraction::EntityExtractionError> {
    let mut entity_ids = Vec::with_capacity(merged.entities.len());
    let mut n_new: u32 = 0;

    // Per-entity upsert. Each entity gets a single statement:
    // INSERT ... ON CONFLICT DO UPDATE SET name_norm = entities.name_norm
    // RETURNING id, (xmax = 0) AS inserted.
    //
    // The `SET name_norm = entities.name_norm` self-assignment is the
    // standard Postgres idiom for "force RETURNING to fire on conflict
    // without changing the row's logical state." It is load-bearing
    // that this clause does NOT touch `quarantine` — if the operator
    // has already approved an entity via the quarantine-review CLI
    // (PR #93), re-extraction must not silently re-quarantine it.
    // Pinned by upsert_preserves_operator_unquarantine_decision in
    // core/tests/entity_extraction_e2e.rs.
    //
    // `xmax = 0` is the canonical inserted-vs-existed discriminator:
    // a fresh row has xmax=0 (no future-deleting transaction); a
    // conflict-hit row carries the conflict txn's xid. This
    // eliminates the previous two-statement path (DO NOTHING +
    // follow-up SELECT) — every entity now costs exactly one
    // round-trip. (Issue #90; Layer A only — full-batch unnest is
    // deferred.)
    //
    // Concurrency bonus: DO UPDATE acquires a row-level exclusive
    // lock on the conflict-hit row and atomically returns the
    // resolved id. The old DO NOTHING + follow-up SELECT did not
    // lock, so concurrent upserts of the same (kind, name_norm)
    // had a narrow window where the SELECT could see uncommitted
    // state. The new path is atomically race-safe; the trade-off
    // is brief serialization under contention on the same key
    // (a non-issue at v2's per-task concurrency).
    //
    // Side effect of the no-op UPDATE: Postgres advances xmin and
    // writes a new tuple version even though no column changed.
    // Acceptable at v2 volume; autovacuum absorbs it without
    // operator action.
    for ent in &merged.entities {
        let name_norm = normalize_entity_name(&ent.text);
        let (id, inserted): (i64, bool) = sqlx::query_as(
            "INSERT INTO entities (kind, name, name_norm, quarantine) \
             VALUES ($1, $2, $3, TRUE) \
             ON CONFLICT (kind, name_norm) DO UPDATE \
               SET name_norm = entities.name_norm \
             RETURNING id, (xmax = 0) AS inserted",
        )
        .bind(&ent.label)
        .bind(&ent.text)
        .bind(&name_norm)
        .fetch_one(pool)
        .await
        .map_err(|e| hhagent_db::DbError::Query(format!("upsert entity: {e}")))?;
        if inserted {
            n_new += 1;
        }
        entity_ids.push(id);
    }

    // Build a (label, name_norm) → id index so we can resolve triple
    // endpoints without re-querying.
    let mut by_key: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();
    for (ent, id) in merged.entities.iter().zip(entity_ids.iter()) {
        by_key.insert(
            (ent.label.clone(), normalize_entity_name(&ent.text)),
            *id,
        );
    }

    let mut n_relations_inserted: u32 = 0;
    for tri in &merged.triples {
        let head_key = (tri.head.r#type.clone(), normalize_entity_name(&tri.head.text));
        let tail_key = (tri.tail.r#type.clone(), normalize_entity_name(&tri.tail.text));
        let head_id = match by_key.get(&head_key) {
            Some(id) => *id,
            None => continue,  // triple references unknown entity — skip
        };
        let tail_id = match by_key.get(&tail_key) {
            Some(id) => *id,
            None => continue,
        };
        let relation_norm = tri.relation
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        // Schema allows multi-edges intentionally (0001 comment); we
        // dedup at the application layer via WHERE NOT EXISTS to make
        // re-extraction idempotent.
        let n: u64 = sqlx::query(
            "INSERT INTO relations (src_id, dst_id, kind, attrs) \
             SELECT $1, $2, $3, '{}'::jsonb \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM relations \
                 WHERE src_id = $1 AND dst_id = $2 AND kind = $3 \
             )",
        )
        .bind(head_id)
        .bind(tail_id)
        .bind(&relation_norm)
        .execute(pool)
        .await
        .map_err(|e| hhagent_db::DbError::Query(format!("insert relation: {e}")))?
        .rows_affected();
        n_relations_inserted += n as u32;
    }

    Ok(UpsertOutcome {
        entity_ids,
        n_entities_upserted_new: n_new,
        n_relations_inserted,
    })
}

use crate::entity_extraction::{EntityExtractor, EntityExtractionError, EntitySeeds, SeedSource};
use crate::workers::gliner_relex::Client;
use async_trait::async_trait;
use hhagent_db::entity_kinds::KindsCache;
use std::sync::Arc;

/// Default thresholds (per spike correction #3 — model is noisy below 0.5).
pub const DEFAULT_THRESHOLD: f32 = 0.5;
pub const DEFAULT_RELATION_THRESHOLD: f32 = 0.5;

pub struct GlinerRelexExtractor {
    client: Client,
    pool: PgPool,
    kinds_cache: Arc<KindsCache>,
    /// v2 ships entities-only. A future slice picks the relation
    /// vocabulary (a `relation_kinds` table mirrors `entity_kinds`).
    relation_labels: Vec<String>,
}

impl GlinerRelexExtractor {
    pub fn new(client: Client, pool: PgPool) -> Self {
        Self {
            client,
            pool,
            kinds_cache: Arc::new(KindsCache::new()),
            relation_labels: Vec::new(),
        }
    }

    /// For tests / future slices that want to pass non-empty relation
    /// labels (triggers triple capture).
    pub fn with_relation_labels(mut self, labels: Vec<String>) -> Self {
        self.relation_labels = labels;
        self
    }
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
        let mut chunk_responses: Vec<(usize, ExtractResponse)> = Vec::new();

        for chunk in &chunks {
            let req = ExtractRequest {
                text: chunk.text.clone(),
                entity_labels: labels.clone(),
                relation_labels: self.relation_labels.clone(),
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
mod tests {
    use super::*;

    #[test]
    fn chunk_text_empty_returns_empty() {
        assert!(chunk_text("", 100, 10).is_empty());
    }

    #[test]
    fn chunk_text_under_cap_returns_single_chunk() {
        let chunks = chunk_text("hello world", 100, 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].byte_offset, 0);
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn chunk_text_exactly_at_cap_returns_single_chunk() {
        let text = "a".repeat(100);
        let chunks = chunk_text(&text, 100, 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.len(), 100);
    }

    #[test]
    fn chunk_text_over_cap_produces_overlapping_chunks() {
        // 250 bytes, cap 100, overlap 20 → stride 80, so chunks at
        // [0..100], [80..180], [160..250]. Three chunks.
        let text = "x".repeat(250);
        let chunks = chunk_text(&text, 100, 20);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].byte_offset, 0);
        assert_eq!(chunks[0].text.len(), 100);
        assert_eq!(chunks[1].byte_offset, 80);
        assert_eq!(chunks[1].text.len(), 100);
        assert_eq!(chunks[2].byte_offset, 160);
        assert_eq!(chunks[2].text.len(), 250 - 160);
    }

    #[test]
    fn chunk_text_walks_utf8_boundary() {
        // "café" is 5 bytes (é is U+00E9 = 0xC3 0xA9). Cap 4 should
        // back off so the chunk ends at the 'f' (byte 3), not split é.
        let text = "café";
        let chunks = chunk_text(text, 4, 1);
        // chunk 0 must be valid UTF-8.
        assert!(std::str::from_utf8(chunks[0].text.as_bytes()).is_ok());
        // No chunk's bytes end mid-codepoint.
        for c in &chunks {
            assert!(std::str::from_utf8(c.text.as_bytes()).is_ok());
        }
    }

    use crate::workers::gliner_relex::{Entity, Triple, TripleEntity, ExtractResponse};

    fn ent(text: &str, label: &str, start: u32, end: u32) -> Entity {
        Entity {
            text: text.into(),
            label: label.into(),
            start, end,
            score: 0.9,
        }
    }

    fn tent(text: &str, ty: &str, idx: u32) -> TripleEntity {
        TripleEntity {
            text: text.into(),
            r#type: ty.into(),
            start: 0,
            end: text.len() as u32,
            entity_idx: idx,
        }
    }

    #[test]
    fn merge_chunks_dedups_entities_by_label_and_norm() {
        let resp_a = ExtractResponse {
            entities: vec![ent("Dr Smith", "person", 0, 8)],
            triples: vec![],
        };
        let resp_b = ExtractResponse {
            // Same person, different case — must dedup.
            entities: vec![ent("DR SMITH", "person", 5, 13)],
            triples: vec![],
        };
        let merged = merge_chunks(vec![(0, resp_a), (7500, resp_b)]);
        assert_eq!(merged.entities.len(), 1, "case-insensitive dedup");
        assert_eq!(merged.entities[0].text, "Dr Smith", "first-writer-wins on display");
    }

    #[test]
    fn merge_chunks_re_anchors_offsets_to_original_text() {
        let resp_a = ExtractResponse {
            entities: vec![ent("alpha", "concept", 0, 5)],
            triples: vec![],
        };
        let resp_b = ExtractResponse {
            entities: vec![ent("beta", "concept", 0, 4)],
            triples: vec![],
        };
        // Second chunk starts at byte 7500 in the original text.
        let merged = merge_chunks(vec![(0, resp_a), (7500, resp_b)]);
        assert_eq!(merged.entities[0].start, 0);
        assert_eq!(merged.entities[0].end, 5);
        assert_eq!(merged.entities[1].start, 7500);
        assert_eq!(merged.entities[1].end, 7500 + 4);
    }

    #[test]
    fn merge_chunks_dedups_triples_by_head_tail_relation() {
        let triple_a = Triple {
            head: tent("Dr Smith", "person", 0),
            tail: tent("asthma", "disease", 1),
            relation: "treats".into(),
            score: 0.95,
        };
        let triple_b = Triple {
            head: tent("DR SMITH", "person", 0),  // case-insensitive same
            tail: tent("Asthma", "disease", 1),
            relation: "TREATS".into(),
            score: 0.92,
        };
        let resp_a = ExtractResponse { entities: vec![], triples: vec![triple_a] };
        let resp_b = ExtractResponse { entities: vec![], triples: vec![triple_b] };
        let merged = merge_chunks(vec![(0, resp_a), (5000, resp_b)]);
        assert_eq!(merged.triples.len(), 1, "case-insensitive triple dedup");
    }
}
