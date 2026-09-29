//! The pure half of #782's per-conversation reply queues: [`ReplyQueues`],
//! each queue's refusal transitions, the give-up bound and the lines. The
//! clock is passed in, so every bound is tested at its edge, with no sleeps.
//! The driver loop that calls these is in `replies_driver`.
use super::*;
use crate::channel::polled_driver::outage::OutageLog;
use crate::channel::polled_driver::refusal::{format_refusal_report, report_refusal, RefusalLine, Refused};
use crate::channel::polled_driver::replies::{
    check_reply_give_up, flush, format_exit_report, format_gave_up_report, format_overflow_report,
    give_up_due, OnRefused, ReplyQueues, MAX_QUEUED_PER_CONVERSATION,
};
use crate::worker_lifecycle::RestartBackoff;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// 100 ms, doubling, capped at 1 s — as in `super::refusal`.
pub(super) const FAST: RestartBackoff = RestartBackoff {
    base: Duration::from_millis(100),
    factor_num: 2,
    factor_den: 1,
    cap: Duration::from_secs(1),
};

/// A reply to `conversation` saying `body`.
///
/// The peer is the body too. The audit hook is never shown a reply's body
/// (#790), so a test that must tell two audited replies apart — often two in
/// one conversation — reads the peer instead.
pub(super) fn reply(conversation: &str, body: &str) -> OutgoingMessage {
    OutgoingMessage {
        channel: ChannelId("t".into()),
        peer: PeerId(body.into()),
        conversation: ConversationId(conversation.into()),
        body: body.into(),
    }
}

fn queues(replies: &[(&str, &str)]) -> ReplyQueues {
    let mut q = ReplyQueues::new(8);
    for (c, b) in replies {
        q.push(reply(c, b)).expect("under the cap");
    }
    q
}

const CODE: i32 = codes::OPERATION_FAILED;

/// The reply a refusal gave up, or a panic naming what happened instead.
fn gave_up(outcome: OnRefused) -> (String, u32, Duration) {
    match outcome {
        OnRefused::GaveUp { reply, refusals, refusing_for } => (reply.body, refusals, refusing_for),
        OnRefused::Retry(r) => panic!("expected a give-up, got a retry: {r:?}"),
    }
}

fn is_retry(outcome: &OnRefused) -> bool {
    matches!(outcome, OnRefused::Retry(_))
}

// ----- the queues -----

#[test]
fn replies_are_grouped_by_conversation_and_keep_their_order() {
    let mut q = queues(&[("!a", "a1"), ("!b", "b1"), ("!a", "a2")]);
    assert_eq!(q.conversations(), 2);
    assert_eq!(q.len(), 3);
    let drained: Vec<(String, Vec<String>)> = q
        .drain()
        .into_iter()
        .map(|(c, r)| (c.0, r.into_iter().map(|o| o.body).collect()))
        .collect();
    assert_eq!(
        drained,
        [("!a".to_string(), vec!["a1".to_string(), "a2".to_string()]), ("!b".into(), vec!["b1".into()])],
        "each conversation in its own order, conversations in the order they began"
    );
    assert_eq!(q.len(), 0, "drain empties the queues");
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

/// The production values, literally: every other test reads the constants,
/// so a changed value would pass them all.
#[test]
fn the_production_bounds_are_an_hour_three_refusals_and_256_replies() {
    assert_eq!(REPLY_GIVE_UP, ReplyGiveUp { after: Duration::from_secs(3600), min_refusals: 3 });
    assert_eq!(MAX_QUEUED_PER_CONVERSATION, 256);
}

// ----- the give-up bound -----

#[test]
fn giving_up_needs_both_the_time_and_the_count() {
    let g = ReplyGiveUp { after: Duration::from_secs(60), min_refusals: 3 };
    let t0 = Instant::now();
    let late = t0 + Duration::from_secs(60);
    assert!(!give_up_due(2, t0, late, &g), "long enough, but too few refusals of this reply");
    assert!(!give_up_due(3, t0, late - Duration::from_millis(1), &g), "enough refusals, too soon");
    assert!(give_up_due(3, t0, late, &g), "both halves met, exactly");
    assert!(give_up_due(9, t0, late + Duration::from_secs(1), &g));
}

/// The count includes the deciding refusal, so 1 (like 0) would drop a reply
/// at its first refusal — the bound `after: 0, min_refusals: 1` really does.
#[test]
fn a_give_up_bound_that_allows_no_retry_of_its_own_is_rejected() {
    check_reply_give_up(&REPLY_GIVE_UP).expect("production");
    check_reply_give_up(&ReplyGiveUp { after: Duration::ZERO, min_refusals: 2 }).expect("one retry");
    for min_refusals in [0, 1] {
        let bad = ReplyGiveUp { min_refusals, ..REPLY_GIVE_UP };
        assert!(check_reply_give_up(&bad).is_err(), "min_refusals {min_refusals}");
    }
}

#[test]
fn spawn_refuses_a_give_up_bound_that_allows_no_retry() {
    let (st, calls) = fake();
    let bad = PolledWorkerSpec {
        reply_give_up: ReplyGiveUp { after: Duration::ZERO, min_refusals: 1 },
        ..TEST_SPEC
    };
    let res = PolledWorkerDriver::spawn(
        bad, calls, test_parse, test_encode, None, None, DriverAudit::none(), ChannelId("t".into()),
    );
    assert!(res.is_err());
    assert_eq!(st.init_calls.load(Ordering::SeqCst), 0, "refused before calling the worker");
}

#[test]
fn a_run_remembers_when_it_began_until_it_is_accepted_or_its_clock_restarts() {
    let mut run = RefusalRun::default();
    let t0 = Instant::now();
    assert_eq!(run.since(), None, "no run yet");
    let _ = run.on_refusal(t0, &FAST, CODE);
    let _ = run.on_refusal(t0 + Duration::from_secs(5), &FAST, CODE);
    assert_eq!(run.since(), Some(t0), "the FIRST refusal of the run, not the latest");
    run.restart_clock();
    assert_eq!(run.since(), None, "a restarted clock waits for the next refusal");
    let t1 = t0 + Duration::from_secs(9);
    let r = run.on_refusal(t1, &FAST, CODE);
    assert_eq!(run.since(), Some(t1));
    assert_eq!(r.delay, Duration::from_millis(400), "the backoff was kept: third refusal, not first");
    let _ = run.on_accepted();
    assert_eq!(run.since(), None, "acceptance ends the run");
}

// ----- one conversation's transitions -----

/// Past `after`, the count still holds the reply until its own `min_refusals`.
#[test]
fn the_count_half_holds_a_reply_the_time_half_alone_would_drop() {
    let g = ReplyGiveUp { after: Duration::from_secs(60), min_refusals: 3 };
    let mut qs = queues(&[("!a", "a1")]);
    let q = qs.queue_mut("!a").unwrap();
    let t0 = Instant::now();
    let late = t0 + Duration::from_secs(120);
    assert!(is_retry(&q.on_front_refused(t0, &g, &FAST, CODE)), "1 of 3");
    assert!(is_retry(&q.on_front_refused(late, &g, &FAST, CODE)), "2 of 3, though an hour is up");
    let (body, refusals, refusing_for) = gave_up(q.on_front_refused(late, &g, &FAST, CODE));
    assert_eq!((body.as_str(), refusals, refusing_for), ("a1", 3, Duration::from_secs(120)));
}

/// A reply queued behind a given-up one starts its OWN count, but inherits
/// the conversation's clock: its give-up needs its own two refusals, not
/// another `after`.
#[test]
fn a_reply_behind_a_given_up_one_counts_afresh_but_inherits_the_clock() {
    let g = ReplyGiveUp { after: Duration::from_secs(60), min_refusals: 2 };
    let mut qs = queues(&[("!a", "a1"), ("!a", "a2")]);
    let q = qs.queue_mut("!a").unwrap();
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_secs(61);
    assert!(is_retry(&q.on_front_refused(t0, &g, &FAST, CODE)));
    assert_eq!(gave_up(q.on_front_refused(t1, &g, &FAST, CODE)).0, "a1");
    assert!(
        is_retry(&q.on_front_refused(t1, &g, &FAST, CODE)),
        "a2's FIRST refusal: a count carried over from a1 would drop it here"
    );
    let (body, refusals, refusing_for) =
        gave_up(q.on_front_refused(t1 + Duration::from_secs(1), &g, &FAST, CODE));
    assert_eq!((body.as_str(), refusals), ("a2", 2));
    assert_eq!(refusing_for, Duration::from_secs(62), "the conversation's clock, not a fresh one");
}

/// #782's review finding: a failure of the whole channel is no evidence
/// against a conversation, so it restarts the give-up clock. Without the
/// restart, `a1` — refused once before a two-minute outage — would be given up
/// at its first refusal after it.
#[test]
fn a_channel_wide_failure_restarts_every_conversations_give_up_clock() {
    let g = ReplyGiveUp { after: Duration::from_secs(60), min_refusals: 2 };
    let mut qs = queues(&[("!a", "a1"), ("!b", "b1")]);
    let t0 = Instant::now();
    assert!(is_retry(&qs.queue_mut("!a").unwrap().on_front_refused(t0, &g, &FAST, CODE)));
    assert!(is_retry(&qs.queue_mut("!b").unwrap().on_front_refused(t0, &g, &FAST, CODE)));
    qs.restart_give_up_clocks();
    let after_outage = t0 + Duration::from_secs(120);
    for c in ["!a", "!b"] {
        let q = qs.queue_mut(c).unwrap();
        assert!(is_retry(&q.on_front_refused(after_outage, &g, &FAST, CODE)), "{c}: a fresh hour");
    }
    let q = qs.queue_mut("!a").unwrap();
    let (_, refusals, refusing_for) =
        gave_up(q.on_front_refused(after_outage + Duration::from_secs(60), &g, &FAST, CODE));
    assert_eq!(refusals, 3, "the count was kept through the restart");
    assert_eq!(refusing_for, Duration::from_secs(60), "measured from the restart");
}

/// `flush` forgets a conversation whose queue emptied, and its refusal state
/// with it.
#[test]
fn a_conversation_whose_queue_empties_is_forgotten() {
    let (st, calls) = fake();
    let mut qs = queues(&[("!a", "a1"), ("!b", "b1")]);
    let down = flush(&mut qs, &*calls, &TEST_SPEC, test_encode, &mut OutageLog::default(), None);
    assert!(!down);
    assert_eq!(st.sends.lock().unwrap().len(), 2);
    assert_eq!(qs.conversations(), 0, "both queues emptied, so both conversations are forgotten");
}

/// The flush-level half of the clock restart: a worker that goes down between
/// two refusals restarts the conversation's clock, so the second refusal does
/// not give the reply up. Without the restart it would (the 120 ms since the
/// first refusal is past `after`, and the count is 2).
#[test]
fn a_dead_worker_restarts_the_clock_between_two_refusals() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec {
        refusal_backoff: FAST,
        reply_give_up: ReplyGiveUp { after: Duration::from_millis(50), min_refusals: 2 },
        ..TEST_SPEC
    };
    let audited: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = audited.clone();
    let audit: crate::channel::polled_driver::ReplyUndeliveredAudit =
        Box::new(move |r| sink.lock().unwrap().push(r.peer.0.clone()));
    let mut qs = queues(&[("!a", "a1")]);
    let mut outage = OutageLog::default();
    let mut flush_once = |qs: &mut ReplyQueues| flush(qs, &*calls, &spec, test_encode, &mut outage, Some(&audit));
    assert!(!flush_once(&mut qs), "refused once");
    std::thread::sleep(Duration::from_millis(120)); // past the 100 ms backoff and the 50 ms bound
    st.down.store(true, Ordering::SeqCst);
    assert!(flush_once(&mut qs), "the worker is down");
    st.down.store(false, Ordering::SeqCst);
    assert!(!flush_once(&mut qs), "refused twice");
    assert!(audited.lock().unwrap().is_empty(), "the outage restarted the clock: nothing is given up");
    assert_eq!(qs.len(), 1);
}

// ----- the lines -----

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
fn the_drop_lines_name_the_conversation_and_say_whether_a_row_is_written() {
    let gave_up =
        format_gave_up_report("matrix.send", "!gone:srv", 3, Duration::from_secs(3600), "forbidden", true);
    for needle in ["!gone:srv", "3 refusals", "3600 s", "recording it as channel.reply_undelivered", "forbidden"] {
        assert!(gave_up.contains(needle), "{needle:?} missing: {gave_up}");
    }
    let overflow = format_overflow_report("!gone:srv", 256, true);
    for needle in ["!gone:srv", "256 replies", "recording it as channel.reply_undelivered"] {
        assert!(overflow.contains(needle), "{needle:?} missing: {overflow}");
    }
    let exit = format_exit_report("!gone:srv", 2, true);
    for needle in ["discarded 2 queued replies", "!gone:srv", "recording each as channel.reply_undelivered"] {
        assert!(exit.contains(needle), "{needle:?} missing: {exit}");
    }
    assert!(format_exit_report("!a", 1, true).contains("discarded 1 queued reply to"));
    // With no sink, no line may claim a row: the review found every drop line
    // saying "recorded" for the CLI probe and the email channel, which have none.
    for line in [
        format_gave_up_report("m", "!a", 3, Duration::ZERO, "x", false),
        format_overflow_report("!a", 256, false),
        format_exit_report("!a", 2, false),
    ] {
        assert!(line.contains("NOT recorded"), "{line}");
        assert!(!line.contains("recording"), "{line}");
    }
}

/// #783's wiring: a refusal line goes out through the marked emitter, which
/// folds in the channel label. Read from the emitter's test record, not from
/// a scoped `tracing` subscriber: that version failed 6 runs in 30 (see
/// `worker_stderr::report::refusal::EMITTED`).
#[test]
fn a_refusal_line_goes_through_the_marked_emitter() {
    let e = anyhow::Error::from(RpcError::new(codes::OPERATION_FAILED, "send failed: nope"));
    let due = Refused { delay: Duration::from_secs(1), report: true };
    report_refusal("t-marked", "t.send", Some("!a"), &e, &due);
    let credential = anyhow::Error::from(RpcError::new(codes::UPSTREAM_AUTH_FAILED, "401"));
    report_refusal("t-marked", "t.poll", None, &credential, &due);
    let not_due = Refused { report: false, ..due };
    report_refusal("t-marked", "t.send", Some("!a"), &e, &not_due);
    let lines = emitted_refusal_lines_for("t-marked");
    assert_eq!(lines.len(), 2, "a line not due is not emitted: {lines:?}");
    assert!(
        lines[0].0.starts_with("channel worker t-marked: the worker refused t.send for conversation !a"),
        "{lines:?}"
    );
    assert_eq!(lines[0].1, RefusalSeverity::Warn);
    assert!(lines[1].0.starts_with("channel worker t-marked: operator action needed"), "{lines:?}");
    assert_eq!(lines[1].1, RefusalSeverity::Error, "a refused credential is an ERROR");
}
