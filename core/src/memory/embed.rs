//! Free-text query → embedding vector, with the system's first
//! `actor='llm:router' action='embed'` audit row.
//!
//! `core::memory::recall`'s semantic lane requires a pre-computed
//! [`EMBEDDING_DIM`]-length vector. This module owns the helper that
//! turns a query string into that vector via
//! [`hhagent_llm_router::Router::embed`], and pins down the audit-row
//! shape that every embedding call writes.
//!
//! ## Why `embed_query` is separate from `recall`
//!
//! Keeping `recall` pure-data (no LLM-router dependency, no audit-row
//! write) means recall integration tests can seed memories with
//! deterministic SHA-256-seeded vectors and never touch a router
//! mock. The composition shape — `let emb = embed_query(...).await?;
//! recall(pool, &RecallParams { query_embedding: Some(&emb), ... }).await?`
//! — pushes the I/O to the caller, which is also where the audit-trail
//! attribution makes sense (the *consumer* of the embedding is the
//! actor whose decision the row records).
//!
//! ## Audit row contract
//!
//! Pinned end-to-end by `core/tests/embedding_recall_e2e.rs`:
//!
//!   actor   = "llm:router"
//!   action  = "embed"
//!   payload = { model, n_texts, dim, backend, latency_ms }
//!
//! Deliberately omits the input texts (privacy — queries may carry
//! user PII), the output embeddings (size + uselessness as audit
//! signal), and HTTP failure context (failures don't write a row at
//! all; matches `Router::send` and `tool_host::dispatch` precedent).

use hhagent_db::audit;
use hhagent_db::memories::EMBEDDING_DIM;
use hhagent_db::DbError;
use hhagent_llm_router::embeddings::EmbeddingRequest;
use hhagent_llm_router::{Router, RouterError};
use sqlx::PgPool;
use std::time::Instant;

/// Errors returned by `core::memory` helpers that touch the LLM
/// router and/or write audit rows.
///
/// `recall` itself is `Result<_, DbError>`-typed and does not produce
/// these variants; [`MemoryError`] is the wider surface used by
/// [`embed_query`].
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("router: {0}")]
    Router(#[from] RouterError),
    #[error("db: {0}")]
    Db(#[from] DbError),
    #[error("embedding dim mismatch: expected {expected}, got {actual} from model {model}")]
    EmbeddingDimMismatch {
        expected: usize,
        actual: usize,
        model: String,
    },
}

/// Build the audit-log payload for an `actor='llm:router' action='embed'`
/// row.
///
/// Pure function — no I/O, no clock reads, no global state. The
/// caller [`embed_query`] measures latency, picks the backend
/// string, knows the request's model and the agreed dim, then calls
/// this helper to compose the JSON object that the row's `payload`
/// column carries.
///
/// **What the payload deliberately omits:**
/// * The input texts (privacy — query may carry user PII).
/// * The output embeddings (size + uselessness as audit signal).
/// * HTTP status / body (failures don't write an audit row at all;
///   matches `Router::send` and `tool_host::dispatch` precedent).
///
/// **What it includes** is the minimal operator-facing summary: which
/// model, how many texts, what dimension, which backend, how long.
fn build_embed_audit_payload(
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

/// Turn a free-text query into a [`EMBEDDING_DIM`]-length embedding
/// vector via the LLM router's embedding backend, writing the first
/// `actor='llm:router' action='embed'` audit row in the process.
///
/// ## Flow
/// 1. Build `EmbeddingRequest::single(router.config().embedding_model, text)`.
/// 2. Time the call to `router.embed(&req).await`.
/// 3. Validate `data.len() == 1` (router already validated against
///    request input length; this is a defensive check for the
///    single-text shape).
/// 4. Validate the returned embedding's length equals
///    [`EMBEDDING_DIM`]; otherwise [`MemoryError::EmbeddingDimMismatch`].
/// 5. Insert one row into `audit_log` with
///    `actor='llm:router' action='embed'` and the payload shape
///    pinned by `build_embed_audit_payload`.
///    **Best-effort:** an audit-insert failure is logged at
///    `tracing::error!` but does **not** mask the embed `Ok(emb)` —
///    matches `tool_host::dispatch` precedent.
/// 6. Return the embedding vector.
///
/// ## What this does NOT do
/// - Does not call `recall`. Caller composes `embed_query` →
///   `RecallParams { query_embedding: Some(&emb), ... }` → `recall`.
/// - Does not retry. The router's reqwest client carries the configured
///   timeout; transport-level retries are a Phase-1-cont. optimisation.
/// - Does not cache. Stateless function.
pub async fn embed_query(
    pool: &PgPool,
    router: &Router,
    text: &str,
) -> Result<Vec<f32>, MemoryError> {
    let model = router.config().embedding_model.clone();
    let req = EmbeddingRequest::single(model.clone(), text);

    let start = Instant::now();
    let resp = router.embed(&req).await?;
    // `as_millis()` is u128; saturate into u64 rather than silently
    // truncating. The saturation arm is unreachable in practice
    // (~584 million years) but the idiom keeps the cast honest.
    let latency_ms: u64 = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    if resp.data.len() != 1 {
        // Router's own count check should fire first; this is
        // belt-and-braces.
        return Err(MemoryError::Router(RouterError::EmbeddingCountMismatch {
            requested: 1,
            returned: resp.data.len(),
        }));
    }
    let emb = resp.data
        .into_iter()
        .next()
        .expect("invariant: data.len()==1 checked above; if this fires a refactor broke the guard")
        .embedding;

    if emb.len() != EMBEDDING_DIM {
        return Err(MemoryError::EmbeddingDimMismatch {
            expected: EMBEDDING_DIM,
            actual: emb.len(),
            model,
        });
    }

    // Source the backend tag from the same policy decision the router
    // made on dispatch. Phase 0/1 always resolves to "local" under
    // `DefaultLocalPolicy`; threading it through `pick_embed_backend`
    // means a Phase-5 gate that picks `Backend::Frontier` for embed
    // records the right tag in the audit row without a follow-up edit.
    let backend_tag = router.pick_embed_backend(&req).as_tag();
    let payload =
        build_embed_audit_payload(&req.model, 1, EMBEDDING_DIM, backend_tag, latency_ms);
    if let Err(e) = audit::insert(pool, "llm:router", "embed", payload).await {
        tracing::error!(
            target: "hhagent::memory",
            error = %e,
            "embed_query audit insert failed; embedding result preserved"
        );
    }

    Ok(emb)
}

#[cfg(test)]
mod tests {
    use super::*;

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
