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

/// The completed-task pump queues a reply and claims nothing; the claim is the
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

    assert_eq!(deliver(&backlog, &ev, &sink, &matrix(), Queued::Ask { task_id: 5, out }).await, Delivery::AskSent);

    assert_eq!(backlog.claim_calls.load(Ordering::SeqCst), 0);
    assert_eq!(sent_bodies(&sink), ["done"]);
    assert!(actions_of(&ev).is_empty(), "{:?}", actions_of(&ev));
}

/// A database that answers `SELECT` but refuses the claim's `UPDATE` used to
/// fail every sweep, which the completed-task pump's streak made loud. The
/// claim now happens here, so `deliver` says so and the pump's own step
/// (`note_claim`) counts it; nothing is sent, and the reply stays in the
/// backlog.
#[tokio::test]
async fn a_refused_claim_feeds_the_pump_claim_streak() {
    use crate::channel::catch_up::{ClaimStreak, CLAIM_FAILURES_LOUD, CLAIM_FAILURES_LOUD_AFTER};
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let sink = SinkChannel::default();
    let mut streak = ClaimStreak::default();
    let t0 = std::time::Instant::now();

    // One failure per `CLAIM_FAILURES_LOUD_AFTER`, so the run is old enough
    // by the time it is long enough.
    let mut loud = None;
    for i in 0..CLAIM_FAILURES_LOUD {
        backlog.claim_fails_once.lock().unwrap().insert(7);
        let d = deliver(&backlog, &ev, &sink, &matrix(), reply(7, Via::CatchUp)).await;
        assert_eq!(d, Delivery::ClaimFailed);
        loud = note_claim(&mut streak, d, t0 + CLAIM_FAILURES_LOUD_AFTER * i);
    }

    assert_eq!(loud, Some(CLAIM_FAILURES_LOUD));
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

    assert_eq!(d.claim_failed(), Some(true), "{d:?}");
    assert!(sent_bodies(&sink).is_empty());
    assert_eq!(actions_of(&ev), [actions::REPLY_UNDELIVERED]);
    assert!(ev.lost.lock().unwrap().is_empty(), "recorded, so not said");
    assert_eq!(Delivery::Sent.claim_failed(), Some(false));
    assert_eq!(Delivery::NotClaimed.claim_failed(), Some(false), "the claim worked: someone else won");
    assert_eq!(Delivery::ClaimFailed.claim_failed(), Some(true));
    assert_eq!(Delivery::AskSent.claim_failed(), None);
}

/// A claim that errored, and a re-read that failed too: the claim may have
/// committed, leaving the task `routed` with nothing sent and no row, and no
/// sweep would find it — so it is said on `[audit-lost]`, not left to a WARN.
#[tokio::test]
async fn a_claim_whose_outcome_cannot_be_known_is_said() {
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("SECRET-REPLY-BODY"))]);
    backlog.claim_commits_then_fails.lock().unwrap().insert(7);
    backlog.settled_fails.store(true, Ordering::SeqCst);
    let ev = FakeEvents::default();
    let sink = SinkChannel::default();
    let Queued::Reply { out, .. } = reply(7, Via::Notify) else { unreachable!() };
    let out = OutgoingMessage { body: "SECRET-REPLY-BODY".into(), ..out };

    let d = deliver(&backlog, &ev, &sink, &matrix(), Queued::Reply { task_id: 7, via: Via::Notify, out }).await;

    assert_eq!(d, Delivery::ClaimFailed, "a failed claim, for the streak");
    assert!(sent_bodies(&sink).is_empty(), "not sent: it may be settled");
    assert!(actions_of(&ev).is_empty(), "no row: nothing is known: {:?}", actions_of(&ev));
    assert_eq!(
        *ev.lost.lock().unwrap(),
        [r#"task 7's reply for channel "matrix", peer "@me:srv" may have been settled without being sent: its claim failed, and so did the re-read that would have told"#]
    );
}

/// An ask makes no claim, so it says nothing about the claim path: between
/// failed claims it must neither reset the run (hiding a refusing database
/// from the ERROR) nor count toward it.
#[tokio::test]
async fn an_ask_neither_resets_nor_feeds_the_claim_streak() {
    use crate::channel::catch_up::{ClaimStreak, CLAIM_FAILURES_LOUD_AFTER};
    let (backlog, _n) = Backlog::new(vec![(7, channel_row("done"))]);
    let ev = FakeEvents::default();
    let sink = SinkChannel::default();
    let mut streak = ClaimStreak::default();
    let t0 = std::time::Instant::now();
    let Queued::Reply { out, .. } = reply(7, Via::Notify) else { unreachable!() };

    let mut said = Vec::new();
    for i in 0..3 {
        let at = t0 + CLAIM_FAILURES_LOUD_AFTER * i;
        backlog.claim_fails_once.lock().unwrap().insert(7);
        let d = deliver(&backlog, &ev, &sink, &matrix(), reply(7, Via::CatchUp)).await;
        said.push(note_claim(&mut streak, d, at));
        let d = deliver(&backlog, &ev, &sink, &matrix(), Queued::Ask { task_id: 5, out: out.clone() }).await;
        assert_eq!(d, Delivery::AskSent);
        said.push(note_claim(&mut streak, d, at));
    }

    assert_eq!(said, [None, None, None, None, Some(3), None], "the third failed claim is said, asks between");
}
