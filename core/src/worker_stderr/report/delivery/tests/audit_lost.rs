//! The `[audit-lost]` report's delivery: recorded at ERROR (#792), and the
//! marker in the traced message as well as the fallback (#828). Split out of
//! `delivery/tests.rs` to keep it under the 500-LOC soft cap.

use super::*;

/// The emitter's own module: the target its `error!` is recorded under.
const AUDIT_LOST: &str = "kastellan_core::worker_stderr::report::audit_lost";

/// #792: a lost audit row is reported at ERROR — recorded under a filter
/// that admits only errors, and falling back under one that admits nothing
/// from its module. The census rows in `delivery/tests.rs` filter at WARN,
/// which admits ERROR and WARN alike, so they cannot tell the two apart.
#[test]
fn the_audit_lost_report_is_recorded_at_error() {
    let emit = || crate::worker_stderr::emit_audit_lost_report(crate::worker_stderr::AuditLostWriter::Shutdown, PROBE_LINE);
    let (fell_back, recorded) = under(&format!("{AUDIT_LOST}=error"), emit);
    assert!(recorded, "a lost audit row must be recorded by an errors-only filter");
    assert!(!fell_back, "and then not also written to stderr");
    let (fell_back, recorded) = under(&format!("info,{AUDIT_LOST}=off"), emit);
    assert!(!recorded && fell_back, "POSITIVE CONTROL: dropped, it falls back");
}

/// #828: the traced report carries the `[audit-lost]` marker too — once, in
/// front of the folded line, exactly the fallback's text — so an alert keyed
/// on the marker sees a lost row at the default `RUST_LOG`, where `tracing`
/// records it and nothing falls back. Before #828 the marker was in the
/// fallback only.
#[test]
fn the_traced_audit_lost_report_carries_the_marker() {
    let writer = crate::worker_stderr::AuditLostWriter::Bus;
    let emit = || crate::worker_stderr::emit_audit_lost_report(writer, PROBE_LINE);
    let (fell_back, recorded) = under_text(&format!("{AUDIT_LOST}=error"), emit);
    assert!(!fell_back, "POSITIVE CONTROL: recorded by tracing, so no fallback: {recorded}");
    let fallback = crate::worker_stderr::format_audit_lost_stderr_fallback(
        &crate::worker_stderr::format_audit_lost_line(writer, PROBE_LINE),
    );
    assert!(recorded.contains(&fallback), "the traced message must be the fallback's text: {recorded}");
    assert_eq!(recorded.matches("[audit-lost]").count(), 1, "the marker once, not wrapped twice: {recorded}");
}

/// #828, through the formatter the daemon actually installs: `.json()`. The
/// marker is not at column 0 there — it opens the `fields.message` string —
/// so the alert that sees every report is a JSON-aware or an unanchored one.
/// What this pins is that the message itself *starts* with the marker.
#[test]
fn the_daemons_json_line_opens_its_message_with_the_marker() {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(format!("{AUDIT_LOST}=error")))
        .with_writer(sink.clone())
        .json()
        .finish();
    let fell_back = tracing::subscriber::with_default(subscriber, || {
        crate::worker_stderr::emit_audit_lost_report(crate::worker_stderr::AuditLostWriter::Bus, PROBE_LINE)
    });
    let recorded = sink.text();
    assert!(!fell_back, "POSITIVE CONTROL: recorded by tracing, so no fallback: {recorded}");
    let event: serde_json::Value = serde_json::from_str(recorded.trim()).expect("one JSON line");
    let message = event["fields"]["message"].as_str().expect("fields.message");
    assert!(
        message.starts_with("[audit-lost] audit rows from bus: "),
        "the JSON message must open with the marker: {message}"
    );
}
