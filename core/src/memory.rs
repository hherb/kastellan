//! `memory::recall` — fused multi-lane retrieval over the `memories`
//! table.
//!
//! ## Role in the system
//!
//! Phase 1's scheduler asks "what does the agent already know that's
//! relevant to this query?" Three retrieval shapes have value:
//!
//!   1. **Semantic** — pgvector cosine over an embedding of the query.
//!      Best when the query and a stored memory share *meaning* but
//!      no surface words ("the meeting last Tuesday" vs. "scheduled
//!      a 1:1 with Pat for 14:00 on the 8th").
//!   2. **Lexical** — Postgres `tsvector` + `ts_rank`. Best when the
//!      query carries a rare word or proper noun that the embedding
//!      model has no special signal for ("CVE-2026-12345").
//!   3. **Graph** — neighbours of named entities in the query.
//!      *Deferred to a follow-up slice.* The schema has no entity↔
//!      memory linkage today; adding one (likely a join table or
//!      `memories.metadata->>'entities'` GIN index) is a separate
//!      design decision. The HANDOVER's Option N description names
//!      the lane; this skeleton ships the two lanes the schema
//!      already supports.
//!
//! [`recall`] runs the requested lanes (each returns a *ranked id-list*
//! from `db::memories`), fuses the lists via Reciprocal Rank Fusion,
//! then hydrates the top-k bodies in one round-trip via
//! `fetch_by_ids`. The fusion step itself is a pure function exposed
//! as [`reciprocal_rank_fusion`] — useful both for unit testing the
//! algorithm and for any future caller that wants to fuse other
//! ranked id-lists (e.g. a re-ranker stage).
//!
//! ## Why RRF and not weighted-sum / softmax-fusion
//!
//! RRF is parameter-free (one constant `k` ≈ 60 from the original
//! 2009 paper, robust across domains), works on rank positions
//! instead of raw scores (so semantic cosine and lexical `ts_rank`
//! don't need calibration to be combined), and is what every
//! contemporary hybrid-search reference (Elasticsearch, Vespa,
//! pgvector docs) recommends for two-lane fusion. The formula:
//!
//!   score(d) = Σ_lanes 1 / (k + rank_lane(d))
//!
//! where `rank_lane(d)` is the 1-based position of document `d` in
//! lane's ordered list, or "absent" (contributes 0) when the
//! document doesn't appear. Items absent from *every* lane do not
//! appear in the output.
//!
//! ## What's deferred
//!
//! * **LLM-router `actor='llm:router'` audit row** for the embedding
//!   call. The HANDOVER calls this out as the slice's first such
//!   audit row — but the embedding call site (turning `query_text`
//!   into `query_embedding`) will live with the *embedding worker*,
//!   which doesn't exist yet. Today's caller passes the embedding in
//!   directly (the integration test uses a deterministic SHA-256-seeded
//!   helper). The audit-log row will materialise when the embedding
//!   worker lands and dispatches its first call through the router.

use hhagent_db::memories::{fetch_by_ids, lexical_search, semantic_search, Memory, EMBEDDING_DIM};
use hhagent_db::DbError;
use sqlx::PgPool;

/// Reciprocal Rank Fusion's `k` constant.
///
/// 60 is the value from the original Cormack/Clarke/Buettcher 2009
/// paper and is what every reference system uses by default. It's a
/// large enough denominator that the difference between rank 1 and
/// rank 2 is roughly 1.6% of total score (1/61 vs 1/62) — strong
/// enough for the top-of-list to pull through, weak enough that two
/// lanes ranking distinct documents in their respective top-1s tie
/// near the top of the fused list.
pub const RRF_K_CONSTANT: f64 = 60.0;

/// Which retrieval lanes [`recall`] should run.
///
/// Setting a flag to `false` skips the corresponding lane entirely —
/// no SQL is issued, no input is required for that lane. Setting all
/// flags to `false` is permitted but yields an empty fused list; the
/// caller almost always wants at least one lane on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecallModes {
    /// Run the pgvector cosine-distance lane. Requires
    /// [`RecallParams::query_embedding`] to be `Some`.
    pub semantic: bool,
    /// Run the `tsvector` + `ts_rank` lane. Requires
    /// [`RecallParams::query_text`] to be a non-empty string.
    pub lexical: bool,
}

impl RecallModes {
    /// Run both lanes — the most common configuration. Phase 1's
    /// scheduler default.
    pub const ALL: RecallModes = RecallModes {
        semantic: true,
        lexical: true,
    };

    /// Run only the semantic lane.
    pub const SEMANTIC_ONLY: RecallModes = RecallModes {
        semantic: true,
        lexical: false,
    };

    /// Run only the lexical lane.
    pub const LEXICAL_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: true,
    };
}

impl Default for RecallModes {
    fn default() -> Self {
        Self::ALL
    }
}

/// Inputs to [`recall`]. Designed as a struct (vs. a positional arg
/// list) so the call site stays readable when the scheduler grows
/// more knobs (filters, recency boost, workspace scope) in later
/// slices — adding a field here is non-breaking.
#[derive(Clone, Debug)]
pub struct RecallParams<'a> {
    /// Free-text query string. Used by the lexical lane; ignored when
    /// [`RecallModes::lexical`] is `false`.
    pub query_text: Option<&'a str>,
    /// Pre-computed query embedding. Used by the semantic lane;
    /// ignored when [`RecallModes::semantic`] is `false`. Must have
    /// length [`EMBEDDING_DIM`] when present and the semantic lane is
    /// enabled.
    pub query_embedding: Option<&'a [f32]>,
    /// Number of fused results to return. The per-lane queries pull
    /// `k * 4` candidates so the fusion has enough overlap to work
    /// with even when the lanes disagree heavily — deeper-than-k per
    /// lane is the standard trick for RRF in production hybrid-search.
    pub k: usize,
    /// Which lanes to run.
    pub modes: RecallModes,
}

impl<'a> RecallParams<'a> {
    /// Common-case constructor: both lanes, default budget.
    pub fn new(query_text: &'a str, query_embedding: &'a [f32]) -> Self {
        Self {
            query_text: Some(query_text),
            query_embedding: Some(query_embedding),
            k: hhagent_db::memories::DEFAULT_RECALL_K,
            modes: RecallModes::ALL,
        }
    }
}

/// Per-lane fan-out factor. We pull `k * LANE_FANOUT` candidates from
/// each lane so the fusion has enough overlap to surface near-misses
/// — a document that's rank 5 in semantic and rank 5 in lexical wins
/// the fused list against a document that's rank 1 in semantic but
/// absent from lexical, but only if both lanes report deep enough.
///
/// 4× is the value used by the BEIR benchmark suite for the same
/// reason; tuning is a Phase-1 follow-up if measurement shows it
/// matters.
const LANE_FANOUT: usize = 4;

/// Run the configured lanes, fuse via RRF, hydrate the top-`k` rows.
///
/// Lanes that are enabled but lack their input (e.g. semantic enabled
/// without a query_embedding) are skipped with a `tracing::warn` —
/// degrading rather than erroring lets a caller flip a mode on
/// optimistically without first checking for the input. The empty
/// fused list is a valid recall result.
///
/// Errors propagate from the underlying sqlx queries via
/// [`DbError`]. The fusion + hydration is best-effort: a hydration
/// of `n` ids may return fewer than `n` rows when one was deleted
/// concurrently — the caller observes a shorter list, not an error.
pub async fn recall(pool: &PgPool, params: &RecallParams<'_>) -> Result<Vec<Memory>, DbError> {
    if params.k == 0 {
        return Ok(Vec::new());
    }
    let lane_k = params.k.saturating_mul(LANE_FANOUT);

    // Run each enabled lane. We could `tokio::join!` the two queries
    // for marginal latency, but Phase 0 throughput doesn't warrant
    // it and sequencing makes the error path simpler — a failure in
    // either lane short-circuits the whole call rather than leaving
    // a half-completed future to abort.
    let mut lane_lists: Vec<Vec<i64>> = Vec::with_capacity(2);

    if params.modes.semantic {
        match params.query_embedding {
            Some(emb) if emb.len() == EMBEDDING_DIM => {
                lane_lists.push(semantic_search(pool, emb, lane_k).await?);
            }
            Some(_) => {
                return Err(DbError::Query(format!(
                    "semantic lane: embedding dim must be {EMBEDDING_DIM}"
                )));
            }
            None => {
                tracing::warn!(
                    target: "hhagent::memory",
                    "semantic lane requested but query_embedding is None; skipping"
                );
            }
        }
    }

    if params.modes.lexical {
        match params.query_text {
            Some(t) if !t.trim().is_empty() => {
                lane_lists.push(lexical_search(pool, t, lane_k).await?);
            }
            _ => {
                tracing::warn!(
                    target: "hhagent::memory",
                    "lexical lane requested but query_text is empty; skipping"
                );
            }
        }
    }

    if lane_lists.is_empty() {
        return Ok(Vec::new());
    }

    // Fuse and truncate to k. RRF returns scores too, but the typed
    // surface this slice exposes is `Vec<Memory>` — the scores are an
    // internal detail. A future slice that wants score-aware
    // post-processing (e.g. an LLM re-ranker) will use
    // `reciprocal_rank_fusion` directly.
    let lane_refs: Vec<&[i64]> = lane_lists.iter().map(|v| v.as_slice()).collect();
    let fused = reciprocal_rank_fusion(&lane_refs, RRF_K_CONSTANT);
    let top: Vec<i64> = fused.into_iter().take(params.k).map(|(id, _)| id).collect();

    fetch_by_ids(pool, &top).await
}

/// Reciprocal Rank Fusion over `lists` of ranked ids.
///
/// Each inner list MUST be in best-first order — the function uses
/// the *position* of an id (1-based) to compute its contribution.
/// An id that appears in multiple lists is summed across appearances:
///
///   score(id) = Σ_list 1 / (k + position_in_list(id))
///
/// `k` is the RRF damping constant. Use [`RRF_K_CONSTANT`] (60.0) for
/// production; tests may pass a smaller value to make the math easier
/// to reason about. The classical paper recommends `k = 60` after
/// empirical evaluation; smaller `k` puts more weight on the very top
/// of each list.
///
/// The returned `Vec<(id, score)>` is sorted by descending score.
/// Ties (identical scores) are broken by the smaller id first — so
/// the order is stable across runs even when the lanes produce
/// score-tied candidates.
///
/// Pure: deterministic, no I/O, no global state. Same input → same
/// output, every call.
pub fn reciprocal_rank_fusion(lists: &[&[i64]], k: f64) -> Vec<(i64, f64)> {
    use std::collections::HashMap;

    let mut scores: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank0, id) in list.iter().enumerate() {
            let rank = (rank0 + 1) as f64;
            *scores.entry(*id).or_insert(0.0) += 1.0 / (k + rank);
        }
    }

    let mut out: Vec<(i64, f64)> = scores.into_iter().collect();
    // Sort by descending score, then ascending id for stable ties.
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    out
}

/// Build the audit-log payload for an `actor='llm:router' action='embed'`
/// row.
///
/// Pure function — no I/O, no clock reads, no global state. The
/// future caller (`embed_query`, landing in Task 7 of the Option O
/// slice) measures latency, picks the backend string, knows the
/// request's model and the agreed dim, then calls this helper to
/// compose the JSON object that the row's `payload` column carries.
///
/// **What the payload deliberately omits:**
/// * The input texts (privacy — query may carry user PII).
/// * The output embeddings (size + uselessness as audit signal).
/// * HTTP status / body (failures don't write an audit row at all;
///   matches `Router::send` and `tool_host::dispatch` precedent).
///
/// **What it includes** is the minimal operator-facing summary: which
/// model, how many texts, what dimension, which backend, how long.
// `embed_query` (the production caller) lands in Task 7. Until then
// `cargo build` warns this is unused; suppress with a narrow allow
// rather than carrying a yellow warning on main.
#[allow(dead_code)]
pub(crate) fn build_embed_audit_payload(
    model: &str,
    n_texts: usize,
    dim: usize,
    backend: &str,
    latency_ms: u64,
) -> serde_json::Value {
    serde_json::json!({
        "model":      model,
        "n_texts":    n_texts,
        "dim":        dim,
        "backend":    backend,
        "latency_ms": latency_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `k = 60` is the canonical RRF damping; pinning it here makes a
    /// future "tune the constant" PR explicit.
    #[test]
    fn rrf_k_constant_is_sixty() {
        assert!((RRF_K_CONSTANT - 60.0).abs() < f64::EPSILON);
    }

    /// `RecallModes::default` enables every lane — the configuration
    /// the scheduler will use 99% of the time.
    #[test]
    fn recall_modes_default_runs_every_lane() {
        let m = RecallModes::default();
        assert!(m.semantic);
        assert!(m.lexical);
    }

    #[test]
    fn recall_modes_all_is_every_lane_on() {
        assert_eq!(RecallModes::ALL, RecallModes { semantic: true, lexical: true });
    }

    #[test]
    fn recall_modes_semantic_only_disables_lexical() {
        let m = RecallModes::SEMANTIC_ONLY;
        assert!(m.semantic);
        assert!(!m.lexical);
    }

    #[test]
    fn recall_modes_lexical_only_disables_semantic() {
        let m = RecallModes::LEXICAL_ONLY;
        assert!(!m.semantic);
        assert!(m.lexical);
    }

    /// Empty input → empty output. RRF over no lists must not produce
    /// phantom scores.
    #[test]
    fn rrf_over_empty_lane_set_is_empty() {
        let out = reciprocal_rank_fusion(&[], 60.0);
        assert!(out.is_empty());
    }

    /// One lane with N items: output ranks match the input order, and
    /// the score sequence is strictly decreasing (rank 1 > rank 2).
    #[test]
    fn rrf_single_lane_preserves_order() {
        let lane: &[i64] = &[10, 20, 30];
        let out = reciprocal_rank_fusion(&[lane], 60.0);
        let ids: Vec<i64> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![10, 20, 30]);
        assert!(out[0].1 > out[1].1);
        assert!(out[1].1 > out[2].1);
    }

    /// Two lanes, full agreement: the same top item wins, and its
    /// score is exactly the sum of the per-lane contributions.
    #[test]
    fn rrf_two_lanes_agreeing_sums_scores() {
        let a: &[i64] = &[10, 20, 30];
        let b: &[i64] = &[10, 20, 30];
        let out = reciprocal_rank_fusion(&[a, b], 60.0);
        let top = out[0];
        assert_eq!(top.0, 10);
        // Doc 10 is rank 1 in both lanes: 2 / (60 + 1) = 0.0327...
        let expected = 2.0 / 61.0;
        assert!(
            (top.1 - expected).abs() < 1e-9,
            "expected {}, got {}",
            expected,
            top.1
        );
    }

    /// Two lanes, one item in lane A, a *different* item in lane B —
    /// both at rank 1 in their lane. The fused list's top is the one
    /// with the smaller id (tiebreaker), and both have identical
    /// scores.
    #[test]
    fn rrf_two_lanes_disagreeing_ties_break_on_smaller_id() {
        let a: &[i64] = &[42];
        let b: &[i64] = &[7];
        let out = reciprocal_rank_fusion(&[a, b], 60.0);
        // Both have identical scores 1 / 61. Tiebreaker is smaller id.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 7);
        assert_eq!(out[1].0, 42);
        assert!((out[0].1 - out[1].1).abs() < 1e-12);
    }

    /// Items absent from a lane contribute 0, not a penalty. So a
    /// top-of-list-in-lane-A item that's absent from lane B still
    /// outranks a mid-list item that's only in lane A.
    #[test]
    fn rrf_absent_items_contribute_zero() {
        let a: &[i64] = &[1, 2, 3];
        let b: &[i64] = &[]; // empty lane
        let out = reciprocal_rank_fusion(&[a, b], 60.0);
        // Same as single-lane.
        let ids: Vec<i64> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    /// A document at rank 1 in one lane and rank 1 in the other beats
    /// a document at rank 1 in only one lane — the value of fusion.
    #[test]
    fn rrf_two_lane_winner_beats_single_lane_winner() {
        // Lane A: doc 1 first, doc 2 second.
        // Lane B: doc 1 second, doc 9 first.
        // Doc 1 is the only doc that appears in both lanes near the
        // top; doc 9 only appears in B (at rank 1); doc 2 only appears
        // in A (at rank 2). Doc 1's two-lane score must be higher than
        // doc 9's single-lane top score.
        let a: &[i64] = &[1, 2];
        let b: &[i64] = &[9, 1];
        let out = reciprocal_rank_fusion(&[a, b], 60.0);
        assert_eq!(out[0].0, 1, "two-lane appearer must rank first: {out:?}");
    }

    /// Smaller `k` puts more weight on rank 1 — sanity check that the
    /// constant is plumbed through and not hardcoded inside.
    #[test]
    fn rrf_smaller_k_weights_top_more() {
        let lane: &[i64] = &[10, 20];
        let out_60 = reciprocal_rank_fusion(&[lane], 60.0);
        let out_1 = reciprocal_rank_fusion(&[lane], 1.0);
        // Top score with k=60: 1/61 ≈ 0.0164
        // Top score with k=1: 1/2 = 0.5
        assert!(out_1[0].1 > out_60[0].1);
        // Ratio between rank 1 and rank 2 widens as k shrinks.
        let ratio_60 = out_60[0].1 / out_60[1].1;
        let ratio_1 = out_1[0].1 / out_1[1].1;
        assert!(ratio_1 > ratio_60);
    }

    /// The audit payload must NOT carry user text or embeddings —
    /// privacy + size. Pinned so a future refactor that "adds context"
    /// to the row gets caught at the right moment.
    ///
    /// Note: `"n_texts"` is an intentional key (count of inputs, not the
    /// inputs themselves). The checks below guard against leaking the
    /// *content* fields by their canonical key names.
    #[test]
    fn embed_audit_payload_excludes_input_text_and_embeddings() {
        let v = build_embed_audit_payload("bge-m3", 1, 1024, "local", 42);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("\"input\""), "input leaked: {s}");
        assert!(!s.contains("\"input_text\""), "input_text leaked: {s}");
        assert!(!s.contains("\"query_text\""), "query_text leaked: {s}");
        assert!(!s.contains("\"query\""), "query leaked: {s}");
        assert!(!s.contains("\"embedding\""), "embedding leaked: {s}");
        assert!(!s.contains("\"data\""), "data leaked: {s}");
    }

    /// The audit payload must carry the operator-facing summary fields.
    #[test]
    fn embed_audit_payload_includes_load_bearing_fields() {
        let v = build_embed_audit_payload("bge-m3", 1, 1024, "local", 87);
        assert_eq!(v["model"], "bge-m3");
        assert_eq!(v["n_texts"], 1);
        assert_eq!(v["dim"], 1024);
        assert_eq!(v["backend"], "local");
        assert_eq!(v["latency_ms"], 87);
    }

    /// `latency_ms` is `u64` upstream; pin that it serialises as a
    /// JSON number (not stringly).
    #[test]
    fn embed_audit_payload_latency_is_numeric() {
        let v = build_embed_audit_payload("m", 1, 4, "local", 12345);
        assert!(v["latency_ms"].is_number(), "latency must be a JSON number");
        assert_eq!(v["latency_ms"].as_u64(), Some(12345));
    }
}
