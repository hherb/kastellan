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
use super::*;
use crate::worker_lifecycle::RestartBackoff;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// A batch `test_parse` rejects: `peer` must be a string.
fn undecodable() -> Value {
    json!({"events": [{"peer": 42, "conversation": "!room:srv", "body": "SECRET-BODY"}]})
}

/// What the hook was handed, per call: `(channel, observed_at)`.
type Undecodables = Arc<Mutex<Vec<(String, time::OffsetDateTime)>>>;

fn undecodable_hook(seen: &Undecodables) -> InboundUndecodableAudit {
    let sink = seen.clone();
    Box::new(move |b: UndecodableBatch<'_>| {
        sink.lock().unwrap().push((b.channel.0.clone(), b.observed_at))
    })
}

/// A backoff whose first wait is `base`, so a test can see the poll held.
fn backoff(base: Duration) -> RestartBackoff {
    RestartBackoff { base, ..REFUSAL_BACKOFF }
}

/// Spawn a driver over the scripted fake with `polls` queued and the
/// undecodable hook wired to `seen`.
fn spawn_with(
    spec: PolledWorkerSpec,
    polls: Vec<Value>,
    encode_ack: Option<EncodeAck>,
    seen: &Undecodables,
) -> (Arc<FakeState>, PolledWorkerDriver) {
    let (st, calls) = fake();
    st.polls.lock().unwrap().extend(polls);
    let audit =
        DriverAudit { inbound_undecodable: Some(undecodable_hook(seen)), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, encode_ack, None, audit,
        ChannelId(spec.label.into()),
    )
    .expect("driver spawn");
    (st, driver)
}

fn polls(st: &FakeState) -> usize {
    st.log.lock().unwrap().iter().filter(|(m, _)| m.ends_with(".poll")).count()
}

/// The headline (Matrix's shape): an undecodable batch is said at ERROR,
/// with the decode error and without the body, and handed to the hook — the
/// channel and when, nothing else. The next batch still arrives, but only
/// after the poll's backoff, and the first good decode says the run ended.
#[test]
fn an_undecodable_batch_that_is_not_redelivered_is_said_at_error_and_recorded() {
    let label = "t-undec-matrix";
    let base = Duration::from_millis(300);
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(base), ..spec_without_ack() };
    let good = json!({"events": [{"peer": "@me:srv", "conversation": "!room:srv", "body": "after"}]});
    let seen: Undecodables = Arc::default();
    let before = time::OffsetDateTime::now_utc();
    let started = Instant::now();
    let (_st, mut driver) = spawn_with(spec, vec![undecodable(), good], None, &seen);

    let msg = driver.inbound_rx.blocking_recv().expect("the batch after the bad one");
    assert_eq!(msg.body, "after");
    assert!(started.elapsed() >= base, "the next poll waited out the backoff: {:?}", started.elapsed());

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, label);
    assert!(before <= seen[0].1, "observed_at is the drop's time");

    wait_until(|| emitted_refusal_lines_for(label).len() >= 2);
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error, "{lines:?}");
    assert!(lines[0].0.contains("did not decode"), "{lines:?}");
    assert!(lines[0].0.contains("does not redeliver"), "{lines:?}");
    assert!(lines[0].0.contains("bad event"), "names the decode error: {lines:?}");
    assert!(lines[0].0.contains("recording it as channel.inbound_undecodable"), "{lines:?}");
    assert!(!lines[0].0.contains("SECRET-BODY"), "never a body: {lines:?}");
    assert_eq!(lines[1].1, RefusalSeverity::Recovered, "{lines:?}");
    assert!(lines[1].0.contains("decode again after 1 undecodable batch"), "{lines:?}");
}

/// A channel with no sink (the `matrix probe`) still says the loss — and
/// says it was NOT recorded, rather than claiming a row nobody wrote.
#[test]
fn an_undecodable_batch_on_a_channel_with_no_sink_is_not_claimed_as_recorded() {
    let label = "t-undec-nosink";
    let spec = PolledWorkerSpec { label, ..spec_without_ack() };
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(undecodable());
    let (_driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, None, None, DriverAudit::none(),
        ChannelId(label.into()),
    )
    .expect("driver spawn");
    wait_until(|| !emitted_refusal_lines_for(label).is_empty());
    let lines = emitted_refusal_lines_for(label);
    assert!(
        lines[0].0.contains("NOT recorded as channel.inbound_undecodable: this channel has no audit sink"),
        "{lines:?}"
    );
}

/// The hot loop (email's shape): a batch that never decodes is not acked, so
/// the worker answers every poll with it again at once. The poll must wait
/// out its backoff instead of re-polling as fast as the worker answers. It is
/// redelivered, so it is not a loss: said at ERROR once for the run, never
/// recorded as a row.
#[test]
fn a_redelivered_batch_that_never_decodes_holds_the_poll_and_writes_no_row() {
    let label = "email-undec";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_secs(5)), ..spec_with_ack() };
    let seen: Undecodables = Arc::default();
    let (st, _driver) =
        spawn_with(spec, vec![undecodable(); 200], Some(encode_test_ack), &seen);
    wait_until(|| polls(&st) >= 1);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(polls(&st), 1, "the poll is held by the backoff, not re-run at once");
    assert!(!st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")), "nothing acked");
    assert!(seen.lock().unwrap().is_empty(), "redelivered, so no row");
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error);
    assert!(lines[0].0.contains("not acked, so the worker redelivers it"), "{lines:?}");
    assert!(lines[0].0.contains("Polling again in 5000 ms"), "{lines:?}");
    assert!(!lines[0].0.contains("inbound_undecodable"), "no row is claimed: {lines:?}");
}

/// A redelivered batch that keeps failing is said once per run, not once
/// per poll: the second and third failures, inside the repeat window, are
/// silent — but each still waits out a growing backoff.
#[test]
fn a_run_of_redelivered_undecodable_batches_is_said_once() {
    let label = "email-undec-run";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_millis(20)), ..spec_with_ack() };
    let seen: Undecodables = Arc::default();
    let (st, _driver) = spawn_with(spec, vec![undecodable(); 3], Some(encode_test_ack), &seen);
    // Three bad batches, then the fake's empty ones: the third decode that
    // works ends the run.
    wait_until(|| polls(&st) >= 4);
    wait_until(|| emitted_refusal_lines_for(label).len() >= 2);
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error);
    assert_eq!(lines[1].1, RefusalSeverity::Recovered);
    assert!(lines[1].0.contains("decode again after 3 undecodable batches"), "{lines:?}");
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
    assert_eq!(seen.lock().unwrap().len(), 1);
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
    let seen: Undecodables = Arc::default();
    let audit =
        DriverAudit { inbound_undecodable: Some(undecodable_hook(&seen)), ..DriverAudit::none() };
    let log = run_until_the_bus_closes_mid_poll_with(spec, undecodable(), Some(encode_test_ack), audit);
    assert!(log.iter().any(|m| m.ends_with(".poll")), "POSITIVE CONTROL: the poll ran: {log:?}");
    assert!(seen.lock().unwrap().is_empty());
    assert!(emitted_refusal_lines_for(label).is_empty());
}

/// Pure: the row is the channel and when, and nothing else — no count (it is
/// unknown), no error text (a decode error can quote a peer's message).
#[test]
fn the_inbound_undecodable_row_is_channel_and_when_only() {
    let v = UndecodableBatch {
        channel: &ChannelId("matrix".into()),
        observed_at: time::macros::datetime!(2026-10-13 12:34:56 UTC),
    }
    .payload();
    assert_eq!(v, json!({"channel": "matrix", "observed_at": "2026-10-13T12:34:56Z"}));
    assert_eq!(
        crate::channel::actions::INBOUND_UNDECODABLE,
        "channel.inbound_undecodable",
        "pinned: operators query audit_log by it"
    );
}

/// Pure: the line says what happened to the batch, names the error, and —
/// for a loss — whether it is recorded; for a redelivered batch, when the
/// poll runs again and that it repeats.
#[test]
fn the_undecodable_line_says_where_the_batch_went() {
    use super::super::undecodable::format_undecodable_report;
    let ms = Some(Duration::from_millis(1000));
    assert_eq!(
        format_undecodable_report(false, "bad event", ms, true),
        "a poll result did not decode (bad event), so its batch is lost: this channel does not \
         redeliver, and how many messages it held is unknown. The worker sent output the core \
         cannot read: a worker bug, or a compromised worker. Polling again in 1000 ms; \
         recording it as channel.inbound_undecodable"
    );
    assert!(format_undecodable_report(false, "e", None, false).ends_with(
        "cannot read: a worker bug, or a compromised worker. NOT recorded as \
         channel.inbound_undecodable: this channel has no audit sink"
    ));
    assert_eq!(
        format_undecodable_report(true, "bad event", ms, true),
        "a poll result did not decode (bad event), so its batch was not acked, so the worker \
         redelivers it. The worker sent output the core cannot read: a worker bug, or a \
         compromised worker. Polling again in 1000 ms with backoff. Repeated every 15 min \
         while it lasts"
    );
}

/// Pure: the recovery line agrees in number, and says nothing when no run
/// ended.
#[test]
fn the_decodes_again_line_counts_the_run() {
    use super::super::undecodable::format_decodes_again_report;
    assert_eq!(format_decodes_again_report(0), None);
    assert_eq!(
        format_decodes_again_report(1).as_deref(),
        Some("poll results decode again after 1 undecodable batch; that run has ended")
    );
    assert!(format_decodes_again_report(2).unwrap().contains("after 2 undecodable batches"));
}
