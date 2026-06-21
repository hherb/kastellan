//! Shared outcome type + pure helpers for the embedding **backfill**
//! workflows (`l1_reembed`, `entity_reembed`). One report shape — scanned /
//! embedded / skipped — describes both, so it lives here rather than in
//! either backfill module.

/// Outcome of a backfill batch.
///
/// Invariant: `embedded + skipped == scanned`. `scanned` is the number of
/// NULL-embedding rows the scan found; `embedded` actually wrote a vector;
/// `skipped` covers every row that did not get embedded (embed declined/
/// failed, a concurrent write won the `IS NULL` guard, or a per-row write
/// error) — none of which fail the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReembedReport {
    /// NULL-embedding rows found by the scan.
    pub scanned: usize,
    /// Rows whose embedding was written this run.
    pub embedded: usize,
    /// Rows scanned but not embedded (degrade-and-warn; not batch failures).
    pub skipped: usize,
}

/// True when a batch found NULL-embedding rows to embed but embedded **none**
/// — `scanned > 0 && embedded == 0`. Equivalent to "every scanned row was
/// skipped" (since `embedded + skipped == scanned`): a total failure,
/// typically an unreachable embed endpoint.
///
/// Distinguished from the idempotent no-op (`scanned == 0`), which is *not* a
/// failure. The CLI maps this to a non-zero exit code so a scripted
/// `reembed && next-step` chain does not treat a wholly-failed backfill as
/// success; the backfill loops use it to emit an aggregate WARN.
pub fn reembed_batch_failed(report: &ReembedReport) -> bool {
    report.scanned > 0 && report.embedded == 0
}

/// Render a [`ReembedReport`] as the one-line operator summary
/// `scanned=<n> embedded=<n> skipped=<n>`. Pure — the CLI prints this to
/// stdout; keeping it a function (not an inline `println!`) makes the exact
/// wording test-pinnable and reusable across backfills.
pub fn format_reembed_report(report: &ReembedReport) -> String {
    format!(
        "scanned={} embedded={} skipped={}",
        report.scanned, report.embedded, report.skipped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The idempotent no-op (nothing scanned) is **not** a failure.
    #[test]
    fn reembed_batch_failed_false_for_empty_scan() {
        let r = ReembedReport { scanned: 0, embedded: 0, skipped: 0 };
        assert!(!reembed_batch_failed(&r));
    }

    /// Any embedded row means progress — not a failure, even with some skips.
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
