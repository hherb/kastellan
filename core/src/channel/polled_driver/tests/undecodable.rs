//! #836: a poll result that did not decode, through the real `run` loop.
//!
//! On a channel that does not redeliver (Matrix) the batch is gone and its
//! size unknown: it is said at ERROR and handed to the
//! `channel.inbound_undecodable` hook — with no count, never a 0 dressed up
//! as one. On an ack channel (email) it is not acked, so it is redelivered:
//! not a loss, so no row, but a batch that never decodes would be re-polled
//! at once, forever — so every channel's poll waits out the refusal backoff
//! after one, and a decode that works again says so once.
//!
//! Each test that reads the emitted lines uses its own channel label, because
//! the emitter's test record is shared by every test in the binary.
use super::inbound_drop::run_until_the_bus_closes_mid_poll_with;
use super::super::audit::slow_hook_reports_for;
use super::super::undecodable::{
    error_code, format_decodes_again_report, format_undecodable_report, report_lost_undecodable, Fate,
    LostPart, DECODE_ERROR_CAP_CHARS,
};
use super::*;
use crate::worker_lifecycle::RestartBackoff;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// A batch `test_parse` rejects: `peer` must be a string.
fn undecodable() -> Value {
    json!({"events": [{"peer": 42, "conversation": "!room:srv", "body": "a body"}]})
}

/// A batch [`parse_failing_with`] rejects with the error `why`.
fn failing_with(why: &str) -> Value {
    json!({"fail": why})
}

/// `test_parse`, except that a batch with a `fail` field fails with its text
/// as the error — so a test can choose the decode error.
fn parse_failing_with(v: Value) -> anyhow::Result<PolledBatch> {
    match v["fail"].as_str() {
        Some(why) => Err(anyhow::anyhow!("{why}")),
        None => test_parse(v),
    }
}

/// What the hook was handed, per call: `(channel, part, observed_at)`.
pub(super) type Undecodables = Arc<Mutex<Vec<(String, UndecodablePart, time::OffsetDateTime)>>>;

pub(super) fn undecodable_hook(seen: &Undecodables) -> InboundUndecodableAudit {
    let sink = seen.clone();
    Box::new(move |b: InboundUndecodable<'_>| {
        sink.lock().unwrap().push((b.channel.0.clone(), b.part, b.observed_at))
    })
}

/// A backoff whose first wait is `base`, so a test can see the poll held.
pub(super) fn backoff(base: Duration) -> RestartBackoff {
    RestartBackoff { base, ..REFUSAL_BACKOFF }
}

/// Spawn a driver over the scripted fake with `polls` queued, decoded by
/// `parse`, and the undecodable hook wired to `seen` when given.
pub(super) fn spawn_with(
    spec: PolledWorkerSpec,
    polls: Vec<Value>,
    parse: ParsePoll,
    encode_ack: Option<EncodeAck>,
    seen: Option<&Undecodables>,
) -> (Arc<FakeState>, PolledWorkerDriver) {
    let (st, calls) = fake();
    st.polls.lock().unwrap().extend(polls);
    let audit = DriverAudit { inbound_undecodable: seen.map(undecodable_hook), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, parse, test_encode, encode_ack, None, audit,
        ChannelId(spec.label.into()),
    )
    .expect("driver spawn");
    (st, driver)
}

pub(super) fn polls(st: &FakeState) -> usize {
    st.log.lock().unwrap().iter().filter(|(m, _)| m.ends_with(".poll")).count()
}

/// The lines for `label`, once `n` of them are out and a short settle has
/// passed with no more — so a count that is too high fails rather than
/// racing the next one.
pub(super) fn settled_lines_for(label: &str, n: usize) -> Vec<(String, RefusalSeverity)> {
    wait_until(|| emitted_refusal_lines_for(label).len() >= n);
    std::thread::sleep(Duration::from_millis(100));
    emitted_refusal_lines_for(label)
}

/// The next inbound message, failing rather than hanging if none comes.
pub(super) fn recv_within_5s(driver: &mut PolledWorkerDriver) -> IncomingMessage {
    let mut got = None;
    wait_until(|| {
        got = driver.inbound_rx.try_recv().ok();
        got.is_some()
    });
    got.expect("set by the wait")
}

/// The headline (Matrix's shape): an undecodable batch is said at ERROR,
/// with the decode error, and handed to the hook — the channel and when,
/// nothing else. The next batch still arrives, but only after the poll's
/// backoff, and the first good decode says the run ended.
#[test]
fn an_undecodable_batch_that_is_not_redelivered_is_said_at_error_and_recorded() {
    let label = "t-undec-matrix";
    let base = Duration::from_millis(300);
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(base), ..spec_without_ack() };
    let good = json!({"events": [{"peer": "@me:srv", "conversation": "!room:srv", "body": "after"}]});
    let seen: Undecodables = Arc::default();
    let before = time::OffsetDateTime::now_utc();
    let started = Instant::now();
    let (_st, mut driver) = spawn_with(spec, vec![undecodable(), good], test_parse, None, Some(&seen));

    let msg = recv_within_5s(&mut driver);
    assert_eq!(msg.body, "after");
    assert!(started.elapsed() >= base, "the next poll waited out the backoff: {:?}", started.elapsed());

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, label);
    assert_eq!(seen[0].1, UndecodablePart::Batch, "a whole result");
    assert!(before <= seen[0].2, "observed_at is the drop's time");

    let lines = settled_lines_for(label, 2);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error, "{lines:?}");
    assert!(lines[0].0.contains("did not decode"), "{lines:?}");
    assert!(lines[0].0.contains("does not redeliver"), "{lines:?}");
    assert!(lines[0].0.contains("bad event"), "names the decode error: {lines:?}");
    assert!(lines[0].0.contains("recording it as channel.inbound_undecodable"), "{lines:?}");
    assert_eq!(lines[1].1, RefusalSeverity::Recovered, "{lines:?}");
    assert!(lines[1].0.contains("decode whole again after 1 that did not"), "{lines:?}");
}

/// Every loss is said and recorded, not just a run's first: each lost batch
/// is a different one. And each waits out a growing backoff, which its line
/// names.
#[test]
fn every_lost_undecodable_batch_is_said_and_recorded() {
    let label = "t-undec-matrix-run";
    let base = Duration::from_millis(20);
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(base), ..spec_without_ack() };
    let seen: Undecodables = Arc::default();
    let (st, _driver) = spawn_with(spec, vec![undecodable(); 3], test_parse, None, Some(&seen));
    wait_until(|| polls(&st) >= 4);
    let lines = settled_lines_for(label, 4);
    assert_eq!(lines.len(), 4, "{lines:?}");
    for (i, ms) in [20, 40, 80].into_iter().enumerate() {
        assert_eq!(lines[i].1, RefusalSeverity::Error, "{lines:?}");
        assert!(lines[i].0.contains(&format!("Polling again in {ms} ms;")), "{i}: {lines:?}");
    }
    assert_eq!(lines[3].1, RefusalSeverity::Recovered, "{lines:?}");
    assert!(lines[3].0.contains("decode whole again after 3 that did not"), "{lines:?}");
    assert_eq!(seen.lock().unwrap().len(), 3, "one row per lost batch");
}

/// A channel with no sink (the `matrix probe`) still says the loss — and
/// says it was NOT recorded, rather than claiming a row nobody wrote.
#[test]
fn an_undecodable_batch_on_a_channel_with_no_sink_is_not_claimed_as_recorded() {
    let label = "t-undec-nosink";
    let spec = PolledWorkerSpec { label, ..spec_without_ack() };
    let (_st, _driver) = spawn_with(spec, vec![undecodable()], test_parse, None, None);
    wait_until(|| !emitted_refusal_lines_for(label).is_empty());
    let lines = emitted_refusal_lines_for(label);
    assert!(
        lines[0].0.contains("NOT recorded as channel.inbound_undecodable: this channel has no audit sink"),
        "{lines:?}"
    );
}

/// An undecodable hook on an ack channel would never be called — its
/// batches are redelivered, not lost — so the driver refuses it at spawn
/// rather than ignoring it at every batch.
#[test]
fn an_undecodable_hook_on_an_ack_channel_is_refused_at_spawn() {
    let (_st, calls) = fake();
    let seen: Undecodables = Arc::default();
    let audit = DriverAudit { inbound_undecodable: Some(undecodable_hook(&seen)), ..DriverAudit::none() };
    let spec = PolledWorkerSpec { label: "email-undec-hooked", ..spec_with_ack() };
    let err = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, Some(encode_test_ack), None, audit,
        ChannelId(spec.label.into()),
    )
    .err()
    .expect("refused");
    assert!(err.to_string().contains("would never be called"), "{err}");
}

/// The hot loop (email's shape): a batch that never decodes is not acked, so
/// the worker answers every poll with it again at once. The poll must wait
/// out its backoff instead of re-polling as fast as the worker answers. It is
/// redelivered, so it is not a loss: said at ERROR once for the run, with no
/// row claimed.
#[test]
fn a_redelivered_batch_that_never_decodes_holds_the_poll() {
    let label = "email-undec";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_secs(5)), ..spec_with_ack() };
    let (st, _driver) = spawn_with(spec, vec![undecodable(); 200], test_parse, Some(encode_test_ack), None);
    wait_until(|| polls(&st) >= 1);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(polls(&st), 1, "the poll is held by the backoff, not re-run at once");
    assert!(!st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")), "nothing acked");
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error);
    assert!(lines[0].0.contains("not acked, so the worker redelivers it"), "{lines:?}");
    assert!(lines[0].0.contains("nothing after it arrives until it decodes"), "{lines:?}");
    assert!(lines[0].0.contains("Polling again in 5000 ms with backoff (1 in a row so far)"), "{lines:?}");
    assert!(!lines[0].0.contains("inbound_undecodable"), "no row is claimed: {lines:?}");
}

/// The hold gates the poll only: the driver's one thread keeps flushing
/// replies while it lasts. A hold that slept the thread instead would stall
/// every send, and a channel stop, for up to the backoff's cap.
#[test]
fn a_held_poll_does_not_hold_the_replies() {
    let label = "email-undec-sends";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_secs(5)), ..spec_with_ack() };
    let (st, driver) = spawn_with(spec, vec![undecodable(); 200], test_parse, Some(encode_test_ack), None);
    wait_until(|| polls(&st) >= 1);
    let sent_at = Instant::now();
    driver.outbound_tx.send(super::replies::reply("!a", "during the hold")).unwrap();
    wait_until(|| !st.sends.lock().unwrap().is_empty());
    assert!(sent_at.elapsed() < Duration::from_secs(1), "sent during the hold: {:?}", sent_at.elapsed());
    assert_eq!(polls(&st), 1, "POSITIVE CONTROL: the poll is still held");
}

/// A redelivered batch that keeps failing is said once per run, not once
/// per poll: the second and third failures, inside the repeat window, are
/// silent.
#[test]
fn a_run_of_redelivered_undecodable_batches_is_said_once() {
    let label = "email-undec-run";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_millis(20)), ..spec_with_ack() };
    let (st, _driver) = spawn_with(spec, vec![undecodable(); 3], test_parse, Some(encode_test_ack), None);
    // Three bad batches, then the fake's empty ones: the first decode that
    // works (the fourth poll) ends a run of three.
    wait_until(|| polls(&st) >= 4);
    let lines = settled_lines_for(label, 2);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error);
    assert_eq!(lines[1].1, RefusalSeverity::Recovered);
    assert!(lines[1].0.contains("decode whole again after 3 that did not"), "{lines:?}");
}

/// A redelivered batch whose decode error changes is said again at once —
/// a different failure is not held to the repeat window — while the same
/// error again stays silent.
#[test]
fn a_different_decode_error_is_said_at_once() {
    let label = "email-undec-changed";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_millis(20)), ..spec_with_ack() };
    let batches = vec![failing_with("error A"), failing_with("error A"), failing_with("error B")];
    let (st, _driver) = spawn_with(spec, batches, parse_failing_with, Some(encode_test_ack), None);
    wait_until(|| polls(&st) >= 4);
    let lines = settled_lines_for(label, 3);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].0.contains("(error A)"), "{lines:?}");
    assert!(lines[1].0.contains("(error B)"), "{lines:?}");
    assert!(lines[1].0.contains("(3 in a row so far)"), "{lines:?}");
    assert_eq!(lines[2].1, RefusalSeverity::Recovered, "{lines:?}");
}

/// The worker died and came back mid-run: its "back up" line must not be the
/// last word on a batch that still does not decode, so the next one is said
/// whatever the cadence. The recovery is noticed here by a reply's send,
/// which the driver flushes before it polls — not by the poll itself.
#[test]
fn an_undecodable_batch_after_the_worker_came_back_is_said_again() {
    let label = "email-undec-respawn";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_millis(20)), ..spec_with_ack() };
    let (st, driver) = spawn_with(spec, vec![undecodable(); 200], test_parse, Some(encode_test_ack), None);
    wait_until(|| emitted_refusal_lines_for(label).len() == 1);
    st.down.store(true, Ordering::SeqCst);
    driver.outbound_tx.send(super::replies::reply("!a", "queued while down")).unwrap();
    wait_until(|| st.down_attempts.lock().unwrap().iter().any(|m| m.ends_with(".send")));
    st.down.store(false, Ordering::SeqCst);
    wait_until(|| !st.sends.lock().unwrap().is_empty());
    let lines = settled_lines_for(label, 2);
    assert_eq!(lines.len(), 2, "said again after the outage, not 15 min later: {lines:?}");
    assert_eq!(lines[1].1, RefusalSeverity::Error, "{lines:?}");
}

/// A refused poll in the middle of a run is not a decode: the run ends at
/// the first batch that decodes, after the poll is accepted again.
#[test]
fn a_refused_poll_does_not_end_an_undecodable_run() {
    let label = "email-undec-refused";
    // Long enough a hold to set the refusals inside it.
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_millis(300)), ..spec_with_ack() };
    let (st, _driver) = spawn_with(spec, vec![undecodable()], test_parse, Some(encode_test_ack), None);
    wait_until(|| emitted_refusal_lines_for(label).len() == 1);
    // The next two polls are refused; the third gets the fake's empty batch.
    *st.refuse.lock().unwrap() = Some(("email.poll".into(), 2));
    let lines = settled_lines_for(label, 4);
    let accepted = lines.iter().position(|(l, _)| l.contains("accepted email.poll again")).expect("{lines:?}");
    let decoded = lines.iter().position(|(l, _)| l.contains("decode whole again after 1")).expect("{lines:?}");
    assert!(accepted < decoded, "the run ends at a decode, not at a refusal: {lines:?}");
    assert_eq!(lines.iter().filter(|(l, _)| l.contains("decode whole again")).count(), 1, "{lines:?}");
}

/// The hook runs on the driver's thread, so it is timed like the others
/// (#793): a slow one is said, not silently waited on.
#[test]
fn a_slow_undecodable_hook_is_said() {
    let label = "slow-undecodable-hook";
    let spec = PolledWorkerSpec { label, ..spec_without_ack() };
    let slow: InboundUndecodableAudit = Box::new(|_: InboundUndecodable<'_>| std::thread::sleep(Duration::from_millis(250)));
    report_lost_undecodable(&spec, &ChannelId(label.into()), "e", LostPart::Batch, None, Some(&slow));
    let lines = slow_hook_reports_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("inbound-undecodable"), "{lines:?}");
}

/// The bus closed while the poll was out, and the batch that came back did
/// not decode (Matrix's shape): the driver exits, and the loss is said and
/// recorded exactly as while the bus is up.
#[test]
fn an_undecodable_batch_that_outlives_the_bus_is_said_and_recorded() {
    let label = "t-undec-closed";
    let spec = PolledWorkerSpec { label, ..spec_without_ack() };
    let seen: Undecodables = Arc::default();
    let audit =
        DriverAudit { inbound_undecodable: Some(undecodable_hook(&seen)), ..DriverAudit::none() };
    let log = run_until_the_bus_closes_mid_poll_with(spec, undecodable(), None, audit);
    assert!(log.iter().any(|m| m.ends_with(".poll")), "POSITIVE CONTROL: the poll ran: {log:?}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, UndecodablePart::Batch);
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error);
    assert!(lines[0].0.contains("recording it as channel.inbound_undecodable"), "{lines:?}");
}

/// An ack channel's batch that outlives the bus is never decoded at all: it
/// is unacked and redelivered whatever it holds, so nothing is said.
#[test]
fn an_ack_channel_s_batch_that_outlives_the_bus_is_not_decoded() {
    let label = "email-undec-closed";
    let spec = PolledWorkerSpec { label, ..spec_with_ack() };
    let log = run_until_the_bus_closes_mid_poll_with(spec, undecodable(), Some(encode_test_ack), DriverAudit::none());
    assert!(log.iter().any(|m| m.ends_with(".poll")), "POSITIVE CONTROL: the poll ran: {log:?}");
    assert!(emitted_refusal_lines_for(label).is_empty());
}

/// Pure: the row is the channel, the part and when, and nothing else — no
/// count for a whole batch or a drain answer (it is unknown), the count for
/// events, and never error text (a decode error can quote a peer's message).
#[test]
fn the_inbound_undecodable_row_is_channel_part_when_and_an_events_count() {
    let row = |part| {
        InboundUndecodable {
            channel: &ChannelId("matrix".into()),
            part,
            observed_at: time::macros::datetime!(2026-10-13 12:34:56 UTC),
        }
        .payload()
    };
    let at = "2026-10-13T12:34:56Z";
    assert_eq!(row(UndecodablePart::Batch), json!({"channel": "matrix", "part": "batch", "observed_at": at}));
    assert_eq!(row(UndecodablePart::Drain), json!({"channel": "matrix", "part": "drain", "observed_at": at}));
    let two = std::num::NonZeroUsize::new(2).unwrap();
    assert_eq!(
        row(UndecodablePart::Events(two)),
        json!({"channel": "matrix", "part": "events", "undecodable": 2, "observed_at": at})
    );
    assert_eq!(
        crate::channel::actions::INBOUND_UNDECODABLE,
        "channel.inbound_undecodable",
        "pinned: operators query audit_log by it"
    );
}

/// Pure: the line says what happened to the batch, names the error, and —
/// for a loss — whether it is recorded; for a redelivered batch, when the
/// poll runs again, how many in a row, and that it repeats.
#[test]
fn the_undecodable_line_says_where_the_batch_went() {
    let ms = Duration::from_millis(1000);
    assert_eq!(
        format_undecodable_report("bad event", Fate::Lost { part: LostPart::Batch, retry_in: Some(ms), recorded: true }),
        "a poll result did not decode (bad event), so its batch is lost: this channel does not \
         redeliver, and how many messages it held is unknown. The worker sent output the core \
         cannot read: a worker bug, or a compromised worker. Polling again in 1000 ms; \
         recording it as channel.inbound_undecodable"
    );
    assert!(format_undecodable_report("e", Fate::Lost { part: LostPart::Batch, retry_in: None, recorded: false }).ends_with(
        "cannot read: a worker bug, or a compromised worker. NOT recorded as \
         channel.inbound_undecodable: this channel has no audit sink"
    ));
    // "15 min" is `REFUSAL_REPEAT`, which `tests/refusal.rs` pins.
    assert_eq!(
        format_undecodable_report("bad event", Fate::Redelivered { retry_in: ms, in_a_row: 2 }),
        "a poll result did not decode (bad event), so its batch was not acked, so the worker \
         redelivers it, and nothing after it arrives until it decodes. The worker sent output \
         the core cannot read: a worker bug, or a compromised worker. Polling again in 1000 ms \
         with backoff (2 in a row so far). Repeated every 15 min while it lasts"
    );
}

/// Pure: a decode error can quote a message body whole, so the line keeps
/// only the first [`DECODE_ERROR_CAP_CHARS`] of it, and says so.
#[test]
fn a_long_decode_error_is_capped_in_its_line() {
    let long = "x".repeat(DECODE_ERROR_CAP_CHARS + 1000);
    let line = format_undecodable_report(&long, Fate::Lost { part: LostPart::Batch, retry_in: None, recorded: true });
    let kept = format!("({}{})", "x".repeat(DECODE_ERROR_CAP_CHARS), crate::channel::audit_text::TRUNCATION_MARKER);
    assert!(line.contains(&kept), "{line}");
    let short = format_undecodable_report("short", Fate::Lost { part: LostPart::Batch, retry_in: None, recorded: true });
    assert!(short.contains("(short)"), "POSITIVE CONTROL: a short error is whole: {short}");
}

/// Pure: the same decode error gets the same code, and a different one —
/// almost surely — a different code.
#[test]
fn a_decode_error_s_code_follows_its_text() {
    assert_eq!(error_code("error A"), error_code("error A"));
    assert_ne!(error_code("error A"), error_code("error B"));
}

/// Pure: the recovery line agrees in number, and says nothing when no run
/// ended.
#[test]
fn the_decodes_again_line_counts_the_run() {
    assert_eq!(format_decodes_again_report(0), None);
    assert_eq!(
        format_decodes_again_report(1).as_deref(),
        Some("poll results decode whole again after 1 that did not; that run has ended")
    );
    assert!(format_decodes_again_report(2).unwrap().contains("after 2 that did not"));
}
