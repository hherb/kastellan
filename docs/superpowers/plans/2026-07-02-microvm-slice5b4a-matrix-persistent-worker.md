# Slice 5b-4a — Matrix onto PersistentWorker + sidecar egress — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Matrix channel's bespoke `drive()`/`supervised_self_spawn` supervision with a reusable `PolledWorkerDriver` layered over the shared `PersistentWorker`, and route the Matrix worker's homeserver traffic through a transparent-tunnel egress sidecar under the global force-routing flag (closes #380).

**Architecture:** `PersistentWorker` (untouched, `core/src/worker_lifecycle/persistent.rs`) owns spawn/respawn/backoff/alarm. A new channel-generic `PolledWorkerDriver` (`core/src/channel/polled_driver.rs`) owns the long-poll loop, login-identity surfacing, and pending-outbound retention. `spawn_matrix_worker` builds a `PersistentFactory` that spawns worker+sidecar 1:1 via `spawn_net_transport` when force-routing is on, or a plain `ClientTransport` when off.

**Tech Stack:** Rust workspace; `std::thread` + `std::sync::mpsc` + `tokio::sync::mpsc` (existing patterns); no new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-02-firecracker-microvm-slice5b4-matrix-in-vm-design.md` (sub-slice 5b-4a sections).

## Global Constraints

- AGPL-compatible deps only; this plan adds **no** new dependencies.
- Cross-platform Linux + macOS: no `#[cfg(target_os)]`-gated logic in this sub-slice.
- `source "$HOME/.cargo/env"` before any cargo command (non-interactive shells).
- All commits: conventional-commit style; **stage specific files, never `git add -A`**.
- All tests green before each commit; workspace clippy `-D warnings` clean at the end.
- Keep files under ~500 LOC where feasible; use the house test-lift pattern (`mod tests;` in a `<file>/tests.rs` sibling) when a file grows past it.
- Branch: `feat/microvm-slice5b4a-matrix-persistent-worker` off current `main`.
- **`PersistentWorker`'s driver loop, `PersistentHandle`, and `RespawnRateAlarm` must not change** (spec decision 3) — the only touch to `persistent.rs` is the additive `ClientTransport::from_client` constructor and one new test.
- The live Matrix channel is production code: nothing merges without the Task 9 DGX gates.

---

### Task 0: Branch setup

**Files:** none (git only)

- [ ] **Step 1: Create the branch**

```bash
cd /Users/hherb/src/kastellan
git checkout main && git pull --ff-only
git checkout -b feat/microvm-slice5b4a-matrix-persistent-worker
```

Expected: clean branch off current `main`.

---

### Task 1: `PolledWorkerDriver` — types, spawn, identity, poll forwarding

**Files:**
- Create: `core/src/channel/polled_driver.rs`
- Create: `core/src/channel/polled_driver/tests.rs`
- Modify: `core/src/channel/mod.rs` (add `pub mod polled_driver;`)

**Interfaces:**
- Consumes: `PersistentHandle::call(&self, &str, Value) -> anyhow::Result<Value>` (`core/src/worker_lifecycle/persistent.rs:235`); `IncomingMessage`/`OutgoingMessage`/`ChannelId`/`PeerId`/`ConversationId` from `core/src/channel/mod.rs`.
- Produces (later tasks rely on these exact names):
  - `pub trait WorkerCalls: Send + 'static { fn call(&self, method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value>; }` + `impl WorkerCalls for PersistentHandle`
  - `pub struct PolledWorkerSpec { pub label: &'static str, pub init_method: &'static str, pub poll_method: &'static str, pub send_method: &'static str, pub poll_timeout_ms: u64 }`
  - `pub struct PolledEvent { pub peer: String, pub conversation: String, pub body: String }`
  - `pub type ParsePoll = fn(serde_json::Value) -> anyhow::Result<Vec<PolledEvent>>;`
  - `pub type EncodeSend = fn(&OutgoingMessage) -> serde_json::Value;`
  - `pub struct PolledWorkerDriver { pub(crate) inbound_rx: tok_mpsc::Receiver<IncomingMessage>, pub(crate) outbound_tx: std_mpsc::Sender<OutgoingMessage>, pub(crate) join: thread::JoinHandle<()> }`
  - `impl PolledWorkerDriver { pub fn spawn(spec: PolledWorkerSpec, calls: Box<dyn WorkerCalls>, parse_poll: ParsePoll, encode_send: EncodeSend, cid: ChannelId) -> anyhow::Result<(Self, serde_json::Value)> }`

- [ ] **Step 1: Write the failing tests**

Create `core/src/channel/polled_driver/tests.rs`:

```rust
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
```

At the end of the (not-yet-written) `core/src/channel/polled_driver.rs`, the
module is declared with `#[cfg(test)] mod tests;` so this file is picked up.

- [ ] **Step 2: Run tests to verify they fail to compile**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib polled_driver 2>&1 | tail -5
```

Expected: compile error — `polled_driver` module does not exist yet.

- [ ] **Step 3: Write the implementation**

Create `core/src/channel/polled_driver.rs`:

```rust
//! Channel-generic driver for a long-lived, pull-only worker supervised by
//! [`PersistentWorker`]: owns the autonomous long-poll loop, surfaces the
//! worker's login identity at startup, and retains queued outbound messages
//! across a worker respawn (no dropped replies). The supervisor underneath
//! owns spawn/respawn/backoff/alarm; this driver only *calls* the worker and
//! retries through the supervisor's `"is restarting"` window.
//!
//! Matrix is the first consumer (`channel/matrix.rs`); IMAP/Telegram channel
//! workers (Phase 2) instantiate the same driver with their own
//! [`PolledWorkerSpec`] + parse/encode fns. Design + trade-offs:
//! `docs/superpowers/specs/2026-07-02-firecracker-microvm-slice5b4-matrix-in-vm-design.md`.

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc as tok_mpsc;

use crate::worker_lifecycle::persistent::PersistentHandle;

use super::{ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerId};

/// Bounded depth of the inbound buffer between the driver thread and the bus.
/// Matches the Matrix channel's historical value; a single-user channel never
/// reaches it (the driver `blocking_send`s past it — backpressure, not drop).
const INBOUND_BUFFER: usize = 256;

/// How long the driver sleeps between retries while the worker is down (the
/// supervisor is respawning it underneath). Short so recovery latency is low;
/// the shutdown check runs every slice so a dead channel's thread exits fast.
const RETRY_SLICE: Duration = Duration::from_millis(200);

/// What a channel-shaped worker looks like to the driver: three JSON-RPC
/// methods plus the worker-side long-poll wait.
#[derive(Clone, Copy, Debug)]
pub struct PolledWorkerSpec {
    /// Log label (also a good supervisor label), e.g. `"matrix"`.
    pub label: &'static str,
    /// Identity/login-proof method, called once at spawn (e.g. `matrix.init`).
    pub init_method: &'static str,
    /// Long-poll method; params are `{"timeout_ms": <poll_timeout_ms>}`.
    pub poll_method: &'static str,
    /// Outbound-delivery method; params come from the `EncodeSend` fn.
    pub send_method: &'static str,
    /// Worker-side long-poll wait. Outbound latency is bounded by this (the
    /// single JSON-RPC pipe serializes poll and send).
    pub poll_timeout_ms: u64,
}

/// One inbound event as the channel layer sees it, before the driver stamps
/// its [`ChannelId`] on. Produced by a [`ParsePoll`] fn from the poll result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolledEvent {
    pub peer: String,
    pub conversation: String,
    pub body: String,
}

/// Decode one poll RESULT into events. A decode error marks the batch as a
/// worker bug (logged + skipped), NOT a worker death.
pub type ParsePoll = fn(serde_json::Value) -> anyhow::Result<Vec<PolledEvent>>;

/// Encode one outbound message into the send method's params.
pub type EncodeSend = fn(&OutgoingMessage) -> serde_json::Value;

/// Seam over "something that can call the worker" so the driver is unit-tested
/// without a supervisor or a process. Production is [`PersistentHandle`].
pub trait WorkerCalls: Send + 'static {
    fn call(&self, method: &str, params: serde_json::Value)
        -> anyhow::Result<serde_json::Value>;
}

impl WorkerCalls for PersistentHandle {
    fn call(&self, method: &str, params: serde_json::Value)
        -> anyhow::Result<serde_json::Value> {
        PersistentHandle::call(self, method, params)
    }
}

/// A running polled-worker driver: the endpoints a channel wraps. Dropping
/// both endpoints stops the driver thread, which drops its [`WorkerCalls`] —
/// for a [`PersistentHandle`] that is the supervisor shutdown (worker + any
/// sidecar torn down via RAII).
pub struct PolledWorkerDriver {
    pub(crate) inbound_rx: tok_mpsc::Receiver<IncomingMessage>,
    pub(crate) outbound_tx: std_mpsc::Sender<OutgoingMessage>,
    pub(crate) join: thread::JoinHandle<()>,
}

impl PolledWorkerDriver {
    /// Call `init_method` once (blocking — the synchronous login-proof
    /// contract; the returned JSON is the worker identity), then start the
    /// driver thread. Fails when init fails: the caller gets no half-alive
    /// channel. The worker process itself is parented to the SUPERVISOR's
    /// persistent thread (PDEATHSIG-safe, #348) — this call only issues RPCs.
    pub fn spawn(
        spec: PolledWorkerSpec,
        calls: Box<dyn WorkerCalls>,
        parse_poll: ParsePoll,
        encode_send: EncodeSend,
        cid: ChannelId,
    ) -> anyhow::Result<(Self, serde_json::Value)> {
        let identity = calls
            .call(spec.init_method, serde_json::json!({}))
            .map_err(|e| anyhow::anyhow!("{}: {e}", spec.init_method))?;
        let (inbound_tx, inbound_rx) = tok_mpsc::channel::<IncomingMessage>(INBOUND_BUFFER);
        let (outbound_tx, outbound_rx) = std_mpsc::channel::<OutgoingMessage>();
        let join =
            thread::spawn(move || run(calls, spec, parse_poll, encode_send, inbound_tx, outbound_rx, cid));
        Ok((Self { inbound_rx, outbound_tx, join }, identity))
    }
}

/// The driver loop. Direct port of the Matrix channel's historical `drive()`
/// semantics minus its respawn state machine (the supervisor owns that now):
/// 1. drain queued outbound messages into `pending` (non-blocking);
/// 2. flush `pending` front-first, stopping at the first error — unacked
///    messages STAY in `pending`, so a death mid-send loses nothing;
/// 3. long-poll for inbound events and forward them to the bus;
/// 4. on any call error, sleep one short slice (shutdown-responsive) and
///    retry — the supervisor is respawning the worker underneath.
fn run(
    calls: Box<dyn WorkerCalls>,
    spec: PolledWorkerSpec,
    parse_poll: ParsePoll,
    encode_send: EncodeSend,
    inbound_tx: tok_mpsc::Sender<IncomingMessage>,
    outbound_rx: std_mpsc::Receiver<OutgoingMessage>,
    cid: ChannelId,
) {
    let mut pending: VecDeque<OutgoingMessage> = VecDeque::new();
    // True while the last worker call failed — logs the down/up transitions
    // once instead of once per retry slice.
    let mut down = false;
    loop {
        // 1) Pull newly-queued replies into the local buffer (non-blocking).
        loop {
            match outbound_rx.try_recv() {
                Ok(out) => pending.push_back(out),
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    tracing::info!(label = spec.label, "outbound sender dropped; polled driver exiting");
                    return;
                }
            }
        }

        // 2) Flush buffered replies (front-first); stop at the first error.
        let mut errored = false;
        while let Some(out) = pending.front() {
            match calls.call(spec.send_method, encode_send(out)) {
                Ok(_) => {
                    if down {
                        tracing::info!(label = spec.label, "worker back up; polled driver resumed");
                        down = false;
                    }
                    pending.pop_front();
                }
                Err(e) => {
                    if !down {
                        tracing::warn!(label = spec.label, error = %e, "send failed; retrying after respawn");
                    }
                    errored = true;
                    break;
                }
            }
        }

        // 3) Long-poll for inbound events → push to the bus.
        if !errored {
            match calls.call(spec.poll_method, serde_json::json!({ "timeout_ms": spec.poll_timeout_ms })) {
                Ok(v) => {
                    if down {
                        tracing::info!(label = spec.label, "worker back up; polled driver resumed");
                        down = false;
                    }
                    match parse_poll(v) {
                        Ok(events) => {
                            for ev in events {
                                let msg = IncomingMessage {
                                    channel: cid.clone(),
                                    peer: PeerId(ev.peer),
                                    conversation: ConversationId(ev.conversation),
                                    body: ev.body,
                                };
                                if inbound_tx.blocking_send(msg).is_err() {
                                    tracing::info!(label = spec.label, "inbound receiver closed; polled driver exiting");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            // A malformed poll result is a worker bug, not a
                            // death — log + skip the batch, keep polling.
                            tracing::warn!(label = spec.label, error = %e, "poll result decode failed; batch skipped");
                        }
                    }
                }
                Err(e) => {
                    if !down {
                        tracing::warn!(label = spec.label, error = %e, "poll failed (worker died or restarting)");
                    }
                    errored = true;
                }
            }
        }

        // 4) Worker down: the supervisor owns respawn/backoff/alarm; just wait
        //    a short, shutdown-responsive slice and retry.
        if errored {
            down = true;
            if inbound_tx.is_closed() {
                tracing::info!(label = spec.label, "inbound receiver closed during retry; polled driver exiting");
                return;
            }
            thread::sleep(RETRY_SLICE);
        }
    }
}

#[cfg(test)]
mod tests;
```

Add to `core/src/channel/mod.rs` (next to the other module declarations):

```rust
pub mod polled_driver;
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p kastellan-core --lib polled_driver
```

Expected: 4 passed (`spawn_surfaces_identity_via_one_init_call`, `polled_events_are_forwarded_as_incoming_messages`, `malformed_poll_result_is_skipped_not_fatal`, `init_failure_fails_spawn`).

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/polled_driver.rs core/src/channel/polled_driver/tests.rs core/src/channel/mod.rs
git commit -m "feat(channel): channel-generic PolledWorkerDriver over PersistentWorker (5b-4a)"
```

---

### Task 2: `PolledWorkerDriver` — pending retention, down-window retry, shutdown

**Files:**
- Modify: `core/src/channel/polled_driver/tests.rs` (add tests only — the Task 1 implementation already contains the logic; these tests pin it)

**Interfaces:** unchanged (tests only).

- [ ] **Step 1: Write the tests**

Append to `core/src/channel/polled_driver/tests.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests**

```bash
cargo test -p kastellan-core --lib polled_driver
```

Expected: 8 passed. If `pending_send_is_retained_across_a_down_window_and_delivered_once` or a shutdown test fails, fix `run()` (Task 1 step 3 logic), not the test.

- [ ] **Step 3: Commit**

```bash
git add core/src/channel/polled_driver/tests.rs
git commit -m "test(channel): pin PolledWorkerDriver pending-retention + shutdown semantics"
```

---

### Task 3: Infrastructure — decision sink through `spawn_net_transport`, `ClientTransport::from_client`, supervisor thread-parent test

**Files:**
- Modify: `core/src/egress/persistent_net.rs` (add `on_decision` param)
- Modify: `core/tests/net_demo_egress_e2e.rs` (caller update: add `, |_row| {}`)
- Modify: `core/tests/net_demo_firecracker_egress_e2e.rs` (caller update: add `, |_row| {}`)
- Modify: `core/src/worker_lifecycle/persistent.rs` (add `from_client`; add one test)

**Interfaces:**
- Consumes: `EgressAuditRow` (`core/src/egress/audit.rs`, already imported by `net_worker.rs`); `spawn_ingest_thread` (`core/src/egress/net_worker.rs:338`).
- Produces:
  - `pub fn spawn_net_transport(params: &NetTransportSpawn<'_>, scratch: &Path, on_decision: impl FnMut(EgressAuditRow) + Send + 'static) -> anyhow::Result<NetClientTransport>`
  - `pub fn ClientTransport::from_client(client: Client) -> ClientTransport`

- [ ] **Step 1: Change `spawn_net_transport` to accept the sink**

In `core/src/egress/persistent_net.rs`: add the import and change the signature + ingest line. The sidecar's per-CONNECT allow/deny decisions currently vanish into a hard-coded no-op; production consumers (matrix) must be able to audit them like every other force-routed worker does.

```rust
use super::audit::EgressAuditRow;
```

Signature (doc comment gains one line explaining the param; demos/tests pass `|_row| {}`):

```rust
pub fn spawn_net_transport(
    params: &NetTransportSpawn<'_>,
    scratch: &Path,
    on_decision: impl FnMut(EgressAuditRow) + Send + 'static,
) -> anyhow::Result<NetClientTransport> {
```

and replace the hard-coded sink line:

```rust
    let ingest = spawn_ingest_thread(stdout, on_decision);
```

- [ ] **Step 2: Fix the two callers**

In `core/tests/net_demo_egress_e2e.rs` and `core/tests/net_demo_firecracker_egress_e2e.rs`, every `spawn_net_transport(&params, &scratch)` becomes:

```rust
let t = spawn_net_transport(&params, &scratch, |_row| {})?;
```

(Locate with `grep -n "spawn_net_transport(" core/tests/*.rs` — update every call site found.)

- [ ] **Step 3: Add `ClientTransport::from_client`**

In `core/src/worker_lifecycle/persistent.rs`, inside `impl ClientTransport` after `spawn`:

```rust
    /// Wrap an ALREADY-CONNECTED client (no sandbox spawn) — the hermetic-test
    /// path over a plain child process. No stderr tail ⇒ death reports carry
    /// exit status only.
    pub fn from_client(client: Client) -> Self {
        Self { client, stderr_tail: None }
    }
```

- [ ] **Step 4: Add the supervisor thread-parent regression test**

The Matrix channel's `supervised_self_spawn_runs_initial_spawn_off_the_caller_thread` test (deleted in Task 4) pins the #348 PDEATHSIG invariant; it moves here, against the shared supervisor. Append to `core/src/worker_lifecycle/persistent.rs`'s `mod tests`:

```rust
    /// #348 invariant: the initial factory() call — which forks the worker, so
    /// bwrap's --die-with-parent PDEATHSIG binds to the calling THREAD — must
    /// run on the persistent driver thread, never the (possibly ephemeral,
    /// e.g. tokio spawn_blocking) caller thread.
    #[test]
    fn initial_spawn_runs_on_the_driver_thread_not_the_caller() {
        let caller = thread::current().id();
        let (tid_tx, tid_rx) = mpsc::channel();
        let factory: PersistentFactory = Box::new(move || {
            let _ = tid_tx.send(thread::current().id());
            Ok(Box::new(FakeTransport { calls: 0, die_after: 1000, gen: 0 }))
        });
        let h = PersistentWorker::spawn("thread-parent-test", factory).unwrap();
        let spawn_thread = tid_rx.recv().unwrap();
        assert_ne!(spawn_thread, caller, "initial factory() must run on the driver thread (#348)");
        h.shutdown();
    }
```

- [ ] **Step 5: Run the affected tests**

```bash
cargo test -p kastellan-core --lib persistent
cargo test -p kastellan-core --lib egress::persistent_net
cargo build -p kastellan-core --tests
```

Expected: all pass; the two e2e files compile (they're `#[ignore]`/skip-gated, compiling is the gate here).

- [ ] **Step 6: Commit**

```bash
git add core/src/egress/persistent_net.rs core/tests/net_demo_egress_e2e.rs core/tests/net_demo_firecracker_egress_e2e.rs core/src/worker_lifecycle/persistent.rs
git commit -m "feat(egress): decision sink param on spawn_net_transport + ClientTransport::from_client (5b-4a)"
```

---

### Task 4: Matrix adopts the driver — rewrite `channel/matrix.rs`

**Files:**
- Modify: `core/src/channel/matrix.rs` (major: deletions + rewiring)
- Possibly create: `core/src/channel/matrix/tests.rs` (house test-lift if the file stays >500 LOC after the deletions)

**Interfaces:**
- Consumes (from Tasks 1–3): `PolledWorkerDriver::spawn`, `PolledWorkerSpec`, `PolledEvent`, `WorkerCalls`, `spawn_net_transport(params, scratch, on_decision)`, `ClientTransport::spawn`, `PersistentWorker::spawn_with_backoff`, `PersistentFactory`, `RestartBackoff` (`core/src/worker_lifecycle/idle_timeout.rs:31`), `NetTransportSpawn` (`core/src/egress/persistent_net.rs:28`), `ForceRoutingConfig` fields `proxy_bin`/`scratch_root`/`make_sink` (pub(crate), same crate).
- Produces (Tasks 5–6 rely on):
  - `pub const MATRIX_POLLED_SPEC: PolledWorkerSpec`
  - `pub fn parse_matrix_poll(v: serde_json::Value) -> anyhow::Result<Vec<PolledEvent>>`
  - `pub fn encode_matrix_send(msg: &OutgoingMessage) -> serde_json::Value`
  - `pub struct MatrixEgress { pub sidecar_backend: Arc<dyn SandboxBackend>, pub routing: Arc<ForceRoutingConfig> }`
  - `impl MatrixChannel { pub fn from_driver(id: ChannelId, driver: PolledWorkerDriver) -> Self }`
  - `pub fn spawn_matrix_worker(backend: Arc<dyn SandboxBackend>, id: ChannelId, cfg: &MatrixSpawnConfig, egress: Option<MatrixEgress>) -> anyhow::Result<SpawnedMatrixWorker>`
- **Deleted** (nothing may reference these afterward): `WorkerClient` (trait), `ProtocolWorkerClient`, `spawn_worker_client`, `WorkerFactory`, `MatrixChannel::new`, `MatrixChannel::supervised`, `MatrixChannel::supervised_self_spawn`, `MatrixChannel::driver_channels`, `MatrixChannel::spawn_driver`, `drive()`, consts `INBOUND_BUFFER`, `RESPAWN_BACKOFF_START`, `RESPAWN_BACKOFF_MAX`, `RESPAWN_POLL_SLICE`, `RESPAWN_ALARM_THRESHOLD`, `RESPAWN_ALARM_WINDOW`, `REAP_ATTEMPTS`, `REAP_TICK`. (`POLL_MS` stays — `MATRIX_POLLED_SPEC` uses it. The `respawn_alarm` module stays — `persistent.rs` consumes it; its thresholds are now defined ONCE, #380 acceptance item 2.)

- [ ] **Step 1: Write the failing tests (new pure fns)**

Add to `matrix.rs`'s `mod tests` (or the lifted sibling):

```rust
    #[test]
    fn parse_matrix_poll_decodes_wire_events() {
        let v = serde_json::json!({"events": [
            {"peer": "@me:srv", "conversation": "!room:srv", "body": "hi"}
        ]});
        let evs = parse_matrix_poll(v).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].peer, "@me:srv");
        assert_eq!(evs[0].conversation, "!room:srv");
        assert_eq!(evs[0].body, "hi");
        assert!(parse_matrix_poll(serde_json::json!("garbage")).is_err());
    }

    #[test]
    fn encode_matrix_send_matches_the_wire_shape() {
        let msg = OutgoingMessage {
            channel: ChannelId("matrix".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "pong".into(),
        };
        assert_eq!(
            encode_matrix_send(&msg),
            serde_json::json!({"conversation": "!room:srv", "body": "pong"})
        );
    }
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p kastellan-core --lib channel::matrix 2>&1 | tail -5
```

Expected: compile error — `parse_matrix_poll` not defined.

- [ ] **Step 3: Implement the rewiring**

(a) New imports at the top of `matrix.rs` (replacing the removed `Client`/`VecDeque`/`Instant`/`RespawnRateAlarm` imports):

```rust
use std::sync::atomic::{AtomicU64, Ordering};

use crate::channel::polled_driver::{PolledEvent, PolledWorkerDriver, PolledWorkerSpec};
use crate::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use crate::worker_lifecycle::force_route::ForceRoutingConfig;
use crate::worker_lifecycle::persistent::{ClientTransport, PersistentFactory, PersistentWorker, PersistentTransport};
use crate::worker_lifecycle::RestartBackoff;
```

(b) The spec constant + wire fns (place where `WorkerClient` used to be):

```rust
/// The matrix instantiation of the channel-generic polled driver.
pub const MATRIX_POLLED_SPEC: PolledWorkerSpec = PolledWorkerSpec {
    label: "matrix",
    init_method: "matrix.init",
    poll_method: "matrix.poll",
    send_method: "matrix.send",
    poll_timeout_ms: POLL_MS,
};

/// Decode a `matrix.poll` result (wire [`PollResult`]) into driver events.
pub fn parse_matrix_poll(v: serde_json::Value) -> anyhow::Result<Vec<PolledEvent>> {
    let pr: PollResult =
        serde_json::from_value(v).map_err(|e| anyhow::anyhow!("decode poll result: {e}"))?;
    Ok(pr
        .events
        .into_iter()
        .map(|e| PolledEvent { peer: e.peer, conversation: e.conversation, body: e.body })
        .collect())
}

/// Encode an outbound message as `matrix.send` params.
pub fn encode_matrix_send(msg: &OutgoingMessage) -> serde_json::Value {
    serde_json::json!({ "conversation": msg.conversation.0, "body": msg.body })
}
```

(Adjust the `use kastellan_matrix_wire::…` line: `Event` is no longer referenced directly — keep `PollResult`.)

(c) `MatrixChannel` keeps its fields; constructors are replaced by:

```rust
impl MatrixChannel {
    /// Wrap a running [`PolledWorkerDriver`]'s endpoints as the bus-facing
    /// [`Channel`]. The driver (and the supervisor + worker + sidecar under
    /// it) shuts down via RAII when this channel is dropped.
    pub fn from_driver(id: ChannelId, driver: PolledWorkerDriver) -> Self {
        let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
        Self { id, inbound_rx, outbound_tx, _driver: join }
    }
}
```

(d) Matrix's respawn envelope (preserves the historical 1 s → 30 s doubling
that the deleted `RESPAWN_BACKOFF_*` consts encoded; the supervisor's default
caps at 60 s, which would double the worst-case blocked-call window during an
extended homeserver outage):

```rust
/// Matrix respawn backoff: 1s → 30s doubling (the channel's historical envelope).
fn matrix_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_secs(1),
        factor_num: 2,
        factor_den: 1,
        cap: Duration::from_secs(30),
    }
}
```

(e) The egress context type:

```rust
/// Egress force-routing context for the matrix worker (5b-4 spec decision 2:
/// matrix rides the global `KASTELLAN_EGRESS_FORCE_ROUTING`). `None` ⇒ legacy
/// direct `Net::Allowlist` (dev / CLI probe). Carries the daemon's resolved
/// [`ForceRoutingConfig`] (proxy binary, scratch root, decision-sink factory)
/// plus the HOST backend the sidecar runs under — the sidecar is the
/// real-network egress boundary; under 5b-4b the WORKER backend becomes a VM,
/// the sidecar backend never does.
pub struct MatrixEgress {
    pub sidecar_backend: Arc<dyn SandboxBackend>,
    pub routing: Arc<ForceRoutingConfig>,
}
```

(f) `spawn_matrix_worker` — steps 1–3 (store dir, password file, policy+env,
`program`) stay byte-identical; steps 4–5 are replaced by:

```rust
    // 4) PersistentFactory: each call brings up a fresh worker — force-routed
    //    through a 1:1 transparent-tunnel sidecar when `egress` is Some (the
    //    sidecar + worker respawn together; decisions flow to the audit sink),
    //    else a plain direct-allowlist spawn (dev / probe). The factory runs on
    //    the SUPERVISOR's persistent thread (PDEATHSIG-safe, #348).
    let allowlist = vec![format!("{host}:{port}")];
    let spawn_seq = AtomicU64::new(0);
    let factory: PersistentFactory = Box::new(move || match &egress {
        Some(eg) => {
            // Fresh unique scratch per spawn/respawn → fresh sidecar UDS (no
            // stale-socket reuse). RAII-cleaned by the EgressSidecar bundle.
            let seq = spawn_seq.fetch_add(1, Ordering::SeqCst);
            let scratch = eg
                .routing
                .scratch_root
                .join(format!("matrix-{}-{seq}", std::process::id()));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)
                .map_err(|e| anyhow::anyhow!("create matrix egress scratch {scratch:?}: {e}"))?;
            let params = NetTransportSpawn {
                backend: &*backend,
                sidecar_backend: &*eg.sidecar_backend,
                proxy_bin: &eg.routing.proxy_bin,
                program: &program,
                args: &[],
                base_policy: policy.clone(),
                allowlist: &allowlist,
                worker_name: "matrix",
                extra_ca: None,
            };
            let sink = (eg.routing.make_sink)();
            let t = spawn_net_transport(&params, &scratch, sink)?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        }
        None => {
            let t = ClientTransport::spawn(&*backend, &policy, &program, &[])?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        }
    });

    // 5) Shared supervisor owns spawn/respawn/backoff/alarm; the polled driver
    //    owns poll/identity/pending-retention. `PolledWorkerDriver::spawn`
    //    blocks on `matrix.init` — the synchronous login-proof contract the
    //    daemon and CLI rely on. Respawns need no re-init: the worker logs in
    //    (or restores its session) inside `LiveSdk::connect` before serving.
    let handle = PersistentWorker::spawn_with_backoff("matrix", factory, matrix_backoff())?;
    let (driver, identity) = PolledWorkerDriver::spawn(
        MATRIX_POLLED_SPEC,
        Box::new(handle),
        parse_matrix_poll,
        encode_matrix_send,
        id.clone(),
    )?;
    Ok(SpawnedMatrixWorker { channel: MatrixChannel::from_driver(id, driver), identity })
```

with the new signature:

```rust
pub fn spawn_matrix_worker(
    backend: Arc<dyn SandboxBackend>,
    id: ChannelId,
    cfg: &MatrixSpawnConfig,
    egress: Option<MatrixEgress>,
) -> anyhow::Result<SpawnedMatrixWorker> {
```

(The doc comment's "Egress force-routing … is **not** wired here yet" paragraph
at `matrix.rs:733-735` is replaced by a sentence describing the `egress` param.)

(g) Delete everything in the **Deleted** list above. Then delete every test in
`mod tests` that no longer compiles (they reference `FakeWorkerClient` /
`drive` / `supervised*` / `ProtocolWorkerClient`); keep the pure-fn tests
(`host_from_url*`, `host_port_from_url*`, `daemon_cfg_*`, `parse_peers_csv` if
present, `build_matrix_policy` tests) and the two new tests from Step 1. The
deleted respawn/pending/thread-parent coverage lives on in
`polled_driver/tests.rs` (Tasks 1–2) and `persistent.rs` (Task 3 step 4).

- [ ] **Step 4: Run the module tests + check the file size**

```bash
cargo test -p kastellan-core --lib channel::matrix
wc -l core/src/channel/matrix.rs
```

Expected: all remaining tests pass. If `matrix.rs` is still meaningfully over
500 lines, do the house test-lift: move the whole `mod tests` body to
`core/src/channel/matrix/tests.rs` and leave `#[cfg(test)] mod tests;` in the
parent (production lines byte-identical), then re-run the same test command.

- [ ] **Step 5: Commit**

```bash
git add core/src/channel/matrix.rs core/src/channel/matrix/tests.rs 2>/dev/null || git add core/src/channel/matrix.rs
git commit -m "feat(matrix): adopt PolledWorkerDriver + PersistentWorker, delete bespoke drive() (closes #380, 5b-4a)"
```

(Note: `core` no longer compiles workspace-wide — `main.rs`, the CLI, and the
e2e still call the old API. Tasks 5–6 fix them; this commit is fine mid-branch
as long as `--lib` tests pass. If you prefer every commit workspace-green,
fold Tasks 4–6 into one commit at the end of Task 6 instead.)

---

### Task 5: Daemon + CLI call-site wiring

**Files:**
- Modify: `core/src/main.rs` (matrix spawn block, ~lines 374–430)
- Modify: `core/src/bin/kastellan-cli/matrix.rs` (probe spawn, ~lines 185–215)

**Interfaces:**
- Consumes: `MatrixEgress`, 4-arg `spawn_matrix_worker` (Task 4); `force_routing: Option<Arc<ForceRoutingConfig>>` already resolved at `core/src/main.rs:124` via `force_route::from_env`.

- [ ] **Step 1: Daemon passes the egress context**

In `core/src/main.rs`, inside the matrix spawn block: after the per-OS
`backend` selection, build the egress option from the daemon's already-resolved
force-routing config, and pass it through. The `backend` binding gains an
explicit trait-object type so both uses (worker + sidecar) coerce once:

```rust
        #[cfg(target_os = "linux")]
        let backend: Arc<dyn kastellan_sandbox::SandboxBackend> = Arc::clone(&sandboxes.bwrap);
        #[cfg(target_os = "macos")]
        let backend: Arc<dyn kastellan_sandbox::SandboxBackend> = Arc::clone(&sandboxes.seatbelt);
        // Matrix rides the global force-routing flag (5b-4 spec decision 2):
        // when the daemon resolved a ForceRoutingConfig, the matrix worker gets
        // a per-worker transparent-tunnel sidecar; decisions audit to PG via
        // the same sink every force-routed worker uses. In 5b-4a the sidecar
        // backend equals the worker backend (both host jails).
        let egress = force_routing.as_ref().map(|fr| {
            kastellan_core::channel::matrix::MatrixEgress {
                sidecar_backend: Arc::clone(&backend),
                routing: Arc::clone(fr),
            }
        });
        let spawn = tokio::task::spawn_blocking(move || {
            kastellan_core::channel::matrix::spawn_matrix_worker(
                backend,
                kastellan_core::channel::ChannelId("matrix".to_string()),
                &spawn_cfg,
                egress,
            )
        });
```

(If `force_routing` from `main.rs:124` has been moved/consumed before this
block, bind an `Option<Arc<ForceRoutingConfig>>` clone of it earlier; do NOT
re-resolve from env.)

- [ ] **Step 2: CLI probe stays direct**

In `core/src/bin/kastellan-cli/matrix.rs`, the probe call becomes:

```rust
    // The probe is an operator diagnostic: it spawns direct-allowlist (no
    // egress sidecar) so a sidecar/DNS problem can be distinguished from an
    // SDK/login problem. The daemon path is the force-routed one.
    let SpawnedMatrixWorker { mut channel, identity } =
        match spawn_matrix_worker(backend, ChannelId("matrix".to_string()), &cfg, None) {
```

- [ ] **Step 3: Build + run the daemon-adjacent tests**

```bash
cargo build -p kastellan-core --bins
cargo test -p kastellan-core --lib
```

Expected: both bins compile; lib tests green.

- [ ] **Step 4: Commit**

```bash
git add core/src/main.rs core/src/bin/kastellan-cli/matrix.rs
git commit -m "feat(daemon): route matrix through the egress sidecar under force-routing (5b-4a)"
```

---

### Task 6: Migrate the hermetic e2e (`matrix_channel_e2e`)

**Files:**
- Modify: `core/tests/matrix_channel_e2e.rs` (the `spawn_matrix_channel` helper + imports; the two `#[tokio::test]`s stay untouched)

**Interfaces:**
- Consumes: `ClientTransport::from_client` (Task 3), `PersistentWorker::spawn` / `PersistentFactory` / `PersistentTransport`, `PolledWorkerDriver::spawn`, `MATRIX_POLLED_SPEC`, `parse_matrix_poll`, `encode_matrix_send`, `MatrixChannel::from_driver` (Task 4). The `fake_matrix_worker` fixture already answers `matrix.init` with `{"user_id": "@bot:srv", "device_id": "FAKE"}` (`workers/matrix/examples/fake_matrix_worker.rs:31`) — no fixture change needed.

- [ ] **Step 1: Rewrite the helper**

Replace the import block's matrix items and the helper:

```rust
use kastellan_core::channel::matrix::{
    encode_matrix_send, parse_matrix_poll, MatrixChannel, MATRIX_POLLED_SPEC,
};
use kastellan_core::channel::polled_driver::PolledWorkerDriver;
use kastellan_core::worker_lifecycle::persistent::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
```

```rust
/// Spawn the fixture worker (plain child, piped stdio) through the PRODUCTION
/// stack: PersistentWorker (supervision) + PolledWorkerDriver (poll/identity/
/// pending) + MatrixChannel — everything but the sandbox and the sidecar.
fn spawn_matrix_channel(sent_file: &PathBuf, peer: &str) -> Option<MatrixChannel> {
    let bin = fixture_bin();
    if !bin.exists() {
        eprintln!(
            "\n[SKIP] fixture not built: {} — run `cargo build -p kastellan-worker-matrix --examples`\n",
            bin.display()
        );
        return None;
    }
    let sent_file = sent_file.clone();
    let peer = peer.to_string();
    let factory: PersistentFactory = Box::new(move || {
        let child = Command::new(&bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env("FAKE_MATRIX_SENT", &sent_file)
            .env("FAKE_MATRIX_PEER", &peer)
            .env("FAKE_MATRIX_ROOM", "!room:srv")
            .env("FAKE_MATRIX_BODY", "hello from peer")
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn fake matrix worker: {e}"))?;
        let client = Client::from_child(child)
            .map_err(|e| anyhow::anyhow!("connect to fake worker: {e}"))?;
        Ok(Box::new(ClientTransport::from_client(client)) as Box<dyn PersistentTransport>)
    });
    let handle = PersistentWorker::spawn("matrix-e2e", factory).expect("persistent spawn");
    let (driver, identity) = PolledWorkerDriver::spawn(
        MATRIX_POLLED_SPEC,
        Box::new(handle),
        parse_matrix_poll,
        encode_matrix_send,
        ChannelId("matrix".into()),
    )
    .expect("polled driver spawn");
    assert_eq!(identity["user_id"], "@bot:srv", "matrix.init identity must surface");
    Some(MatrixChannel::from_driver(ChannelId("matrix".into()), driver))
}
```

- [ ] **Step 2: Build the fixture and run the e2e**

```bash
cargo build -p kastellan-worker-matrix --examples
cargo test -p kastellan-core --test matrix_channel_e2e -- --nocapture
```

Expected: 2 passed, no `[SKIP]` lines (fixture exists). This is the proof the restructure preserves `Channel` semantics end-to-end.

- [ ] **Step 3: Commit**

```bash
git add core/tests/matrix_channel_e2e.rs
git commit -m "test(matrix): migrate hermetic channel e2e onto the PersistentWorker + PolledWorkerDriver stack"
```

---

### Task 7: macOS workspace verification

**Files:** none (verification only; fix anything it surfaces)

- [ ] **Step 1: Full build, tests, clippy**

```bash
source "$HOME/.cargo/env"
cargo build --workspace
cargo test --workspace 2>&1 | tail -20
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: build clean; tests green (standing macOS gotcha: a full-workspace run under `KASTELLAN_PG_BIN_DIR` may flake ~4 `embedding_recall_e2e` PG-bring-up tests — skip-as-pass per HANDOVER; run them individually if in doubt); clippy zero warnings.

- [ ] **Step 2: Commit any fixes, then push the branch**

```bash
git push -u origin feat/microvm-slice5b4a-matrix-persistent-worker
```

---

### Task 8: Docs + PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (header + session block per its own checklist)
- Modify: `docs/devel/ROADMAP.md` (5b-4 line: mark the 5b-4a half, keep 5b-4b open)

- [ ] **Step 1: Update HANDOVER.md + ROADMAP.md** per the "How to update this document at session end" checklist in HANDOVER.md (header fields first, test counts, Next TODO = 5b-4b).

- [ ] **Step 2: Commit + open the PR**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): 5b-4a matrix PersistentWorker adoption session"
git push
gh pr create --title "feat(matrix): slice 5b-4a — Matrix onto shared PersistentWorker + transparent-tunnel sidecar egress (closes #380)" --body "$(cat <<'EOF'
## Summary
- New channel-generic `PolledWorkerDriver` (`core/src/channel/polled_driver.rs`): long-poll loop, login-identity surfacing, pending-outbound retention across respawns — layered over the untouched `PersistentWorker`.
- Matrix channel adopts it: bespoke `drive()`/`supervised_self_spawn`/`spawn_worker_client`/`ProtocolWorkerClient` deleted; respawn/backoff/alarm now live ONCE in the shared supervisor. Closes #380.
- Matrix egress joins the global `KASTELLAN_EGRESS_FORCE_ROUTING`: per-worker transparent-tunnel sidecar via `spawn_net_transport` (worker keeps end-to-end TLS; sidecar decisions audit to PG through the standard sink). CLI probe stays direct (diagnostic).
- `spawn_net_transport` gains a decision-sink param; `ClientTransport::from_client` for hermetic tests.

Spec: `docs/superpowers/specs/2026-07-02-firecracker-microvm-slice5b4-matrix-in-vm-design.md` (sub-slice 5b-4a). Sub-slice 5b-4b (matrix-in-a-VM) follows separately.

## Test plan
- [ ] `polled_driver` unit suite (identity, forwarding, pending retention, shutdown)
- [ ] `persistent.rs` #348 thread-parent regression test
- [ ] Hermetic `matrix_channel_e2e` on the new stack (both platforms)
- [ ] macOS workspace test + clippy clean
- [ ] DGX workspace gate (real bwrap/KVM/live-PG)
- [ ] DGX LIVE matrix round-trip force-routed (sidecar DNS risk check) + `matrix_restart_recovers_downtime_message`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

### Task 9: DGX gates (merge blockers)

**Files:** none (remote verification)

Run from the Mac via `ssh dgx '<cmd>'` (exact prefix form — the allow rule is a prefix match). Logs go to `~` on the DGX, never `/tmp` (scrubbed mid-run).

- [ ] **Step 1: Workspace gate on the branch**

```bash
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout feat/microvm-slice5b4a-matrix-persistent-worker && git pull --ff-only'
ssh dgx 'cd ~/src/kastellan && setsid bash -lc "source ~/.cargo/env; cd ~/src/kastellan; { cargo build --workspace && cargo test --workspace; echo TEST_EXIT=\$?; cargo clippy --workspace --all-targets -- -D warnings; echo CLIPPY_EXIT=\$?; echo DONE_EXIT; } > ~/dgx-5b4a-gate.log 2>&1" </dev/null & echo STARTED'
# poll:
ssh dgx 'grep -E "TEST_EXIT|CLIPPY_EXIT|DONE_EXIT" ~/dgx-5b4a-gate.log; grep "^test result" ~/dgx-5b4a-gate.log | awk -F"[ ;]" "{p+=\$4; f+=\$7; i+=\$10} END {print p\" / \"f\" / \"i}"'
```

Expected: `TEST_EXIT=0`, `CLIPPY_EXIT=0`, pass count ≥ the 2266 main baseline + the new driver tests, 0 failed.

- [ ] **Step 2: Live matrix e2e (SDK correctness, unsandboxed — unchanged path)**

Env per `core/tests/matrix_live_e2e.rs:67-80` (bot + peer creds against `matrix.kastellan.dev`; the operator's standing values live in the DGX's env setup):

```bash
ssh dgx 'cd ~/src/kastellan && bash -lc "source ~/.cargo/env; KASTELLAN_MATRIX_LIVE_E2E=1 cargo test -p kastellan-core --test matrix_live_e2e -- --ignored --nocapture"'
```

Expected: `matrix_send_recv_round_trip` + `matrix_restart_recovers_downtime_message` (#321) both pass.

- [ ] **Step 3: Live FORCE-ROUTED daemon round-trip (the decision-2 risk check)**

On the DGX, restart the supervised daemon with `KASTELLAN_EGRESS_FORCE_ROUTING=1` in `~/.config/kastellan/kastellan.env` (it is the install default — verify it's present), deploy the branch build (`scripts/upgrade_from_git.sh` flow), send a DM from the operator account, and verify:
- the reply arrives (worker reached the homeserver THROUGH the sidecar);
- `audit_log` gained egress decision rows for worker `matrix` (allow verdicts for the homeserver, `tls_intercepted=false`);
- `journalctl --user -u kastellan-core` shows no sidecar-DNS failures.
If the sidecar cannot resolve `matrix.kastellan.dev` (the known DGX resolver
quirk), that is an environment fix (resolver config for the sidecar netns),
not a code rollback — document the finding in HANDOVER.md either way.

- [ ] **Step 4: Merge** once Steps 1–3 are green and the PR is reviewed.

---

## Self-review notes (kept for the implementer)

- **Spec coverage:** 5b-4a.1 → Tasks 1–2; 5b-4a.2 → Task 4; 5b-4a.3 → Tasks 3–5; 5b-4a.4 → Tasks 2, 3, 6, 9. The `#380` acceptance items: shared spawn sequence = `ClientTransport::spawn` is now the only spawn path (the duplicate `spawn_worker_client` is deleted, Task 4); alarm thresholds defined once = matrix's `RESPAWN_ALARM_*` consts deleted (Task 4); matrix delegates to `PersistentWorker` (Task 4).
- **Type consistency:** `PolledWorkerDriver::spawn` returns `(Self, serde_json::Value)` everywhere (Tasks 1, 4, 6); `spawn_net_transport` is 3-arg everywhere (Tasks 3, 4, and both net-demo e2e); `spawn_matrix_worker` is 4-arg everywhere (Tasks 4, 5).
- **Known post-merge deltas:** DGX baseline will exceed 2266 (new driver + persistent tests); update HANDOVER counts from the Task 9 log, not from memory.
