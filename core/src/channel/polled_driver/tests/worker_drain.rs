//! #835: what the worker itself still held when the driver exited. A channel
//! whose worker queues inbound events between polls (Matrix's live sync) names
//! a drain method; the driver calls it once, on every exit, and reports what
//! it held at the [`InboundDropStage::Worker`] stage — counted, said and
//! audited, exactly like the stages the driver and the bus hold (#826, #846).
//!
//! Each test that reads the emitted lines uses its own channel label, because
//! the emitter's test record is shared by every test in the binary.
use std::num::NonZeroUsize;

use super::inbound_drop::{counts, hook, Dropped};
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
            join.join().expect("the driver exits cleanly");
        }
        Exit::Outbound => {
            drop(outbound_tx);
            join.join().expect("the driver exits cleanly");
            drop(inbound_rx);
        }
        Exit::Inbound => {
            drop(inbound_rx);
            join.join().expect("the driver exits cleanly");
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

/// Every way out of the driver drains the worker, exactly once.
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

/// A worker that cannot answer (down, or a stale binary without the method)
/// leaves the count unknown: said, never assumed to be zero.
#[test]
fn a_drain_the_worker_cannot_answer_is_said_and_records_nothing() {
    for (label, answer) in [("t-held-err", None), ("t-held-junk", Some(json!({"events": []})))] {
        let (st, calls) = fake();
        match &answer {
            None => *st.fail_method.lock().unwrap() = Some("matrix.drain".into()),
            Some(v) => *st.drain.lock().unwrap() = v.clone(),
        }
        let dropped: Dropped = Arc::default();
        let audit = DriverAudit { inbound_dropped: Some(hook(&dropped)), ..DriverAudit::none() };
        let spec = spec_with_drain(label);
        let (driver, _) = PolledWorkerDriver::spawn(
            spec, calls, test_parse, test_encode, None, None, audit, ChannelId(label.into()),
        )
        .expect("driver spawn");
        let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
        drop(inbound_rx);
        drop(outbound_tx);
        join.join().expect("the driver exits cleanly");
        assert!(dropped.lock().unwrap().is_empty(), "{label}");
        let lines = emitted_refusal_lines_for(label);
        assert_eq!(lines.len(), 1, "{label}: {lines:?}");
        assert!(lines[0].0.contains("could not ask the worker"), "{label}: {lines:?}");
        assert!(lines[0].0.contains("lost uncounted"), "{label}: {lines:?}");
        assert_eq!(lines[0].1, RefusalSeverity::Warn);
    }
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
