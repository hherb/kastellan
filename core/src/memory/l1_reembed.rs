//! L1 embedding **backfill** — `kastellan-cli memory l1 reembed` (issue #325).
//!
//! The forward write path ([`crate::memory::l1_promote::promote_l1`], #324)
//! embeds agent-raised L1 insights on insert. Two categories of `layer = 1`
//! rows still carry `embedding IS NULL` and are therefore invisible to the
//! semantic recall lane (`semantic_search` filters `WHERE embedding IS NOT
//! NULL`):
//!
//!   1. **pre-existing rows** written before #324 (and any after migration
//!      0019's dim-change discard), and
//!   2. **operator-added rows** — `memory l1 add` injects a
//!      [`crate::memory::NoOpEmbedder`] by design, so operator insights are
//!      stored embedding-free.
//!
//! [`reembed_l1_null`] scans those rows and (re)embeds each body through the
//! **same** [`Embedder`] chokepoint the write path uses — the CLI injects a
//! [`crate::memory::RouterEmbedder`], so a backfilled vector is byte-identical
//! to what an on-insert embed would have produced (Matryoshka-truncated to
//! `EMBEDDING_DIM`, unit-norm, with an `action='embed'` audit row per call).
//!
//! ## Safety / idempotency
//!
//! The backfill is safe to re-run. The scan ([`load_unembedded_at_layer`])
//! only returns `embedding IS NULL` rows, and the write
//! ([`set_embedding`]) re-asserts `WHERE embedding IS NULL`, so a row embedded
//! by either the forward path or a prior backfill run simply drops out — no
//! double-embed, no overwrite. A transient embed failure **skips that row**
//! (degrade-and-warn) rather than failing the batch, mirroring the forward
//! path's posture.

use kastellan_db::memories::{load_unembedded_at_layer, set_embedding, MemoryLayer};
use kastellan_db::DbError;
use sqlx::PgPool;

use crate::memory::embedder::Embedder;

/// Outcome of a [`reembed_l1_null`] batch.
///
/// Invariant: `embedded + skipped == scanned`. `scanned` is the number of
/// NULL-embedding L1 rows the scan found; `embedded` actually wrote a vector;
/// `skipped` covers every row that did not get embedded (embed declined/
/// failed, a concurrent write won the `IS NULL` guard, or a per-row write
/// error) — none of which fail the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReembedReport {
    /// NULL-embedding L1 rows found by the scan.
    pub scanned: usize,
    /// Rows whose embedding was written this run.
    pub embedded: usize,
    /// Rows scanned but not embedded (degrade-and-warn; not batch failures).
    pub skipped: usize,
}

/// (Re)embed every `layer = 1` row whose `embedding IS NULL`, writing each
/// vector back through the guarded [`set_embedding`] updater.
///
/// Per-row degrade-and-warn: a `None` from the embedder (transient failure or
/// an intentional skip — the [`crate::memory::RouterEmbedder`] already logs
/// the WARN), a lost race on the `IS NULL` guard, or a write error all count
/// as `skipped` and the loop continues. The only `Err` returned is a failure
/// of the **initial scan** ([`load_unembedded_at_layer`]) — there is nothing
/// to back-fill if we cannot even read the work-list.
///
/// `scanned` is a point-in-time snapshot; a row inserted-and-embedded by the
/// forward path *after* the scan is simply not in this batch (it was never
/// NULL when scanned), which is correct — the backfill only owns the rows
/// that were already stranded.
pub async fn reembed_l1_null(
    pool: &PgPool,
    embedder: &dyn Embedder,
) -> Result<ReembedReport, DbError> {
    let rows = load_unembedded_at_layer(pool, MemoryLayer::Index).await?;
    let scanned = rows.len();
    let mut embedded = 0usize;
    let mut skipped = 0usize;

    for (id, body) in rows {
        match embedder.embed_for_storage(&body).await {
            Some(vector) => match set_embedding(pool, id, &vector).await {
                // Wrote the column.
                Ok(true) => embedded += 1,
                // The `IS NULL` guard no-op'd: the row was embedded
                // concurrently (forward path) or removed between scan and
                // update. Not an error — count it as skipped.
                Ok(false) => {
                    tracing::warn!(
                        target: "kastellan::memory",
                        memory_id = id,
                        "L1 reembed: row no longer NULL at update time; skipped"
                    );
                    skipped += 1;
                }
                // A per-row write failure must not abort the batch.
                Err(e) => {
                    tracing::warn!(
                        target: "kastellan::memory",
                        memory_id = id,
                        error = %e,
                        "L1 reembed: embedding write failed; row left NULL, skipped"
                    );
                    skipped += 1;
                }
            },
            // Embed declined/failed (the RouterEmbedder logged the WARN).
            // Degrade-and-warn: skip this row, keep going.
            None => skipped += 1,
        }
    }

    let report = ReembedReport { scanned, embedded, skipped };

    // Aggregate signal: rows were scanned but none embedded. The per-row
    // `None` path can't WARN generically (it depends on the injected
    // `Embedder`), so a batch that found work yet wrote nothing — typically
    // an unreachable embed endpoint — must be visible at the batch level
    // regardless of which embedder was used.
    if reembed_batch_failed(&report) {
        tracing::warn!(
            target: "kastellan::memory",
            scanned = report.scanned,
            skipped = report.skipped,
            "L1 reembed: all scanned rows skipped, none embedded — embed endpoint may be unreachable"
        );
    }

    Ok(report)
}

/// True when a batch found NULL-embedding rows to embed but embedded **none**
/// — `scanned > 0 && embedded == 0`. Equivalent to "every scanned row was
/// skipped" (since `embedded + skipped == scanned`): a total failure,
/// typically an unreachable embed endpoint.
///
/// Distinguished from the idempotent no-op (`scanned == 0`), which is *not* a
/// failure — a re-run with nothing left to do is success. The CLI maps this
/// to a non-zero exit code so a scripted `reembed && next-step` chain does not
/// treat a wholly-failed backfill as success; [`reembed_l1_null`] uses it to
/// emit the aggregate WARN.
pub fn reembed_batch_failed(report: &ReembedReport) -> bool {
    report.scanned > 0 && report.embedded == 0
}

/// Render a [`ReembedReport`] as the one-line operator summary
/// `scanned=<n> embedded=<n> skipped=<n>`. Pure — the CLI prints this to
/// stdout; keeping it a function (not an inline `println!`) makes the exact
/// wording test-pinnable and reusable.
pub fn format_reembed_report(report: &ReembedReport) -> String {
    format!(
        "scanned={} embedded={} skipped={}",
        report.scanned, report.embedded, report.skipped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-pin the public signature (mirrors the `promote_l1` pin in
    /// `l1_promote.rs`): `reembed_l1_null` takes a `&PgPool` + a `&dyn
    /// Embedder` and yields a `Result<ReembedReport, DbError>`. The behaviour
    /// is exercised by `core/tests/memory_l1_reembed_e2e.rs` against live PG.
    #[allow(dead_code)]
    fn reembed_l1_null_signature_compile_pin() {
        fn _assert<'a>(
            pool: &'a PgPool,
            embedder: &'a dyn Embedder,
        ) -> impl std::future::Future<Output = Result<ReembedReport, DbError>> + 'a {
            reembed_l1_null(pool, embedder)
        }
    }

    /// The report's documented invariant holds for a hand-built value.
    #[test]
    fn report_parts_sum_to_scanned() {
        let r = ReembedReport { scanned: 5, embedded: 3, skipped: 2 };
        assert_eq!(r.embedded + r.skipped, r.scanned);
    }

    /// The operator-facing one-line summary is stable and greppable.
    #[test]
    fn format_reembed_report_is_stable_one_line() {
        let r = ReembedReport { scanned: 7, embedded: 5, skipped: 2 };
        assert_eq!(format_reembed_report(&r), "scanned=7 embedded=5 skipped=2");
    }

    /// The empty backfill (nothing to do) renders all-zeros, not a blank line.
    #[test]
    fn format_reembed_report_empty_batch() {
        let r = ReembedReport { scanned: 0, embedded: 0, skipped: 0 };
        assert_eq!(format_reembed_report(&r), "scanned=0 embedded=0 skipped=0");
    }

    /// The idempotent no-op (nothing scanned) is **not** a failure — a re-run
    /// with no NULL rows left is success, so a scripted `reembed && next` must
    /// proceed.
    #[test]
    fn reembed_batch_failed_false_for_empty_scan() {
        let r = ReembedReport { scanned: 0, embedded: 0, skipped: 0 };
        assert!(!reembed_batch_failed(&r));
    }

    /// Any embedded row means the batch made progress — not a failure, even
    /// with some skips (e.g. lost `IS NULL` races).
    #[test]
    fn reembed_batch_failed_false_when_any_embedded() {
        let all = ReembedReport { scanned: 3, embedded: 3, skipped: 0 };
        let partial = ReembedReport { scanned: 5, embedded: 3, skipped: 2 };
        assert!(!reembed_batch_failed(&all));
        assert!(!reembed_batch_failed(&partial));
    }

    /// Rows scanned but none embedded (every row skipped) is the total-failure
    /// signal the CLI maps to a non-zero exit code.
    #[test]
    fn reembed_batch_failed_true_when_all_skipped() {
        let r = ReembedReport { scanned: 4, embedded: 0, skipped: 4 };
        assert!(reembed_batch_failed(&r));
    }
}
