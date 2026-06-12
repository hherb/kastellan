# Matrix inbound via a sandboxed worker (comms slice #2) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or
> superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make Matrix a live, E2E, **sandboxed** inbound channel wired into the daemon: a new
`workers/matrix` worker wrapping `matrix-rust-sdk` (login + E2E sync loop + buffered inbound + three
JSON-RPC methods), a core-side `MatrixChannel` implementing the slice-#1 `Channel` trait via a
blocking driver thread, the sandbox+egress spawn path for a long-lived channel worker, and
config-gated `main.rs` wiring.

**Architecture:** see `docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md`.
The synchronous `kastellan-protocol::Client` is preserved — the worker stays a pure JSON-RPC server
(`matrix.init`/`matrix.poll`/`matrix.send`); all streaming concurrency lives in the worker's internal
async buffer + the core-side driver thread.

**Verification posture (operator decision):** tasks in **Phases A, B, C, E** are hermetic +
verifiable in any environment (fake SDK seam, fake `Client`, fake-worker stub binary — no homeserver,
no network). **Phase D** (the real `matrix-rust-sdk` impl + live tests) is built here but **verified
on the DGX / a box with a conduwuit homeserver**; its tests are `#[ignore]`/skip-as-pass elsewhere.

**Reference (read first):**
- Slice #1 — `core/src/channel/{mod,auth,ingest,route,bus}.rs` (the `Channel` trait + the four seams).
- A worker crate — `workers/web-search/{Cargo.toml,src/{main,handler}.rs}` (serve_stdio + Handler + from_env fail-closed).
- The protocol — `protocol/src/{client,server,lib}.rs` (blocking request/response `Client`; `serve_stdio`; `RpcError`/`codes`).
- The prelude — `workers/prelude` (`serve_stdio`, `lock_down`, Landlock RW/RO + seccomp `WorkerNetClient`).
- Egress coupling — `core/src/egress/{spawn,net_worker}.rs` (`spawn_sidecar`, `rewrite_worker_policy`); `web-common::ProxyConnectGet` (custom-CA rustls + CONNECT-over-UDS).
- Secrets — `core/src/secrets/vault.rs` + `db::secrets` (materialize a `secret://` ref).

**Build/test prelude:** `source "$HOME/.cargo/env"` before every cargo step.

---

## Phase A — `workers/matrix` crate: wire types, SDK seam, handler (hermetic, TDD)

### Task 1: Crate skeleton + JSON-RPC wire types + the `MatrixSdk` seam

**Files:** create `workers/matrix/Cargo.toml`, `workers/matrix/src/{main,wire,sdk}.rs`; modify root `Cargo.toml` (members).

- [ ] **Step 1: Add `"workers/matrix",`** to the workspace `members` (after `"workers/web-search",`).

- [ ] **Step 2: `workers/matrix/Cargo.toml`** — mirror `web-search`; deps `kastellan-protocol`,
  `kastellan-worker-prelude`, `serde`, `serde_json`, `anyhow`, `tokio` (rt-multi-thread + macros).
  **Do NOT add `matrix-rust-sdk` yet** — it lands in Task 7 (Phase D) so Phases A–C compile fast and
  stay hermetic. Add a `[features] live-matrix = ["dep:matrix-sdk", ...]` gate so the heavy dep is
  opt-in; the default build (and CI here) excludes it.

- [ ] **Step 3: `wire.rs`** — the serde shapes crossing the JSON-RPC boundary (pure, fully unit-tested):

```rust
//! Wire shapes for the matrix worker's JSON-RPC surface. Pure serde; the only
//! contract the core-side driver and the worker share.

use serde::{Deserialize, Serialize};

/// One decrypted inbound text message the worker surfaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub conversation: String, // room id
    pub peer: String,         // sender mxid
    pub body: String,         // decrypted plaintext
}

/// `matrix.poll` result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollResult {
    pub events: Vec<Event>,
}

/// `matrix.poll` params.
#[derive(Clone, Debug, Deserialize)]
pub struct PollParams {
    #[serde(default = "default_poll_ms")]
    pub timeout_ms: u64,
}
fn default_poll_ms() -> u64 { 2000 }

/// `matrix.send` params.
#[derive(Clone, Debug, Deserialize)]
pub struct SendParams {
    pub conversation: String,
    pub body: String,
}

/// `matrix.init` result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResult {
    pub user_id: String,
    pub device_id: String,
}
```
Unit tests: round-trip each shape; `PollParams` default; `SendParams` missing-field error.

- [ ] **Step 4: `sdk.rs`** — the **seam** that the handler depends on, so dispatch is testable
  without matrix-rust-sdk. The real impl is Task 7 (behind `live-matrix`).

```rust
//! The SDK seam: the handler talks to Matrix only through this trait, so the
//! JSON-RPC dispatch + buffering is unit-tested with a fake (no homeserver).

use crate::wire::{Event, InitResult};

/// Synchronous facade over the (internally async) matrix client. Impls
/// `block_on` the SDK calls behind these methods.
pub trait MatrixSdk: Send {
    /// Login + first-sync already done at construction; report identity.
    fn identity(&self) -> InitResult;
    /// Drain currently-buffered inbound events (the sync task fills the buffer).
    /// `timeout_ms`: if empty, wait up to this long for the first event.
    fn poll(&mut self, timeout_ms: u64) -> Vec<Event>;
    /// Send an E2E message to a room.
    fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()>;
}
```

- [ ] **Step 5: `main.rs`** stub — `mod wire; mod sdk; mod handler;` + a `fn main()` that (for now)
  returns `Ok(())` (real wiring in Task 2/7). Build + run wire unit tests.

- [ ] **Commit:** `feat(matrix-worker): crate skeleton + JSON-RPC wire types + MatrixSdk seam`.

---

### Task 2: Worker handler — `matrix.init/poll/send` dispatch + buffer cap (fake-SDK unit tests)

**Files:** create `workers/matrix/src/handler.rs`; modify `main.rs`.

- [ ] **Step 1: `handler.rs`** — a `kastellan_protocol::server::Handler` generic over `MatrixSdk`:

```rust
use kastellan_protocol::{codes, server::Handler, RpcError};
use serde_json::Value;

use crate::sdk::MatrixSdk;
use crate::wire::{PollParams, PollResult, SendParams};

pub struct MatrixHandler<S: MatrixSdk> {
    sdk: S,
}
impl<S: MatrixSdk> MatrixHandler<S> {
    pub fn new(sdk: S) -> Self { Self { sdk } }
}
impl<S: MatrixSdk> Handler for MatrixHandler<S> {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "matrix.init" => Ok(serde_json::to_value(self.sdk.identity()).unwrap()),
            "matrix.poll" => {
                let p: PollParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let events = self.sdk.poll(p.timeout_ms);
                Ok(serde_json::to_value(PollResult { events }).unwrap())
            }
            "matrix.send" => {
                let p: SendParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                self.sdk.send(&p.conversation, &p.body)
                    .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("send failed: {e}")))?;
                Ok(serde_json::json!({"ok": true}))
            }
            other => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {other}"))),
        }
    }
}
```

- [ ] **Step 2: Fake-SDK unit tests** — a `FakeSdk { identity, queued: VecDeque<Event>, sent: Vec<(String,String)> }`:
  init returns identity; poll drains queued (and returns empty when none); send records; bad params →
  INVALID_PARAMS; unknown method → METHOD_NOT_FOUND. Also pin a **buffer drop-oldest cap** helper if
  the cap lives handler-side (or in the real SDK impl — put the cap in a small pure
  `push_bounded(&mut VecDeque, Event, cap)` in `sdk.rs` and unit-test it: pushing > cap drops oldest).

- [ ] **Step 3:** run `cargo test -p kastellan-worker-matrix`; **commit**
  `feat(matrix-worker): JSON-RPC handler over the MatrixSdk seam + buffer cap`.

---

## Phase B — Core `MatrixChannel` + blocking driver (hermetic, TDD)

### Task 3 (plan) / Task 4 (spec): `MatrixChannel` over a `WorkerClient` seam

**Files:** create `core/src/channel/matrix.rs`; modify `core/src/channel/mod.rs` (`pub mod matrix;`).

- [ ] **Step 1: the `WorkerClient` seam** — abstracts the blocking `kastellan_protocol::Client` so the
  driver is unit-tested without spawning a process:

```rust
//! Core-side Matrix channel: drives the sandboxed matrix worker over the
//! blocking kastellan-protocol Client from a dedicated thread, bridged to the
//! async Channel trait via tokio mpsc. The blocking Client is one-request-at-a-
//! time, so the driver serializes poll + send on its thread; concurrency for the
//! bus is provided by the mpsc buffers (so recv() is cancellation-safe).

use std::sync::mpsc as std_mpsc;
use std::thread;

use tokio::sync::mpsc as tok_mpsc;

use super::{Channel, ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerId};
use crate::channel::matrix::wire::{Event, PollResult, SendParams}; // re-export the worker wire types or dup

/// Seam over the worker RPC. Real impl wraps `kastellan_protocol::Client`.
pub trait WorkerClient: Send {
    fn poll(&mut self, timeout_ms: u64) -> anyhow::Result<Vec<Event>>;
    fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()>;
}
```

> The `Event`/`SendParams` shapes are the worker's `wire.rs`. Either (a) factor `wire.rs` into a tiny
> shared crate `workers/matrix-wire` consumed by both worker + core (cleanest, no drift), or (b)
> duplicate the 3 structs core-side. Recommend (a) — a `kastellan-matrix-wire` lib crate (serde
> only). Decide at impl; the plan assumes (a).

- [ ] **Step 2: `MatrixChannel` + the driver thread:**

```rust
const POLL_MS: u64 = 2000;

pub struct MatrixChannel {
    id: ChannelId,
    inbound_rx: tokio::sync::Mutex<tok_mpsc::Receiver<IncomingMessage>>,
    outbound_tx: std_mpsc::Sender<OutgoingMessage>,
    _driver: thread::JoinHandle<()>,
}

impl MatrixChannel {
    /// Spawn the driver thread over a `WorkerClient`. `id` is e.g. "matrix".
    pub fn new(id: ChannelId, mut client: Box<dyn WorkerClient>) -> Self {
        let (inbound_tx, inbound_rx) = tok_mpsc::channel::<IncomingMessage>(256);
        let (outbound_tx, outbound_rx) = std_mpsc::channel::<OutgoingMessage>();
        let cid = id.clone();
        let driver = thread::spawn(move || {
            loop {
                // 1) drain outbound (non-blocking) → matrix.send
                while let Ok(out) = outbound_rx.try_recv() {
                    if let Err(e) = client.send(&out.conversation.0, &out.body) {
                        tracing::warn!(error = %e, "matrix.send failed");
                    }
                }
                // 2) poll for inbound (long-poll up to POLL_MS)
                match client.poll(POLL_MS) {
                    Ok(events) => {
                        for ev in events {
                            let msg = IncomingMessage {
                                channel: cid.clone(),
                                peer: PeerId(ev.peer),
                                conversation: ConversationId(ev.conversation),
                                body: ev.body,
                            };
                            // blocking_send: backpressure if the bus is slow.
                            if inbound_tx.blocking_send(msg).is_err() {
                                tracing::info!("matrix inbound channel closed; driver exiting");
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "matrix.poll failed; driver exiting");
                        return; // worker died — supervisor restarts the whole channel
                    }
                }
            }
        });
        Self { id, inbound_rx: tokio::sync::Mutex::new(inbound_rx), outbound_tx, _driver: driver }
    }
}

#[async_trait::async_trait]
impl Channel for MatrixChannel {
    fn id(&self) -> ChannelId { self.id.clone() }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.inbound_rx.get_mut().recv().await   // cancellation-safe: buffered in the channel
    }
    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()> {
        self.outbound_tx.send(msg).map_err(|e| anyhow::anyhow!("outbound queue closed: {e}"))?;
        Ok(())
    }
}
```

- [ ] **Step 3: driver unit tests over a fake `WorkerClient`:**
  - poll returns 2 events → two `recv()` calls yield them in order with the right channel id;
  - `send()` enqueues → the fake records the `matrix.send` (conversation,body);
  - **cancellation-safety:** start a `recv()` future, drop it (simulate the bus `select!` cancelling),
    then a fresh `recv()` still returns the buffered event (the tok_mpsc buffer holds it);
  - poll `Err` → driver exits → `recv()` eventually returns `None` (channel closed).

- [ ] **Commit:** `feat(channel): MatrixChannel + blocking driver thread over a WorkerClient seam`.

---

## Phase C — Spawn path + main.rs wiring + hermetic worker e2e

### Task 5 (plan): long-lived sandboxed channel-worker spawn + the real `WorkerClient`

**Files:** create `core/src/channel/matrix/spawn.rs` (or extend `matrix.rs`); reuse `core/src/egress`.

- [ ] **Step 1: real `WorkerClient`** wrapping `kastellan_protocol::Client` (its `call` is blocking;
  map `ClientError` → `anyhow`). `poll` = `client.call("matrix.poll", json!({"timeout_ms":…}))` →
  deserialize `PollResult`; `send` = `client.call("matrix.send", json!({conversation,body}))`.

- [ ] **Step 2: the spawn function** `spawn_matrix_worker(pool, vault, exe_dir, force_routing) ->
  anyhow::Result<(MatrixChannel, SpawnGuard)>`:
  - resolve the worker binary (sibling of `kastellan`, `current_exe()`-relative, like `discover_binary`);
  - materialize secrets from the `Vault` (homeserver URL, user, access token, recovery key, store
    passphrase) → build the jail **env**;
  - build the `SandboxPolicy`: `Net::Allowlist([homeserver_host:443])`, `Profile::WorkerNetClient`,
    `fs_read=[bin, /etc/{resolv.conf,hosts,nsswitch.conf}]`, `fs_write=[persistent store dir
    ~/.local/state/kastellan/matrix/store]`, `proxy_uds` set when force-routing;
  - **spawn the egress sidecar first** (`egress::spawn_sidecar`, wait for `ca.pem`), `rewrite_worker_policy`
    to bind the CA + set `KASTELLAN_EGRESS_PROXY_CA`/`_UDS`, then `tool_host::spawn_worker` →
    `Client::from_child`;
  - wrap the `Client` in the real `WorkerClient`, build `MatrixChannel::new`;
  - `SpawnGuard` owns the sidecar handle + worker for teardown (drop = kill both). Restart-backoff
    supervision can be a thin wrapper loop (or deferred — note it).
  - **Verifiable here:** unit-test the *pure* policy builder (`build_matrix_policy(host, store_dir,
    bin, ca?) -> SandboxPolicy`) — net entry, fs_read/fs_write sets, proxy_uds presence. The live
    spawn is exercised by the fake-worker e2e (Task 7) + the DGX live test (Task 8).

- [ ] **Commit:** `feat(channel): sandboxed long-lived matrix-worker spawn + persistent store + egress coupling`.

### Task 6 (plan): `from_env` + `main.rs` wiring (config-gated)

**Files:** `core/src/channel/matrix.rs` (`from_env`), `core/src/main.rs`.

- [ ] **Step 1:** `core::channel::matrix::from_env(pool, vault, exe_dir, force_routing) ->
  anyhow::Result<Option<(MatrixChannel, SpawnGuard, StaticPairings)>>` — returns `None` when
  `KASTELLAN_MATRIX_HOMESERVER` is unset (daemon stays byte-identical); else builds the channel +
  `StaticPairings::from_peers(parse_csv(KASTELLAN_MATRIX_PEERS))` (empty ⇒ deny-all, `warn!`).

- [ ] **Step 2: `main.rs`** — after the scheduler spawn, if `from_env` returns `Some`, build
  `PgChannelEvents::new(pool.clone())` + `PgCompletedTasks::connect(pool.clone()).await?` and
  `ChannelBus::spawn(vec![Box::new(channel)], Arc::new(pairings), Arc::new(events), Box::new(completed))`.
  Add `bus.shutdown().await` + `guard` teardown to the graceful-shutdown sequence (before scheduler).
  Gate the whole block so the absent-config path adds nothing.

- [ ] **Step 3:** unit-test `parse_csv` peer parsing + the `None`-when-unset contract; build the
  workspace (compile-gate the wiring). **Commit:** `feat(core): wire MatrixChannel + ChannelBus into the daemon (config-gated)`.

### Task 7 (plan): hermetic `matrix_channel_e2e` with a fake-worker stub binary

**Files:** create `workers/matrix/src/bin/fake_matrix_worker.rs` (or a test-only stub under
`core/tests/support/`), `core/tests/matrix_channel_e2e.rs`.

- [ ] **Step 1: a tiny fake-worker binary** speaking the three JSON-RPC methods over stdio via
  `serve_stdio` + a `FakeSdk` (canned inbound event on first poll, records sends, no network, no
  lockdown needed for the hermetic run — or lockdown with `Net::Deny` since it needs no network).

- [ ] **Step 2: the e2e** — spawn the fake worker under the **real sandbox** (`Net::Deny`, no egress)
  via `tool_host::spawn_worker`, wrap in the real `WorkerClient`, build `MatrixChannel` + the real
  `ChannelBus` with the slice-#1 **fake DB seams** (reuse the pattern from `channel_bus_e2e.rs`):
  - assert the canned inbound message round-trips: paired peer → enqueued payload captured →
    synthetic completion → `matrix.send` recorded by the fake worker;
  - **negative:** an inbound from a peer not in `StaticPairings` → no enqueue, no send.
  Proves spawn + protocol + driver + bus integration **without matrix-rust-sdk or a homeserver**.

- [ ] **Commit:** `test(channel): hermetic matrix_channel_e2e via a fake-worker stub (no homeserver)`.

---

## Phase D — Live matrix-rust-sdk integration (built here, verified on the DGX)

### Task 8 (plan = spec Task 3 + 8): real `MatrixSdk` impl + dev homeserver + `#[ignore]` live test

**Files:** `workers/matrix/src/sdk_live.rs` (behind `live-matrix`), `workers/matrix/src/main.rs`
(real wiring), `scripts/matrix/setup-conduwuit.sh`, `core/tests/matrix_live_e2e.rs` (`#[ignore]`).

- [ ] **Step 0 (SPIKE FIRST — the biggest risk):** confirm matrix-rust-sdk's HTTP client accepts the
  egress proxy: a **custom root CA** (the per-instance egress CA) + a **CONNECT-over-UDS proxy**. If
  it doesn't cleanly, take the spec's fallback (a): add the homeserver to a slice-#3 **MITM-bypass
  pin** (proxy still does allowlist + SSRF + IP-pin; no TLS interception for the homeserver — it's
  trusted infra). Decide before writing the sync loop.
- [ ] **Step 1:** add `matrix-sdk` (SQLite store, rustls) under `[features] live-matrix`; run the
  license pass on the new subtree (`cargo deny`/manual) — block any non-AGPL-compat license.
- [ ] **Step 2: `sdk_live.rs`** — `LiveSdk` impl of `MatrixSdk`: holds a tokio `Runtime`; at
  construction `block_on`s login (token/recovery from env) + opens the persistent encrypted store +
  first sync; spawns the sync task that decrypts room text events into a bounded `Mutex<VecDeque>`
  (reuse `push_bounded`); `poll` drains (with the long-poll wait); `send` `block_on`s an E2E room send.
- [ ] **Step 3: worker `main.rs`** (under `live-matrix`): build `LiveSdk` (network — through the proxy
  UDS), **then** `prelude::lock_down`, then `serve_stdio(MatrixHandler::new(sdk))`. Mirror the egress
  proxy's "do the network-needing init, THEN lock_down" ordering.
- [ ] **Step 4: `scripts/matrix/setup-conduwuit.sh`** — stand up a local conduwuit (federation-off,
  closed registration), create the bot account + an access token, print the env the daemon needs.
- [ ] **Step 5: `core/tests/matrix_live_e2e.rs`** `#[ignore]` — against the local homeserver: real
  login + E2E + send/recv a round-trip. Runs on the DGX/dev box; `#[ignore]` keeps CI green.
- [ ] **Commit:** `feat(matrix-worker): live matrix-rust-sdk MatrixSdk impl + dev homeserver + #[ignore] live e2e`.

---

## Phase E — Docs

### Task 9 (plan): license pass + threat-model + ROADMAP + HANDOVER

- [ ] License pass recorded (matrix-rust-sdk subtree AGPL-compatible; note the result).
- [ ] **threat-model.md** — add a Matrix-worker line under "Negative tests" (inbound from a peer not
  in `KASTELLAN_MATRIX_PEERS` → dropped, `channel.rejected_unpaired`, via the fake-worker e2e) and a
  note in the "Communication channel" section that the Matrix client is now sandboxed +
  egress-routed with a persistent encrypted store.
- [ ] **ROADMAP** — tick "Matrix inbound" with the branch/date + terse note; cross-reference the
  homeserver-unit item as slice #6.
- [ ] **HANDOVER** — what's green (Phases A–C + E hermetic), what's DGX-pending (Phase D live), the
  egress-MITM spike outcome, deferred slices (#3 pairing, #4 outbound, #5 email, #6 homeserver unit).
- [ ] Final gate: `cargo test -p kastellan-core channel`, `cargo test -p kastellan-worker-matrix`,
  `cargo clippy --workspace --all-targets -- -D warnings` (default features — `live-matrix` off).
- [ ] **Commit:** `docs(matrix): threat-model + ROADMAP + HANDOVER (comms slice #2)`.

---

## What this plan leaves true

Phases A–C + E deliver, and verify hermetically anywhere, the entire Matrix channel **except** the
matrix-rust-sdk network code: the worker's JSON-RPC surface + buffer, the core `MatrixChannel` +
driver, the sandbox+egress spawn path (pure policy builder unit-tested; live spawn proven by a
fake-worker e2e under the real sandbox), and the config-gated daemon wiring. Phase D adds the real
SDK + a dev homeserver + an `#[ignore]` live round-trip, built here and verified on the DGX. The
slice-#1 seams are unchanged; deferred to later slices: the pairing handshake replacing
`StaticPairings` (#3), outbound richness (#4), the email fallback (#5), and the production homeserver
supervisor unit + Tier A/B/C hardening (#6).
