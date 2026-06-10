//! Multi-lane recall over the `memories` table.
//!
//! This module owns the retrieval surface: it runs the configured
//! lanes (semantic via pgvector, lexical via `tsvector`+`ts_rank`),
//! fuses their ranked id-lists via Reciprocal Rank Fusion, then
//! hydrates the top-`k` rows. The lanes themselves live in
//! `kastellan_db::memories`; this module composes them.
//!
//! ## Why RRF and not weighted-sum / softmax-fusion
//!
//! RRF is parameter-free (one constant `k` ≈ 60 from the original
//! 2009 Cormack/Clarke/Buettcher paper, robust across domains), works
//! on rank positions instead of raw scores (so semantic cosine and
//! lexical `ts_rank` don't need calibration to be combined), and is
//! what every contemporary hybrid-search reference (Elasticsearch,
//! Vespa, pgvector docs) recommends for two-lane fusion. The formula:
//!
//!   score(d) = Σ_lanes 1 / (k + rank_lane(d))
//!
//! where `rank_lane(d)` is the 1-based position of document `d` in a
//! lane's ordered list, or "absent" (contributes 0) when the document
//! doesn't appear. Items absent from *every* lane do not appear in
//! the output.

use kastellan_db::graph::Graph;
use kastellan_db::memories::{fetch_by_ids, lexical_search, semantic_search, Memory, EMBEDDING_DIM};
use kastellan_db::DbError;
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
    /// Run the graph lane (1-hop outbound expansion of
    /// [`RecallParams::seed_entity_ids`] via [`kastellan_db::graph::Graph::neighbors`],
    /// then ranking via [`kastellan_db::memories::graph_search`]).
    /// Requires `seed_entity_ids` to be a non-empty slice.
    pub graph: bool,
}

impl RecallModes {
    /// Run every lane — the most common configuration. Phase 1's
    /// scheduler default.
    pub const ALL: RecallModes = RecallModes {
        semantic: true,
        lexical: true,
        graph: true,
    };

    /// Run only the semantic lane.
    pub const SEMANTIC_ONLY: RecallModes = RecallModes {
        semantic: true,
        lexical: false,
        graph: false,
    };

    /// Run only the lexical lane.
    pub const LEXICAL_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: true,
        graph: false,
    };

    /// Run only the graph lane.
    pub const GRAPH_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: false,
        graph: true,
    };

    /// Semantic + lexical lanes, graph off. The default
    /// [`RecallParams::new`] modes (graph requires explicit seeds the
    /// no-seeds constructor can't provide). Use
    /// [`RecallModes::ALL`] when seeds are populated.
    pub const SEMANTIC_AND_LEXICAL: RecallModes = RecallModes {
        semantic: true,
        lexical: true,
        graph: false,
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
    /// Pre-resolved seed entity ids. Used by the graph lane; ignored
    /// when [`RecallModes::graph`] is `false`. The caller resolves
    /// entity names → ids out-of-band (via
    /// [`kastellan_db::graph::Graph::get_entity`] or a future
    /// extraction worker) before invoking recall. An empty slice with
    /// the graph lane enabled is a warn-and-skip, not an error.
    pub seed_entity_ids: Option<&'a [i64]>,
    /// Number of fused results to return. The per-lane queries pull
    /// `k * LANE_FANOUT` candidates so the fusion has enough overlap
    /// to work with even when the lanes disagree heavily — deeper-
    /// than-k per lane is the standard trick for RRF in production
    /// hybrid-search.
    pub k: usize,
    /// Which lanes to run.
    pub modes: RecallModes,
}

impl<'a> RecallParams<'a> {
    /// Common-case constructor: semantic + lexical lanes
    /// ([`RecallModes::SEMANTIC_AND_LEXICAL`]), default budget, no graph
    /// seeds. The graph lane stays off because there are no seeds to
    /// run it against; turning it on without seeds would warn-and-skip
    /// on every call (and is rejected outright once it becomes the only
    /// enabled lane — see [`recall`]). Callers that have entity seeds
    /// use [`RecallParams::with_seeds`] for the graph-enabled shape.
    pub fn new(query_text: &'a str, query_embedding: &'a [f32]) -> Self {
        Self {
            query_text: Some(query_text),
            query_embedding: Some(query_embedding),
            seed_entity_ids: None,
            k: kastellan_db::memories::DEFAULT_RECALL_K,
            modes: RecallModes::SEMANTIC_AND_LEXICAL,
        }
    }

    /// Seed-bearing constructor: all three lanes ([`RecallModes::ALL`]),
    /// default budget, seeds wired in for the graph lane. Use when the
    /// caller has already resolved entity ids (e.g. from an
    /// entity-extraction step or a [`kastellan_db::graph::Graph::get_entity`]
    /// lookup) and wants the graph lane to contribute to fusion.
    pub fn with_seeds(
        query_text: &'a str,
        query_embedding: &'a [f32],
        seed_entity_ids: &'a [i64],
    ) -> Self {
        Self {
            query_text: Some(query_text),
            query_embedding: Some(query_embedding),
            seed_entity_ids: Some(seed_entity_ids),
            k: kastellan_db::memories::DEFAULT_RECALL_K,
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

/// Per-seed cap on outbound neighbour expansion in the graph lane.
///
/// Bounds the worst case: a "hub" entity with thousands of relations
/// (followers, mentions, etc.) cannot flood the expanded set. The
/// value is the order-of-magnitude that [`kastellan_db::graph::Graph::neighbors`]'s
/// `limit` param accepts — generous for typical knowledge graphs,
/// tight against pathological hubs.
pub const GRAPH_FANOUT_CAP_PER_SEED: i64 = 32;

/// Run the configured lanes, fuse via RRF, hydrate the top-`k` rows.
///
/// ## Missing-input policy (hybrid; pinned by [issue #17][0])
///
/// A single enabled lane whose input is missing (e.g. semantic on, no
/// `query_embedding`) is **skipped with a `tracing::warn`** —
/// degrading per-lane lets a caller flip a mode on optimistically when
/// the other lanes have what they need.
///
/// If **every** enabled lane lacks its input, this is a caller bug —
/// fusion over zero lanes is unambiguously an empty result, and silent
/// `Ok(vec![])` would mask the bug at the call site. Returns
/// [`DbError::Query`] with a message identifying which lanes were
/// requested and what input they expected.
///
/// Zero enabled lanes (`modes: RecallModes { ..false }`) is treated
/// the same way: a caller asking for *no* lanes is asking for nothing.
///
/// [0]: https://github.com/hherb/kastellan/issues/17
///
/// ## Other error modes
///
/// Errors propagate from the underlying sqlx queries via
/// [`DbError`]. A `query_embedding` of the wrong dimension is an
/// immediate [`DbError::Query`] (dim mismatch is a hard contract, not
/// a degrade case). The fusion + hydration is best-effort: a
/// hydration of `n` ids may return fewer than `n` rows when one was
/// deleted concurrently — the caller observes a shorter list, not an
/// error.
pub async fn recall(pool: &PgPool, params: &RecallParams<'_>) -> Result<Vec<Memory>, DbError> {
    if params.k == 0 {
        return Ok(Vec::new());
    }
    let lane_k = params.k.saturating_mul(LANE_FANOUT);

    // Track whether any enabled lane actually has the input it needs.
    // The "every enabled lane skipped" case is rejected at the bottom
    // of this function — see the missing-input policy in the docstring.
    let mut any_enabled = false;

    // Run each enabled lane. We could `try_join!` the three lane
    // queries for marginal latency, but Phase 0 throughput doesn't
    // warrant it and sequencing makes the error path simpler — a
    // failure in any lane short-circuits the whole call rather than
    // leaving half-completed futures to abort. (The graph lane fans
    // its *internal* per-seed `neighbors` calls via `try_join_all`
    // because it has no other work to interleave.)
    let mut lane_lists: Vec<Vec<i64>> = Vec::with_capacity(3);

    if params.modes.semantic {
        any_enabled = true;
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
                    target: "kastellan::memory",
                    "semantic lane requested but query_embedding is None; skipping"
                );
            }
        }
    }

    if params.modes.lexical {
        any_enabled = true;
        match params.query_text {
            Some(t) if !t.trim().is_empty() => {
                lane_lists.push(lexical_search(pool, t, lane_k).await?);
            }
            _ => {
                tracing::warn!(
                    target: "kastellan::memory",
                    "lexical lane requested but query_text is empty; skipping"
                );
            }
        }
    }

    if params.modes.graph {
        any_enabled = true;
        match params.seed_entity_ids {
            Some(seeds) if !seeds.is_empty() => {
                // 1-hop outbound expansion via the Graph chokepoint,
                // fanned out in parallel. Per-seed cap defends against
                // hub explosion: an entity with thousands of outbound
                // edges contributes at most GRAPH_FANOUT_CAP_PER_SEED.
                let graph = kastellan_db::graph::PgGraph::new(pool);
                let neighbour_lists = futures::future::try_join_all(
                    seeds.iter().map(|&s| {
                        graph.neighbors(s, None, GRAPH_FANOUT_CAP_PER_SEED)
                    }),
                )
                .await?;

                // Deduped expanded set: seeds ∪ all returned neighbour ids.
                // HashSet strips duplicates when two seeds share a 1-hop
                // hop, or when a seed is also a neighbour of another seed.
                // Pre-sized to the upper bound (seeds + every returned
                // neighbour) so the hot path doesn't rehash on hub-heavy
                // seed sets — `GRAPH_FANOUT_CAP_PER_SEED` already bounds
                // the worst case so this allocation is finite.
                let neighbour_count: usize =
                    neighbour_lists.iter().map(|l| l.len()).sum();
                let mut expanded: std::collections::HashSet<i64> =
                    std::collections::HashSet::with_capacity(
                        seeds.len() + neighbour_count,
                    );
                expanded.extend(seeds.iter().copied());
                for list in &neighbour_lists {
                    for entity in list {
                        expanded.insert(entity.id);
                    }
                }
                let expanded_vec: Vec<i64> = expanded.into_iter().collect();

                lane_lists.push(
                    kastellan_db::memories::graph_search(
                        pool,
                        &expanded_vec,
                        lane_k,
                        false,
                    )
                    .await?,
                );
            }
            _ => {
                tracing::warn!(
                    target: "kastellan::memory",
                    "graph lane requested but seed_entity_ids is empty or None; skipping"
                );
            }
        }
    }

    // Hybrid missing-input policy: zero enabled lanes OR every enabled
    // lane skipped (lane_lists still empty) is a caller bug — see the
    // docstring. The error carries the diagnostic info the caller
    // would otherwise have to dig out of a warn-log line.
    if lane_lists.is_empty() {
        return Err(DbError::Query(format!(
            "recall: no lanes ran (any_enabled={any_enabled}); \
             at least one enabled lane must have its required input — \
             semantic needs query_embedding, lexical needs non-empty query_text, \
             graph needs non-empty seed_entity_ids"
        )));
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

#[cfg(test)]
mod tests;
