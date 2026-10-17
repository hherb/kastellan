//! `inbound_dropped_sink`'s unit tests (#826, #846); `#[path]`-included there,
//! so `super::` is `inbound_dropped_sink`. The first three moved here from
//! `matrix_boot_tests.rs` with the hook they test.

use super::*;
use crate::audit_sink::{Bounds, Ledger, SinkWriter};
use kastellan_core::channel::ChannelId;
use kastellan_core::worker_stderr::AuditLostWriter;
use kastellan_db::audit::InsertOutcome;

static MATRIX_INBOUND_SAID: std::sync::Mutex<Vec<(AuditLostWriter, String)>> =
    std::sync::Mutex::new(Vec::new());

/// #826: an inbound-dropped row the sink could not write (here, shed: no
/// queue room) reaches the reporter as the MATRIX writer, naming the count.
#[test]
fn the_matrix_sink_reports_an_inbound_dropped_row_it_could_not_write() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    static LEDGER: Ledger = Ledger::new(Bounds { queued: 0, connections: 1 });
    let sink = inbound_dropped_audit_sink(
        Arc::new(SinkWriter::with_ledger(&LEDGER, pool, rt.handle().clone(), SinkChannel::Matrix)),
        |w, line| MATRIX_INBOUND_SAID.lock().unwrap().push((w, line.to_string())),
    );
    let channel = ChannelId("matrix".into());
    let dropped = NonZeroUsize::new(5).unwrap();
    let stage = InboundDropStage::Polled;
    sink(InboundDropped { channel: &channel, stage, dropped, observed_at: time::OffsetDateTime::now_utc() });
    let said = MATRIX_INBOUND_SAID.lock().unwrap();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, AuditLostWriter::Matrix);
    assert!(
        said[0].1.starts_with(
            "channel.inbound_dropped row for 5 dropped inbound messages (polled) not written: shed"
        ),
        "{said:?}"
    );
}

/// #826/#846: the lost-row line names the count, in number, the stage, and
/// the cause.
#[test]
fn a_lost_inbound_dropped_row_names_its_count_stage_and_cause() {
    let refused = Unwritten::failed_insert(InsertOutcome::NotWritten, "read-only");
    assert_eq!(
        format_inbound_row_lost(InboundDropStage::Polled, NonZeroUsize::MIN, &refused),
        "channel.inbound_dropped row for 1 dropped inbound message (polled) not written: \
         postgres query failed: read-only; refused row 1 of this burst. The drop's \
         [worker-refusal] line stands"
    );
    // #858: a row whose insert lost its connection may be in the table.
    let unknown = Unwritten::failed_insert(InsertOutcome::Unknown, "reset");
    assert!(
        format_inbound_row_lost(InboundDropStage::Buffered, NonZeroUsize::new(2).unwrap(), &unknown)
            .starts_with(
                "channel.inbound_dropped row for 2 dropped inbound messages (buffered) may not \
                 have been written: "
            ),
    );
}

/// #826/#846: the row the sink writes — the bus's actor, the action spelled
/// literally, channel + stage + count + when. A recording hook in the driver
/// tests sees only what the driver passed; this pins what the sink hands to
/// the insert (whose `stored_form` changes nothing in this payload).
#[test]
fn the_inbound_dropped_row_is_the_bus_s_actor_and_channel_stage_count_and_when() {
    let channel = ChannelId("matrix".into());
    let at = time::macros::datetime!(2026-10-07 12:34:56 UTC);
    let dropped = NonZeroUsize::new(2).unwrap();
    let stage = InboundDropStage::Buffered;
    let (actor, action, payload) =
        inbound_dropped_row(&InboundDropped { channel: &channel, stage, dropped, observed_at: at });
    assert_eq!(actor, "channel");
    assert_eq!(action, "channel.inbound_dropped");
    assert_eq!(
        payload,
        serde_json::json!({
            "channel": "matrix",
            "stage": "buffered",
            "dropped": 2,
            "observed_at": "2026-10-07T12:34:56Z",
        })
    );
}

/// #846: a pending row is named for the shutdown line by channel AND stage,
/// so "unread" rows can be told from a batch the driver held. Every pair
/// differs, and each names its own channel.
#[test]
fn the_shutdown_name_says_the_channel_and_whether_the_bus_had_the_messages() {
    let all = [
        (SinkChannel::Matrix, InboundDropStage::Polled),
        (SinkChannel::Matrix, InboundDropStage::Buffered),
        (SinkChannel::Email, InboundDropStage::Polled),
        (SinkChannel::Email, InboundDropStage::Buffered),
        (SinkChannel::Matrix, InboundDropStage::Worker),
        (SinkChannel::Email, InboundDropStage::Worker),
    ];
    let names: Vec<&str> = all.iter().map(|&(c, s)| row_what(c, s)).collect();
    for (i, a) in names.iter().enumerate() {
        for b in &names[i + 1..] {
            assert_ne!(a, b, "{names:?}");
        }
    }
    assert!(names[0].starts_with("matrix ") && names[3].starts_with("email "), "{names:?}");
    assert!(names[1].ends_with("unread") && names[3].ends_with("unread"), "{names:?}");
    // #835: held in the worker, never reached the driver.
    assert!(names[4].starts_with("matrix ") && names[5].starts_with("email "), "{names:?}");
    assert!(names[4].ends_with("in the worker") && names[5].ends_with("in the worker"), "{names:?}");
}

static EMAIL_INBOUND_SAID: std::sync::Mutex<Vec<(AuditLostWriter, String)>> =
    std::sync::Mutex::new(Vec::new());

/// #846: the same hook built on the EMAIL writer names the email channel —
/// in its pending row's name and as the writer of its lost-row line.
#[test]
fn the_email_sink_names_email_on_its_pending_row_and_its_lost_line() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    static PENDING: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let sink = inbound_dropped_audit_sink(
        Arc::new(SinkWriter::with_ledger(&PENDING, pool.clone(), rt.handle().clone(), SinkChannel::Email)),
        |_, _| {},
    );
    let channel = ChannelId("email".into());
    let dropped = NonZeroUsize::new(4).unwrap();
    let stage = InboundDropStage::Buffered;
    crate::audit_sink::test_support::assert_returns_at_once("the email inbound-dropped sink", || {
        sink(InboundDropped { channel: &channel, stage, dropped, observed_at: time::OffsetDateTime::now_utc() })
    });
    assert_eq!(PENDING.named_pending(), ["email inbound messages dropped unread (4)"]);

    static SHED: Ledger = Ledger::new(Bounds { queued: 0, connections: 1 });
    let sink = inbound_dropped_audit_sink(
        Arc::new(SinkWriter::with_ledger(&SHED, pool, rt.handle().clone(), SinkChannel::Email)),
        |w, line| EMAIL_INBOUND_SAID.lock().unwrap().push((w, line.to_string())),
    );
    sink(InboundDropped { channel: &channel, stage, dropped, observed_at: time::OffsetDateTime::now_utc() });
    let said = EMAIL_INBOUND_SAID.lock().unwrap();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, AuditLostWriter::Email);
    assert!(said[0].1.contains("4 dropped inbound messages (buffered)"), "{said:?}");
}

// ----- #836: the inbound-undecodable hook -----

static MATRIX_UNDECODABLE_SAID: std::sync::Mutex<Vec<(AuditLostWriter, String)>> =
    std::sync::Mutex::new(Vec::new());

/// #836: the row the sink writes — the bus's actor, the action spelled
/// literally, channel + part + when, and no count: the batch did not decode.
#[test]
fn the_inbound_undecodable_row_is_the_bus_s_actor_and_channel_part_and_when() {
    let channel = ChannelId("matrix".into());
    let at = time::macros::datetime!(2026-10-13 12:34:56 UTC);
    let part = UndecodablePart::Batch;
    let (actor, action, payload) =
        inbound_undecodable_row(&InboundUndecodable { channel: &channel, part, observed_at: at });
    assert_eq!(actor, "channel");
    assert_eq!(action, "channel.inbound_undecodable");
    assert_eq!(
        payload,
        serde_json::json!({"channel": "matrix", "part": "batch", "observed_at": "2026-10-13T12:34:56Z"})
    );
}

/// #836, #852, #853: the lost-row line names the row, what it was for and
/// the cause, and points at the loss's own line.
#[test]
fn a_lost_inbound_undecodable_row_names_its_cause() {
    let x = Unwritten::failed_insert(InsertOutcome::NotWritten, "x");
    assert_eq!(
        format_undecodable_row_lost(UndecodablePart::Batch, &x),
        "channel.inbound_undecodable row for an inbound batch lost undecodable not written: \
         postgres query failed: x; refused row 1 of this burst. Its [worker-refusal] line stands"
    );
    let one = NonZeroUsize::new(1).unwrap();
    let three = NonZeroUsize::new(3).unwrap();
    assert!(format_undecodable_row_lost(UndecodablePart::Events(one), &x)
        .contains("row for 1 inbound event lost undecodable not written"));
    assert!(format_undecodable_row_lost(UndecodablePart::Events(three), &x)
        .contains("row for 3 inbound events lost undecodable not written"));
    // #858: the verdict follows the insert's outcome.
    let unknown = Unwritten::failed_insert(InsertOutcome::Unknown, "x");
    assert!(format_undecodable_row_lost(UndecodablePart::Drain, &unknown)
        .contains("row for an undecodable drain answer may not have been written: "));
}

/// #852, #853: a pending row is named for the shutdown line by its part, and
/// carries its count only for events — a batch or a drain answer has none.
#[test]
fn a_pending_undecodable_row_is_labelled_by_its_part() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    // No connections, so every row stays pending and named.
    static PENDING: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let sink = inbound_undecodable_audit_sink(
        Arc::new(SinkWriter::with_ledger(&PENDING, pool, rt.handle().clone(), SinkChannel::Matrix)),
        |_, _| {},
    );
    let channel = ChannelId("matrix".into());
    let four = UndecodablePart::Events(NonZeroUsize::new(4).unwrap());
    for part in [UndecodablePart::Batch, four, UndecodablePart::Drain] {
        sink(InboundUndecodable { channel: &channel, part, observed_at: time::OffsetDateTime::now_utc() });
    }
    let mut named = PENDING.named_pending();
    named.sort();
    assert_eq!(
        named,
        [
            "matrix inbound batch lost undecodable",
            "matrix inbound events lost undecodable (4)",
            "matrix undecodable drain answer",
        ]
    );
}

/// #836: a row the sink could not write (shed: no queue room) reaches the
/// reporter as the MATRIX writer; a pending one is named for the shutdown
/// line by its channel.
#[test]
fn the_matrix_undecodable_sink_names_its_pending_row_and_reports_a_lost_one() {
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    let (pool, _listener) = rt.block_on(async { crate::audit_sink::test_support::stalled_pool() });
    let channel = ChannelId("matrix".into());
    let batch = || InboundUndecodable {
        channel: &channel,
        part: UndecodablePart::Batch,
        observed_at: time::OffsetDateTime::now_utc(),
    };

    static PENDING: Ledger = Ledger::new(Bounds { queued: 8, connections: 0 });
    let sink = inbound_undecodable_audit_sink(
        Arc::new(SinkWriter::with_ledger(&PENDING, pool.clone(), rt.handle().clone(), SinkChannel::Matrix)),
        |_, _| {},
    );
    crate::audit_sink::test_support::assert_returns_at_once("the Matrix undecodable sink", || sink(batch()));
    assert_eq!(PENDING.named_pending(), ["matrix inbound batch lost undecodable"]);

    static SHED: Ledger = Ledger::new(Bounds { queued: 0, connections: 1 });
    let sink = inbound_undecodable_audit_sink(
        Arc::new(SinkWriter::with_ledger(&SHED, pool, rt.handle().clone(), SinkChannel::Matrix)),
        |w, line| MATRIX_UNDECODABLE_SAID.lock().unwrap().push((w, line.to_string())),
    );
    sink(batch());
    let said = MATRIX_UNDECODABLE_SAID.lock().unwrap();
    assert_eq!(said.len(), 1, "{said:?}");
    assert_eq!(said[0].0, AuditLostWriter::Matrix);
    assert!(said[0].1.starts_with("channel.inbound_undecodable row for "), "{said:?}");
}
