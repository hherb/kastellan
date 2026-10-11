//! #832: the per-channel pump's half of a reply — `deliver`, which claims it
//! where it is sent. A child of the claim tests so it shares their `Backlog`
//! fake and `SinkChannel`.

use super::*;

fn reply(task_id: i64, via: Via) -> Queued {
    Queued::Reply {
        task_id,
        via,
        out: OutgoingMessage {
            channel: matrix(),
            peer: PeerId("@me:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "done".into(),
        },
    }
}

fn sent_bodies(sink: &SinkChannel) -> Vec<String> {
    sink.sent.lock().unwrap().iter().map(|m| m.body.clone()).collect()
}

/// The outbound pump queues a reply and claims nothing; the claim is the
/// per-channel pump's, right before the send. So a reply that never reaches
/// `deliver` — a stop, a flap, a queue that closed — is still in the backlog.
#[tokio::test]
async fn a_queued_reply_is_claimed_only_when_delivered() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let (senders, mut rx) = matrix_sender();

    let queued = handle_completed(&backlog, &ev, &senders, 7, Via::Notify).await.expect("queued");
    assert_eq!(backlog.disposition(7), None, "queued unclaimed");
    assert!(actions_of(&ev).is_empty(), "no row until it is delivered: {:?}", actions_of(&ev));

    let item = rx.try_recv().expect("in the queue");
    assert_eq!(item, queued);
    let sink = SinkChannel::default();
    assert_eq!(deliver(&backlog, &ev, &sink, &matrix(), item).await, Delivery::Sent);

    assert_eq!(backlog.disposition(7), Some(ReplyDisposition::Routed));
    assert_eq!(sent_bodies(&sink), ["done"]);
    assert_eq!(actions_of(&ev), [actions::REPLIED]);
    assert!(ev.lost.lock().unwrap().is_empty(), "a delivery that ends says nothing");
}

/// NOTIFY and a sweep tick can both queue the same task while its queue is
/// backed up. The claim decides: the second copy is not sent and writes
/// nothing.
#[tokio::test]
async fn the_same_task_queued_twice_is_sent_once() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let sink = SinkChannel::default();

    assert_eq!(deliver(&backlog, &ev, &sink, &matrix(), reply(7, Via::Notify)).await, Delivery::Sent);
    assert_eq!(deliver(&backlog, &ev, &sink, &matrix(), reply(7, Via::CatchUp)).await, Delivery::NotClaimed);

    assert_eq!(sent_bodies(&sink), ["done"]);
    assert_eq!(actions_of(&ev), [actions::REPLIED]);
    assert!(ev.lost.lock().unwrap().is_empty());
}

/// A raised ask has no task to claim: `ask.delivered` was its row. It goes
/// straight to the transport.
#[tokio::test]
async fn an_ask_is_sent_without_a_claim() {
    let (backlog, _n) = Backlog::new(vec![]);
    let ev = FakeEvents::default();
    let sink = SinkChannel::default();
    let Queued::Reply { out, .. } = reply(7, Via::Notify) else { unreachable!() };

    assert_eq!(deliver(&backlog, &ev, &sink, &matrix(), Queued::Ask(out)).await, Delivery::Sent);

    assert_eq!(backlog.claim_calls.load(Ordering::SeqCst), 0);
    assert_eq!(sent_bodies(&sink), ["done"]);
    assert!(actions_of(&ev).is_empty(), "{:?}", actions_of(&ev));
}

/// A database that answers `SELECT` but refuses the claim's `UPDATE` used to
/// fail every sweep, which the outbound pump's streak made loud. The claim
/// now happens here, so `deliver` says so, for the per-channel pump's own
/// streak; nothing is sent, and the reply stays in the backlog.
#[tokio::test]
async fn a_refused_claim_is_reported_for_the_pump_streak() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let sink = SinkChannel::default();
    let mut streak = crate::channel::catch_up::Streak::default();

    let mut loud = None;
    for _ in 0..crate::channel::catch_up::SWEEP_FAILURES_LOUD {
        backlog.claim_fails_once.lock().unwrap().insert(7);
        let d = deliver(&backlog, &ev, &sink, &matrix(), reply(7, Via::CatchUp)).await;
        assert_eq!(d, Delivery::ClaimFailed);
        loud = streak.record(d == Delivery::ClaimFailed);
    }

    assert_eq!(loud, Some(crate::channel::catch_up::SWEEP_FAILURES_LOUD));
    assert!(sent_bodies(&sink).is_empty());
    assert_eq!(backlog.disposition(7), None, "left for catch-up");
    assert!(ev.lost.lock().unwrap().is_empty(), "the guard settled each time");
}

/// A claim that errored after committing is recorded (`claim_unknown`) and
/// not sent — and it is a failed claim all the same: a link that commits
/// and then drops the connection turns every reply into "claimed, not sent",
/// so it must feed the pump's streak like any other refused claim.
#[tokio::test]
async fn a_claim_that_committed_then_failed_counts_as_a_failed_claim() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    backlog.claim_commits_then_fails.lock().unwrap().insert(7);
    let ev = FakeEvents::default();
    let sink = SinkChannel::default();

    let d = deliver(&backlog, &ev, &sink, &matrix(), reply(7, Via::Notify)).await;

    assert!(d.is_claim_failure(), "{d:?}");
    assert!(sent_bodies(&sink).is_empty());
    assert_eq!(actions_of(&ev), [actions::REPLY_UNDELIVERED]);
    assert!(!Delivery::Sent.is_claim_failure() && !Delivery::NotClaimed.is_claim_failure());
    assert!(Delivery::ClaimFailed.is_claim_failure());
}
