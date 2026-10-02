//! `email_boot`'s unit tests. Split out of `email_boot.rs` to keep it under the 500-LOC
//! soft cap; `#[path]`-included there, so `super::` is `email_boot`.

use super::*;

/// A PARTIAL config must be FATAL: the process environment is fixed for
/// this daemon's lifetime, so no number of retries can complete it. The
/// operator-facing message already says "fix it, then restart" — retrying
/// instead would make that message a lie and spin forever.
#[test]
fn a_partial_config_is_fatal_not_retryable() {
    let err = anyhow::anyhow!("KASTELLAN_EMAIL_AUTHSERV_ID is not set");
    let outcome = classify_config_error(err);
    assert!(matches!(outcome, BootOutcome::Fatal(_)), "{outcome:?}");
}

/// The fatal cause keeps the underlying detail, which for a config problem
/// names every missing variable — that is the whole value of the loud line.
#[test]
fn the_fatal_cause_still_names_the_missing_variable() {
    let err = anyhow::anyhow!("KASTELLAN_EMAIL_AUTHSERV_ID is not set");
    match classify_config_error(err) {
        BootOutcome::Fatal(e) => {
            let rendered = format!("{e:#}");
            assert!(rendered.contains("KASTELLAN_EMAIL_AUTHSERV_ID"), "{rendered}");
            assert!(rendered.contains("incomplete or invalid"), "{rendered}");
        }
        other => panic!("expected Fatal, got {other:?}"),
    }
}

/// #789: the skipped-id sink returns at once against a Postgres that never
/// answers. It used to `block_on` its insert on the driver's thread, which
/// every conversation, poll and ack wait on.
#[test]
fn the_skipped_id_sink_does_not_hold_the_driver_thread() {
    use crate::audit_sink::test_support::{
        assert_insert_attempted, assert_returns_at_once, stalled_pool,
    };
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, listener) = rt.block_on(async { stalled_pool() });
    static LEDGER: crate::audit_sink::Ledger = crate::audit_sink::Ledger::new(crate::audit_sink::Bounds { queued: 8, connections: 8 });
    let sink = email_skipped_audit_sink(
        crate::audit_sink::SinkWriter::with_ledger(
            &LEDGER,
            pool,
            rt.handle().clone(),
            crate::audit_sink::SinkKind::SilentOnExit,
        ),
        |_, _| {},
    );
    let channel = ChannelId("email".into());
    let skipped = kastellan_core::channel::SkippedId {
        channel: &channel,
        message_id: "<id@host>",
        reason: "unattributable",
        observed_at: time::OffsetDateTime::now_utc(),
    };
    assert_returns_at_once("the email skipped-id sink", || sink(skipped));
    assert_insert_attempted("the email skipped-id sink", &listener);
    // #802: the row's label for the shutdown line — the insert is still
    // waiting on the stalled pool, so the row is pending and named.
    assert_eq!(LEDGER.final_snapshot().named, [r#"email skipped message "<id@host>""#]);
    // #792: the sink holds its lease until the driver drops it, so the
    // shutdown drain waits for the driver that owns it.
    assert_eq!(LEDGER.snapshot().sinks_live, 1, "the email skipped-id sink holds a lease");
    drop(sink);
    assert_eq!(LEDGER.snapshot().sinks_live, 0, "and gives it back when dropped");
}

/// The recorder for [`the_email_sink_reports_a_row_it_could_not_write`]:
/// a `fn`, like the production reporter, so one static holds it.
static EMAIL_SAID: std::sync::Mutex<Vec<(AuditLostWriter, String)>> =
    std::sync::Mutex::new(Vec::new());

/// #799: the sink's failure closure reaches the reporter, as the EMAIL
/// writer, and names the message. A ledger with no queue room sheds every
/// row, so `on_failure` runs at once, on this thread. Swapping the closure
/// for `|_| {}` — which nothing else here noticed — fails this.
#[test]
fn the_email_sink_reports_a_row_it_could_not_write() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    static LEDGER: crate::audit_sink::Ledger =
        crate::audit_sink::Ledger::new(crate::audit_sink::Bounds { queued: 0, connections: 1 });
    let sink = email_skipped_audit_sink(
        crate::audit_sink::SinkWriter::with_ledger(
            &LEDGER,
            pool,
            rt.handle().clone(),
            crate::audit_sink::SinkKind::SilentOnExit,
        ),
        |w, line| EMAIL_SAID.lock().unwrap().push((w, line.to_string())),
    );
    let channel = ChannelId("email".into());
    sink(kastellan_core::channel::SkippedId {
        channel: &channel,
        message_id: "<shed-me@host>",
        reason: "unattributable",
        observed_at: time::OffsetDateTime::now_utc(),
    });
    let said = EMAIL_SAID.lock().unwrap();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, AuditLostWriter::Email);
    assert!(said[0].1.contains("<shed-me@host>") && said[0].1.contains("shed:"), "{said:?}");
}

/// #798: a worker-supplied id is bounded where it is quoted, and the row's
/// label for the shutdown line is too.
#[test]
fn a_hostile_message_id_is_capped_in_the_lost_row_line() {
    let long = "x".repeat(10_000);
    let line = format_skipped_row_lost(&long, &"shed");
    assert!(line.len() < 1_000, "{} bytes", line.len());
    assert!(line.contains("...(truncated)"), "POSITIVE CONTROL: the cap bit");
}

/// #792: a skipped-id row that was not written names the message and why,
/// and says the id will not come back.
#[test]
fn a_lost_skipped_row_names_its_message_and_cause() {
    let line = format_skipped_row_lost("<id@host>", &"shed: too many");
    assert_eq!(
        line,
        "channel.skipped_ack_only row for message \"<id@host>\" not written: shed: too many. \
         The driver's line for the skip stands; the id's ack was about to be sent, and \
         unless that ack then failed it is not redelivered"
    );
}

/// The row the email sink writes: the bus's actor, the skipped-ack-only
/// action, and the view's payload (pinned field by field, cap included,
/// beside `SkippedId` in the lib). The sink's fast return proves nothing
/// about what it stores; this does.
#[test]
fn the_skipped_id_row_is_the_bus_s_actor_and_the_view_s_payload() {
    let channel = ChannelId("email".into());
    let skipped = kastellan_core::channel::SkippedId {
        channel: &channel,
        message_id: "<id@host>",
        reason: "no usable From address",
        observed_at: time::macros::datetime!(2026-09-30 12:34:56 UTC),
    };
    let (actor, action, payload) = email_skipped_row(&skipped);
    assert_eq!(actor, "channel");
    assert_eq!(action, "channel.skipped_ack_only");
    assert_eq!(payload, skipped.payload());
    assert_eq!(payload["message_id"], "<id@host>", "POSITIVE CONTROL: the view's own payload");
}

/// #802: the email driver audits nothing once its bus is gone, so one still
/// in its long-poll at shutdown is no loss. Pinned, like Matrix's.
#[test]
fn the_email_sink_is_one_that_is_silent_on_exit() {
    assert_eq!(SINK_KIND, crate::audit_sink::SinkKind::SilentOnExit);
}

#[test]
fn a_worker_spawn_failure_is_retryable() {
    let err = anyhow::anyhow!("egress-proxy sidecar exited before becoming ready");
    let outcome = classify_spawn_error(err);
    assert!(matches!(outcome, BootOutcome::Retry(_)), "{outcome:?}");
}
