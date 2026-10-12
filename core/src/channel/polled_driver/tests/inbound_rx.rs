//! #846: inbound messages the driver had handed to the bus that the bus had
//! not read when it stopped. The bus-side [`InboundReceiver`] counts them as
//! it is dropped and reports them at the buffered stage — on every channel,
//! because an ack channel had already acked them.
//!
//! Each test that reads the emitted lines uses its own channel label, because
//! the emitter's test record is shared by every test in the binary.
use super::inbound_drop::{counts, events, hook, Dropped};
use super::*;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};
use super::super::inbound_rx::{drain_unread, Drained};
use tokio::sync::mpsc as tok_mpsc;

fn message(body: &str) -> IncomingMessage {
    IncomingMessage {
        channel: ChannelId("t".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: body.into(),
        evidence: None,
    }
}

/// A receiver over a fresh buffer holding `n` messages, and the buffer's
/// sender (kept alive, as the driver's is while it runs).
fn buffered(
    label: &'static str,
    n: usize,
    acks: bool,
    audit: Option<InboundDroppedAudit>,
) -> (InboundReceiver, tok_mpsc::Sender<IncomingMessage>) {
    let (tx, rx) = tok_mpsc::channel(INBOUND_BUFFER);
    for i in 0..n {
        tx.try_send(message(&format!("m{i}"))).expect("room in the buffer");
    }
    (InboundReceiver::new(rx, label, ChannelId(label.into()), acks, audit), tx)
}

// ----- the drain -----

/// An empty buffer drains to 0, a buffer of 3 to 3 — with the sender still
/// alive, which must not make the drain wait for more: both SETTLE, i.e. the
/// drain returned on `Disconnected`, not by running out of spins (which a
/// timing bound could not tell apart — it is under a millisecond).
#[test]
fn the_drain_counts_what_is_buffered_and_does_not_wait_on_a_live_sender() {
    let (tx, mut rx) = tok_mpsc::channel::<u8>(8);
    assert_eq!(drain_unread(&mut rx), Drained { unread: 0, settled: true });
    let (tx2, mut rx2) = tok_mpsc::channel::<u8>(8);
    for i in 0..3 {
        tx2.try_send(i).unwrap();
    }
    assert_eq!(drain_unread(&mut rx2), Drained { unread: 3, settled: true });
    drop((tx, tx2));
}

/// The tokio contract the drain is built on, pinned so a tokio upgrade that
/// changes it fails here rather than silently: after `close()`, a send that
/// was granted its slot before the close reads as `Empty` (not
/// `Disconnected`) until it lands, can still land, and only then does the
/// buffer read `Disconnected`.
#[test]
fn a_send_granted_before_the_close_still_lands_and_holds_disconnected_back() {
    use tok_mpsc::error::TryRecvError;
    let (tx, mut rx) = tok_mpsc::channel::<u8>(8);
    let permit = tx.try_reserve().expect("a slot");
    rx.close();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)), "not settled while a slot is held");
    permit.send(7);
    assert_eq!(rx.try_recv().ok(), Some(7), "the granted send lands after the close");
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)), "and then it settles");
}

/// A send granted its slot that never lands within the bound: the drain
/// stops waiting, says the buffer did not settle, and the loss counts it —
/// it would land in a buffer about to be dropped.
#[test]
fn an_unsettled_drain_counts_the_send_still_on_its_way() {
    let (tx, mut rx) = tok_mpsc::channel::<u8>(8);
    tx.try_send(1).unwrap();
    let permit = tx.try_reserve().expect("a slot");
    let drained = drain_unread(&mut rx);
    assert_eq!(drained, Drained { unread: 1, settled: false });
    assert_eq!(drained.lost(), 2);
    drop(permit);
}

/// Pure: a settled drain loses exactly what it took out.
#[test]
fn a_settled_drain_loses_exactly_what_it_took_out() {
    assert_eq!(Drained { unread: 0, settled: true }.lost(), 0);
    assert_eq!(Drained { unread: 5, settled: true }.lost(), 5);
    assert_eq!(Drained { unread: 0, settled: false }.lost(), 1);
}

/// The drain closes the buffer FIRST, so the driver cannot add a message the
/// count then misses: its next send fails, and it reports what it holds.
#[test]
fn the_drain_closes_the_buffer_so_the_driver_s_next_send_fails() {
    let (tx, mut rx) = tok_mpsc::channel::<u8>(8);
    tx.try_send(1).unwrap();
    assert_eq!(drain_unread(&mut rx), Drained { unread: 1, settled: true });
    assert!(tx.is_closed(), "the sender sees the close");
    assert!(tx.try_send(2).is_err(), "and cannot add to the count it was not part of");
}

/// A message read before the stop is the bus's, not a loss: only what is
/// still buffered is counted.
#[test]
fn a_message_the_bus_read_is_not_counted() {
    let dropped: Dropped = Arc::default();
    let (mut rx, _tx) = buffered("t-unread-read", 3, false, Some(hook(&dropped)));
    assert!(rx.blocking_recv().is_some(), "POSITIVE CONTROL: the bus read one");
    drop(rx);
    assert_eq!(counts(&dropped.lock().unwrap()), [("t-unread-read".into(), InboundDropStage::Buffered, 2)]);
}

// ----- the report -----

/// The headline, on Matrix's shape (no ack method): two messages buffered
/// unread when the bus drops the receiver are counted, said once at WARN,
/// and handed to the hook at the buffered stage — never a body.
#[test]
fn messages_buffered_unread_are_counted_said_and_audited() {
    let dropped: Dropped = Arc::default();
    let (rx, _tx) = buffered("t-unread", 2, false, Some(hook(&dropped)));
    let before = time::OffsetDateTime::now_utc();
    drop(rx);
    let after = time::OffsetDateTime::now_utc();
    let calls = dropped.lock().unwrap().clone();
    assert_eq!(counts(&calls), [("t-unread".into(), InboundDropStage::Buffered, 2)]);
    let at = calls[0].3;
    assert!(before <= at && at <= after, "observed_at is the drop's time");
    let lines = emitted_refusal_lines_for("t-unread");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Warn);
    assert!(lines[0].0.contains("dropped 2 inbound messages: the bus stopped"), "{lines:?}");
    assert!(lines[0].0.contains("this channel does not redeliver"), "{lines:?}");
    assert!(lines[0].0.contains("recording it as channel.inbound_dropped"), "{lines:?}");
    assert!(!lines[0].0.contains("m0"), "never a body: {lines:?}");
}

/// The common case: the bus had read everything. Nothing is said or recorded.
#[test]
fn an_empty_buffer_reports_nothing() {
    let dropped: Dropped = Arc::default();
    let (rx, _tx) = buffered("t-unread-empty", 0, false, Some(hook(&dropped)));
    drop(rx);
    assert!(dropped.lock().unwrap().is_empty());
    assert!(emitted_refusal_lines_for("t-unread-empty").is_empty());
}

/// An ack channel's buffered messages are reported too — they were acked
/// when the buffer took them — and the line says that is why.
#[test]
fn an_ack_channel_s_buffered_messages_are_reported_as_acked() {
    let dropped: Dropped = Arc::default();
    let (rx, _tx) = buffered("email-unread", 1, true, Some(hook(&dropped)));
    drop(rx);
    assert_eq!(counts(&dropped.lock().unwrap()), [("email-unread".into(), InboundDropStage::Buffered, 1)]);
    let lines = emitted_refusal_lines_for("email-unread");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("this channel acked it when it was handed to the bus"), "{lines:?}");
    assert!(!lines[0].0.contains("does not redeliver"), "{lines:?}");
}

/// A channel with no audit sink still says the drop, and says it was NOT
/// recorded.
#[test]
fn a_buffered_drop_with_no_sink_is_said_and_not_claimed_as_recorded() {
    let (rx, _tx) = buffered("t-unread-nosink", 3, false, None);
    drop(rx);
    let lines = emitted_refusal_lines_for("t-unread-nosink");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("NOT recorded as channel.inbound_dropped"), "{lines:?}");
}

/// Pure: the buffered-stage line, in number, with each channel's reason.
#[test]
fn the_unread_line_counts_and_gives_the_channel_s_reason() {
    use super::super::inbound_drop::format_unread_drop_report;
    let line = |n: usize, acks: bool, sink: bool| {
        format_unread_drop_report(std::num::NonZeroUsize::new(n).unwrap(), acks, sink)
    };
    assert_eq!(
        line(1, false, true),
        "dropped 1 inbound message: the bus stopped (the channel was restarted or shut down) \
         before reading it from its inbound buffer, and this channel does not redeliver; \
         recording it as channel.inbound_dropped"
    );
    assert_eq!(
        line(3, true, true),
        "dropped 3 inbound messages: the bus stopped (the channel was restarted or shut down) \
         before reading them from its inbound buffer, and this channel acked them when they were \
         handed to the bus, so they are not redelivered (any whose ack failed, or was held back by \
         a refused ack earlier in its batch, is redelivered, and counted here anyway); recording \
         it as channel.inbound_dropped"
    );
    assert!(line(2, false, false).ends_with("this channel has no audit sink"), "{}", line(2, false, false));
}

// ----- through the real driver -----

/// Spawn a driver whose one poll answers `n` events, wait until all `n` sit
/// in the buffer unread, then drop the receiver (the bus stopping) and wait
/// for the driver to exit. Returns what the hook was handed and how many acks
/// the worker saw.
fn run_bus_stops_unread(label: &'static str, n: usize, ack: bool) -> (Vec<(String, InboundDropStage, usize)>, usize) {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(events(n, ack));
    let base = if ack { spec_with_ack() } else { spec_without_ack() };
    let spec = PolledWorkerSpec { label, ..base };
    let encode_ack: Option<EncodeAck> = if ack { Some(encode_test_ack) } else { None };
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, encode_ack, None, audit, ChannelId(label.into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    wait_until(|| inbound_rx.len() == n);
    if ack {
        let log = &st.log;
        wait_until(|| log.lock().unwrap().iter().filter(|(m, _)| m.ends_with(".ack")).count() == n);
    }
    drop(inbound_rx);
    join.join().expect("the driver exits cleanly");
    drop(outbound_tx);
    let acks = st.log.lock().unwrap().iter().filter(|(m, _)| m.ends_with(".ack")).count();
    let dropped = counts(&dropped.lock().unwrap());
    (dropped, acks)
}

/// The issue's own case, end to end on Matrix's shape: the driver handed 3
/// messages over and the bus never read them. Exactly one report, at the
/// buffered stage — the driver, exiting on its next poll, adds nothing.
#[test]
fn a_bus_that_stops_with_unread_messages_reports_them_once() {
    let (dropped, _acks) = run_bus_stops_unread("t-unread-run", 3, false);
    assert_eq!(dropped, [("t-unread-run".into(), InboundDropStage::Buffered, 3)]);
}

/// The same on email's shape: the driver acked all three as the buffer took
/// them (the positive control), so they are lost and reported.
#[test]
fn an_ack_channel_s_acked_unread_messages_are_reported_once() {
    let (dropped, acks) = run_bus_stops_unread("email-unread-run", 3, true);
    assert_eq!(acks, 3, "POSITIVE CONTROL: the driver acked what the buffer took");
    assert_eq!(dropped, [("email-unread-run".into(), InboundDropStage::Buffered, 3)]);
}
