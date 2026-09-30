//! A worker's **refusal** — a structured `RpcError` from a live worker — is
//! paced and reported by the driver itself, and does not stop inbound (#769).
//!
//! The pure half ([`RefusalRun`]) is tested with an injected clock; the driver
//! half runs the real `run` loop against the scripted fake in `super`, with a
//! millisecond-scale backoff so it stays fast.
use super::*;
use crate::channel::polled_driver::refusal::{
    check_refusal_backoff, format_accepted_report, is_refusal, refusal_line, RefusalLine, RefusalRun,
    Refused, REFUSAL_REPEAT,
};
use crate::worker_lifecycle::RestartBackoff;
use crate::worker_stderr::{emitted_refusal_lines_for, RefusalSeverity};

/// The code the fake's refusals carry.
const OP: i32 = codes::OPERATION_FAILED;

/// 100 ms, doubling, capped at 1 s — the shape of the production default
/// (1 s → 60 s) at a scale a unit test can wait for.
const FAST: RestartBackoff = RestartBackoff {
    base: Duration::from_millis(100),
    factor_num: 2,
    factor_den: 1,
    cap: Duration::from_secs(1),
};

// ----- pure: is_refusal -----

#[test]
fn only_a_typed_rpc_error_is_a_refusal() {
    let refused = anyhow::Error::from(RpcError::new(codes::OPERATION_FAILED, "send failed: nope"));
    assert!(is_refusal(&refused));
    let credential = anyhow::Error::from(kastellan_protocol::upstream_auth_refusal("localmail", 401).unwrap());
    assert!(is_refusal(&credential), "a credential refusal is a refusal too");
    // What `PersistentHandle::call` returns for a death or a respawn in
    // progress, and a refusal that lost its type on the way.
    let restarting = anyhow::anyhow!("persistent worker is restarting");
    let flattened = anyhow::anyhow!("{refused}");
    for e in [restarting, flattened] {
        assert!(!is_refusal(&e), "{e}");
    }
}

// ----- pure: RefusalRun -----

#[test]
fn a_run_backs_off_exponentially_up_to_the_cap() {
    let mut run = RefusalRun::default();
    let t = Instant::now();
    assert!(run.ready(t), "nothing refused yet");
    let delays: Vec<Duration> = (0..6).map(|_| run.on_refusal(t, &FAST, OP).delay).collect();
    let ms: Vec<u128> = delays.iter().map(Duration::as_millis).collect();
    assert_eq!(ms, [100, 200, 400, 800, 1000, 1000]);
}

#[test]
fn a_refused_method_is_not_ready_until_its_delay_has_passed() {
    let mut run = RefusalRun::default();
    let t = Instant::now();
    let Refused { delay, .. } = run.on_refusal(t, &FAST, OP);
    assert!(!run.ready(t));
    assert!(!run.ready(t + delay - Duration::from_millis(1)), "one ms early");
    assert!(run.ready(t + delay), "exactly on time");
}

#[test]
fn a_run_is_reported_first_then_every_repeat_interval_not_per_refusal() {
    let mut run = RefusalRun::default();
    let t0 = Instant::now();
    assert!(run.on_refusal(t0, &FAST, OP).report, "the first refusal is reported");
    let almost = t0 + REFUSAL_REPEAT - Duration::from_secs(1);
    assert!(!run.on_refusal(almost, &FAST, OP).report, "not yet due");
    let due = t0 + REFUSAL_REPEAT;
    assert!(run.on_refusal(due, &FAST, OP).report, "due again");
    assert!(!run.on_refusal(due, &FAST, OP).report, "and re-armed from then");
}

#[test]
fn acceptance_ends_the_run_and_resets_it() {
    let mut run = RefusalRun::default();
    let t = Instant::now();
    assert_eq!(run.on_accepted(), 0, "no run to end");
    let _ = run.on_refusal(t, &FAST, OP);
    let _ = run.on_refusal(t, &FAST, OP);
    assert_eq!(run.on_accepted(), 2, "says how many refusals it ended");
    assert!(run.ready(t), "callable at once");
    let next = run.on_refusal(t, &FAST, OP);
    assert_eq!(next, Refused { delay: FAST.base, report: true }, "a new run starts from scratch");
}

/// The #770 review's case, which a time-only cadence brought back: a run that
/// starts as a transient refusal (a localmail restart's 503) and turns into a
/// credential refusal must get its operator-action ERROR at once, not up to
/// 15 min later behind the WARN it started with.
#[test]
fn a_refusal_with_a_new_code_is_reported_at_once() {
    let mut run = RefusalRun::default();
    let t0 = Instant::now();
    let auth = codes::UPSTREAM_AUTH_FAILED;
    assert!(run.on_refusal(t0, &FAST, OP).report);
    assert!(!run.on_refusal(t0 + Duration::from_secs(1), &FAST, OP).report, "same code, not due");
    assert!(run.on_refusal(t0 + Duration::from_secs(2), &FAST, auth).report, "the code changed");
    assert!(!run.on_refusal(t0 + Duration::from_secs(3), &FAST, auth).report, "and the cadence resumes");
}

/// A refusal that ends an outage is logged even when the cadence says no:
/// the "back up" line just said the driver resumed.
#[test]
fn a_rearmed_run_reports_its_next_refusal() {
    let mut run = RefusalRun::default();
    let t0 = Instant::now();
    assert!(run.on_refusal(t0, &FAST, OP).report);
    run.rearm_report();
    assert!(run.on_refusal(t0 + Duration::from_secs(1), &FAST, OP).report);
}

/// The #674 line choice, pure: the ERROR (and its operator action) only for a
/// typed credential refusal, and nothing at all when the cadence says no.
#[test]
fn only_a_due_credential_refusal_gets_the_error_line() {
    let credential = anyhow::Error::from(kastellan_protocol::upstream_auth_refusal("localmail", 401).unwrap());
    let other = anyhow::Error::from(RpcError::new(OP, "send failed: nope"));
    let due = Refused { delay: FAST.base, report: true };
    let not_due = Refused { delay: FAST.base, report: false };
    assert_eq!(refusal_line(&credential, &due), RefusalLine::CredentialError);
    assert_eq!(refusal_line(&other, &due), RefusalLine::Warn);
    assert_eq!(refusal_line(&credential, &not_due), RefusalLine::Silent);
    assert_eq!(refusal_line(&other, &not_due), RefusalLine::Silent);
}

/// Both refusal lines say "Repeated every 15 min".
#[test]
fn the_repeat_interval_is_the_one_the_log_lines_name() {
    assert_eq!(REFUSAL_REPEAT, Duration::from_secs(15 * 60));
}

#[test]
fn a_backoff_that_would_not_pace_anything_is_rejected() {
    check_refusal_backoff(&REFUSAL_BACKOFF).expect("production");
    check_refusal_backoff(&FAST).expect("the tests' own");
    let zero_factor = RestartBackoff { factor_num: 0, factor_den: 0, ..FAST };
    check_refusal_backoff(&zero_factor).expect("read as a factor of 1: a constant delay still paces");
    for bad in [
        RestartBackoff { base: Duration::ZERO, ..FAST },
        RestartBackoff { factor_num: 1, factor_den: 2, ..FAST },
        RestartBackoff { cap: Duration::from_millis(50), ..FAST },
        RestartBackoff { cap: Duration::from_secs(2 * 60 * 60), ..FAST },
    ] {
        assert!(check_refusal_backoff(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn spawn_refuses_a_backoff_that_would_not_pace_anything() {
    let (st, calls) = fake();
    let bad = spec(RestartBackoff { base: Duration::ZERO, ..FAST });
    let res = PolledWorkerDriver::spawn(bad, calls, test_parse, test_encode, None, None, DriverAudit::none(), ChannelId("t".into()));
    assert!(res.is_err(), "a zero base would retry a refused call every loop");
    assert_eq!(st.init_calls.load(Ordering::SeqCst), 0, "refused before calling the worker");
}

// ----- the driver loop -----

fn spec(refusal_backoff: RestartBackoff) -> PolledWorkerSpec {
    PolledWorkerSpec { refusal_backoff, ..TEST_SPEC }
}

fn spawn_with(spec: PolledWorkerSpec, calls: Box<dyn WorkerCalls>) -> PolledWorkerDriver {
    PolledWorkerDriver::spawn(spec, calls, test_parse, test_encode, None, None, DriverAudit::none(), ChannelId("t".into()))
        .expect("driver spawn")
        .0
}

fn outgoing(body: &str) -> OutgoingMessage {
    OutgoingMessage {
        channel: ChannelId("t".into()),
        peer: PeerId("@me:srv".into()),
        conversation: ConversationId("!room:srv".into()),
        body: body.into(),
    }
}

/// Bounded receive: a regression must FAIL the test, not hang the suite
/// (`blocking_recv` would wait forever for inbound that never comes).
fn recv_within(driver: &mut PolledWorkerDriver, limit: Duration) -> IncomingMessage {
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(msg) = driver.inbound_rx.try_recv() {
            return msg;
        }
        assert!(Instant::now() < deadline, "no inbound message within {limit:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn calls_to(st: &FakeState, method: &str) -> usize {
    st.log.lock().unwrap().iter().filter(|(m, _)| m == method).count()
}

/// The user-visible half of #769's head-of-line question. A refused send
/// used to skip the poll, so one reply the homeserver would not take froze
/// inbound until it was accepted. The worker is alive: inbound must flow.
#[test]
fn a_refused_send_does_not_stop_inbound() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.send".into(), usize::MAX));
    let mut driver = spawn_with(spec(FAST), calls);
    driver.outbound_tx.send(outgoing("refused forever")).unwrap();
    wait_until(|| calls_to(&st, "t.send") >= 1);
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "still arrives"}
    ]}));
    let msg = recv_within(&mut driver, Duration::from_secs(5));
    assert_eq!(msg.body, "still arrives", "inbound keeps flowing");
    assert!(st.sends.lock().unwrap().is_empty(), "the refused reply was never accepted");
}

/// The refused reply keeps its place: it is retried after its backoff, then
/// delivered exactly once, and the reply queued behind it is not sent first.
#[test]
fn a_refused_send_is_retried_after_its_backoff_and_order_is_kept() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 2));
    let driver = spawn_with(spec(FAST), calls);
    let queued = Instant::now();
    driver.outbound_tx.send(outgoing("first")).unwrap();
    driver.outbound_tx.send(outgoing("second")).unwrap();
    wait_until(|| st.sends.lock().unwrap().len() == 2);
    // Two refusals = 100 + 200 ms of backoff before `first` can be accepted.
    // Without it the fake's refusals are used up in microseconds.
    let took = queued.elapsed();
    assert!(took >= Duration::from_millis(300), "the refused reply waited out its backoff: {took:?}");
    std::thread::sleep(Duration::from_millis(100)); // catch a double delivery
    let bodies: Vec<Value> = st.sends.lock().unwrap().iter().map(|v| v["body"].clone()).collect();
    assert_eq!(bodies, [json!("first"), json!("second")]);
    assert_eq!(calls_to(&st, "t.send"), 4, "two refusals of `first`, then one call each");
}

/// The pacing job the supervisor's respawn used to do by accident: a
/// refused poll is retried on the backoff, not every 200 ms `RETRY_SLICE`.
/// Before #769 the respawn's delay held it to about one a second; with
/// neither, the email channel with an expired key would ask localmail five
/// times a second. With the backoff it settles at one a minute.
#[test]
fn a_refused_poll_backs_off_instead_of_retrying_every_slice() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.poll".into(), usize::MAX));
    let slow = RestartBackoff { base: Duration::from_millis(300), cap: Duration::from_secs(10), ..FAST };
    let _driver = spawn_with(spec(slow), calls);
    std::thread::sleep(Duration::from_millis(1_300));
    // Due at 0, 0.3 and 0.9 s (then 2.1 s). Every slice would be ~7. An
    // upper bound, so a loaded machine can only make it pass more easily.
    let polls = calls_to(&st, "t.poll");
    assert!((1..=3).contains(&polls), "{polls} poll calls in 1.3 s");
}

/// Once a refused poll is accepted, polling is back to its own pace: no
/// backoff left over, and no extra slice after a completed poll. (That the
/// *next* run starts again from the base delay is pinned by
/// `a_new_poll_refusal_run_starts_from_the_base_delay`.)
#[test]
fn polling_resumes_at_full_pace_once_the_refusal_ends() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.poll".into(), 3));
    let mut driver = spawn_with(spec(FAST), calls);
    // 3 refusals: 100 + 200 + 400 ms of backoff, then accepted.
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "after the refusals"}
    ]}));
    let msg = recv_within(&mut driver, Duration::from_secs(5));
    assert_eq!(msg.body, "after the refusals", "delivered once accepted");
    let before = calls_to(&st, "t.poll");
    std::thread::sleep(Duration::from_millis(300));
    // The fake's poll timeout is 5 ms: unthrottled, 300 ms is dozens of polls.
    assert!(calls_to(&st, "t.poll") - before >= 5, "polling is back to its own pace");
}

/// 300 ms, doubling, capped at 10 s: slow enough that the first delays are
/// easy to tell apart at test scale.
const SLOW: RestartBackoff = RestartBackoff { base: Duration::from_millis(300), cap: Duration::from_secs(10), ..FAST };

fn refusals_left(st: &FakeState) -> usize {
    st.refuse.lock().unwrap().as_ref().map_or(0, |(_, n)| *n)
}

/// The driver must END a refusal run when the method is accepted, or the next
/// run starts at the old run's accumulated delay — and its first refusal goes
/// unlogged for up to 15 min. The pure reset is pinned above; this pins that
/// the driver calls it.
#[test]
fn a_new_poll_refusal_run_starts_from_the_base_delay() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.poll".into(), 2));
    let _driver = spawn_with(spec(SLOW), calls);
    // 300 + 600 ms of backoff, then accepted.
    let first_run_over = calls_to(&st, "t.poll");
    wait_until(|| refusals_left(&st) == 0 && calls_to(&st, "t.poll") > first_run_over + 2);
    *st.refuse.lock().unwrap() = Some(("t.poll".into(), 1));
    wait_until(|| refusals_left(&st) == 0);
    let refused_at = calls_to(&st, "t.poll");
    std::thread::sleep(Duration::from_millis(800));
    // A fresh run waits 300 ms. Carried over, the third refusal waits 1.2 s.
    let since = calls_to(&st, "t.poll") - refused_at;
    assert!(since >= 5, "polling resumed after the base delay: {since} polls in 800 ms");
}

/// The same for send.
#[test]
fn a_new_send_refusal_run_starts_from_the_base_delay() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 2));
    let driver = spawn_with(spec(SLOW), calls);
    driver.outbound_tx.send(outgoing("first")).unwrap();
    wait_until(|| st.sends.lock().unwrap().len() == 1);
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 1));
    let queued = Instant::now();
    driver.outbound_tx.send(outgoing("second")).unwrap();
    wait_until(|| st.sends.lock().unwrap().len() == 2);
    // A fresh run waits 300 ms. Carried over, the third refusal waits 1.2 s.
    let took = queued.elapsed();
    assert!(took < Duration::from_millis(1_000), "retried after the base delay: {took:?}");
}

/// The other direction of independence: a refused poll must not hold the
/// replies. (A Matrix sync refused while sends still work.)
#[test]
fn a_refused_poll_does_not_stall_sends() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.poll".into(), usize::MAX));
    let long = RestartBackoff { base: Duration::from_secs(2), cap: Duration::from_secs(10), ..FAST };
    let driver = spawn_with(spec(long), calls);
    wait_until(|| calls_to(&st, "t.poll") >= 1);
    let queued = Instant::now();
    driver.outbound_tx.send(outgoing("while polls are refused")).unwrap();
    wait_until(|| st.sends.lock().unwrap().len() == 1);
    let took = queued.elapsed();
    assert!(took < Duration::from_secs(1), "sent within a slice, not after the poll's 2 s: {took:?}");
}

/// A refused ack left the cursor where it was, so the next poll returns the
/// same messages — and a poll that returns events does not wait. Before #769
/// the respawn a refusal caused paced that loop; without pacing the agent
/// would receive the same email as fast as localmail answers. So a refused
/// ack holds the next poll on its backoff, and stops acking its batch (the
/// rest would be refused the same way).
#[test]
fn a_refused_ack_holds_the_next_poll_and_stops_the_batch() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("email.ack".into(), usize::MAX));
    for _ in 0..50 {
        st.polls.lock().unwrap().push_back(json!({"events": [
            {"peer": "me@example.org", "conversation": "<a>", "body": "same mail", "ack_token": "1"},
            {"peer": "me@example.org", "conversation": "<b>", "body": "next mail", "ack_token": "2"}
        ]}));
    }
    let spec = PolledWorkerSpec { refusal_backoff: SLOW, ..spec_with_ack() };
    let _driver = PolledWorkerDriver::spawn(
        spec,
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        None,
        DriverAudit::none(),
        ChannelId("email".into()),
    )
    .unwrap();
    // Polls due at about 0, 0.4 and 1.0 s (300 then 600 ms of ack backoff,
    // on 200 ms slices), then not before 2.2 s. Unpaced: all 50 at once.
    std::thread::sleep(Duration::from_millis(1_300));
    let polls = calls_to(&st, "email.poll");
    let acks = calls_to(&st, "email.ack");
    assert!((1..=3).contains(&polls), "{polls} polls in 1.3 s");
    assert_eq!(acks, polls, "one refused ack per batch, not one per event");
}

// ----- #788: the line that says a refusal ended -----

#[test]
fn the_recovery_line_names_what_was_refused_and_how_often() {
    assert_eq!(
        format_accepted_report("matrix.send", Some("!room:srv"), 3).as_deref(),
        Some(
            "the worker accepted matrix.send for conversation !room:srv again after 3 refusals; \
             that refusal has ended"
        )
    );
    let one = format_accepted_report("email.poll", None, 1).expect("a run of one ended");
    assert!(one.contains("accepted email.poll again after 1 refusal;"), "{one}");
}

#[test]
fn an_accepted_call_that_ends_no_run_says_nothing() {
    assert_eq!(format_accepted_report("email.poll", None, 0), None);
}

/// Through the real loop: the end of a poll refusal run is said once, at
/// INFO, on the marked emitter — so a reader of the `[worker-refusal]` lines
/// sees the refusal close, not only open (#788).
#[test]
fn the_end_of_a_poll_refusal_run_is_reported_once_at_info() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.poll".into(), 2));
    let spec = PolledWorkerSpec { label: "t-poll-recovered", ..spec(FAST) };
    let mut driver = spawn_with(spec, calls);
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "after the refusals"}
    ]}));
    recv_within(&mut driver, Duration::from_secs(5));
    let ended = |(l, _): &(String, RefusalSeverity)| l.contains("again after");
    wait_until(|| emitted_refusal_lines_for("t-poll-recovered").iter().any(ended));
    std::thread::sleep(Duration::from_millis(100)); // catch a second recovery line
    let lines = emitted_refusal_lines_for("t-poll-recovered");
    let ended: Vec<_> = lines.iter().filter(|(l, _)| l.contains("again after")).collect();
    assert_eq!(ended.len(), 1, "one recovery line per run: {lines:?}");
    assert!(ended[0].0.contains("accepted t.poll again after 2 refusals"), "{ended:?}");
    assert_eq!(ended[0].1, RefusalSeverity::Recovered);
    assert!(
        lines.iter().any(|(l, s)| l.contains("refused t.poll") && *s == RefusalSeverity::Warn),
        "POSITIVE CONTROL: the refusals it closes were reported too: {lines:?}"
    );
}

/// A send run's recovery line names the conversation, as its refusal lines do.
#[test]
fn the_end_of_a_send_refusal_run_names_its_conversation() {
    let (st, calls) = fake();
    *st.refuse.lock().unwrap() = Some(("t.send".into(), 1));
    let spec = PolledWorkerSpec { label: "t-send-recovered", ..spec(FAST) };
    let driver = spawn_with(spec, calls);
    driver.outbound_tx.send(outgoing("refused once")).unwrap();
    // On the line, not on the send: the fake records the send before the
    // driver hears it was accepted.
    let ended = |(l, _): &(String, RefusalSeverity)| l.contains("again after");
    wait_until(|| emitted_refusal_lines_for("t-send-recovered").iter().any(ended));
    let lines = emitted_refusal_lines_for("t-send-recovered");
    assert!(
        lines.iter().any(|(l, s)| {
            l.contains("accepted t.send for conversation !room:srv again after 1 refusal;")
                && *s == RefusalSeverity::Recovered
        }),
        "{lines:?}"
    );
    assert_eq!(st.sends.lock().unwrap().len(), 1, "POSITIVE CONTROL: the reply went through");
}
