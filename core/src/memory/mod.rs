//! `memory` — fused multi-lane retrieval over the `memories` table,
//! plus the helper that turns a free-text query into the embedding
//! vector the semantic lane needs.
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
//!      *Deferred to a follow-up slice* (Option P). The schema has no
//!      entity↔memory linkage today.
//!
//! [`recall`] runs the requested lanes (each returns a *ranked id-list*
//! from `db::memories`), fuses the lists via Reciprocal Rank Fusion,
//! then hydrates the top-k bodies in one round-trip via
//! `fetch_by_ids`.
//!
//! ## Module layout
//!
//! The implementation lives in two siblings so neither file grows past
//! the 500-LOC soft cap in `CLAUDE.md`:
//!
//! * [`recall`] — the retrieval lanes themselves: [`recall`] (the async
//!   entry point), [`reciprocal_rank_fusion`] (the pure fusion
//!   algorithm), [`RecallParams`] / [`RecallModes`] (input shape),
//!   [`RRF_K_CONSTANT`].
//! * [`embed`] — [`embed_query`] (turn a free-text query into a 256-
//!   float embedding via the LLM router and write the first
//!   `actor='llm:router' action='embed'` audit row), plus the shared
//!   [`MemoryError`] surface.
//!
//! Callers compose: `let emb = embed_query(pool, router, q).await?;`
//! then `recall(pool, &RecallParams { query_embedding: Some(&emb), … })`.
//! Keeping `recall` pure-data (no I/O beyond pgvector + tsvector) means
//! tests can seed deterministic embeddings without dragging in a router
//! mock.

mod embed;
pub mod embedder;
pub mod entity_link;
pub mod l0_seed;
pub mod l1_promote;
pub mod entity_reembed;
pub mod l1_reembed;
pub mod reembed;
pub mod l3_approval;
pub mod l3py_approval;
pub mod l3_crystallise;
pub mod l3py_crystallise;
pub mod l3_invoke;
pub mod l3py_invoke;
pub mod l3_surface;
pub mod layers;
mod recall;

// Re-export the public surface so external callers see a flat
// `kastellan_core::memory::{...}` namespace — splitting into submodules
// is an internal refactor that must not break import sites.
pub use embed::{embed_query, MemoryError};
pub use embedder::{Embedder, NoOpEmbedder, RouterEmbedder};
pub use entity_reembed::{entity_embedding_text, reembed_entities_null};
pub use l1_reembed::reembed_l1_null;
pub use reembed::{format_reembed_report, reembed_batch_failed, ReembedReport};
pub use recall::{
    recall, reciprocal_rank_fusion, RecallModes, RecallParams, GRAPH_FANOUT_CAP_PER_SEED,
    RRF_K_CONSTANT,
};
