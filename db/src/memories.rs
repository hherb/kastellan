//! Typed sqlx helpers for the `memories` table.
//!
//! ## What this module owns
//!
//! Every read and write of `memories` goes through one of the helpers
//! below — same chokepoint discipline `db::audit` and `db::secrets`
//! follow. Outside callers (today: `core::memory::recall`) never write
//! raw SQL against the table. Two payoffs:
//!
//!   1. The `vector(256)` bind shape lives in *one* place. We choose
//!      to encode embeddings as their canonical Postgres-text form
//!      (`'[0.12, 0.34, ...]'::vector`) rather than pull in the
//!      `pgvector` Rust crate. The reasons are documented on
//!      [`vector_literal`]; if a future call site grows enough
//!      embedding-traffic to make the dep worthwhile, the swap is
//!      strictly local.
//!   2. Fusion (RRF) and per-lane retrieval are decoupled. Each `*_search`
//!      helper returns a `Vec<i64>` of memory ids in best-first order;
//!      the fusion in `core::memory` is then a pure function over those
//!      ranked id-lists. That makes the fusion unit-testable without a
//!      DB and pins the per-lane shape to "ranked id-list," which is
//!      exactly what RRF needs.
//!
//! ## Why no HNSW index in this slice
//!
//! `0001_init.sql` deliberately omits the HNSW index on
//! `memories.embedding`; HNSW build cost is dominated by the row count
//! at index-creation time, so building against an empty table just to
//! grow it row-by-row is strictly worse than building once after the
//! first batch ingest. Phase 1's first-load step is where the index
//! materialises. Until then the `<=>` cosine-distance ORDER BY is a
//! sequential scan, which is fine at the corpus sizes this slice is
//! exercised against (the integration test seeds 3 rows).
//!
//! ## Phase-1 surface
//!
//! * **Graph lane.** Shipped 2026-05-12. The `memory_entities` join
//!   table (migration 0007) backs entity↔memory linkage; the
//!   writer-side helper [`link_memory_to_entities`] and the read-side
//!   helper [`graph_search`] live in this module. The 1-hop outbound
//!   expansion (via the `db::graph::Graph` chokepoint) happens in
//!   `core::memory::recall`. Future entity-similarity over
//!   `entities.embedding` (still NULL today) is a separate Phase-1
//!   follow-up.
//! * **Embedding worker.** `insert_memory` accepts an `Option<&[f32]>`
//!   and stores NULL when absent. `embed_query` shipped via Option O
//!   in `core::memory::embed`; the production caller routes the body
//!   through the embedding worker before inserting. Tests use the
//!   deterministic SHA-256-seeded helper documented in
//!   `core/tests/memory_recall_e2e.rs`.

use std::fmt::Write as _;

use crate::DbError;

// The read and write query helpers live in sibling modules to keep each
// file under the 500-LOC cap (split 2026-05-30). They are re-exported
// here so every external call site keeps its `db::memories::<name>`
// path unchanged — the split is invisible to callers. Both siblings
// reach the shared vocabulary in this parent (the consts, the
// `check_embedding_dim` / `limit_as_i64` guards, `vector_literal`, and
// the `Memory` / `MemoryLayer` types) via `super::`.
mod search;
mod write;

pub use search::{
    fetch_by_ids, graph_search, lexical_search, load_active_l0, load_layer, load_layer_by_trust,
    semantic_search,
};
pub use write::{
    delete_memory_at_layer, insert_memory, insert_memory_at_layer, insert_memory_light,
    link_memory_to_entities, seed_meta_memory, set_skill_trust,
};

/// Required dimensionality of every embedding written to `memories`.
///
/// Pinned by migration `0019_embedding_dim_256.sql`'s `vector(256)`
/// column type. The active embedding model is **embeddinggemma**, a
/// Matryoshka (MRL) model whose 256-dim prefix retains strong retrieval
/// quality at ~3× less storage and faster ANN than its native 768-dim
/// output — so [`truncate_to_embedding_dim`] keeps the leading 256
/// components and renormalizes. (Historical note: migrations 0001/0008
/// created these columns as `vector(1024)` for the original bge-m3
/// candidate; 0019 narrowed them.)
///
/// A mismatch surfaces as a Postgres error at INSERT time
/// (`expected 256 dimensions, not <N>`); the application-layer check in
/// [`insert_memory`] catches it earlier with an operator-readable
/// message.
pub const EMBEDDING_DIM: usize = 256;

/// Default fusion budget when a caller hasn't specified one.
///
/// 10 is the order-of-magnitude that Phase 1's scheduler will start
/// with — small enough that the LLM's context budget is undisturbed,
/// large enough that RRF has multiple candidates per lane to fuse.
pub const DEFAULT_RECALL_K: usize = 10;

/// Reject embeddings whose length doesn't match [`EMBEDDING_DIM`].
///
/// Shared by [`insert_memory`] (write path) and [`semantic_search`]
/// (read path) so the operator-readable error message is identical at
/// both ends. The check fires before any sqlx call, so unit tests can
/// exercise it without a live executor.
fn check_embedding_dim(label: &str, v: &[f32]) -> Result<(), DbError> {
    if v.len() != EMBEDDING_DIM {
        return Err(DbError::Query(format!(
            "{label} embedding dim mismatch: got {}, expected {}",
            v.len(),
            EMBEDDING_DIM
        )));
    }
    Ok(())
}

/// Adapt a raw model embedding to the storage contract via Matryoshka
/// (MRL) truncation: keep the leading [`EMBEDDING_DIM`] components, then
/// L2-renormalize.
///
/// embeddinggemma — and other MRL-trained models — pack the most
/// information-dense signal into the leading components, so the
/// leading-`N` prefix is itself a valid lower-dimensional embedding.
/// Truncation breaks unit norm, so we renormalize to restore it.
///
/// Note the renormalization does **not** change semantic-recall
/// ranking today: `semantic_search` orders by pgvector's cosine
/// operator (`<=>`, see `db::memories::search`), which is scale-
/// invariant. We renormalize anyway so the stored representation is
/// canonical unit-norm — matching the other unit-norm vectors recall
/// compares against and keeping cosine equal to the dot product for any
/// future inner-product (`<#>`) path.
///
/// Pure function — no I/O, no global state — so the write path
/// ([`insert_memory`]) and the query path
/// (`core::memory::embed_query`) share one canonicalization and the
/// 256-dim contract lives in exactly one place.
///
/// # Errors
/// [`DbError::Query`] if the model returned *fewer* than
/// [`EMBEDDING_DIM`] components — Matryoshka can shrink an embedding but
/// not synthesize missing dimensions. A zero-norm prefix (vanishingly
/// unlikely from a real model) is returned un-normalized rather than
/// dividing by zero.
pub fn truncate_to_embedding_dim(raw: &[f32]) -> Result<Vec<f32>, DbError> {
    if raw.len() < EMBEDDING_DIM {
        return Err(DbError::Query(format!(
            "embedding too short for Matryoshka truncation: got {}, need at least {}",
            raw.len(),
            EMBEDDING_DIM
        )));
    }
    let mut v = raw[..EMBEDDING_DIM].to_vec();
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    Ok(v)
}

/// `usize` → `i64` for SQL `LIMIT` binds. Saturates at `i64::MAX`
/// rather than wrapping to a negative value (which Postgres would
/// reject with a runtime error far from the call site).
fn limit_as_i64(k: usize) -> i64 {
    i64::try_from(k).unwrap_or(i64::MAX)
}

/// Memory hierarchy layers, mirroring GenericAgent's 5-layer design.
///
/// Discriminant values 0..=4 match the SMALLINT stored in
/// `memories.layer` and `deleted_memories.layer` (migrations 0013 +
/// 0014). The CHECK constraint at the DB boundary guarantees no other
/// value is ever read back, so [`MemoryLayer::from_db`] only needs to
/// defend against a corrupted-row case; production code paths never
/// trip it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i16)]
pub enum MemoryLayer {
    /// L0 — meta-rules / hard constraints (e.g. "never `rm -rf`").
    /// Hand-curated seed data only; never written by the agent itself.
    /// [`insert_memory_at_layer`] **rejects** this variant with
    /// [`DbError::PolicyViolation`]; the only writer path is
    /// [`seed_meta_memory`], deliberately named so a `grep` over the
    /// tree surfaces every L0 write site.
    Meta = 0,
    /// L1 — insight index. Small routing pointers loaded
    /// unconditionally into every system prompt by
    /// `core::memory::layers::load_l1`. The whole point of the layer
    /// is "fits in the prompt regardless of similarity score."
    Index = 1,
    /// L2 — stable accumulated facts. Default for [`insert_memory`]
    /// and the layer every pre-migration row backfills to.
    Stable = 2,
    /// L3 — skills / SOPs (parameterised procedures). Reserved; no
    /// writer in the slice that introduced this enum.
    Skill = 3,
    /// L4 — session digests. Reserved; no writer in the slice that
    /// introduced this enum.
    Digest = 4,
}

impl MemoryLayer {
    /// Decode the SMALLINT stored in `memories.layer` / `deleted_memories.layer`.
    ///
    /// The DB CHECK constraint forbids out-of-range values, so this
    /// only returns `Err` if the column was tampered with via a path
    /// that bypassed the constraint (e.g. a future migration with a
    /// bug). The error type is [`DbError::Invariant`] specifically
    /// because hitting it means the schema invariant was broken —
    /// not a transient query failure.
    pub fn from_db(raw: i16) -> Result<Self, DbError> {
        match raw {
            0 => Ok(Self::Meta),
            1 => Ok(Self::Index),
            2 => Ok(Self::Stable),
            3 => Ok(Self::Skill),
            4 => Ok(Self::Digest),
            other => Err(DbError::Invariant(format!(
                "memory layer out of range: {other}"
            ))),
        }
    }

    /// Encode the layer as the SMALLINT value bound to SQL parameters.
    /// Pair with [`Self::from_db`] for round-trips.
    pub fn as_db(self) -> i16 {
        self as i16
    }
}

/// One row from `memories` returned from a fully hydrated query.
///
/// `embedding` is intentionally NOT decoded back into a `Vec<f32>` —
/// callers that need the raw vector should be retrieving it through a
/// dedicated path that opts in to the (future) `pgvector` Rust crate's
/// decode. Recall does not need the bytes; the column existence is
/// enough.
#[derive(Clone, Debug)]
pub struct Memory {
    /// Strictly monotonic `BIGSERIAL` from the table.
    pub id: i64,
    /// Free-form body. Phase 1's scheduler renders this into the
    /// LLM context.
    pub body: String,
    /// JSONB metadata. Phase 1's caller may store workspace, channel,
    /// source URL, originator entity, etc. The schema enforces no
    /// shape — that's by design.
    pub metadata: serde_json::Value,
    /// Memory hierarchy layer (migrations 0013 + 0014). Defaults to
    /// [`MemoryLayer::Stable`] at the DB level for any row inserted
    /// without an explicit layer; [`insert_memory_at_layer`] is the
    /// writer-side helper for non-default layers.
    pub layer: MemoryLayer,
    /// `now()`-derived insertion timestamp. The recall path returns it
    /// unsorted (the caller may sort by recency as a tiebreaker
    /// downstream).
    pub created_at: time::OffsetDateTime,
}


/// Format a `Vec<f32>` as the canonical pgvector text representation.
///
/// pgvector's text input format is `[v0,v1,...,vN-1]` with a trailing
/// `]`, no whitespace, and standard floating-point literals. The
/// extension's parser accepts both decimal (`0.5`) and scientific
/// (`5e-1`) forms; we delegate to Rust's `f32::Display`, which emits
/// the shortest round-trippable representation — usually decimal for
/// human-scale magnitudes (`0.5`, `-1.25`) but scientific for very
/// small or very large values (`1e-10`, `3.4e38`). Both forms are
/// accepted by pgvector and round-trip losslessly, so the choice is
/// invisible to correctness; the only operator-visible effect is the
/// shape of values they read in EXPLAIN.
///
/// **Why text-cast and not the `pgvector` Rust crate.** The crate
/// wraps the same string round-trip with stronger types and a sqlx
/// `Encode`/`Decode` impl. We avoid the dep for two reasons:
///
///   1. **Dep audit surface.** Every workspace dep is licence-checked
///      and pulled in across all build targets. The `pgvector` crate
///      is MIT (AGPL-compatible) but pulls `byteorder` and an extra
///      sqlx-feature shim; until a second consumer needs decode, the
///      text-cast is strictly cheaper.
///   2. **Throughput shape.** Phase-0 scale: a handful of recall calls
///      per minute. The cost is dominated by the network round-trip
///      and the index lookup, not the formatter.
///
/// When the embedding worker (Phase 1+) lands and starts streaming
/// vectors at higher rates, swap this for `pgvector::Vector::from(v)`
/// + `.bind(...)`. The swap is strictly local to this module.
///
/// Pure: no I/O, deterministic — same input, same string every call.
pub fn vector_literal(v: &[f32]) -> String {
    // Heuristic capacity: each f32 prints to ~10 chars on average; the
    // exact value doesn't matter for correctness, just allocation
    // pressure on a hot path.
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // `f32` Display gives the shortest round-trippable
        // representation (decimal for human-scale values, scientific
        // for very small/large) — both are valid pgvector input.
        // NaN/Inf produce strings pgvector rejects, but we never
        // expect those: embeddings come from a normalised model
        // output and are pre-validated by the embedding worker.
        // Defense in depth: a future caller that introduces
        // unsanitised floats will get a clear pgvector error at
        // INSERT time, not silent corruption.
        write!(&mut s, "{}", x).expect("write to String cannot fail");
    }
    s.push(']');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the embedding dim. Cluster-side `vector(256)` (migration
    /// 0019) and Rust-side constant must agree; if either drifts the
    /// integration test will trip immediately.
    #[test]
    fn embedding_dim_is_256() {
        assert_eq!(EMBEDDING_DIM, 256);
    }

    /// Default fusion budget is non-zero. A zero default would let a
    /// caller construct an "empty modes" recall that returns nothing
    /// without an obvious cause — we'd rather force the explicit
    /// passing of `k = 0` if someone genuinely wants that.
    // `DEFAULT_RECALL_K` is a const, so the comparison is const-foldable
    // — intentional: this is a drift pin that trips if the default is
    // ever changed to zero.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn default_recall_k_is_at_least_one() {
        assert!(DEFAULT_RECALL_K >= 1);
    }

    /// Empty vector is still valid input — pgvector rejects it at
    /// INSERT (it expects exactly 256 dimensions), but the formatter
    /// emits the canonical `[]` shape regardless. Rejecting it here
    /// would mask the operator-readable error.
    #[test]
    fn vector_literal_handles_empty_slice() {
        assert_eq!(vector_literal(&[]), "[]");
    }

    /// Single-element shape. The bracket-comma shape with no trailing
    /// comma matches the pgvector parser's expectation.
    #[test]
    fn vector_literal_single_element_no_trailing_comma() {
        assert_eq!(vector_literal(&[0.5]), "[0.5]");
    }

    /// Multi-element ordering preserved. `<=>` similarity is sensitive
    /// to position, so a permutation of the input would silently corrupt
    /// the cosine distance — this test pins the order.
    #[test]
    fn vector_literal_preserves_order() {
        let v = [1.0_f32, 2.0, 3.0];
        assert_eq!(vector_literal(&v), "[1,2,3]");
    }

    /// Negative values flow through verbatim. Embedding components are
    /// signed; if a refactor ever tried to abs() them the cosine
    /// similarity would be silently wrong. (Defensive: caught by
    /// integration test too, but this is faster feedback.)
    #[test]
    fn vector_literal_passes_through_negatives() {
        assert_eq!(vector_literal(&[-0.5_f32, 0.5]), "[-0.5,0.5]");
    }

    /// Dim-check shape pin: the shared helper rejects a too-short
    /// vector with a `Query` error whose message names both expected
    /// and actual dim, plus the call-site label so an operator can
    /// tell INSERT-side from query-side errors apart. Pure — runs
    /// without a DB. Both `insert_memory` and `semantic_search` route
    /// through this same helper, so this is the real production path.
    #[test]
    fn check_embedding_dim_rejects_too_short() {
        let too_short: Vec<f32> = vec![0.0; 10];
        let err = check_embedding_dim("insert", &too_short).unwrap_err();
        match err {
            DbError::Query(msg) => {
                assert!(msg.contains("dim mismatch"), "msg: {msg}");
                assert!(msg.contains("insert"), "label missing in: {msg}");
                assert!(msg.contains("10"), "got-dim missing in: {msg}");
                assert!(msg.contains("256"), "expected-dim missing in: {msg}");
            }
            other => panic!("expected DbError::Query, got {other:?}"),
        }
    }

    /// Same helper accepts an exact-length input.
    #[test]
    fn check_embedding_dim_accepts_correct_length() {
        let ok: Vec<f32> = vec![0.0; EMBEDDING_DIM];
        check_embedding_dim("query", &ok).expect("exact-length input must pass");
    }

    /// `limit_as_i64` saturates at `i64::MAX` rather than wrapping.
    /// Realistic `k` values (≤ a few hundred) flow through unchanged;
    /// the saturation is defense-in-depth against a future caller
    /// passing an unreasonably large `k` from a config file.
    #[test]
    fn limit_as_i64_saturates_at_i64_max() {
        assert_eq!(limit_as_i64(0), 0);
        assert_eq!(limit_as_i64(40), 40);
        assert_eq!(limit_as_i64(usize::MAX), i64::MAX);
    }

    /// Matryoshka truncation of a larger model output (e.g.
    /// embeddinggemma's native 768) yields exactly [`EMBEDDING_DIM`]
    /// components, ready to satisfy `check_embedding_dim` / the
    /// `vector(256)` column.
    #[test]
    fn truncate_shrinks_oversized_output_to_embedding_dim() {
        let raw: Vec<f32> = (0..768).map(|i| (i as f32) + 1.0).collect();
        let out = truncate_to_embedding_dim(&raw).expect("768 ≥ 256 truncates");
        assert_eq!(out.len(), EMBEDDING_DIM);
        check_embedding_dim("query", &out).expect("truncated output passes the dim gate");
    }

    /// The result is L2-normalised (unit norm) so cosine == dot product
    /// downstream — truncation breaks the source norm, renormalization
    /// restores it.
    #[test]
    fn truncate_output_is_unit_norm() {
        let raw: Vec<f32> = (0..512).map(|i| (i as f32) * 0.5 - 3.0).collect();
        let out = truncate_to_embedding_dim(&raw).expect("512 ≥ 256");
        let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    /// Truncation keeps the leading components' *direction*: each output
    /// element is the matching input element scaled by one shared
    /// factor (1/‖prefix‖), so ratios between components are preserved.
    #[test]
    fn truncate_preserves_leading_component_direction() {
        let raw: Vec<f32> = (0..300).map(|i| (i as f32) + 1.0).collect();
        let out = truncate_to_embedding_dim(&raw).expect("300 ≥ 256");
        // out[k] / out[0] must equal raw[k] / raw[0] (same scale factor).
        let ratio_in = raw[100] / raw[0];
        let ratio_out = out[100] / out[0];
        assert!(
            (ratio_in - ratio_out).abs() < 1e-4,
            "direction not preserved: in {ratio_in} vs out {ratio_out}"
        );
    }

    /// An exact-length input is sliced (no-op) then renormalized — still
    /// [`EMBEDDING_DIM`] long and unit-norm.
    #[test]
    fn truncate_accepts_exact_length() {
        let raw: Vec<f32> = vec![0.25; EMBEDDING_DIM];
        let out = truncate_to_embedding_dim(&raw).expect("exact length is valid");
        assert_eq!(out.len(), EMBEDDING_DIM);
    }

    /// A model that returns *fewer* than [`EMBEDDING_DIM`] components
    /// cannot be Matryoshka-upscaled — reject with a clear message.
    #[test]
    fn truncate_rejects_too_short_output() {
        let raw: Vec<f32> = vec![0.1; EMBEDDING_DIM - 1];
        let err = truncate_to_embedding_dim(&raw).expect_err("128 < 256 must error");
        match err {
            DbError::Query(msg) => {
                assert!(msg.contains("too short"), "msg: {msg}");
                assert!(msg.contains("256"), "expected-dim missing in: {msg}");
            }
            other => panic!("expected DbError::Query, got {other:?}"),
        }
    }

    /// A zero-norm prefix is returned un-normalized rather than dividing
    /// by zero (defends the `norm > 0.0` guard).
    #[test]
    fn truncate_zero_vector_does_not_divide_by_zero() {
        let raw: Vec<f32> = vec![0.0; EMBEDDING_DIM];
        let out = truncate_to_embedding_dim(&raw).expect("zero vector is length-valid");
        assert_eq!(out.len(), EMBEDDING_DIM);
        assert!(out.iter().all(|x| *x == 0.0), "zero stays zero, no NaN");
    }
}
