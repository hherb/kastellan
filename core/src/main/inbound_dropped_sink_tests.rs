//! `inbound_dropped_sink`'s unit tests (#826, #846); `#[path]`-included there,
//! so `super::` is `inbound_dropped_sink`. The first three moved here from
//! `matrix_boot_tests.rs` with the hook they test.

use super::*;
use crate::audit_sink::{Bounds, Ledger, SinkWriter};
use kastellan_core::channel::ChannelId;
use kastellan_core::worker_stderr::AuditLostWriter;

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
    assert_eq!(
        format_inbound_row_lost(InboundDropStage::Polled, NonZeroUsize::MIN, &"shed: too many"),
        "channel.inbound_dropped row for 1 dropped inbound message (polled) not written: shed: \
         too many. The drop's [worker-refusal] line stands"
    );
    assert!(
        format_inbound_row_lost(InboundDropStage::Buffered, NonZeroUsize::new(2).unwrap(), &"x")
            .starts_with("channel.inbound_dropped row for 2 dropped inbound messages (buffered) "),
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
    ];
    let names: Vec<&str> = all.iter().map(|&(c, s)| row_what(c, s)).collect();
    for (i, a) in names.iter().enumerate() {
        for b in &names[i + 1..] {
            assert_ne!(a, b, "{names:?}");
        }
    }
    assert!(names[0].starts_with("matrix ") && names[3].starts_with("email "), "{names:?}");
    assert!(names[1].ends_with("unread") && names[3].ends_with("unread"), "{names:?}");
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
