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
    let (mut events, mut bad, mut first_error) = (Vec::new(), 0, None);
    for e in raw {
        match test_parse_event(e) {
            Ok(ev) => events.push(ev),
            Err(err) => {
                bad += 1;
                first_error.get_or_insert(err.to_string());
            }
        }
    }
    let undecodable = NonZeroUsize::new(bad)
        .map(|count| UndecodableEvents { count, first_error: first_error.unwrap_or_default() });
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
    let base = Duration::from_millis(300);
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
    assert!(!lines[0].0.contains("a peer's words"), "POSITIVE CONTROL: the bad event's body is not in it");
    assert_eq!(lines[1].1, RefusalSeverity::Recovered, "{lines:?}");
    assert!(lines[1].0.contains("decode again after 1 undecodable batch"), "{lines:?}");
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
            "a poll result did not decode (1 of the batch's 3 events did not decode; first: bad \
             event), so its batch was not acked"
        ),
        "{lines:?}"
    );
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
    assert!(!undecodable.0.contains("Polling again"), "the driver is exiting: {lines:?}");
}

/// Pure: on an ack channel, a batch with undecodable events becomes an
/// `Err` that names the count, the batch's size and the first error; every
/// other batch passes through unchanged.
#[test]
fn only_an_ack_channel_turns_undecodable_events_into_an_undecodable_batch() {
    let partial = || PolledBatch {
        events: vec![test_parse_event(&good("a")).unwrap()],
        undecodable: Some(UndecodableEvents { count: NonZeroUsize::new(2).unwrap(), first_error: "why".into() }),
    };
    let e = whole_batch_on_ack_channel(partial(), true).expect_err("an ack channel");
    assert_eq!(e.to_string(), "2 of the batch's 3 events did not decode; first: why");
    assert_eq!(whole_batch_on_ack_channel(partial(), false).unwrap(), partial(), "no ack method");
    let whole = PolledBatch::whole(vec![]);
    assert_eq!(whole_batch_on_ack_channel(whole.clone(), true).unwrap(), whole, "nothing undecodable");
}

/// Pure: the line agrees in number, says what decoded without claiming it
/// reached the bus, and says nothing of the rest when nothing decoded.
#[test]
fn the_undecodable_events_line_counts_both_sides() {
    let lost = |lost: usize, of: usize| {
        let part = LostPart::Events { lost: NonZeroUsize::new(lost).unwrap(), of };
        format_undecodable_report("why", Fate::Lost { part, retry_in: None, recorded: true })
    };
    assert!(lost(1, 2).starts_with(
        "1 of the 2 events in a poll result did not decode (first: why), so it is lost: this \
         channel does not redeliver. The other one decoded. The worker sent output"
    ));
    assert!(lost(2, 5).contains("so they are lost: this channel does not redeliver. The other 3 decoded."));
    assert!(lost(3, 3).contains("does not redeliver. The worker sent output"), "{}", lost(3, 3));
}
