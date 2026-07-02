//! Unit tests for the channel-generic polled-worker driver, against a scripted
//! in-process fake — no worker process, no supervisor, no sandbox.
use super::*;
use crate::channel::{ChannelId, ConversationId, OutgoingMessage, PeerId};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Scripted fake worker: `t.init` returns a fixed identity, `t.poll` pops the
/// next canned poll RESULT (empty batch when none queued), `t.send` records its
/// params. While `down` is set every call fails (simulating the supervisor's
/// respawn window, where `PersistentHandle::call` returns `Err`).
struct FakeState {
    down: AtomicBool,
    polls: Mutex<VecDeque<Value>>,
    sends: Mutex<Vec<Value>>,
    init_calls: AtomicUsize,
}
struct FakeCalls(Arc<FakeState>);
impl WorkerCalls for FakeCalls {
    fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        if self.0.down.load(Ordering::SeqCst) {
            anyhow::bail!("persistent worker is restarting");
        }
        match method {
            "t.init" => {
                self.0.init_calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"user_id": "@fake:srv"}))
            }
            "t.poll" => Ok(self
                .0
                .polls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| json!({"events": []}))),
            "t.send" => {
                self.0.sends.lock().unwrap().push(params);
                Ok(json!({}))
            }
            m => anyhow::bail!("unknown method {m}"),
        }
    }
}

fn fake() -> (Arc<FakeState>, Box<dyn WorkerCalls>) {
    let st = Arc::new(FakeState {
        down: AtomicBool::new(false),
        polls: Mutex::new(VecDeque::new()),
        sends: Mutex::new(Vec::new()),
        init_calls: AtomicUsize::new(0),
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
    poll_timeout_ms: 5,
};

fn spawn_test_driver(
    calls: Box<dyn WorkerCalls>,
) -> (PolledWorkerDriver, Value) {
    PolledWorkerDriver::spawn(TEST_SPEC, calls, test_parse, test_encode, ChannelId("t".into()))
        .expect("driver spawn")
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
    let res =
        PolledWorkerDriver::spawn(TEST_SPEC, calls, test_parse, test_encode, ChannelId("t".into()));
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
