//! #835: what the worker itself still held when the driver exited. A channel
//! whose worker queues inbound events between polls (Matrix's live sync) names
//! a drain method; the driver calls it once, on every exit, and reports what
//! it held at the [`InboundDropStage::Worker`] stage — counted, said and
//! audited, exactly like the stages the driver and the bus hold (#826, #846).
//!
//! Each test that reads the emitted lines uses its own channel label, because
//! the emitter's test record is shared by every test in the binary.
use std::num::NonZeroUsize;
use std::thread::JoinHandle;

use super::super::undecodable::DECODE_ERROR_CAP_CHARS;
use super::super::worker_drain::format_drain_undecodable_report;
use super::inbound_drop::{counts, events, hook, Dropped};
use super::undecodable::{undecodable_hook, Undecodables};
use super::*;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// The test channel's drain decoder: the same two fields Matrix's wire uses.
fn test_parse_drain(v: Value) -> anyhow::Result<WorkerHeld> {
    Ok(WorkerHeld {
        discarded: v["discarded"].as_u64().ok_or_else(|| anyhow::anyhow!("no discarded"))? as usize,
        interrupted: v["interrupted"].as_bool().ok_or_else(|| anyhow::anyhow!("no interrupted"))?,
    })
}

/// A Matrix-shaped spec (no ack method) whose worker can be drained.
fn spec_with_drain(label: &'static str) -> PolledWorkerSpec {
    PolledWorkerSpec {
        label,
        drain: Some(WorkerDrainSpec { method: "matrix.drain", parse: test_parse_drain }),
        ..spec_without_ack()
    }
}

/// Join the driver within 5 s, or fail with `log`: a mutant that never exits
/// is a red test, not a hung binary.
fn join_within(join: JoinHandle<()>, st: &FakeState) {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(join.join().is_ok());
    });
    match done_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(true) => {}
        Ok(false) => panic!("the driver panicked"),
        Err(_) => panic!("the driver did not exit within 5s: {:?}", st.log.lock().unwrap()),
    }
}

/// How the test makes the driver exit: which endpoint(s) it drops.
#[derive(Clone, Copy)]
enum Exit {
    /// Both, as a channel's own drop does.
    Both,
    /// The outbound sender only: the driver's step 1 sees `Disconnected`.
    Outbound,
    /// The inbound receiver only: the driver sees a closed bus after a poll.
    Inbound,
}

/// Spawn a driver over the scripted fake with `spec`, let it poll once, make
/// it exit by `exit`, and wait for it. The fake's `.drain` answers `held`.
/// Returns what the hook was handed and every method the worker was called
/// with, in order.
fn run_and_exit(spec: PolledWorkerSpec, held: Value, exit: Exit) -> (Vec<(String, InboundDropStage, usize)>, Vec<String>) {
    let (st, calls) = fake();
    *st.drain.lock().unwrap() = held;
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, None, None, audit, ChannelId(spec.label.into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    wait_until(|| st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".poll")));
    match exit {
        Exit::Both => {
            drop(inbound_rx);
            drop(outbound_tx);
            join_within(join, &st);
        }
        Exit::Outbound => {
            drop(outbound_tx);
            join_within(join, &st);
            drop(inbound_rx);
        }
        Exit::Inbound => {
            drop(inbound_rx);
            join_within(join, &st);
            drop(outbound_tx);
        }
    }
    let log = st.log.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
    let dropped = counts(&dropped.lock().unwrap());
    (dropped, log)
}

/// The headline: two events the worker still held when the channel stopped
/// are counted, said once at WARN, and handed to the hook at the worker stage.
/// The drain is the last call the worker gets.
#[test]
fn messages_the_worker_still_held_at_exit_are_counted_said_and_audited() {
    let label = "t-held-2";
    let held = json!({"discarded": 2, "interrupted": false});
    let (dropped, log) = run_and_exit(spec_with_drain(label), held, Exit::Both);
    assert_eq!(dropped, [(label.to_string(), InboundDropStage::Worker, 2)]);
    assert_eq!(log.last().map(String::as_str), Some("matrix.drain"), "{log:?}");
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("dropped 2 inbound messages"), "{lines:?}");
    assert!(lines[0].0.contains("the worker still held them"), "{lines:?}");
    assert!(lines[0].0.contains("recording it as channel.inbound_dropped"), "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Warn);
}

/// Each exit this helper drives (outbound gone; bus closed after a poll; both)
/// drains the worker exactly once. The mid-batch and retry-slice exits have
/// their own tests below; all of them leave `run` by the same `break 'drive`.
#[test]
fn every_exit_drains_the_worker_once() {
    for (label, exit) in [("t-held-both", Exit::Both), ("t-held-out", Exit::Outbound), ("t-held-in", Exit::Inbound)] {
        let held = json!({"discarded": 1, "interrupted": false});
        let (dropped, log) = run_and_exit(spec_with_drain(label), held, exit);
        let drains = log.iter().filter(|m| *m == "matrix.drain").count();
        assert_eq!(drains, 1, "{label}: {log:?}");
        assert_eq!(dropped, [(label.to_string(), InboundDropStage::Worker, 1)], "{label}");
    }
}

/// The common case: the worker held nothing, so nothing is said or recorded.
#[test]
fn a_worker_that_held_nothing_reports_nothing() {
    let label = "t-held-0";
    let held = json!({"discarded": 0, "interrupted": false});
    let (dropped, log) = run_and_exit(spec_with_drain(label), held, Exit::Both);
    assert!(log.iter().any(|m| m == "matrix.drain"), "still asked: {log:?}");
    assert!(dropped.is_empty(), "{dropped:?}");
    assert!(emitted_refusal_lines_for(label).is_empty());
}

/// A stop that may have cut a sync response short is said even when nothing
/// was counted — but there is no count to record, so there is no row.
#[test]
fn an_interrupted_stop_is_said_without_a_row() {
    let label = "t-held-cut";
    let held = json!({"discarded": 0, "interrupted": true});
    let (dropped, _log) = run_and_exit(spec_with_drain(label), held, Exit::Both);
    assert!(dropped.is_empty(), "{dropped:?}");
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("an unknown number"), "{lines:?}");
    assert!(lines[0].0.contains("possibly none"), "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Warn);
}

/// Both at once: the count is recorded, and the cut-short response is said
/// beside it.
#[test]
fn an_interrupted_stop_with_a_count_records_the_count_and_says_both() {
    let label = "t-held-cut-3";
    let held = json!({"discarded": 3, "interrupted": true});
    let (dropped, _log) = run_and_exit(spec_with_drain(label), held, Exit::Both);
    assert_eq!(dropped, [(label.to_string(), InboundDropStage::Worker, 3)]);
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().any(|l| l.0.contains("dropped 3 inbound messages")), "{lines:?}");
    assert!(lines.iter().any(|l| l.0.contains("an unknown number")), "{lines:?}");
}

/// Exit a drain-spec driver at once and return what each hook was handed.
/// `answer` is what the fake's `.drain` says; `None` makes the call fail.
fn exit_with_drain_answer(label: &'static str, answer: Option<Value>) -> (Dropped, Undecodables) {
    let (st, calls) = fake();
    match answer {
        None => *st.fail_method.lock().unwrap() = Some("matrix.drain".into()),
        Some(v) => *st.drain.lock().unwrap() = v,
    }
    let dropped: Dropped = Arc::default();
    let seen: Undecodables = Arc::default();
    let audit = DriverAudit {
        inbound_dropped: Some(hook(&dropped)),
        inbound_undecodable: Some(undecodable_hook(&seen)),
        ..DriverAudit::none()
    };
    let (driver, _) = PolledWorkerDriver::spawn(
        spec_with_drain(label), calls, test_parse, test_encode, None, None, audit, ChannelId(label.into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    drop(inbound_rx);
    drop(outbound_tx);
    join_within(join, &st);
    (dropped, seen)
}

/// A worker that cannot be asked (it died on the call, or a stale binary
/// without the method) leaves the count unknown: said at WARN, never assumed
/// to be zero, and no row — nothing the worker sent was unreadable.
#[test]
fn a_drain_the_worker_cannot_answer_is_said_and_records_nothing() {
    let label = "t-held-err";
    let (dropped, seen) = exit_with_drain_answer(label, None);
    assert!(dropped.lock().unwrap().is_empty());
    assert!(seen.lock().unwrap().is_empty(), "a failed call is not an undecodable answer");
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("could not ask the worker"), "{lines:?}");
    assert!(lines[0].0.contains("lost uncounted"), "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Warn);
}

/// #853: a drain answer that does not decode is the #836 event — the worker
/// sent output the core cannot read — so it is said at ERROR with the decode
/// error, and recorded as `channel.inbound_undecodable` with part `drain`;
/// never as a count, and never mixed in with "could not ask".
#[test]
fn an_undecodable_drain_answer_is_said_at_error_and_recorded() {
    let label = "t-held-junk";
    let (dropped, seen) = exit_with_drain_answer(label, Some(json!({"events": []})));
    assert!(dropped.lock().unwrap().is_empty(), "no count to record");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!((seen[0].0.as_str(), seen[0].1), (label, UndecodablePart::Drain));
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error);
    assert_eq!(
        lines[0].0,
        "channel worker t-held-junk: the worker's answer to matrix.drain did not decode (no discarded), so how many inbound \
         messages it still held is unknown; any it held, received but not yet polled, are lost \
         uncounted. The worker sent output the core cannot read: a worker bug, or a compromised \
         worker. recording it as channel.inbound_undecodable"
    );
}

/// Pure: with no sink the drain line says it was NOT recorded, and a long
/// decode error is capped as a poll's is.
#[test]
fn the_undecodable_drain_line_caps_its_error_and_says_when_unrecorded() {
    let long = "x".repeat(DECODE_ERROR_CAP_CHARS + 50);
    let line = format_drain_undecodable_report("matrix.drain", &long, false);
    assert!(line.ends_with("NOT recorded as channel.inbound_undecodable: this channel has no audit sink"), "{line}");
    assert!(!line.contains(&"x".repeat(DECODE_ERROR_CAP_CHARS + 1)), "capped: {line}");
    assert!(line.contains(&"x".repeat(DECODE_ERROR_CAP_CHARS)), "POSITIVE CONTROL: the cap keeps its share");
}

/// The exit from step 4's retry slice: every poll is refused, so no poll ever
/// completes, and the driver notices the closed bus while waiting. The worker
/// answered (a refusal is not an outage), so it is asked, and its count lands.
#[test]
fn an_exit_from_the_retry_slice_drains_the_worker() {
    let label = "t-held-retry";
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("matrix.poll".into(), usize::MAX));
    *st.drain.lock().unwrap() = json!({"discarded": 1, "interrupted": false});
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _) = PolledWorkerDriver::spawn(
        spec_with_drain(label), calls, test_parse, test_encode, None, None, audit, ChannelId(label.into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    wait_until(|| st.log.lock().unwrap().iter().any(|(m, _)| m == "matrix.poll"));
    drop(inbound_rx);
    join_within(join, &st);
    drop(outbound_tx);
    let log: Vec<String> = st.log.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
    assert_eq!(log.iter().filter(|m| *m == "matrix.drain").count(), 1, "{log:?}");
    assert_eq!(log.last().map(String::as_str), Some("matrix.drain"), "{log:?}");
    assert_eq!(counts(&dropped.lock().unwrap()), [(label.to_string(), InboundDropStage::Worker, 1)]);
}

/// A worker that was down at the driver's last call is NOT asked: the
/// supervisor would answer "restarting" after a backoff and then respawn a
/// fresh worker for nobody, holding this exit past the shutdown bound. The
/// loss is said instead — no row, because there is no count.
#[test]
fn a_worker_down_at_exit_is_not_asked_and_the_loss_is_said() {
    let label = "t-held-down";
    let (st, calls) = fake();
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _) = PolledWorkerDriver::spawn(
        spec_with_drain(label), calls, test_parse, test_encode, None, None, audit, ChannelId(label.into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    st.down.store(true, Ordering::SeqCst);
    wait_until(|| st.down_attempts.lock().unwrap().iter().any(|m| m == "matrix.poll"));
    drop(inbound_rx);
    drop(outbound_tx);
    join_within(join, &st);
    let attempts = st.down_attempts.lock().unwrap().clone();
    assert!(!attempts.iter().any(|m| m == "matrix.drain"), "a down worker was asked: {attempts:?}");
    assert!(!st.log.lock().unwrap().iter().any(|(m, _)| m == "matrix.drain"));
    assert!(dropped.lock().unwrap().is_empty());
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("did not ask the worker"), "{lines:?}");
    assert!(lines[0].0.contains("it was down"), "{lines:?}");
    assert!(lines[0].0.contains("lost uncounted"), "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Warn);
}

/// The mid-batch exit, with all three stages at once: the batch tail the
/// driver could not hand over (polled), the full buffer the bus never read
/// (buffered), and what the worker still held (worker) — three rows, never
/// summed, and the drain is still the last call.
#[test]
fn a_mid_batch_exit_drains_the_worker_and_keeps_the_three_stages_apart() {
    let label = "t-held-mid";
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(events(INBOUND_BUFFER + 2, false));
    *st.drain.lock().unwrap() = json!({"discarded": 3, "interrupted": false});
    let dropped: Dropped = Arc::default();
    let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
    let (driver, _) = PolledWorkerDriver::spawn(
        spec_with_drain(label), calls, test_parse, test_encode, None, None, audit, ChannelId(label.into()),
    )
    .expect("driver spawn");
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    wait_until(|| inbound_rx.len() == INBOUND_BUFFER);
    drop(inbound_rx);
    join_within(join, &st);
    drop(outbound_tx);
    assert_eq!(
        counts(&dropped.lock().unwrap()),
        [
            (label.to_string(), InboundDropStage::Buffered, INBOUND_BUFFER),
            (label.to_string(), InboundDropStage::Polled, 2),
            (label.to_string(), InboundDropStage::Worker, 3),
        ]
    );
    let log: Vec<String> = st.log.lock().unwrap().iter().map(|(m, _)| m.clone()).collect();
    assert_eq!(log.last().map(String::as_str), Some("matrix.drain"), "{log:?}");
}

/// A drain method on an ack channel would say "does not redeliver" and write
/// a false row, so `spawn` refuses the spec before calling the worker at all.
#[test]
fn spawn_refuses_a_drain_method_on_an_ack_channel() {
    let (st, calls) = fake();
    let spec = PolledWorkerSpec { ack_method: Some("matrix.ack"), ..spec_with_drain("t-held-ack") };
    let err = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, None, None, DriverAudit::none(), ChannelId("t-held-ack".into()),
    )
    .err()
    .expect("refused");
    assert!(err.to_string().contains("does not redeliver"), "{err:#}");
    assert_eq!(st.init_calls.load(Ordering::SeqCst), 0, "refused before init");
}

/// Both production specs pass the check: Matrix drains and does not ack,
/// email acks and does not drain.
#[test]
fn the_production_specs_pass_the_drain_check() {
    use super::super::worker_drain::check_drain;
    check_drain(&crate::channel::matrix::MATRIX_POLLED_SPEC).expect("matrix");
    check_drain(&crate::channel::email::wire::EMAIL_POLLED_SPEC).expect("email");
}

/// A channel with no drain method (email: localmail keeps the messages) is
/// never sent one.
#[test]
fn a_channel_without_a_drain_method_is_never_asked() {
    let spec = PolledWorkerSpec { label: "t-no-drain", ..spec_without_ack() };
    let held = json!({"discarded": 5, "interrupted": false});
    let (dropped, log) = run_and_exit(spec, held, Exit::Both);
    assert!(!log.iter().any(|m| m.ends_with(".drain")), "{log:?}");
    assert!(dropped.is_empty(), "{dropped:?}");
}

/// Pure: the line's wording, one and many, with and without a sink — and it
/// never names anything a peer chose.
#[test]
fn the_worker_stage_line_says_where_the_messages_were() {
    let one = super::super::inbound_drop::format_worker_drop_report(NonZeroUsize::MIN, true);
    assert_eq!(
        one,
        "dropped 1 inbound message: the channel stopped while the worker still held it, received \
         but not yet polled, and this channel does not redeliver; recording it as \
         channel.inbound_dropped"
    );
    let many = super::super::inbound_drop::format_worker_drop_report(NonZeroUsize::new(4).unwrap(), false);
    assert!(many.starts_with("dropped 4 inbound messages: "), "{many}");
    assert!(many.contains("still held them"), "{many}");
    assert!(many.ends_with("NOT recorded as channel.inbound_dropped: this channel has no audit sink"), "{many}");
}

#[test]
fn the_worker_stage_is_spelled_worker() {
    assert_eq!(InboundDropStage::Worker.as_str(), "worker", "pinned: operators query by it");
}
