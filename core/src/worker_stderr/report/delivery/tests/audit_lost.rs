//! The `[audit-lost]` report's delivery: recorded at ERROR (#792), and the
//! marker in the traced message as well as the fallback (#828). Split out of
//! `delivery/tests.rs` to keep it under the 500-LOC soft cap.

use super::*;

/// #792: a lost audit row is reported at ERROR — recorded under a filter
/// that admits only errors, and falling back under one that admits nothing
/// from its module. The census rows in `delivery/tests.rs` filter at WARN, which admits ERROR
/// and WARN alike, so they cannot tell the two arms apart.
#[test]
fn the_audit_lost_report_is_recorded_at_error() {
    const AUDIT_LOST: &str = "kastellan_core::worker_stderr::report::audit_lost";
    let emit = || crate::worker_stderr::emit_audit_lost_report(crate::worker_stderr::AuditLostWriter::Shutdown, PROBE_LINE);
    let (fell_back, recorded) = under(&format!("{AUDIT_LOST}=error"), emit);
    assert!(recorded, "a lost audit row must be recorded by an errors-only filter");
    assert!(!fell_back, "and then not also written to stderr");
    let (fell_back, recorded) = under(&format!("info,{AUDIT_LOST}=off"), emit);
    assert!(!recorded && fell_back, "POSITIVE CONTROL: dropped, it falls back");
}

/// #828: the traced report carries the `[audit-lost]` marker too — the same
/// bytes as the fallback line — so an alert keyed on the marker sees a lost
/// row at the default `RUST_LOG`, where `tracing` records it and nothing
/// falls back. Before #828 the marker was in the fallback only.
#[test]
fn the_traced_audit_lost_report_carries_the_marker() {
    const AUDIT_LOST: &str = "kastellan_core::worker_stderr::report::audit_lost";
    let emit = || crate::worker_stderr::emit_audit_lost_report(crate::worker_stderr::AuditLostWriter::Bus, PROBE_LINE);
    let (fell_back, recorded) = under_text(&format!("{AUDIT_LOST}=error"), emit);
    assert!(!fell_back, "POSITIVE CONTROL: recorded by tracing, so no fallback: {recorded}");
    assert!(
        recorded.contains(&format!("[audit-lost] audit rows from bus: {PROBE_LINE}")),
        "the traced line must carry the marker before the folded line: {recorded}"
    );
}
