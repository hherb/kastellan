//! `recall_assembly` — runs a per-query retrieval and packages the
//! result for prompt assembly.
//!
//! ## Role in the system
//!
//! Sibling of [`crate::prompt_assembly`]. Both modules run inside
//! `RouterAgent::formulate_plan` before each LLM call:
//!
//! 1. [`RecallBuilder::build`] (this module) — embeds the task
//!    instruction, fans out to `recall(SEMANTIC | LEXICAL)`, and
//!    returns the ranked rows plus a SHA-256 of the query text.
//! 2. [`crate::prompt_assembly::SystemPromptBuilder::build_with_recalled`]
//!    consumes the [`RecalledContext`] and threads it into the
//!    assembled `<l0>/<l1>/<recalled>/<base>` system message.
//!
//! Recall is **enrichment, not policy**: failure here degrades to an
//! empty context with a `tracing::warn!`, and the agent still plans
//! against the L0/L1/base prompt. This is asymmetric to
//! [`crate::prompt_assembly::PromptAssemblyError`], which is
//! fail-closed (a missing L0 rule must never silently reach the
//! model).
//!
//! ## Module layout
//!
//! * [`pg_builder::PgRecallBuilder`] — production impl. Holds a
//!   [`sqlx::PgPool`] and an [`kastellan_llm_router::Router`]; composes
//!   [`crate::memory::embed_query`] + [`crate::memory::recall`].
//! * [`pg_builder::StaticRecallBuilder`] — test impl. Returns a fixed
//!   [`RecalledContext`] regardless of the query string.
//!
//! ## Why a trait instead of a free function
//!
//! Mirrors the [`crate::prompt_assembly::SystemPromptBuilder`] precedent:
//! tests swap in [`pg_builder::StaticRecallBuilder`]; production wires
//! [`pg_builder::PgRecallBuilder`] through `RouterAgent::new`. A future
//! "history-aware" recall (one that includes prior plan iterations in
//! the query text) is a new type implementing the same trait, not a
//! rewrite of the call site.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::memory::MemoryError;
use kastellan_db::DbError;

pub mod pg_builder;

pub use pg_builder::{PgRecallBuilder, StaticRecallBuilder};

/// Errors returned by [`RecallBuilder::build`].
///
/// Note: the caller in `RouterAgent::formulate_plan` is expected to
/// **swallow** these (treat as [`RecalledContext::empty()`] and emit a
/// `tracing::warn!`). The enum exists so impls can distinguish embed
/// failures from DB failures in logs / tests, not so the agent can
/// retry.
#[derive(Debug, Error)]
pub enum RecallError {
    /// The embedding call (`Router::embed`) failed; see the wrapped
    /// [`MemoryError`] for the specific cause (transport, dim
    /// mismatch, count mismatch).
    #[error("embed_query failed: {0}")]
    EmbedQuery(#[from] MemoryError),
    /// One of the recall lanes (semantic, lexical) returned a DB
    /// error. Wraps [`DbError`] from `core::memory::recall`.
    #[error("recall lane failed: {0}")]
    DbLane(#[from] DbError),
}

/// Output of a [`RecallBuilder::build`] call.
///
/// **Invariant:** `bodies.len() == ids.len()`. Enforced by
/// [`Self::new`] (the canonical constructor) — direct struct-literal
/// construction skips the check and is reserved for the
/// `RecalledContext::empty` sentinel and internal callers that build
/// the two vectors together (e.g. `cap_and_split`). External crates
/// must use [`Self::new`].
///
/// Both vectors are in fused-rank order (semantic + lexical, fused via
/// RRF; see [`crate::memory::recall`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecalledContext {
    /// Memory ids in fused order, capped at the byte cap (see
    /// [`L_RECALL_CAP_BYTES`]). Written to the `recalled_memory_ids`
    /// audit-row key.
    pub ids: Vec<i64>,
    /// Bodies in the same order as [`Self::ids`]. Cumulative byte
    /// length ≤ [`L_RECALL_CAP_BYTES`]; rows that would breach the
    /// cap are dropped with `tracing::warn!`. This is the field the
    /// assembler iterates over and the canonical source for
    /// [`Self::len`].
    pub bodies: Vec<String>,
    /// Hex SHA-256 of the query text (the task instruction). Lets
    /// observation phase detect paraphrase-vs-drift across captures.
    /// Always 64 hex chars (SHA-256 of any input, including empty).
    ///
    /// **Caveat on the degrade path:** when [`RecallBuilder::build`]
    /// returns `Err` and the caller in `RouterAgent::formulate_plan`
    /// substitutes [`Self::empty`], this field becomes the SHA-256 of
    /// the *empty string* — not the SHA-256 of the actual instruction
    /// that was being processed. Observation SQL that joins rows on
    /// `recall_query_sha256` must treat the canonical empty-string
    /// digest (`e3b0c442…`) as "recall did not run successfully" rather
    /// than "this query was empty".
    pub query_sha256: String,
}

impl RecalledContext {
    /// Canonical constructor. Enforces the `ids.len() == bodies.len()`
    /// invariant the rest of the module relies on (the assembler
    /// iterates over `bodies`; `recalled_count` audit-row key is sourced
    /// from `bodies.len()`). Panics on mismatch — this is a programmer
    /// error, not a runtime failure mode.
    pub fn new(ids: Vec<i64>, bodies: Vec<String>, query_sha256: String) -> Self {
        assert_eq!(
            ids.len(),
            bodies.len(),
            "RecalledContext::new: ids.len() must equal bodies.len() \
             (assembler renders bodies, audit row counts bodies — \
             a divergence would silently desync the wire shape)",
        );
        Self { ids, bodies, query_sha256 }
    }

    /// The empty/degraded-recall sentinel.
    ///
    /// `query_sha256` is the SHA-256 of the empty byte string so the
    /// field is always 64 hex chars (consumers can pin the length
    /// without a special case for "no recall ran").
    pub fn empty() -> Self {
        Self {
            ids: Vec::new(),
            bodies: Vec::new(),
            // Call sha256_hex(b"") rather than inlining the hash so
            // there's a single hash-format control point (`sha256_hex`
            // below). Forward reference within the same module is
            // fine in Rust; preserves the canonical sentinel via the
            // helper a downstream contributor would expect.
            query_sha256: sha256_hex(b""),
        }
    }

    /// True iff zero rows were recalled (the failure-degraded state
    /// also satisfies this).
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    /// Number of recalled rows. Sourced from `bodies` (what the
    /// assembler actually renders) so any divergence from `ids` —
    /// which the `new()` constructor prevents — fails towards the
    /// rendered truth rather than the labelling.
    pub fn len(&self) -> usize {
        self.bodies.len()
    }
}

/// Hard cap on the cumulative bytes of recalled bodies. Mirrors
/// [`crate::memory::layers::L1_DEFAULT_CAP_BYTES`] (4 KiB). A single
/// row whose body exceeds this cap is dropped entirely with
/// `tracing::warn!` carrying the dropped `memory_id`.
pub const L_RECALL_CAP_BYTES: usize = 4096;

/// Async seam between `RouterAgent` and the embed+recall composition.
///
/// Production: [`PgRecallBuilder`] (runs `embed_query` + `recall`).
/// Tests: [`StaticRecallBuilder`] (fixed context, no I/O).
///
/// **Degrade-and-warn contract:** callers (specifically
/// `RouterAgent::formulate_plan`) are expected to swallow `Err`
/// returns and substitute `RecalledContext::empty()`. The async
/// signature mirrors [`crate::prompt_assembly::SystemPromptBuilder`]
/// so the agent can keep both calls structurally similar.
#[async_trait]
pub trait RecallBuilder: Send + Sync {
    /// Build a [`RecalledContext`] for the given query text + seed
    /// entity ids. `seeds = &[]` is valid and means "no graph lane
    /// this call" — semantic + lexical only.
    async fn build_with_seeds(
        &self,
        query: &str,
        seeds: &[i64],
    ) -> Result<RecalledContext, RecallError>;

    /// Default-impl shim. Existing call sites that don't pass seeds
    /// still compile. Production code goes through `build_with_seeds`
    /// via `RouterAgent::formulate_plan`; this shim is for test
    /// fixtures and any non-formulate caller.
    async fn build(&self, query: &str) -> Result<RecalledContext, RecallError> {
        self.build_with_seeds(query, &[]).await
    }
}

/// Compute the hex SHA-256 of a byte slice. Used by [`PgRecallBuilder`]
/// to populate [`RecalledContext::query_sha256`] and by
/// [`StaticRecallBuilder::with`] in tests.
///
/// Pure helper, no I/O.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_is_empty_and_has_64_char_sha256() {
        let c = RecalledContext::empty();
        assert!(c.is_empty());
        assert!(c.ids.is_empty());
        assert!(c.bodies.is_empty());
        // SHA-256 of empty byte string is well-known.
        assert_eq!(
            c.query_sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "query_sha256 of empty input must equal the canonical SHA-256 empty digest"
        );
        assert_eq!(c.query_sha256.len(), 64, "query_sha256 must always be 64 hex chars");
    }

    #[test]
    fn sha256_hex_matches_known_answer_test_for_abc() {
        // NIST FIPS 180-2 test vector for "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[test]
    fn new_accepts_matched_lengths_and_sets_len_from_bodies() {
        let c = RecalledContext::new(
            vec![10, 20],
            vec!["a".into(), "b".into()],
            sha256_hex(b"q"),
        );
        assert_eq!(c.len(), 2);
        assert!(!c.is_empty());
        assert_eq!(c.ids.len(), c.bodies.len());
    }

    #[test]
    #[should_panic(expected = "ids.len() must equal bodies.len()")]
    fn new_panics_on_length_mismatch() {
        let _ = RecalledContext::new(
            vec![10, 20],
            vec!["only one".into()],
            sha256_hex(b"q"),
        );
    }

    #[test]
    fn empty_has_len_zero_and_is_empty() {
        let c = RecalledContext::empty();
        assert_eq!(c.len(), 0);
        assert!(c.is_empty());
    }
}
