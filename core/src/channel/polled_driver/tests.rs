//! Unit tests for the channel-generic polled-worker driver, against a scripted
//! in-process fake — no worker process, no supervisor, no sandbox.
use super::refusal::is_upstream_auth_refusal;
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
/// failure path without taking the whole worker down). `refuse`, if set,
/// makes method `m` answer its next `n` calls with a structured `RpcError` —
/// a *refusal* from a live worker, which the driver treats differently from
/// every other failure (#769).
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
    /// `Some((m, n))`: the next `n` calls to method `m` are refused.
    refuse: Mutex<Option<(String, usize)>>,
    /// The `RpcError` code `refuse` answers with (default `OPERATION_FAILED`;
    /// `UPSTREAM_AUTH_FAILED` makes it a credential refusal).
    refuse_code: std::sync::atomic::AtomicI32,
    /// `Some(c)`: every `*.send` whose params' `conversation` is `c` is
    /// refused with `OPERATION_FAILED` (#782's stuck room).
    refuse_conversation: Mutex<Option<String>>,
}
struct FakeCalls(Arc<FakeState>);
impl WorkerCalls for FakeCalls {
    fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if self.0.down.load(Ordering::SeqCst) {
            anyhow::bail!("persistent worker is restarting");
        }
        self.0.log.lock().unwrap().push((method.to_string(), params.clone()));
        if let Some((m, left)) = self.0.refuse.lock().unwrap().as_mut() {
            if m == method && *left > 0 {
                *left -= 1;
                let code = self.0.refuse_code.load(Ordering::SeqCst);
                return Err(RpcError::new(code, "fake: refused").into());
            }
        }
        if method.ends_with(".send") {
            let stuck = self.0.refuse_conversation.lock().unwrap().clone();
            if stuck.is_some() && params["conversation"].as_str() == stuck.as_deref() {
                return Err(RpcError::new(codes::OPERATION_FAILED, "fake: room refused").into());
            }
        }
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
        refuse: Mutex::new(None),
        refuse_code: std::sync::atomic::AtomicI32::new(codes::OPERATION_FAILED),
        refuse_conversation: Mutex::new(None),
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
    refusal_backoff: REFUSAL_BACKOFF,
    reply_give_up: REPLY_GIVE_UP,
};

fn spawn_test_driver(
    calls: Box<dyn WorkerCalls>,
) -> (PolledWorkerDriver, Value) {
    PolledWorkerDriver::spawn(TEST_SPEC, calls, test_parse, test_encode, None, None, DriverAudit::none(), ChannelId("t".into()))
        .expect("driver spawn")
}

/// Spec for the ack-bearing tests (`tests/ack.rs`): an email-fallback-shaped channel
/// whose worker keeps a server-side polling cursor that must be advanced.
fn spec_with_ack() -> PolledWorkerSpec {
    PolledWorkerSpec {
        label: "email",
        init_method: "email.init",
        poll_method: "email.poll",
        send_method: "email.send",
        ack_method: Some("email.ack"),
        poll_timeout_ms: 50,
        refusal_backoff: REFUSAL_BACKOFF,
        reply_give_up: REPLY_GIVE_UP,
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
        refusal_backoff: REFUSAL_BACKOFF,
        reply_give_up: REPLY_GIVE_UP,
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
        DriverAudit::none(),
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

/// #674: only a structured `UPSTREAM_AUTH_FAILED` refusal reads as "the
/// credential was refused"; a death, a respawn in progress, a flattened
/// refusal and any other `RpcError` code do not.
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

// ----- OutageLog: the worker-down latch (#674, #769) -----

/// The first failure of an outage warns, the rest are silent, and an answer
/// ends the outage (and says so exactly once).
#[test]
fn an_outage_warns_once_and_reports_recovery_once() {
    let mut log = OutageLog::default();
    assert!(!log.on_answer(), "no outage to end yet");
    assert!(log.on_down(), "the first failure of an outage is reported");
    assert!(!log.on_down(), "the rest are not");
    assert!(log.on_answer(), "the outage ends");
    assert!(!log.on_answer(), "and only once");
    assert!(log.on_down(), "a new outage warns again");
}

mod ack;
mod audit;
mod refusal;
mod replies;
mod replies_driver;
