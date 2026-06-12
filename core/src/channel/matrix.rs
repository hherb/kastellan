//! Core-side Matrix channel: drives the sandboxed `kastellan-worker-matrix` over
//! the blocking `kastellan-protocol` `Client` from a dedicated thread, bridged to
//! the async [`Channel`] trait via tokio mpsc.
//!
//! Why a thread: `kastellan_protocol::client::Client` is synchronous, blocking,
//! and one-request-at-a-time (strict request→response, no server-initiated
//! notifications). A Matrix client must *push* inbound events, so we keep the
//! worker a pure JSON-RPC server and put the streaming concern here: a driver
//! thread serializes `matrix.poll` + `matrix.send` on the single pipe, while the
//! mpsc buffers give the bus a cancellation-safe `recv()` and a non-blocking
//! `send()`. See
//! `docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md`.
//!
//! The production spawn path (sandbox + egress force-routing + persistent
//! encrypted E2E store + restart supervision) and the real `matrix-rust-sdk`
//! worker are comms-slice-#2 **Phase D** (built + verified on the DGX). This
//! module ships the transport-and-driver mechanism + the pure policy builder,
//! proven by `core/tests/matrix_channel_e2e.rs` against a fake-worker stub.

use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::thread;

use tokio::sync::mpsc as tok_mpsc;

use kastellan_matrix_wire::{Event, PollResult};
use kastellan_protocol::client::Client;
use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

use super::{Channel, ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerId};

/// How long the driver waits in one `matrix.poll` before looping to check the
/// outbound queue. Outbound latency is bounded by this; a few seconds is fine for
/// a single-user assistant.
pub const POLL_MS: u64 = 2000;

/// Bounded depth of the in-core inbound buffer between the driver thread and the
/// bus. Backpressure (the driver `blocking_send`s) past this — a single-user
/// channel never reaches it.
const INBOUND_BUFFER: usize = 256;

/// Seam over the worker RPC so the driver is unit-tested without spawning a
/// process. The real impl ([`ProtocolWorkerClient`]) wraps the blocking
/// `kastellan_protocol::Client`.
pub trait WorkerClient: Send {
    /// `matrix.poll` — return buffered inbound events (long-poll up to `timeout_ms`).
    fn poll(&mut self, timeout_ms: u64) -> anyhow::Result<Vec<Event>>;
    /// `matrix.send` — deliver an outbound message to a room.
    fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()>;
}

/// Real [`WorkerClient`] over the blocking `kastellan_protocol` [`Client`] — the
/// synchronous JSON-RPC pipe to the spawned matrix worker.
pub struct ProtocolWorkerClient {
    client: Client,
}

impl ProtocolWorkerClient {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl WorkerClient for ProtocolWorkerClient {
    fn poll(&mut self, timeout_ms: u64) -> anyhow::Result<Vec<Event>> {
        let v = self
            .client
            .call("matrix.poll", serde_json::json!({ "timeout_ms": timeout_ms }))
            .map_err(|e| anyhow::anyhow!("matrix.poll: {e}"))?;
        let pr: PollResult =
            serde_json::from_value(v).map_err(|e| anyhow::anyhow!("decode poll result: {e}"))?;
        Ok(pr.events)
    }
    fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()> {
        self.client
            .call(
                "matrix.send",
                serde_json::json!({ "conversation": conversation, "body": body }),
            )
            .map_err(|e| anyhow::anyhow!("matrix.send: {e}"))?;
        Ok(())
    }
}

/// Spawn the matrix worker under `backend` + `policy` and return a connected
/// [`ProtocolWorkerClient`]. Applies the same worker-side lockdown-env derivation
/// (`KASTELLAN_LANDLOCK_*` / `KASTELLAN_SECCOMP_PROFILE`) that `tool_host`'s tool
/// spawn does — the channel worker is locked down identically; it just isn't a
/// `ToolRegistry` tool and its `poll`/`send` are transport plumbing, not audited
/// dispatches, so it holds a raw `Client` rather than the dispatch-sealed
/// `SupervisedWorker`.
pub fn spawn_worker_client<B: SandboxBackend + ?Sized>(
    backend: &B,
    policy: &SandboxPolicy,
    program: &str,
    args: &[&str],
) -> anyhow::Result<ProtocolWorkerClient> {
    let derived = crate::tool_host::derive_lockdown_env(policy);
    let child = backend
        .spawn_under_policy(&derived, program, args)
        .map_err(|e| anyhow::anyhow!("spawn matrix worker: {e}"))?;
    let client = Client::from_child(child)
        .map_err(|e| anyhow::anyhow!("connect to matrix worker: {e}"))?;
    Ok(ProtocolWorkerClient::new(client))
}

/// A live Matrix channel: owns the driver thread; implements the [`Channel`]
/// trait the [`super::bus::ChannelBus`] consumes.
pub struct MatrixChannel {
    id: ChannelId,
    inbound_rx: tok_mpsc::Receiver<IncomingMessage>,
    outbound_tx: std_mpsc::Sender<OutgoingMessage>,
    // Kept so the driver thread is joined-on-drop semantics are explicit; the
    // thread exits when the worker dies or the outbound sender is dropped.
    _driver: thread::JoinHandle<()>,
}

impl MatrixChannel {
    /// Spawn the driver thread over a [`WorkerClient`]. `id` is the channel id
    /// (e.g. `"matrix"`) stamped onto every inbound message + matched on replies.
    pub fn new(id: ChannelId, mut client: Box<dyn WorkerClient>) -> Self {
        let (inbound_tx, inbound_rx) = tok_mpsc::channel::<IncomingMessage>(INBOUND_BUFFER);
        let (outbound_tx, outbound_rx) = std_mpsc::channel::<OutgoingMessage>();
        let cid = id.clone();
        let driver = thread::spawn(move || {
            loop {
                // 1) Drain any pending outbound replies (non-blocking) → matrix.send.
                loop {
                    match outbound_rx.try_recv() {
                        Ok(out) => {
                            if let Err(e) = client.send(&out.conversation.0, &out.body) {
                                tracing::warn!(error = %e, "matrix.send failed; reply dropped");
                            }
                        }
                        Err(std_mpsc::TryRecvError::Empty) => break,
                        Err(std_mpsc::TryRecvError::Disconnected) => {
                            tracing::info!("matrix outbound sender dropped; driver exiting");
                            return;
                        }
                    }
                }
                // 2) Long-poll for inbound events → push to the bus.
                match client.poll(POLL_MS) {
                    Ok(events) => {
                        for ev in events {
                            let msg = IncomingMessage {
                                channel: cid.clone(),
                                peer: PeerId(ev.peer),
                                conversation: ConversationId(ev.conversation),
                                body: ev.body,
                            };
                            if inbound_tx.blocking_send(msg).is_err() {
                                tracing::info!("matrix inbound receiver closed; driver exiting");
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "matrix.poll failed; driver exiting (worker likely died)");
                        return;
                    }
                }
            }
        });
        Self { id, inbound_rx, outbound_tx, _driver: driver }
    }
}

#[async_trait::async_trait]
impl Channel for MatrixChannel {
    fn id(&self) -> ChannelId {
        self.id.clone()
    }

    async fn recv(&mut self) -> Option<IncomingMessage> {
        // Cancellation-safe: a dropped `recv()` future (the bus `select!` losing
        // the race to an outbound) leaves any buffered event in the channel for
        // the next call.
        self.inbound_rx.recv().await
    }

    async fn send(&self, msg: OutgoingMessage) -> anyhow::Result<()> {
        self.outbound_tx
            .send(msg)
            .map_err(|e| anyhow::anyhow!("matrix outbound queue closed: {e}"))
    }
}

/// Build the [`SandboxPolicy`] for the long-lived Matrix worker. Pure +
/// unit-tested; the spawn that consumes it is Phase D.
///
/// - `Net::Allowlist([homeserver_host:443])` — the worker reaches only the
///   homeserver (via the egress proxy when `proxy_uds` is set).
/// - `Profile::WorkerNetClient` — outbound HTTPS via the proxy.
/// - `fs_read`: the worker binary + the resolver config files (DNS in-jail) +
///   the egress CA when force-routed.
/// - `fs_write`: the **persistent** E2E store dir (NOT ephemeral scratch — the
///   SDK persists device keys + sync token there across restarts).
pub fn build_matrix_policy(
    binary: PathBuf,
    homeserver_host: &str,
    store_dir: PathBuf,
    proxy_uds: Option<PathBuf>,
    egress_ca: Option<PathBuf>,
) -> SandboxPolicy {
    let mut fs_read = vec![
        binary,
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/etc/hosts"),
        PathBuf::from("/etc/nsswitch.conf"),
    ];
    if let Some(ca) = egress_ca {
        fs_read.push(ca);
    }
    SandboxPolicy {
        fs_read,
        fs_write: vec![store_dir],
        net: Net::Allowlist(vec![format!("{homeserver_host}:443")]),
        cpu_ms: 0, // long-lived; no per-process CPU cap (bounded by cgroup/quota)
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env: Vec::new(), // spawn fills env (homeserver/user/secret refs) at Phase D
        proxy_uds,
    }
}

/// Operator configuration for the Matrix channel, read from the daemon env.
/// `from_env` returns `None` when `KASTELLAN_MATRIX_HOMESERVER` is unset — the
/// daemon then starts no channel bus and is byte-identical to a Matrix-less
/// build. The actual spawn (sandbox + egress + persistent store + the live
/// matrix-rust-sdk worker) + `ChannelBus` wiring is comms-slice-#2 Phase D.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixConfig {
    /// Homeserver host (e.g. `matrix.example.org`) — used for the `Net::Allowlist`.
    pub homeserver: String,
    /// Recognised peers (the fail-closed `StaticPairings` set until slice #3's
    /// pairing flow). Empty ⇒ deny all (logged).
    pub peers: Vec<PeerId>,
}

impl MatrixConfig {
    /// Read config from the env. `None` when the homeserver is unset.
    pub fn from_env() -> Option<Self> {
        let homeserver = std::env::var("KASTELLAN_MATRIX_HOMESERVER").ok()?;
        let peers = parse_peers_csv(&std::env::var("KASTELLAN_MATRIX_PEERS").unwrap_or_default());
        Some(Self { homeserver, peers })
    }
}

/// Parse a comma-separated recognised-peer list into [`PeerId`]s, trimming
/// whitespace and dropping empty entries.
pub fn parse_peers_csv(csv: &str) -> Vec<PeerId> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| PeerId(s.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Fake WorkerClient over an injectable shared inbox (tests push events at
    /// will) + a recorded sends log. `poll` drains the inbox; empty polls sleep
    /// briefly so the driver loop doesn't spin. `fail_after` simulates worker
    /// death after N polls.
    #[derive(Clone)]
    struct FakeWorker {
        inbox: Arc<Mutex<VecDeque<Event>>>,
        sent: Arc<Mutex<Vec<(String, String)>>>,
        fail_after: Arc<Mutex<Option<usize>>>,
        polls: Arc<Mutex<usize>>,
    }
    impl FakeWorker {
        fn new() -> Self {
            Self {
                inbox: Arc::new(Mutex::new(VecDeque::new())),
                sent: Arc::new(Mutex::new(vec![])),
                fail_after: Arc::new(Mutex::new(None)),
                polls: Arc::new(Mutex::new(0)),
            }
        }
        fn push(&self, e: Event) {
            self.inbox.lock().unwrap().push_back(e);
        }
    }
    impl WorkerClient for FakeWorker {
        fn poll(&mut self, _timeout_ms: u64) -> anyhow::Result<Vec<Event>> {
            {
                let mut n = self.polls.lock().unwrap();
                *n += 1;
                if let Some(limit) = *self.fail_after.lock().unwrap() {
                    if *n > limit {
                        anyhow::bail!("simulated worker death");
                    }
                }
            }
            let drained: Vec<Event> = self.inbox.lock().unwrap().drain(..).collect();
            if drained.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(drained)
        }
        fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push((conversation.to_string(), body.to_string()));
            Ok(())
        }
    }

    fn ev(body: &str) -> Event {
        Event { conversation: "!room:srv".into(), peer: "@me:srv".into(), body: body.into() }
    }

    #[tokio::test]
    async fn poll_events_surface_on_recv_in_order() {
        let worker = FakeWorker::new();
        worker.push(ev("a"));
        worker.push(ev("b"));
        let mut ch = MatrixChannel::new(ChannelId("matrix".into()), Box::new(worker));

        let a = ch.recv().await.expect("a");
        assert_eq!(a.body, "a");
        assert_eq!(a.channel, ChannelId("matrix".into()));
        assert_eq!(a.peer, PeerId("@me:srv".into()));
        assert_eq!(a.conversation, ConversationId("!room:srv".into()));
        let b = ch.recv().await.expect("b");
        assert_eq!(b.body, "b");
    }

    #[tokio::test]
    async fn send_reaches_the_worker() {
        let worker = FakeWorker::new();
        let sent = worker.sent.clone();
        let ch = MatrixChannel::new(ChannelId("matrix".into()), Box::new(worker));

        ch.send(OutgoingMessage {
            channel: ChannelId("matrix".into()),
            peer: PeerId("@me:srv".into()),
            conversation: ConversationId("!room:srv".into()),
            body: "hello there".into(),
        })
        .await
        .expect("send queued");

        // The driver delivers within a poll cycle; poll until recorded (bounded).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some((conv, body)) = sent.lock().unwrap().first().cloned() {
                assert_eq!(conv, "!room:srv");
                assert_eq!(body, "hello there");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "send never reached worker");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn recv_is_cancellation_safe() {
        // Dropping a recv() future before it resolves must not lose a later
        // event: the next recv() still returns it. Deterministic: the inbox is
        // empty while we cancel (so recv stays pending and the timeout wins),
        // then we inject the event and recv() again.
        let worker = FakeWorker::new();
        let inbox = worker.inbox.clone();
        let mut ch = MatrixChannel::new(ChannelId("matrix".into()), Box::new(worker));

        // Inbox empty → recv() pending → the 50ms timeout wins, dropping recv().
        tokio::select! {
            _ = ch.recv() => panic!("no event queued yet; timeout must win"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        // Now inject an event; the next recv() must observe it (nothing lost).
        inbox.lock().unwrap().push_back(ev("kept"));
        let m = ch.recv().await.expect("buffered event survives cancellation");
        assert_eq!(m.body, "kept");
    }

    #[tokio::test]
    async fn poll_error_closes_the_channel() {
        let worker = FakeWorker::new();
        *worker.fail_after.lock().unwrap() = Some(0); // first poll errors
        let mut ch = MatrixChannel::new(ChannelId("matrix".into()), Box::new(worker));
        // Driver exits on the poll error → inbound sender dropped → recv() None.
        assert!(ch.recv().await.is_none());
    }

    #[test]
    fn policy_builder_shape() {
        let p = build_matrix_policy(
            PathBuf::from("/opt/kastellan/kastellan-worker-matrix"),
            "matrix.example.org",
            PathBuf::from("/var/lib/kastellan/matrix/store"),
            Some(PathBuf::from("/run/egress.sock")),
            Some(PathBuf::from("/run/ca.pem")),
        );
        assert!(matches!(p.net, Net::Allowlist(ref v) if v == &["matrix.example.org:443"]));
        assert!(matches!(p.profile, Profile::WorkerNetClient));
        assert_eq!(p.fs_write, vec![PathBuf::from("/var/lib/kastellan/matrix/store")]);
        assert!(p.fs_read.contains(&PathBuf::from("/run/ca.pem")));
        assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        assert_eq!(p.proxy_uds, Some(PathBuf::from("/run/egress.sock")));
    }

    #[test]
    fn parse_peers_csv_trims_and_drops_empties() {
        assert!(parse_peers_csv("").is_empty());
        assert!(parse_peers_csv("  , ,, ").is_empty());
        assert_eq!(
            parse_peers_csv(" @a:s , @b:s ,, @c:s "),
            vec![PeerId("@a:s".into()), PeerId("@b:s".into()), PeerId("@c:s".into())]
        );
    }

    #[test]
    fn policy_builder_omits_ca_when_not_force_routed() {
        let p = build_matrix_policy(
            PathBuf::from("/opt/k/kastellan-worker-matrix"),
            "m.example.org",
            PathBuf::from("/store"),
            None,
            None,
        );
        assert!(p.proxy_uds.is_none());
        assert!(!p.fs_read.iter().any(|x| x.to_string_lossy().contains("ca.pem")));
    }
}
