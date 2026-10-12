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

/// The other end of a held slot: it need not land. On a full buffer the slot
/// the bus frees goes straight to the driver waiting in its send; if the
/// buffer closes before that driver runs, its send is refused and the slot
/// given back — so an unsettled drain can count a message that never entered
/// the buffer (which the driver then reports too, or the worker redelivers).
/// The send future is polled by hand, so "before the driver runs" is exact,
/// not a race.
#[test]
fn a_slot_handed_to_a_waiting_sender_can_end_in_a_refused_send() {
    use std::future::Future;
    use std::task::{Context, Poll, Wake, Waker};
    struct NoWake;
    impl Wake for NoWake {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(NoWake));
    let mut cx = Context::from_waker(&waker);

    let (tx, mut rx) = tok_mpsc::channel::<u8>(1);
    tx.try_send(0).unwrap();
    let mut send = Box::pin(tx.send(1));
    assert!(send.as_mut().poll(&mut cx).is_pending(), "the buffer is full: the sender waits");
    assert_eq!(rx.try_recv().ok(), Some(0), "the bus reads one, handing its slot to the waiter");
    assert_eq!(
        drain_unread(&mut rx),
        Drained { unread: 0, settled: false },
        "the waiter holds the slot, so the drain cannot settle"
    );
    assert!(
        matches!(send.as_mut().poll(&mut cx), Poll::Ready(Err(_))),
        "the waiter finds the buffer closed: its send is refused, not landed"
    );
    assert_eq!(drain_unread(&mut rx), Drained { unread: 0, settled: true }, "and the slot is given back");
}

/// Pure: a drain loses what it took out, plus one for an unsettled slot.
#[test]
fn lost_counts_what_was_taken_out_plus_an_unsettled_slot() {
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

/// A slot held as the receiver drops is counted in the row, not only by
/// [`Drained::lost`]: the `Drop` reports what the drain lost, the slot with
/// it.
#[test]
fn a_receiver_dropped_while_a_slot_is_held_counts_the_slot() {
    let dropped: Dropped = Arc::default();
    let (rx, tx) = buffered("t-unread-inflight", 1, false, Some(hook(&dropped)));
    let permit = tx.try_reserve().expect("a slot");
    drop(rx);
    assert_eq!(counts(&dropped.lock().unwrap()), [("t-unread-inflight".into(), InboundDropStage::Buffered, 2)]);
    let lines = emitted_refusal_lines_for("t-unread-inflight");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("dropped 2 inbound messages"), "{lines:?}");
    drop(permit);
}

/// The driver had already exited (its sender is gone): an empty buffer still
/// reports nothing, and messages it left behind are still counted.
#[test]
fn a_receiver_whose_driver_has_exited_counts_only_what_it_left() {
    let dropped: Dropped = Arc::default();
    let (rx, tx) = buffered("t-unread-gone-empty", 0, false, Some(hook(&dropped)));
    drop(tx);
    drop(rx);
    assert!(dropped.lock().unwrap().is_empty());
    assert!(emitted_refusal_lines_for("t-unread-gone-empty").is_empty());

    let (rx, tx) = buffered("t-unread-gone", 2, false, Some(hook(&dropped)));
    drop(tx);
    drop(rx);
    assert_eq!(counts(&dropped.lock().unwrap()), [("t-unread-gone".into(), InboundDropStage::Buffered, 2)]);
}

/// A slow hook at the buffered stage is said — as holding the bus's stopping
/// pump, which is where it runs, not the driver thread.
#[test]
fn a_slow_hook_at_the_buffered_stage_is_said_to_hold_the_stopping_bus() {
    use super::super::audit::slow_hook_reports_for;
    let slow: InboundDroppedAudit =
        Arc::new(|_: InboundDropped<'_>| std::thread::sleep(Duration::from_millis(250)));
    let (rx, _tx) = buffered("t-unread-slow-hook", 1, false, Some(slow));
    drop(rx);
    let lines = slow_hook_reports_for("t-unread-slow-hook");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("the inbound-dropped audit hook held the bus's stopping pump"),
        "{lines:?}"
    );
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
         handed to the bus, so they are not redelivered (any held back by a refused ack earlier in \
         its batch, or whose own ack failed, may be redelivered, and is counted here anyway); \
         recording it as channel.inbound_dropped"
    );
    assert!(line(2, false, false).ends_with("this channel has no audit sink"), "{}", line(2, false, false));
}

// ----- through the real driver -----

/// Spawn a driver whose one poll answers `n` events, wait until all `n` sit
/// in the buffer unread, then drop the receiver (the bus stopping) and wait
/// for the driver to exit. Returns what the hook was handed and how many acks
/// the worker saw. `refuse_first_ack` (ack shape only) refuses the batch's
/// first ack, so the driver acks none of the rest.
fn run_bus_stops_unread(
    label: &'static str,
    n: usize,
    ack: bool,
    refuse_first_ack: bool,
) -> (Vec<(String, InboundDropStage, usize)>, usize) {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(events(n, ack));
    let base = if ack { spec_with_ack() } else { spec_without_ack() };
    let spec = PolledWorkerSpec { label, ..base };
    if refuse_first_ack {
        *st.refuse.lock().unwrap() = Some((spec.ack_method.expect("an ack shape").into(), 1));
    }
    // The refused ack is still a call the worker saw.
    let expected_acks = if refuse_first_ack { 1 } else { n };
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
        wait_until(|| log.lock().unwrap().iter().filter(|(m, _)| m.ends_with(".ack")).count() == expected_acks);
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
    let (dropped, _acks) = run_bus_stops_unread("t-unread-run", 3, false, false);
    assert_eq!(dropped, [("t-unread-run".into(), InboundDropStage::Buffered, 3)]);
    // The receiver the real spawn built knows this channel does not ack.
    let lines = emitted_refusal_lines_for("t-unread-run");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("this channel does not redeliver"), "{lines:?}");
    assert!(!lines[0].0.contains("acked"), "{lines:?}");
}

/// The same on email's shape: the driver acked all three as the buffer took
/// them (the positive control), so they are lost and reported.
#[test]
fn an_ack_channel_s_acked_unread_messages_are_reported_once() {
    let (dropped, acks) = run_bus_stops_unread("email-unread-run", 3, true, false);
    assert_eq!(acks, 3, "POSITIVE CONTROL: the driver acked what the buffer took");
    assert_eq!(dropped, [("email-unread-run".into(), InboundDropStage::Buffered, 3)]);
}

/// The deliberate over-count, pinned as behaviour: a refused first ack stops
/// the driver acking the rest of its batch, which the worker therefore
/// redelivers — but the receiver cannot see that, so it counts all three, and
/// its line says such messages may be redelivered.
#[test]
fn an_ack_channel_s_refused_ack_tail_is_still_counted() {
    let (dropped, acks) = run_bus_stops_unread("email-unread-refused", 3, true, true);
    assert_eq!(acks, 1, "POSITIVE CONTROL: the first ack was refused, and no other was tried");
    assert_eq!(dropped, [("email-unread-refused".into(), InboundDropStage::Buffered, 3)]);
    let lines = emitted_refusal_lines_for("email-unread-refused");
    let drop_lines: Vec<_> = lines.iter().filter(|(l, _)| l.contains("inbound message")).collect();
    assert_eq!(drop_lines.len(), 1, "{lines:?}");
    assert!(drop_lines[0].0.contains("held back by a refused ack"), "{lines:?}");
    assert!(drop_lines[0].0.contains("may be redelivered"), "{lines:?}");
}
