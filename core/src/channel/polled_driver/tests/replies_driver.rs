//! #782 through the real `run` loop, against the scripted fake in `super`,
//! with millisecond-scale bounds: one conversation does not hold another, a
//! refused reply is given up and audited, a channel-wide failure gives nothing
//! up, the queue is capped, and an exiting driver accounts for what it drops.
//!
//! Each test that reads the emitted lines uses its own channel label, because
//! the emitter's test record is shared by every test in the binary.
use super::replies::{reply, FAST};
use super::*;
use crate::channel::polled_driver::replies::MAX_QUEUED_PER_CONVERSATION;
use crate::channel::{UndeliveredReason, UndeliveredReply};
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// Every audited reply's identity and reason, in order. The identity is the
/// peer, which `replies::reply` sets to the body: the hook never sees a body
/// (#790).
type Audited = Arc<Mutex<Vec<(String, UndeliveredReason)>>>;

fn bodies(audited: &Audited) -> Vec<String> {
    audited.lock().unwrap().iter().map(|(b, _)| b.clone()).collect()
}

fn spawn_audited(spec: PolledWorkerSpec, calls: Box<dyn WorkerCalls>) -> (PolledWorkerDriver, Audited) {
    let audited: Audited = Arc::default();
    let sink = audited.clone();
    let audit = DriverAudit {
        reply_undelivered: Some(Box::new(move |r: UndeliveredReply<'_>| {
            sink.lock().unwrap().push((r.peer.0.clone(), r.reason));
        })),
        ..DriverAudit::none()
    };
    let driver = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, None, None, audit, ChannelId("t".into()),
    )
    .expect("driver spawn")
    .0;
    (driver, audited)
}

fn sent_bodies(st: &FakeState) -> Vec<String> {
    st.sends.lock().unwrap().iter().map(|v| v["body"].as_str().unwrap().to_string()).collect()
}

/// The conversation of every `t.send` call, accepted or refused, in order.
fn send_attempts(st: &FakeState) -> Vec<String> {
    st.log
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "t.send")
        .map(|(_, p)| p["conversation"].as_str().unwrap().to_string())
        .collect()
}

/// The emitted lines for `label` that contain `needle`.
fn lines_with(label: &str, needle: &str) -> Vec<(String, RefusalSeverity)> {
    emitted_refusal_lines_for(label).into_iter().filter(|(l, _)| l.contains(needle)).collect()
}

/// #782's headline: a room the upstream will not take holds only its own
/// replies. Before, `b1` and `b2` waited behind `a1` for ever. And the
/// refusal line says which room is stuck.
#[test]
fn a_refused_conversation_does_not_hold_another_conversations_replies() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec { label: "t-isolated", refusal_backoff: FAST, ..TEST_SPEC };
    let (driver, audited) = spawn_audited(spec, calls);
    for (c, b) in [("!a", "a1"), ("!b", "b1"), ("!a", "a2"), ("!b", "b2")] {
        driver.outbound_tx.send(reply(c, b)).unwrap();
    }
    wait_until(|| sent_bodies(&st).len() == 2);
    std::thread::sleep(Duration::from_millis(300)); // catch a late `a` delivery
    assert_eq!(sent_bodies(&st), ["b1", "b2"], "the other conversation flows, in its own order");
    assert!(audited.lock().unwrap().is_empty(), "an hour is not up: nothing is given up yet");
    let refused = lines_with("t-isolated", "refused t.send for conversation !a");
    assert!(!refused.is_empty(), "the refusal line names the stuck conversation");
    assert!(refused.iter().all(|(_, s)| *s == RefusalSeverity::Warn));
}

/// Past the bound the refused reply is dropped and audited, the next one in
/// the same conversation follows it (its own refusals counted afresh), and
/// the conversation works again once the upstream takes it.
#[test]
fn a_reply_refused_past_the_bound_is_given_up_and_audited_in_order() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec {
        label: "t-gave-up",
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::from_millis(200), min_refusals: 2 },
        ..TEST_SPEC
    };
    let (driver, audited) = spawn_audited(spec, calls);
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    driver.outbound_tx.send(reply("!a", "a2")).unwrap();
    wait_until(|| audited.lock().unwrap().len() == 2);
    assert_eq!(
        *audited.lock().unwrap(),
        [("a1".into(), UndeliveredReason::GaveUp), ("a2".into(), UndeliveredReason::GaveUp)],
        "given up in order, each exactly once"
    );
    assert!(send_attempts(&st).len() >= 4, "each reply was refused at least min_refusals (2) times");
    assert!(sent_bodies(&st).is_empty(), "nothing was accepted");
    // `a1` needs a third refusal to reach the 200 ms (its backoff is 100 then
    // 200 ms); `a2` inherits the clock and goes at its second.
    let lines = lines_with("t-gave-up", "gave up on a reply to conversation !a after");
    assert_eq!(lines.len(), 2, "one give-up line per reply: {lines:?}");
    assert!(lines.iter().all(|(l, _)| l.contains("recording it as channel.reply_undelivered")));

    *st.refuse_conversation.lock().unwrap() = None;
    driver.outbound_tx.send(reply("!a", "a3")).unwrap();
    wait_until(|| sent_bodies(&st) == ["a3"]);
    assert_eq!(audited.lock().unwrap().len(), 2, "a delivered reply is not audited");
}

/// With no audit sink, the give-up line must not claim a row was written.
#[test]
fn a_give_up_with_no_audit_sink_says_it_was_not_recorded() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec {
        label: "t-no-sink",
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::ZERO, min_refusals: 2 },
        ..TEST_SPEC
    };
    let driver = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, None, None, DriverAudit::none(), ChannelId("t".into()),
    )
    .expect("driver spawn")
    .0;
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    wait_until(|| !lines_with("t-no-sink", "gave up").is_empty());
    let line = &lines_with("t-no-sink", "gave up")[0].0;
    assert!(line.contains("NOT recorded as channel.reply_undelivered"), "{line}");
}

/// A failure of the whole channel — a refused credential, an unavailable
/// upstream — holds every conversation (no other conversation is tried while
/// it lasts), and gives nothing up, even under a bound that gives up an
/// ordinary refusal at its second one.
fn a_channel_wide_refusal_holds_every_conversation(code: i32, label: &'static str) {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 3));
    st.refuse_code.store(code, Ordering::SeqCst);
    let spec = PolledWorkerSpec {
        label,
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::ZERO, min_refusals: 2 },
        ..TEST_SPEC
    };
    let (driver, audited) = spawn_audited(spec, calls);
    let queued = Instant::now();
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    driver.outbound_tx.send(reply("!b", "b1")).unwrap();
    wait_until(|| sent_bodies(&st).len() == 2);
    let took = queued.elapsed();
    assert!(took >= FAST.base, "the hold waited out its backoff before retrying: {took:?}");
    assert!(audited.lock().unwrap().is_empty(), "a channel-wide refusal gives nothing up");
    assert_eq!(
        send_attempts(&st),
        ["!a", "!a", "!a", "!a", "!b"],
        "`!b` is not tried while the hold runs; then both go, in order"
    );
}

#[test]
fn a_credential_refusal_holds_every_conversation_and_gives_nothing_up() {
    a_channel_wide_refusal_holds_every_conversation(codes::UPSTREAM_AUTH_FAILED, "t-cred");
    let lines = lines_with("t-cred", "operator action needed");
    assert!(!lines.is_empty(), "the credential line is emitted");
    assert!(lines.iter().all(|(_, s)| *s == RefusalSeverity::Error), "at ERROR: {lines:?}");
}

/// #782 review: the Matrix worker now answers a dead homeserver with
/// `UPSTREAM_UNAVAILABLE`, and that must not age any room out either.
#[test]
fn an_unavailable_upstream_holds_every_conversation_and_gives_nothing_up() {
    a_channel_wide_refusal_holds_every_conversation(codes::UPSTREAM_UNAVAILABLE, "t-unavail");
    let lines = lines_with("t-unavail", "the worker refused t.send");
    assert!(!lines.is_empty());
    assert!(lines.iter().all(|(l, _)| !l.contains("for conversation")), "channel-wide: {lines:?}");
}

/// The control for the two tests above: the same bound DOES give up an
/// ordinary refusal, so the channel-wide cases are not passing because the
/// bound never fires.
#[test]
fn the_same_bound_gives_up_an_ordinary_refusal() {
    let (st, calls) = fake();
    // Three, as above: `a1` and `b1` are refused once each, then `a1` again —
    // its second refusal, which the bound gives up — and `b1` goes through.
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 3));
    let spec = PolledWorkerSpec {
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::ZERO, min_refusals: 2 },
        ..TEST_SPEC
    };
    let (driver, audited) = spawn_audited(spec, calls);
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    driver.outbound_tx.send(reply("!b", "b1")).unwrap();
    wait_until(|| sent_bodies(&st) == ["b1"] && !audited.lock().unwrap().is_empty());
    assert_eq!(bodies(&audited), ["a1"]);
}

/// A failing POLL is a channel-wide failure too: while every poll fails, a
/// refusing conversation's clock keeps restarting and nothing is given up.
/// Once polls succeed, the same bound fires (the positive control).
#[test]
fn a_failing_poll_keeps_a_refusing_conversation_from_being_given_up() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    *st.fail_method.lock().unwrap() = Some("t.poll".into());
    let spec = PolledWorkerSpec {
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::from_millis(300), min_refusals: 2 },
        ..TEST_SPEC
    };
    let (driver, audited) = spawn_audited(spec, calls);
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert!(send_attempts(&st).len() >= 3, "the reply kept being refused: {:?}", send_attempts(&st));
    assert!(audited.lock().unwrap().is_empty(), "every failed poll restarted the clock");
    *st.fail_method.lock().unwrap() = None;
    wait_until(|| bodies(&audited) == ["a1"]);
}

/// A stuck conversation cannot grow the queue without bound: the reply past
/// the cap is dropped and audited at once, and the queued ones keep waiting.
#[test]
fn a_reply_past_a_full_conversation_queue_is_dropped_and_audited() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec { label: "t-overflow", refusal_backoff: FAST, ..TEST_SPEC };
    let (driver, audited) = spawn_audited(spec, calls);
    for i in 1..=MAX_QUEUED_PER_CONVERSATION + 1 {
        driver.outbound_tx.send(reply("!a", &format!("a{i}"))).unwrap();
    }
    let last = format!("a{}", MAX_QUEUED_PER_CONVERSATION + 1);
    wait_until(|| audited.lock().unwrap().len() == 1);
    assert_eq!(*audited.lock().unwrap(), [(last, UndeliveredReason::QueueFull)]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(audited.lock().unwrap().len(), 1, "only the one past the cap");
    assert!(sent_bodies(&st).is_empty());
    let lines = lines_with("t-overflow", "dropped a new reply to conversation !a: 256 replies");
    assert_eq!(lines.len(), 1, "{lines:?}");
}

/// A driver that exits (the channel restarting or shutting down) drops what is
/// still queued — and says so, per conversation, and audits each reply. Before
/// the review they vanished with no row, after `channel.replied` had already
/// been written for each.
#[test]
fn an_exiting_driver_accounts_for_every_reply_it_drops() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec { label: "t-exit", refusal_backoff: FAST, ..TEST_SPEC };
    let (driver, audited) = spawn_audited(spec, calls);
    for (c, b) in [("!a", "a1"), ("!a", "a2"), ("!b", "b1")] {
        driver.outbound_tx.send(reply(c, b)).unwrap();
    }
    wait_until(|| sent_bodies(&st) == ["b1"]);
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    drop(outbound_tx);
    drop(inbound_rx);
    join.join().expect("the driver exits cleanly");
    assert_eq!(
        *audited.lock().unwrap(),
        [("a1".into(), UndeliveredReason::DriverExit), ("a2".into(), UndeliveredReason::DriverExit)],
        "the two refused replies, and not the delivered one"
    );
    let lines = lines_with("t-exit", "discarded 2 queued replies to conversation !a");
    assert_eq!(lines.len(), 1, "{:?}", emitted_refusal_lines_for("t-exit"));
}
