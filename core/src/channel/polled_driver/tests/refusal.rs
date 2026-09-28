//! A worker's **refusal** — a structured `RpcError` from a live worker — is
//! paced and reported by the driver itself, and does not stop inbound (#769).
//!
//! The pure half ([`RefusalRun`]) is tested with an injected clock; the driver
//! half runs the real `run` loop against the scripted fake in `super`, with a
//! millisecond-scale backoff so it stays fast.
use super::*;
use crate::channel::polled_driver::refusal::{is_refusal, RefusalRun, Refused, REFUSAL_REPEAT};
use crate::worker_lifecycle::RestartBackoff;

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
    let delays: Vec<Duration> = (0..6).map(|_| run.on_refusal(t, &FAST).delay).collect();
    let ms: Vec<u128> = delays.iter().map(Duration::as_millis).collect();
    assert_eq!(ms, [100, 200, 400, 800, 1000, 1000]);
}

#[test]
fn a_refused_method_is_not_ready_until_its_delay_has_passed() {
    let mut run = RefusalRun::default();
    let t = Instant::now();
    let Refused { delay, .. } = run.on_refusal(t, &FAST);
    assert!(!run.ready(t));
    assert!(!run.ready(t + delay - Duration::from_millis(1)), "one ms early");
    assert!(run.ready(t + delay), "exactly on time");
}

#[test]
fn a_run_is_reported_first_then_every_repeat_interval_not_per_refusal() {
    let mut run = RefusalRun::default();
    let t0 = Instant::now();
    assert!(run.on_refusal(t0, &FAST).report, "the first refusal is reported");
    let almost = t0 + REFUSAL_REPEAT - Duration::from_secs(1);
    assert!(!run.on_refusal(almost, &FAST).report, "not yet due");
    let due = t0 + REFUSAL_REPEAT;
    assert!(run.on_refusal(due, &FAST).report, "due again");
    assert!(!run.on_refusal(due, &FAST).report, "and re-armed from then");
}

#[test]
fn acceptance_ends_the_run_and_resets_it() {
    let mut run = RefusalRun::default();
    let t = Instant::now();
    assert_eq!(run.on_accepted(), 0, "no run to end");
    run.on_refusal(t, &FAST);
    run.on_refusal(t, &FAST);
    assert_eq!(run.on_accepted(), 2, "says how many refusals it ended");
    assert!(run.ready(t), "callable at once");
    let next = run.on_refusal(t, &FAST);
    assert_eq!(next, Refused { delay: FAST.base, report: true }, "a new run starts from scratch");
}

// ----- the driver loop -----

fn spec(refusal_backoff: RestartBackoff) -> PolledWorkerSpec {
    PolledWorkerSpec { refusal_backoff, ..TEST_SPEC }
}

fn spawn_with(spec: PolledWorkerSpec, calls: Box<dyn WorkerCalls>) -> PolledWorkerDriver {
    PolledWorkerDriver::spawn(spec, calls, test_parse, test_encode, None, None, None, ChannelId("t".into()))
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
/// For the email channel with an expired key that is the difference between
/// five requests a second at localmail and one a minute.
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
/// *next* run starts again from the base delay is pinned by the pure
/// `acceptance_ends_the_run_and_resets_it`.)
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
