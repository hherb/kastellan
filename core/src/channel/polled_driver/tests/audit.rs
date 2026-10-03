//! #793: the driver times every audit hook call and says so when one holds its
//! thread. Driven through the real callers (`discard_on_exit`, `ack_skipped`,
//! `enqueue` and, for a give-up, the driver loop itself — #799), so a caller
//! that called its hook directly, untimed, fails here. Each test
//! reads the slow-hook record for its own channel label.
use super::*;
use crate::channel::polled_driver::audit::{format_slow_hook_report, slow_hook_reports_for, HOOK_BUDGET};
use crate::channel::polled_driver::outage::OutageLog;
use crate::channel::polled_driver::refusal::RefusalRun;
use crate::channel::polled_driver::replies::ReplyQueues;
use crate::channel::polled_driver::{ack, replies};
use crate::channel::{SkippedId, UndeliveredReply};

/// Long enough past [`HOOK_BUDGET`] to be unmistakable on a loaded host.
const SLOW: Duration = Duration::from_millis(250);

#[test]
fn a_hook_is_reported_only_past_its_budget() {
    assert_eq!(format_slow_hook_report("reply-undelivered", HOOK_BUDGET), None);
    let line = format_slow_hook_report("reply-undelivered", HOOK_BUDGET + Duration::from_millis(1))
        .expect("past the budget");
    assert_eq!(
        line,
        "the reply-undelivered audit hook held the driver thread for 101 ms; an audit hook must \
         not block (#789): every conversation, the poll and the ack wait on this thread"
    );
}

/// A reply hook that blocks is reported, once per call; a fast one is not.
#[test]
fn a_reply_hook_that_blocks_is_reported() {
    let slow: ReplyUndeliveredAudit = Box::new(|_: UndeliveredReply<'_>| std::thread::sleep(SLOW));
    let mut queues = ReplyQueues::new(MAX_QUEUED_PER_CONVERSATION);
    replies::discard_on_exit(&mut queues, [replies_reply("!a", "one")], "slow-reply-hook", Some(&slow));
    let lines = slow_hook_reports_for("slow-reply-hook");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("the reply-undelivered audit hook held the driver thread"), "{lines:?}");

    let fast: ReplyUndeliveredAudit = Box::new(|_: UndeliveredReply<'_>| {});
    replies::discard_on_exit(&mut queues, [replies_reply("!a", "two")], "fast-reply-hook", Some(&fast));
    assert!(slow_hook_reports_for("fast-reply-hook").is_empty(), "POSITIVE CONTROL: a fast hook is not");
}

/// A skipped-id hook that blocks is reported; the ack still goes out.
#[test]
fn a_skipped_id_hook_that_blocks_is_reported() {
    let (st, calls) = fake();
    let spec = PolledWorkerSpec { label: "slow-skip-hook", ..spec_with_ack() };
    let slow: AckOnlyAudit = Box::new(|_: SkippedId<'_>| std::thread::sleep(SLOW));
    ack::ack_skipped(
        &*calls,
        &spec,
        encode_test_ack,
        vec![("10".into(), "no usable From address".into())],
        Some(&slow),
        &ChannelId("email".into()),
        &mut OutageLog::default(),
        &mut RefusalRun::default(),
    );
    let lines = slow_hook_reports_for("slow-skip-hook");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("the skipped-id audit hook held the driver thread"), "{lines:?}");
    assert!(
        st.log.lock().unwrap().iter().any(|(m, p)| m == "email.ack" && p["cursor"] == "10"),
        "a slow hook is reported, not a reason to skip the ack"
    );
}

/// The view the skipped-id hook is handed: the driver's channel, the id and
/// the reason in their own fields (#793), and a time the driver took.
#[test]
fn the_skipped_id_hook_sees_named_fields_and_the_driver_s_channel() {
    let (_st, calls) = fake();
    let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let sink = seen.clone();
    let hook: AckOnlyAudit = Box::new(move |s: SkippedId<'_>| {
        sink.lock().unwrap().push((s.channel.0.clone(), s.message_id.into(), s.reason.into()));
    });
    let before = time::OffsetDateTime::now_utc();
    let stamped: Arc<Mutex<Option<time::OffsetDateTime>>> = Arc::default();
    let stamp = stamped.clone();
    let timed: AckOnlyAudit = Box::new(move |s: SkippedId<'_>| *stamp.lock().unwrap() = Some(s.observed_at));
    for audit in [&hook, &timed] {
        ack::ack_skipped(
            &*calls,
            &spec_with_ack(),
            encode_test_ack,
            vec![("10".into(), "no usable From address".into())],
            Some(audit),
            &ChannelId("email-bus".into()),
            &mut OutageLog::default(),
            &mut RefusalRun::default(),
        );
    }
    assert_eq!(
        *seen.lock().unwrap(),
        [("email-bus".to_string(), "10".to_string(), "no usable From address".to_string())]
    );
    let at = stamped.lock().unwrap().expect("stamped");
    assert!(at >= before && at <= time::OffsetDateTime::now_utc(), "the skip's own time: {at}");
}

/// A dropped reply is stamped with the time of the drop: its row is written
/// after the fact (#789), so `audit_log.ts` cannot say when it happened.
#[test]
fn a_dropped_reply_is_stamped_with_the_time_of_the_drop() {
    let stamped: Arc<Mutex<Option<time::OffsetDateTime>>> = Arc::default();
    let stamp = stamped.clone();
    let hook: ReplyUndeliveredAudit =
        Box::new(move |r: UndeliveredReply<'_>| *stamp.lock().unwrap() = Some(r.observed_at));
    let before = time::OffsetDateTime::now_utc();
    let mut queues = ReplyQueues::new(MAX_QUEUED_PER_CONVERSATION);
    replies::discard_on_exit(&mut queues, [replies_reply("!a", "one")], "stamp-reply-hook", Some(&hook));
    let at = stamped.lock().unwrap().expect("stamped");
    assert!(at >= before && at <= time::OffsetDateTime::now_utc(), "the drop's own time: {at}");
}

/// A reply to `conversation` (the replies tests' helper, peer = body).
fn replies_reply(conversation: &str, body: &str) -> OutgoingMessage {
    super::replies::reply(conversation, body)
}

/// #799: a reply dropped past a full conversation queue (`QueueFull`) goes
/// through the timed call too, not a direct, untimed one.
#[test]
fn a_queue_full_hook_that_blocks_is_reported() {
    let slow: ReplyUndeliveredAudit = Box::new(|_: UndeliveredReply<'_>| std::thread::sleep(SLOW));
    let mut queues = ReplyQueues::new(1);
    replies::enqueue(&mut queues, replies_reply("!a", "one"), "slow-queue-full-hook", Some(&slow));
    assert!(
        slow_hook_reports_for("slow-queue-full-hook").is_empty(),
        "POSITIVE CONTROL: a reply that fits its queue calls no hook"
    );
    replies::enqueue(&mut queues, replies_reply("!a", "two"), "slow-queue-full-hook", Some(&slow));
    let lines = slow_hook_reports_for("slow-queue-full-hook");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("the reply-undelivered audit hook held the driver thread"), "{lines:?}");
}

/// #799: a reply given up after its refusals (`GaveUp`) goes through the timed
/// call too. Through the real loop, because the give-up is decided inside it.
#[test]
fn a_gave_up_hook_that_blocks_is_reported() {
    let (st, calls) = fake();
    *st.refuse_conversation.lock().unwrap() = Some("!a".into());
    let spec = PolledWorkerSpec {
        label: "slow-gave-up-hook",
        refusal_backoff: super::replies::FAST,
        reply_give_up: ReplyGiveUp { after: Duration::from_millis(200), min_refusals: 2 },
        ..TEST_SPEC
    };
    let slow: ReplyUndeliveredAudit = Box::new(|_: UndeliveredReply<'_>| std::thread::sleep(SLOW));
    let audit = DriverAudit { reply_undelivered: Some(slow), ..DriverAudit::none() };
    let (driver, _identity) = PolledWorkerDriver::spawn(
        spec, calls, test_parse, test_encode, None, None, audit, ChannelId("t".into()),
    )
    .expect("driver spawn");
    driver.outbound_tx.send(replies_reply("!a", "one")).unwrap();
    wait_until(|| !slow_hook_reports_for("slow-gave-up-hook").is_empty());
    let lines = slow_hook_reports_for("slow-gave-up-hook");
    assert_eq!(lines.len(), 1, "one given-up reply, one timed call: {lines:?}");
}
