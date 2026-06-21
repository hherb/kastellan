//! Entity-embedding **backfill** — `kastellan-cli entities reembed`.
//!
//! Every `entities.embedding` is NULL today (no write path populates it).
//! [`reembed_entities_null`] scans those rows and embeds each through the
//! injected [`Embedder`] chokepoint — the CLI injects a
//! [`crate::memory::RouterEmbedder`], so a backfilled entity vector is
//! Matryoshka-truncated to `EMBEDDING_DIM`, unit-norm, with an
//! `action='embed'` audit row per call, exactly like the L1 path.
//!
//! ## What gets embedded
//!
//! [`entity_embedding_text`] composes the string fed to the embedder:
//! `"<kind>: <name>"` (e.g. `"person: Horst Herb"`). The `kind` prefix gives
//! the embedder type context and disambiguates same-named entities of
//! different kinds. It is the single source of truth for entity embed text,
//! so a future forward (embed-on-insert) path embeds identically.
//!
//! ## Safety / idempotency
//!
//! Safe to re-run: the scan only returns `embedding IS NULL` rows and the
//! write ([`set_entity_embedding`]) re-asserts `embedding IS NULL`, so a row
//! embedded by a prior run or a concurrent writer no-ops. A per-row embed
//! failure **skips that row** (degrade-and-warn) rather than failing the
//! batch — mirrors [`crate::memory::l1_reembed::reembed_l1_null`].

use kastellan_db::entity_embedding::{load_unembedded_entities, set_entity_embedding};
use kastellan_db::DbError;
use sqlx::PgPool;

use crate::memory::embedder::Embedder;
use crate::memory::reembed::{reembed_batch_failed, ReembedReport};

/// Compose the text embedded for an entity: `"<kind>: <name>"`. Pure; the
/// single source of truth for entity embed text (see module docs).
pub fn entity_embedding_text(kind: &str, name: &str) -> String {
    format!("{kind}: {name}")
}

/// Embed every entity whose `embedding IS NULL`, writing each vector back
/// through the guarded [`set_entity_embedding`] updater.
///
/// Per-row degrade-and-warn: a `None` from the embedder (transient failure or
/// an intentional skip — the [`crate::memory::RouterEmbedder`] logs the
/// WARN), a lost race on the `IS NULL` guard, or a write error all count as
/// `skipped` and the loop continues. The only `Err` returned is a failure of
/// the **initial scan** ([`load_unembedded_entities`]) — there is nothing to
/// back-fill if we cannot even read the work-list.
pub async fn reembed_entities_null(
    pool: &PgPool,
    embedder: &dyn Embedder,
) -> Result<ReembedReport, DbError> {
    let rows = load_unembedded_entities(pool).await?;
    let scanned = rows.len();
    let mut embedded = 0usize;
    let mut skipped = 0usize;

    for (id, kind, name) in rows {
        let text = entity_embedding_text(&kind, &name);
        match embedder.embed_for_storage(&text).await {
            Some(vector) => match set_entity_embedding(pool, id, &vector).await {
                Ok(true) => embedded += 1,
                // The `IS NULL` guard no-op'd: embedded concurrently or the
                // row vanished between scan and update. Not an error.
                Ok(false) => {
                    tracing::warn!(
                        target: "kastellan::memory",
                        entity_id = id,
                        "entity reembed: row no longer NULL at update time; skipped"
                    );
                    skipped += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "kastellan::memory",
                        entity_id = id,
                        error = %e,
                        "entity reembed: embedding write failed; row left NULL, skipped"
                    );
                    skipped += 1;
                }
            },
            // Embed declined/failed (the RouterEmbedder logged the WARN).
            None => skipped += 1,
        }
    }

    let report = ReembedReport { scanned, embedded, skipped };

    // Aggregate signal: rows were scanned but none embedded — typically an
    // unreachable embed endpoint. The per-row `None` path can't WARN
    // generically, so surface it at the batch level.
    if reembed_batch_failed(&report) {
        tracing::warn!(
            target: "kastellan::memory",
            scanned = report.scanned,
            skipped = report.skipped,
            "entity reembed: all scanned rows skipped, none embedded — embed endpoint may be unreachable"
        );
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embed text is `"<kind>: <name>"` — the exact contract a future
    /// forward path must match so backfilled + on-insert vectors agree.
    #[test]
    fn entity_embedding_text_is_kind_colon_name() {
        assert_eq!(entity_embedding_text("person", "Horst Herb"), "person: Horst Herb");
    }

    /// Empty kind still produces a deterministic, non-panicking string.
    #[test]
    fn entity_embedding_text_handles_empty_kind() {
        assert_eq!(entity_embedding_text("", "x"), ": x");
    }

    /// Unicode names pass through unchanged (no normalization here — that is
    /// the extractor's job; this is purely the embed-text shape).
    #[test]
    fn entity_embedding_text_passes_through_unicode() {
        assert_eq!(entity_embedding_text("place", "München"), "place: München");
    }

    /// Compile-pin the public signature (mirrors the `reembed_l1_null` pin):
    /// `&PgPool` + `&dyn Embedder` → `Result<ReembedReport, DbError>`. The
    /// behaviour is exercised by `core/tests/entity_reembed_e2e.rs`.
    #[allow(dead_code)]
    fn reembed_entities_null_signature_compile_pin() {
        fn _assert<'a>(
            pool: &'a PgPool,
            embedder: &'a dyn Embedder,
        ) -> impl std::future::Future<Output = Result<ReembedReport, DbError>> + 'a {
            reembed_entities_null(pool, embedder)
        }
    }
}
