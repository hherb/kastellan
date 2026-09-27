//! Unit tests for the channel-generic polled-worker driver, against a scripted
//! in-process fake — no worker process, no supervisor, no sandbox.
use super::outage::{FailureLine, CREDENTIAL_REFUSAL_REPEAT};
use super::*;
use kastellan_protocol::{codes, RpcError};
use crate::channel::{ChannelId, ConversationId, OutgoingMessage, PeerId};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Scripted fake worker: a `*.init` method returns a fixed identity, `*.poll`
/// pops the next canned poll RESULT (empty batch when none queued), `*.send`
/// records its params, `*.ack` is accepted (recorded in `log` only — no
/// dedicated field, since only the ack tests care about it). Matched by
/// suffix rather than the literal `t.*` names so the same fake serves both
/// `TEST_SPEC` and the ack-specific specs below (`email.*`, `matrix.*`).
/// While `down` is set every call fails (simulating the supervisor's respawn
/// window, where `PersistentHandle::call` returns `Err`); `fail_method`, if
/// set, fails only that one exact method (used to exercise the ack-specific
/// failure path without taking the whole worker down).
struct FakeState {
    down: AtomicBool,
    polls: Mutex<VecDeque<Value>>,
    sends: Mutex<Vec<Value>>,
    init_calls: AtomicUsize,
    /// Every accepted call, in order, as `(method, params)` — the ack tests
    /// use this to assert an ack method was (or was not) invoked, and with
    /// which params.
    log: Mutex<Vec<(String, Value)>>,
    /// When `Some(m)`, calls to method `m` return `Err` instead of the usual
    /// canned response, independent of `down`. `None` means no per-method
    /// failure is injected.
    fail_method: Mutex<Option<String>>,
}
struct FakeCalls(Arc<FakeState>);
impl WorkerCalls for FakeCalls {
    fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if self.0.down.load(Ordering::SeqCst) {
            anyhow::bail!("persistent worker is restarting");
        }
        self.0.log.lock().unwrap().push((method.to_string(), params.clone()));
        if self.0.fail_method.lock().unwrap().as_deref() == Some(method) {
            anyhow::bail!("fake: forced failure for {method}");
        }
        if method.ends_with(".init") {
            self.0.init_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"user_id": "@fake:srv"}));
        }
        if method.ends_with(".poll") {
            return Ok(self
                .0
                .polls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| json!({"events": []})));
        }
        if method.ends_with(".send") {
            self.0.sends.lock().unwrap().push(params);
            return Ok(json!({}));
        }
        if method.ends_with(".ack") {
            return Ok(json!({}));
        }
        anyhow::bail!("unknown method {method}")
    }
}

fn fake() -> (Arc<FakeState>, Box<dyn WorkerCalls>) {
    let st = Arc::new(FakeState {
        down: AtomicBool::new(false),
        polls: Mutex::new(VecDeque::new()),
        sends: Mutex::new(Vec::new()),
        init_calls: AtomicUsize::new(0),
        log: Mutex::new(Vec::new()),
        fail_method: Mutex::new(None),
    });
    (st.clone(), Box::new(FakeCalls(st)))
}

fn test_parse(v: Value) -> anyhow::Result<Vec<PolledEvent>> {
    let evs = v["events"].as_array().cloned().unwrap_or_default();
    evs.into_iter()
        .map(|e| {
            Ok(PolledEvent {
                peer: e["peer"].as_str().ok_or_else(|| anyhow::anyhow!("bad event"))?.into(),
                conversation: e["conversation"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("bad event"))?
                    .into(),
                body: e["body"].as_str().ok_or_else(|| anyhow::anyhow!("bad event"))?.into(),
                evidence: None,
                ack_token: e["ack_token"].as_str().map(String::from),
            })
        })
        .collect()
}
fn test_encode(m: &OutgoingMessage) -> Value {
    json!({"conversation": m.conversation.0, "body": m.body})
}
const TEST_SPEC: PolledWorkerSpec = PolledWorkerSpec {
    label: "t",
    init_method: "t.init",
    poll_method: "t.poll",
    send_method: "t.send",
    ack_method: None,
    poll_timeout_ms: 5,
};

fn spawn_test_driver(
    calls: Box<dyn WorkerCalls>,
) -> (PolledWorkerDriver, Value) {
    PolledWorkerDriver::spawn(TEST_SPEC, calls, test_parse, test_encode, None, None, None, ChannelId("t".into()))
        .expect("driver spawn")
}

/// Spec for the ack-bearing tests below: an email-fallback-shaped channel
/// whose worker keeps a server-side polling cursor that must be advanced.
fn spec_with_ack() -> PolledWorkerSpec {
    PolledWorkerSpec {
        label: "email",
        init_method: "email.init",
        poll_method: "email.poll",
        send_method: "email.send",
        ack_method: Some("email.ack"),
        poll_timeout_ms: 50,
    }
}

/// Spec shaped like Matrix's real [`crate::channel::matrix::wire::MATRIX_POLLED_SPEC`]:
/// `ack_method: None`. Used to pin that a spec without an ack method never
/// triggers an ack call, regardless of what the events carry.
fn spec_without_ack() -> PolledWorkerSpec {
    PolledWorkerSpec {
        label: "matrix",
        init_method: "matrix.init",
        poll_method: "matrix.poll",
        send_method: "matrix.send",
        ack_method: None,
        poll_timeout_ms: 50,
    }
}

fn encode_test_ack(cursor: &str) -> Value {
    json!({ "cursor": cursor })
}

#[test]
fn spawn_surfaces_identity_via_one_init_call() {
    let (st, calls) = fake();
    let (driver, identity) = spawn_test_driver(calls);
    assert_eq!(identity["user_id"], "@fake:srv");
    assert_eq!(st.init_calls.load(Ordering::SeqCst), 1, "exactly one init (login proof)");
    drop(driver);
}

#[test]
fn polled_events_are_forwarded_as_incoming_messages() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "hello"}
    ]}));
    let (mut driver, _identity) = spawn_test_driver(calls);
    let msg = driver.inbound_rx.blocking_recv().expect("inbound message");
    assert_eq!(msg.channel, ChannelId("t".into()));
    assert_eq!(msg.peer, PeerId("@me:srv".into()));
    assert_eq!(msg.conversation, ConversationId("!room:srv".into()));
    assert_eq!(msg.body, "hello");
}

#[test]
fn malformed_poll_result_is_skipped_not_fatal() {
    let (st, calls) = fake();
    // First a batch test_parse rejects, then a good one: the driver must skip
    // the bad batch (worker bug, not a death) and forward the good one.
    st.polls.lock().unwrap().push_back(json!({"events": [{"peer": 42}]}));
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!room:srv", "body": "after-bad"}
    ]}));
    let (mut driver, _identity) = spawn_test_driver(calls);
    let msg = driver.inbound_rx.blocking_recv().expect("inbound message");
    assert_eq!(msg.body, "after-bad");
}

#[test]
fn init_failure_fails_spawn() {
    let (st, calls) = fake();
    st.down.store(true, Ordering::SeqCst);
    let res = PolledWorkerDriver::spawn(
        TEST_SPEC,
        calls,
        test_parse,
        test_encode,
        None,
        None,
        None,
        ChannelId("t".into()),
    );
    assert!(res.is_err(), "init error must fail the spawn (login proof)");
}

/// Bounded wait for a condition, so a regression fails the test rather than
/// hanging the suite.
fn wait_until(mut cond: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("condition not reached within 5s");
}

#[test]
fn outbound_message_is_delivered_encoded() {
    let (st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    driver
        .outbound_tx
        .send(OutgoingMessage {
            channel: ChannelId("t".into()),
            peer: PeerId("@me:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "pong".into(),
        })
        .unwrap();
    wait_until(|| !st.sends.lock().unwrap().is_empty());
    let sent = st.sends.lock().unwrap();
    assert_eq!(sent[0], json!({"conversation": "!room:srv", "body": "pong"}));
}

#[test]
fn pending_send_is_retained_across_a_down_window_and_delivered_once() {
    let (st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    // Worker goes down (supervisor respawn window: every call errors).
    st.down.store(true, Ordering::SeqCst);
    driver
        .outbound_tx
        .send(OutgoingMessage {
            channel: ChannelId("t".into()),
            peer: PeerId("@me:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "survives".into(),
        })
        .unwrap();
    // Give the driver a few retry slices while down: nothing may be delivered.
    std::thread::sleep(Duration::from_millis(600));
    assert!(st.sends.lock().unwrap().is_empty(), "no delivery while worker is down");
    // Worker comes back: the retained message must arrive exactly once.
    st.down.store(false, Ordering::SeqCst);
    wait_until(|| !st.sends.lock().unwrap().is_empty());
    std::thread::sleep(Duration::from_millis(100)); // catch double-delivery
    let sent = st.sends.lock().unwrap();
    assert_eq!(sent.len(), 1, "retained send must be delivered exactly once");
    assert_eq!(sent[0]["body"], "survives");
}

#[test]
fn dropping_endpoints_stops_the_driver_thread() {
    let (_st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    drop(inbound_rx);
    drop(outbound_tx);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("driver thread must exit once both endpoints are dropped");
}

#[test]
fn dropping_endpoints_during_a_down_window_stops_the_driver_thread() {
    let (st, calls) = fake();
    let (driver, _identity) = spawn_test_driver(calls);
    st.down.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(100)); // let it enter the retry loop
    let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
    drop(inbound_rx);
    drop(outbound_tx);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("driver must exit from the retry loop when endpoints drop");
}

#[test]
fn ack_is_called_after_the_event_reaches_the_bus() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "hi", "ack_token": "42"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        None,
        None,
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "me@example.org");

    wait_until(|| st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
    let log = st.log.lock().unwrap();
    let entry = log.iter().find(|(m, _)| m == "email.ack").cloned().unwrap();
    assert_eq!(entry.1["cursor"], "42", "ack must carry the event's own cursor");
}

#[test]
fn no_ack_method_means_no_ack_call() {
    // Matrix must be untouched: its spec has ack_method: None. The event
    // carries an ack_token on purpose — this isolates "ack_method: None
    // gates the call" from "ack_token: None gates the call" (the latter has
    // its own dedicated test below); without a token here, this test would
    // pass even if the ack_method gate were broken.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "@me:srv", "conversation": "!r", "body": "hi", "ack_token": "99"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_without_ack(),
        calls,
        test_parse,
        test_encode,
        None,
        None,
        None,
        ChannelId("matrix".into()),
    )
    .unwrap();
    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "@me:srv");
    // No ack ever fires here, so there is no bus-side signal to wait on;
    // give the (busy-spinning) driver a few loop iterations before asserting
    // absence, same pattern as the retention test above.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")),
        "a spec without ack_method must never call ack"
    );
}

#[test]
fn an_event_without_an_ack_token_is_not_acked() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "hi"}
    ]}));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        None,
        None,
        ChannelId("email".into()),
    )
    .unwrap();
    let msg = driver.inbound_rx.blocking_recv().expect("one inbound event");
    assert_eq!(msg.peer.0, "me@example.org");
    std::thread::sleep(Duration::from_millis(150));
    assert!(!st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));
}

#[test]
fn ack_failure_is_non_fatal_and_the_driver_keeps_polling() {
    // The whole reason the ack fires AFTER the bus hand-off is to guarantee
    // redelivery when the ack itself fails. This test pins that: it forces
    // `email.ack` to error while init/poll/send keep succeeding, and proves
    // the driver thread survives — it must still be alive and polling well
    // after the failed ack, not dead from an accidental early return.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "first", "ack_token": "1"}
    ]}));
    *st.fail_method.lock().unwrap() = Some("email.ack".to_string());
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        None,
        None,
        ChannelId("email".into()),
    )
    .unwrap();

    let first = driver.inbound_rx.blocking_recv().expect("first inbound event");
    assert_eq!(first.body, "first");

    // Wait for the (failing) ack attempt itself, so we know the ack branch
    // actually ran — not merely that it was skipped for some other reason.
    wait_until(|| st.log.lock().unwrap().iter().any(|(m, _)| m == "email.ack"));

    // Only now queue a second event: if the driver thread died in the
    // ack-failure arm (e.g. a stray `return;`), inbound_rx would be closed
    // and blocking_recv below would return None instead of the event —
    // panicking with a clear message rather than hanging, since a dead
    // driver drops its inbound_tx sender.
    st.polls.lock().unwrap().push_back(json!({"events": [
        {"peer": "me@example.org", "conversation": "<a>", "body": "second", "ack_token": "2"}
    ]}));
    let second = driver
        .inbound_rx
        .blocking_recv()
        .expect("driver must keep polling after a failed ack, not die");
    assert_eq!(second.body, "second");
}

/// Extract `skipped[].(message_id, reason)` from a poll result shaped like
/// email-in's — the fixture `ParseAckOnly` these tests use.
fn test_parse_ack_only(v: &Value) -> Vec<(String, String)> {
    v.get("skipped")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let id = e.get("message_id").and_then(|m| m.as_str())?.to_string();
                    let reason = e.get("reason").and_then(|r| r.as_str()).unwrap_or("").to_string();
                    Some((id, reason))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn skipped_ids_are_acked_even_though_they_never_become_events() {
    // A poll result with an EMPTY events list but a non-empty `skipped` list
    // (email-in's real shape when every message in a batch was unattributable)
    // must still ack every skipped id — otherwise the worker's server-side
    // cursor wedges on the first one forever, since nothing else will ever
    // ack an id nobody ever saw as an event.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [],
        "skipped": [
            {"message_id": "10", "reason": "no usable From address"},
            {"message_id": "11", "reason": "localmail 404: not found"}
        ]
    }));
    let (_driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        None,
        ChannelId("email".into()),
    )
    .unwrap();

    wait_until(|| st.log.lock().unwrap().iter().filter(|(m, _)| m == "email.ack").count() >= 2);
    let log = st.log.lock().unwrap();
    let acked: Vec<&str> = log
        .iter()
        .filter(|(m, _)| m == "email.ack")
        .map(|(_, p)| p["cursor"].as_str().unwrap())
        .collect();
    assert!(acked.contains(&"10"), "acked = {acked:?}");
    assert!(acked.contains(&"11"), "acked = {acked:?}");
}

#[test]
fn skipped_ids_alongside_real_events_are_both_acked() {
    // Realistic mixed batch: one usable event (acked via its own ack_token,
    // the existing per-event path) plus one unattributable message in
    // `skipped` (acked via the new ParseAckOnly path). Neither path must
    // starve the other.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [
            {"peer": "me@example.org", "conversation": "<a>", "body": "hi", "ack_token": "5"}
        ],
        "skipped": [{"message_id": "6", "reason": "no usable From address"}]
    }));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        None,
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("the one real event");
    assert_eq!(msg.body, "hi");

    wait_until(|| st.log.lock().unwrap().iter().filter(|(m, _)| m == "email.ack").count() >= 2);
    let log = st.log.lock().unwrap();
    let acked: Vec<&str> = log
        .iter()
        .filter(|(m, _)| m == "email.ack")
        .map(|(_, p)| p["cursor"].as_str().unwrap())
        .collect();
    assert!(acked.contains(&"5"), "the real event's own ack_token must still be acked: {acked:?}");
    assert!(acked.contains(&"6"), "the skipped id must also be acked: {acked:?}");
}

#[test]
fn no_ack_method_means_skipped_ids_are_never_acked_either() {
    // Symmetry with `no_ack_method_means_no_ack_call`: a spec without
    // ack_method (Matrix's shape) must never call ack, regardless of whether
    // a `parse_ack_only` happens to be wired — the ack_method gate covers
    // both ack paths, not just the per-event one.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [],
        "skipped": [{"message_id": "99", "reason": "no usable From address"}]
    }));
    let (_driver, _identity) = PolledWorkerDriver::spawn(
        spec_without_ack(),
        calls,
        test_parse,
        test_encode,
        None,
        Some(test_parse_ack_only),
        None,
        ChannelId("matrix".into()),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !st.log.lock().unwrap().iter().any(|(m, _)| m.ends_with(".ack")),
        "ack_method: None must suppress the skipped-id ack path too"
    );
}

#[test]
fn a_skipped_id_is_not_acked_when_the_same_batchs_events_fail_to_decode() {
    // The worker's ack cursor is ONE monotonic high-water mark shared by
    // `events` and `skipped` — not two independent counters. If `events`
    // fails to decode (a worker bug: `test_parse` errors on a non-string
    // `peer`) and the driver acked `skipped` anyway, that would silently
    // advance the shared cursor PAST whatever those undecoded events were,
    // permanently losing them (unlike a failed ack, an advanced cursor can
    // never be wound back). So this batch must produce NO ack call at all.
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [{"peer": 42}], // malformed: peer must be a string — test_parse errors
        "skipped": [{"message_id": "77", "reason": "no usable From address"}]
    }));
    // A second, well-formed poll follows so the test can prove the driver is
    // still alive and polling (not wedged) without racing a fixed sleep.
    st.polls.lock().unwrap().push_back(json!({
        "events": [
            {"peer": "me@example.org", "conversation": "<a>", "body": "after-bad", "ack_token": "1"}
        ],
        "skipped": []
    }));
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        None,
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("the second batch's good event");
    assert_eq!(msg.body, "after-bad");

    // Give any (incorrect) ack attempt for "77" a chance to have happened.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !st.log.lock().unwrap().iter().any(|(m, p)| m == "email.ack" && p["cursor"] == "77"),
        "a skipped id from a batch whose events failed to decode must never be acked"
    );
}

#[test]
fn audit_ack_only_is_called_with_id_and_reason_for_every_acked_skipped_id() {
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [],
        "skipped": [
            {"message_id": "10", "reason": "no usable From address"},
            {"message_id": "11", "reason": "localmail 404: not found"}
        ]
    }));
    let audited: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let audited_cl = audited.clone();
    let audit: AckOnlyAudit = Box::new(move |id, reason| {
        audited_cl.lock().unwrap().push((id.to_string(), reason.to_string()));
    });
    let (_driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        Some(audit),
        ChannelId("email".into()),
    )
    .unwrap();

    wait_until(|| audited.lock().unwrap().len() >= 2);
    let got = audited.lock().unwrap();
    assert!(got.contains(&("10".to_string(), "no usable From address".to_string())), "{got:?}");
    assert!(got.contains(&("11".to_string(), "localmail 404: not found".to_string())), "{got:?}");
}

#[test]
fn audit_ack_only_is_not_called_when_the_same_batchs_events_fail_to_decode() {
    // Symmetric with the ack-suppression test above: the audit call sits in
    // the exact same gated block as the ack call (see `run`'s comment), so a
    // batch whose events fail to decode must produce no audit call either —
    // otherwise the audit trail would claim a message was "discarded" when
    // its cursor was never actually advanced (it will be redelivered).
    let (st, calls) = fake();
    st.polls.lock().unwrap().push_back(json!({
        "events": [{"peer": 42}], // malformed — test_parse errors
        "skipped": [{"message_id": "77", "reason": "no usable From address"}]
    }));
    st.polls.lock().unwrap().push_back(json!({
        "events": [
            {"peer": "me@example.org", "conversation": "<a>", "body": "after-bad", "ack_token": "1"}
        ],
        "skipped": []
    }));
    let audited: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let audited_cl = audited.clone();
    let audit: AckOnlyAudit = Box::new(move |id, reason| {
        audited_cl.lock().unwrap().push((id.to_string(), reason.to_string()));
    });
    let (mut driver, _identity) = PolledWorkerDriver::spawn(
        spec_with_ack(),
        calls,
        test_parse,
        test_encode,
        Some(encode_test_ack),
        Some(test_parse_ack_only),
        Some(audit),
        ChannelId("email".into()),
    )
    .unwrap();

    let msg = driver.inbound_rx.blocking_recv().expect("the second batch's good event");
    assert_eq!(msg.body, "after-bad");

    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !audited.lock().unwrap().iter().any(|(id, _)| id == "77"),
        "audit must not fire for a skipped id whose batch's events failed to decode"
    );
}

/// #674: only a structured `UPSTREAM_AUTH_FAILED` refusal reads as "the
/// credential was refused"; a death, a respawn in progress or any other
/// worker refusal keeps the old "worker died or restarting" wording.
#[test]
fn only_an_upstream_auth_refusal_is_reported_as_a_credential_problem() {
    let refused = anyhow::Error::from(kastellan_protocol::upstream_auth_refusal("localmail", 401).unwrap());
    assert!(is_upstream_auth_refusal(&refused));

    let other_rpc = anyhow::Error::from(RpcError::new(codes::OPERATION_FAILED, "localmail 500: boom"));
    let restarting = anyhow::anyhow!("persistent worker is restarting");
    // The pre-#674 shape: an auth refusal flattened to text is NOT recognised,
    // which is why `client_error_to_anyhow` keeps the type.
    let flattened = anyhow::anyhow!("{refused}");
    for e in [other_rpc, restarting, flattened] {
        assert!(!is_upstream_auth_refusal(&e), "{e}");
    }
}

// ----- OutageLog (#674, #770 review) -----

/// The plain case: the first failure of an outage warns, the rest are silent,
/// and a success ends the outage (and says so exactly once).
#[test]
fn an_ordinary_outage_warns_once_and_reports_recovery_once() {
    let mut log = OutageLog::default();
    let t = Instant::now();
    assert!(!log.on_success(), "no outage to end yet");
    assert_eq!(log.on_failure(false, t), FailureLine::Warn);
    assert_eq!(log.on_failure(false, t), FailureLine::Silent);
    assert!(log.on_success(), "the outage ends");
    assert!(!log.on_success(), "and only once");
    assert_eq!(log.on_failure(false, t), FailureLine::Warn, "a new outage warns again");
}

/// The review's scenario: an outage that begins as a restart and comes back
/// as a 401 must still reach ERROR — the single up/down latch hid it.
#[test]
fn a_credential_refusal_is_reported_even_when_the_outage_began_otherwise() {
    let mut log = OutageLog::default();
    let t = Instant::now();
    assert_eq!(log.on_failure(false, t), FailureLine::Warn);
    assert_eq!(log.on_failure(true, t), FailureLine::CredentialError);
}

/// Every refusal respawns the worker (#769), so a lasting refusal alternates
/// with "restarting" failures: neither may re-log each cycle, but the ERROR
/// comes back after `CREDENTIAL_REFUSAL_REPEAT` so it is not buried.
#[test]
fn a_lasting_credential_refusal_repeats_on_its_interval_and_not_per_cycle() {
    let mut log = OutageLog::default();
    let t0 = Instant::now();
    assert_eq!(log.on_failure(true, t0), FailureLine::CredentialError);
    let almost = t0 + CREDENTIAL_REFUSAL_REPEAT - Duration::from_secs(1);
    assert_eq!(log.on_failure(false, almost), FailureLine::Silent, "the respawn's restarting error");
    assert_eq!(log.on_failure(true, almost), FailureLine::Silent, "not yet due");
    let due = t0 + CREDENTIAL_REFUSAL_REPEAT;
    assert_eq!(log.on_failure(true, due), FailureLine::CredentialError, "due again");
    assert_eq!(log.on_failure(true, due), FailureLine::Silent, "and re-armed from then");
    // A success resets the cadence: the next refusal is a new outage.
    assert!(log.on_success());
    assert_eq!(log.on_failure(true, due), FailureLine::CredentialError);
}
