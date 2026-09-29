//! Replies are queued per conversation, and one the worker keeps refusing is
//! given up and audited (#782). Refusal lines carry the `[worker-refusal]`
//! marker's emitter (#783).
//!
//! The pure half ([`ReplyQueues`], [`give_up_due`], the line formatters) is
//! tested directly; the driver half runs the real `run` loop against the
//! scripted fake in `super`, with millisecond-scale bounds.
use super::*;
use crate::channel::polled_driver::refusal::{
    format_refusal_report, report_refusal, RefusalLine, RefusalRun, Refused,
};
use crate::channel::polled_driver::replies::{
    check_reply_give_up, format_gave_up_report, format_overflow_report, give_up_due, ReplyQueues,
    MAX_QUEUED_PER_CONVERSATION,
};
use crate::worker_lifecycle::RestartBackoff;

/// 100 ms, doubling, capped at 1 s — as in `super::refusal`.
const FAST: RestartBackoff = RestartBackoff {
    base: Duration::from_millis(100),
    factor_num: 2,
    factor_den: 1,
    cap: Duration::from_secs(1),
};

fn reply(conversation: &str, body: &str) -> OutgoingMessage {
    OutgoingMessage {
        channel: ChannelId("t".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId(conversation.into()),
        body: body.into(),
    }
}

// ----- pure: the queues -----

#[test]
fn replies_are_grouped_by_conversation_and_keep_their_order() {
    let mut q = ReplyQueues::new(8);
    for (c, b) in [("!a", "a1"), ("!b", "b1"), ("!a", "a2")] {
        q.push(reply(c, b)).expect("under the cap");
    }
    assert_eq!(q.conversations(), 2);
    assert_eq!(q.len(), 3);
}

#[test]
fn a_full_conversation_hands_the_new_reply_back_and_leaves_the_others_alone() {
    let mut q = ReplyQueues::new(2);
    q.push(reply("!a", "a1")).unwrap();
    q.push(reply("!a", "a2")).unwrap();
    let back = q.push(reply("!a", "a3")).expect_err("the cap is 2");
    assert_eq!(back.body, "a3", "the NEW reply is the one refused; the queued ones keep their place");
    q.push(reply("!b", "b1")).expect("another conversation has its own cap");
    assert_eq!(q.len(), 3);
}

// ----- pure: the give-up bound -----

#[test]
fn giving_up_needs_both_the_time_and_the_count() {
    let g = ReplyGiveUp { after: Duration::from_secs(60), min_refusals: 3 };
    let t0 = Instant::now();
    let late = t0 + Duration::from_secs(60);
    assert!(!give_up_due(2, t0, late, &g), "long enough, but too few refusals (a host that slept)");
    assert!(!give_up_due(3, t0, late - Duration::from_millis(1), &g), "enough refusals, too soon");
    assert!(give_up_due(3, t0, late, &g), "both halves met, exactly");
    assert!(give_up_due(9, t0, late + Duration::from_secs(1), &g));
}

#[test]
fn a_give_up_bound_that_drops_at_the_first_refusal_is_rejected() {
    check_reply_give_up(&REPLY_GIVE_UP).expect("production");
    let bad = ReplyGiveUp { min_refusals: 0, ..REPLY_GIVE_UP };
    assert!(check_reply_give_up(&bad).is_err());
}

#[test]
fn spawn_refuses_a_give_up_bound_of_zero_refusals() {
    let (st, calls) = fake();
    let bad = PolledWorkerSpec {
        reply_give_up: ReplyGiveUp { after: Duration::ZERO, min_refusals: 0 },
        ..TEST_SPEC
    };
    let res = PolledWorkerDriver::spawn(
        bad, calls, test_parse, test_encode, None, None, DriverAudit::default(), ChannelId("t".into()),
    );
    assert!(res.is_err());
    assert_eq!(st.init_calls.load(Ordering::SeqCst), 0, "refused before calling the worker");
}

#[test]
fn a_run_remembers_when_it_began_until_it_is_accepted() {
    let mut run = RefusalRun::default();
    let t0 = Instant::now();
    assert_eq!(run.since(), None, "no run yet");
    let _ = run.on_refusal(t0, &FAST, codes::OPERATION_FAILED);
    let _ = run.on_refusal(t0 + Duration::from_secs(5), &FAST, codes::OPERATION_FAILED);
    assert_eq!(run.since(), Some(t0), "the FIRST refusal of the run, not the latest");
    let _ = run.on_accepted();
    assert_eq!(run.since(), None, "acceptance ends the run");
}

// ----- pure: the lines -----

#[test]
fn a_send_refusal_line_names_the_stuck_conversation() {
    // #782 option 3: the WARN says where the queue is stuck.
    let line = format_refusal_report(
        &RefusalLine::Warn,
        "matrix.send",
        Some("!room:srv"),
        "send failed: forbidden",
        Duration::from_millis(4000),
    )
    .expect("a due line");
    assert!(line.contains("matrix.send for conversation !room:srv"), "{line}");
    assert!(line.contains("send failed: forbidden"), "{line}");
    assert!(line.contains("retrying in 4000 ms"), "{line}");
    assert!(line.contains("Repeated every 15 min"), "{line}");
}

#[test]
fn the_credential_line_names_the_operator_action_and_silent_is_no_line() {
    let line = format_refusal_report(
        &RefusalLine::CredentialError,
        "email.poll",
        None,
        "localmail rejected the credential",
        Duration::from_secs(1),
    )
    .expect("a due line");
    assert!(line.starts_with("operator action needed:"), "{line}");
    assert!(line.contains("email.poll was refused"), "{line}");
    assert!(line.contains("Repeated every 15 min"), "{line}");
    assert_eq!(
        format_refusal_report(&RefusalLine::Silent, "email.poll", None, "x", Duration::ZERO),
        None
    );
}

#[test]
fn the_drop_lines_name_the_conversation_and_the_audit_row() {
    let gave_up =
        format_gave_up_report("matrix.send", "!gone:srv", 3, Duration::from_secs(3600), "forbidden");
    for needle in ["!gone:srv", "3 refusals", "3600 s", "channel.reply_undelivered", "forbidden"] {
        assert!(gave_up.contains(needle), "{needle:?} missing: {gave_up}");
    }
    let overflow = format_overflow_report("!gone:srv", 256);
    for needle in ["!gone:srv", "256 replies", "channel.reply_undelivered"] {
        assert!(overflow.contains(needle), "{needle:?} missing: {overflow}");
    }
}

/// #783's wiring: a refusal line goes out through the marked emitter, which
/// folds in the channel label (`channel worker <label>:`). A driver that went
/// back to a bare `tracing::warn!` would lose the stderr fallback and this
/// prefix together.
#[test]
fn a_refusal_line_goes_through_the_marked_emitter() {
    use std::io::Write;
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
        .with_writer(sink.clone())
        .with_ansi(false)
        .finish();
    let e = anyhow::Error::from(RpcError::new(codes::OPERATION_FAILED, "send failed: nope"));
    let due = Refused { delay: Duration::from_secs(1), report: true };
    // ⚠️ `tracing` caches each callsite's interest process-wide, and the other
    // tests in this module hit this same callsite from their driver threads
    // with NO subscriber. A registration racing the scoped one below cached
    // "never" and this test failed 1 run in 5 (measured). So: finish the
    // callsite's registration here first (it falls back to stderr, harmlessly),
    // then rebuild the cache inside the scope, where it sees the sink.
    report_refusal("t", "t.send", Some("!a"), &e, &due);
    tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        report_refusal("t", "t.send", Some("!a"), &e, &due);
    });
    let out = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
    assert!(
        out.contains("channel worker t: the worker refused t.send for conversation !a"),
        "{out}"
    );
}

// ----- the driver loop -----

/// Every audited reply's body, in order.
type Audited = Arc<Mutex<Vec<String>>>;

fn spawn_audited(spec: PolledWorkerSpec, calls: Box<dyn WorkerCalls>) -> (PolledWorkerDriver, Audited) {
    let audited: Audited = Arc::default();
    let sink = audited.clone();
    let audit = DriverAudit {
        reply_undelivered: Some(Box::new(move |out: &OutgoingMessage| {
            sink.lock().unwrap().push(out.body.clone());
        })),
        ..DriverAudit::default()
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

/// #782's headline: a room the upstream will not take holds only its own
/// replies. Before, `b1` and `b2` waited behind `a1` for ever.
#[test]
fn a_refused_conversation_does_not_hold_another_conversations_replies() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let (driver, audited) = spawn_audited(PolledWorkerSpec { refusal_backoff: FAST, ..TEST_SPEC }, calls);
    for (c, b) in [("!a", "a1"), ("!b", "b1"), ("!a", "a2"), ("!b", "b2")] {
        driver.outbound_tx.send(reply(c, b)).unwrap();
    }
    wait_until(|| sent_bodies(&st).len() == 2);
    std::thread::sleep(Duration::from_millis(300)); // catch a late `a` delivery
    assert_eq!(sent_bodies(&st), ["b1", "b2"], "the other conversation flows, in its own order");
    assert!(audited.lock().unwrap().is_empty(), "an hour is not up: nothing is given up yet");
}

/// Past the bound the refused reply is dropped and audited, the next one in
/// the same conversation follows it (its own refusals counted afresh), and
/// the conversation works again once the upstream takes it.
#[test]
fn a_reply_refused_past_the_bound_is_given_up_and_audited_in_order() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec {
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::from_millis(200), min_refusals: 2 },
        ..TEST_SPEC
    };
    let (driver, audited) = spawn_audited(spec, calls);
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    driver.outbound_tx.send(reply("!a", "a2")).unwrap();
    wait_until(|| audited.lock().unwrap().len() == 2);
    assert_eq!(*audited.lock().unwrap(), ["a1", "a2"], "given up in order, each exactly once");
    let attempts = send_attempts(&st).len();
    assert!(attempts >= 4, "each reply was refused at least min_refusals (2) times: {attempts}");
    assert!(sent_bodies(&st).is_empty(), "nothing was accepted");

    *st.refuse_conversation.lock().unwrap() = None;
    driver.outbound_tx.send(reply("!a", "a3")).unwrap();
    wait_until(|| sent_bodies(&st) == ["a3"]);
    assert_eq!(audited.lock().unwrap().len(), 2, "a delivered reply is not audited");
}

/// A refused credential is the channel's problem: it holds every
/// conversation (no other conversation is tried while it lasts), and it never
/// counts toward a give-up — even under a bound that would drop an ordinary
/// refusal at once.
#[test]
fn a_credential_refusal_holds_every_conversation_and_gives_nothing_up() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 1));
    st.refuse_code.store(codes::UPSTREAM_AUTH_FAILED, Ordering::SeqCst);
    let spec = PolledWorkerSpec {
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::ZERO, min_refusals: 1 },
        ..TEST_SPEC
    };
    let (driver, audited) = spawn_audited(spec, calls);
    let queued = Instant::now();
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    driver.outbound_tx.send(reply("!b", "b1")).unwrap();
    wait_until(|| sent_bodies(&st).len() == 2);
    let took = queued.elapsed();
    assert!(took >= FAST.base, "the hold waited out its backoff before retrying: {took:?}");
    assert!(audited.lock().unwrap().is_empty(), "a credential refusal gives nothing up");
    assert_eq!(
        send_attempts(&st),
        ["!a", "!a", "!b"],
        "`!b` is not tried while the credential hold runs; then both go, in order"
    );
}

/// The control for the test above: the same bound DOES give up an ordinary
/// refusal at once, so the credential case is not passing because the bound
/// never fires.
#[test]
fn the_same_bound_gives_up_an_ordinary_refusal_at_once() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 1));
    let spec = PolledWorkerSpec {
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::ZERO, min_refusals: 1 },
        ..TEST_SPEC
    };
    let (driver, audited) = spawn_audited(spec, calls);
    driver.outbound_tx.send(reply("!a", "a1")).unwrap();
    driver.outbound_tx.send(reply("!b", "b1")).unwrap();
    wait_until(|| sent_bodies(&st) == ["b1"]);
    assert_eq!(*audited.lock().unwrap(), ["a1"]);
}

/// A stuck conversation cannot grow the queue without bound: the reply past
/// the cap is dropped and audited at once, and the queued ones keep waiting.
#[test]
fn a_reply_past_a_full_conversation_queue_is_dropped_and_audited() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let (driver, audited) = spawn_audited(PolledWorkerSpec { refusal_backoff: FAST, ..TEST_SPEC }, calls);
    for i in 1..=MAX_QUEUED_PER_CONVERSATION + 1 {
        driver.outbound_tx.send(reply("!a", &format!("a{i}"))).unwrap();
    }
    let last = format!("a{}", MAX_QUEUED_PER_CONVERSATION + 1);
    wait_until(|| audited.lock().unwrap().len() == 1);
    assert_eq!(*audited.lock().unwrap(), [last]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(audited.lock().unwrap().len(), 1, "only the one past the cap");
    assert!(sent_bodies(&st).is_empty());
}
