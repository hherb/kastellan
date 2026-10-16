//! #852: a poll result some of whose events did not decode, through the real
//! `run` loop.
//!
//! A decoder that reads events one at a time (Matrix's) returns the ones it
//! could read and counts the rest. On a channel that does not redeliver the
//! good ones reach the bus, the bad ones are said at ERROR and handed to the
//! `channel.inbound_undecodable` hook with their count, and the next poll is
//! held as after a whole undecodable batch. On an ack channel such a batch is
//! wholly undecodable: acking past a bad event would lose it for good.
//!
//! Each test that reads the emitted lines uses its own channel label, because
//! the emitter's test record is shared by every test in the binary.
use std::num::NonZeroUsize;

use super::super::undecodable::{format_undecodable_report, whole_batch_on_ack_channel, Fate, LostPart};
use super::inbound_drop::{counts, hook, run_until_the_bus_closes_mid_poll_parsed, Dropped};
use super::undecodable::{
    backoff, polls, recv_within_5s, settled_lines_for, spawn_with, undecodable_hook, Undecodables,
};
use super::*;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// The per-event decoder, Matrix's shape: each event of `test_parse`'s shape
/// on its own. Only a result with no `events` array fails whole.
fn test_parse_each(v: Value) -> anyhow::Result<PolledBatch> {
    let Some(raw) = v["events"].as_array() else {
        anyhow::bail!("no events array");
    };
    let (mut events, mut undecodable) = (Vec::new(), None);
    for e in raw {
        match test_parse_event(e) {
            Ok(ev) => events.push(ev),
            Err(err) => UndecodableEvents::note(&mut undecodable, || err.to_string()),
        }
    }
    Ok(PolledBatch { events, undecodable })
}

/// An event that decodes, with `body`.
fn good(body: &str) -> Value {
    json!({"peer": "@me:srv", "conversation": "!room:srv", "body": body})
}

/// An event that does not: `peer` must be a string.
fn bad() -> Value {
    json!({"peer": 42, "conversation": "!room:srv", "body": "a peer's words"})
}

/// The headline (Matrix's shape): the two good events of a batch reach the
/// bus at once; the bad one is said at ERROR with its count and handed to the
/// hook as `events`. The next batch waits out the backoff, and the first batch
/// that decodes whole ends the run.
#[test]
fn the_good_events_of_a_batch_reach_the_bus_and_the_bad_one_is_said_and_recorded() {
    let label = "t-undec-events";
    // Wide enough that the upper bound below cannot flake on a loaded sweep.
    let base = Duration::from_secs(1);
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(base), ..spec_without_ack() };
    let seen: Undecodables = Arc::default();
    let batches = vec![json!({"events": [good("a"), bad(), good("b")]}), json!({"events": [good("c")]})];
    let started = Instant::now();
    let (_st, mut driver) = spawn_with(spec, batches, test_parse_each, None, Some(&seen));

    assert_eq!(recv_within_5s(&mut driver).body, "a");
    assert_eq!(recv_within_5s(&mut driver).body, "b");
    assert!(started.elapsed() < base, "the batch's good events are not held: {:?}", started.elapsed());
    assert_eq!(recv_within_5s(&mut driver).body, "c");
    assert!(started.elapsed() >= base, "the next poll waited out the backoff: {:?}", started.elapsed());

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!((seen[0].0.as_str(), seen[0].1), (label, UndecodablePart::Events(NonZeroUsize::new(1).unwrap())));

    let lines = settled_lines_for(label, 2);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error, "{lines:?}");
    assert!(
        lines[0].0.contains(
            "1 of the 3 events in a poll result did not decode (first: bad event), so it is lost: \
             this channel does not redeliver. The other 2 decoded."
        ),
        "{lines:?}"
    );
    assert!(lines[0].0.contains("recording it as channel.inbound_undecodable"), "{lines:?}");
    assert_eq!(lines[1].1, RefusalSeverity::Recovered, "{lines:?}");
    assert!(lines[1].0.contains("decode whole again after 1 that did not"), "{lines:?}");
}

/// Every partial batch's loss is said and recorded, not just a run's first —
/// each holds different events, so the cadence that thins a redelivered
/// batch's lines does not apply. Each extends the run and waits out a growing
/// backoff; their good events all reach the bus.
#[test]
fn every_partly_undecodable_batch_is_said_and_recorded() {
    let label = "t-undec-events-run";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_millis(20)), ..spec_without_ack() };
    let seen: Undecodables = Arc::default();
    let partial = json!({"events": [good("a"), bad(), good("b")]});
    let (st, mut driver) = spawn_with(spec, vec![partial; 3], test_parse_each, None, Some(&seen));
    let bodies: Vec<String> = (0..6).map(|_| recv_within_5s(&mut driver).body).collect();
    assert_eq!(bodies, ["a", "b", "a", "b", "a", "b"]);
    wait_until(|| polls(&st) >= 4);
    let lines = settled_lines_for(label, 4);
    assert_eq!(lines.len(), 4, "{lines:?}");
    for (i, ms) in [20, 40, 80].into_iter().enumerate() {
        assert_eq!(lines[i].1, RefusalSeverity::Error, "{lines:?}");
        assert!(lines[i].0.contains("1 of the 3 events in a poll result did not decode"), "{i}: {lines:?}");
        assert!(lines[i].0.contains(&format!("Polling again in {ms} ms;")), "{i}: {lines:?}");
    }
    assert_eq!(lines[3].1, RefusalSeverity::Recovered, "{lines:?}");
    assert!(lines[3].0.contains("decode whole again after 3 that did not"), "{lines:?}");
    let seen = seen.lock().unwrap().clone();
    let one = UndecodablePart::Events(NonZeroUsize::new(1).unwrap());
    assert_eq!(seen.iter().map(|s| s.1).collect::<Vec<_>>(), [one; 3], "one row per partial batch");
}

/// A channel with no sink says the loss, and that it was NOT recorded.
#[test]
fn undecodable_events_on_a_channel_with_no_sink_are_not_claimed_as_recorded() {
    let label = "t-undec-events-nosink";
    let spec = PolledWorkerSpec { label, ..spec_without_ack() };
    let (_st, _driver) = spawn_with(spec, vec![json!({"events": [bad(), bad()]})], test_parse_each, None, None);
    wait_until(|| !emitted_refusal_lines_for(label).is_empty());
    let lines = emitted_refusal_lines_for(label);
    assert!(lines[0].0.contains("2 of the 2 events in a poll result did not decode"), "{lines:?}");
    assert!(
        lines[0].0.contains("NOT recorded as channel.inbound_undecodable: this channel has no audit sink"),
        "{lines:?}"
    );
}

/// On an ack channel a batch with an undecodable event is wholly
/// undecodable: none of it is acked (acking the event after the bad one would
/// move the monotonic cursor past it for good), none of it reaches the bus,
/// and it is said as a redelivered batch, with the poll held.
#[test]
fn on_an_ack_channel_a_batch_with_an_undecodable_event_is_not_acked_at_all() {
    let label = "email-undec-events";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_secs(5)), ..spec_with_ack() };
    let mut first = good("a");
    first["ack_token"] = json!("1");
    let mut last = good("b");
    last["ack_token"] = json!("3");
    let batch = json!({"events": [first, bad(), last]});
    let (st, mut driver) = spawn_with(spec, vec![batch; 200], test_parse_each, Some(encode_test_ack), None);
    wait_until(|| polls(&st) >= 1);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(polls(&st), 1, "the poll is held");
    assert!(!st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")), "nothing acked");
    assert!(driver.inbound_rx.try_recv().is_err(), "nothing reached the bus");
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].1, RefusalSeverity::Error);
    assert!(
        lines[0].0.contains(
            "a poll result did not decode (1 of the batch's 3 events did not decode: bad event), \
             so its batch was not acked"
        ),
        "{lines:?}"
    );
}

/// On an ack channel a redelivered batch with a bad event can grow as new
/// messages arrive behind it. Its line names the counts, but its cadence
/// follows only the decode error: the same bad event in a bigger batch is
/// not a different error, so it is not said again inside the repeat window.
#[test]
fn a_growing_redelivered_batch_with_the_same_bad_event_is_said_once() {
    let label = "email-undec-grows";
    let spec = PolledWorkerSpec { label, refusal_backoff: backoff(Duration::from_millis(20)), ..spec_with_ack() };
    let batches = vec![
        json!({"events": [good("a"), bad()]}),
        json!({"events": [good("a"), bad(), good("b")]}),
        json!({"events": [good("a"), bad(), good("b"), good("c")]}),
    ];
    let (st, _driver) = spawn_with(spec, batches, test_parse_each, Some(encode_test_ack), None);
    wait_until(|| polls(&st) >= 4);
    let lines = settled_lines_for(label, 2);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].0.contains("(1 of the batch's 2 events did not decode: bad event)"), "{lines:?}");
    assert_eq!(lines[1].1, RefusalSeverity::Recovered, "{lines:?}");
    assert!(lines[1].0.contains("after 3 that did not"), "POSITIVE CONTROL: all three were bad: {lines:?}");
}

/// The bus closed while the poll was out (Matrix's shape): the good events are
/// dropped at the polled stage and the bad one is lost undecodable — each
/// counted on its own row, and its own line, with no next poll named.
#[test]
fn a_partly_undecodable_batch_that_outlives_the_bus_is_counted_both_ways() {
    let label = "t-undec-events-closed";
    let spec = PolledWorkerSpec { label, ..spec_without_ack() };
    let dropped: Dropped = Arc::default();
    let seen: Undecodables = Arc::default();
    let audit = DriverAudit {
        inbound_dropped: Some(hook(&dropped)),
        inbound_undecodable: Some(undecodable_hook(&seen)),
        ..DriverAudit::none()
    };
    let batch = json!({"events": [good("a"), bad(), good("b")]});
    let log = run_until_the_bus_closes_mid_poll_parsed(spec, batch, test_parse_each, None, audit);
    assert!(log.iter().any(|m| m.ends_with(".poll")), "POSITIVE CONTROL: the poll ran: {log:?}");
    assert_eq!(counts(&dropped.lock().unwrap()), [(label.to_string(), InboundDropStage::Polled, 2)]);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].1, UndecodablePart::Events(NonZeroUsize::new(1).unwrap()));
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 2, "{lines:?}");
    let undecodable = lines.iter().find(|(l, _)| l.contains("did not decode")).expect("{lines:?}");
    assert_eq!(undecodable.1, RefusalSeverity::Error);
    assert!(undecodable.0.contains("1 of the 3 events in a poll result did not decode"), "{lines:?}");
    assert!(!undecodable.0.contains("Polling again"), "the driver is exiting: {lines:?}");
}

/// The bus closed while the poll was out, and no event of the result decoded:
/// nothing was dropped at the polled stage, so there is no such row or line;
/// all of it is lost undecodable.
#[test]
fn a_wholly_bad_batch_that_outlives_the_bus_is_lost_undecodable_only() {
    let label = "t-undec-events-closed-bad";
    let spec = PolledWorkerSpec { label, ..spec_without_ack() };
    let dropped: Dropped = Arc::default();
    let seen: Undecodables = Arc::default();
    let audit = DriverAudit {
        inbound_dropped: Some(hook(&dropped)),
        inbound_undecodable: Some(undecodable_hook(&seen)),
        ..DriverAudit::none()
    };
    let batch = json!({"events": [bad(), bad()]});
    let log = run_until_the_bus_closes_mid_poll_parsed(spec, batch, test_parse_each, None, audit);
    assert!(log.iter().any(|m| m.ends_with(".poll")), "POSITIVE CONTROL: the poll ran: {log:?}");
    assert!(dropped.lock().unwrap().is_empty(), "nothing decoded, so nothing was dropped");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.iter().map(|s| s.1).collect::<Vec<_>>(), [UndecodablePart::Events(NonZeroUsize::new(2).unwrap())]);
    let lines = emitted_refusal_lines_for(label);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].0.contains("2 of the 2 events in a poll result did not decode"), "{lines:?}");
}

/// Pure: on an ack channel, a batch with undecodable events becomes an
/// `Err` that names the count, the batch's size and the first error — the
/// counts as context, the error as the root cause the cadence follows; every
/// other batch passes through unchanged.
#[test]
fn only_an_ack_channel_turns_undecodable_events_into_an_undecodable_batch() {
    let partial = || PolledBatch {
        events: vec![test_parse_event(&good("a")).unwrap()],
        undecodable: Some(UndecodableEvents { count: NonZeroUsize::new(2).unwrap(), first_error: "why".into() }),
    };
    let e = whole_batch_on_ack_channel(partial(), true).expect_err("an ack channel");
    assert_eq!(format!("{e:#}"), "2 of the batch's 3 events did not decode: why");
    assert_eq!(e.root_cause().to_string(), "why", "the cadence key carries no count");
    assert_eq!(whole_batch_on_ack_channel(partial(), false).unwrap(), partial(), "no ack method");
    let whole = PolledBatch::whole(vec![]);
    assert_eq!(whole_batch_on_ack_channel(whole.clone(), true).unwrap(), whole, "nothing undecodable");
}

/// Pure: the line agrees in number, says what decoded without claiming it
/// reached the bus, and says nothing of the rest when nothing decoded.
#[test]
fn the_undecodable_events_line_counts_both_sides() {
    let lost = |lost: usize, of: usize| {
        let part = LostPart::Events { lost: NonZeroUsize::new(lost).unwrap(), decoded: of - lost };
        format_undecodable_report("why", Fate::Lost { part, retry_in: None, recorded: true })
    };
    assert!(lost(1, 2).starts_with(
        "1 of the 2 events in a poll result did not decode (first: why), so it is lost: this \
         channel does not redeliver. The other one decoded. The worker sent output"
    ));
    assert!(lost(2, 5).contains("so they are lost: this channel does not redeliver. The other 3 decoded."));
    assert!(lost(3, 3).contains("does not redeliver. The worker sent output"), "{}", lost(3, 3));
}

/// Pure: `note` keeps the first error and counts every one.
#[test]
fn undecodable_events_keep_the_first_error_and_count_them_all() {
    let mut u = None;
    UndecodableEvents::note(&mut u, || "first".into());
    UndecodableEvents::note(&mut u, || unreachable!("only the first error is kept"));
    UndecodableEvents::note(&mut u, || unreachable!("only the first error is kept"));
    assert_eq!(u, Some(UndecodableEvents { count: NonZeroUsize::new(3).unwrap(), first_error: "first".into() }));
}
