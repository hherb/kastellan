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

    /// Call `matrix.init`: the worker has already logged in + first-synced before
    /// it answers any RPC (login happens in `LiveSdk::connect`, before serving),
    /// so a successful return proves the live login and yields the bot identity
    /// (`user_id`, `device_id`). Used at spawn time as the login smoke check.
    pub fn init(&mut self) -> anyhow::Result<serde_json::Value> {
        self.client
            .call("matrix.init", serde_json::json!({}))
            .map_err(|e| anyhow::anyhow!("matrix.init: {e}"))
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

/// Build the daemon's [`MatrixSpawnConfig`] from the environment, gated on
/// `KASTELLAN_MATRIX_HOMESERVER_URL` (returns `None` when unset, so the
/// Matrix-less daemon is byte-identical). `exe_dir` is the directory holding the
/// daemon binary; the worker is its sibling unless `KASTELLAN_MATRIX_WORKER_BIN`
/// overrides.
///
/// Env contract:
/// - `KASTELLAN_MATRIX_HOMESERVER_URL` (required) — e.g. `https://matrix.kastellan.dev`.
/// - `KASTELLAN_MATRIX_USER` (required) — e.g. `@kastellan:matrix.kastellan.dev`.
/// - `KASTELLAN_MATRIX_STORE` (optional) — default `<state>/matrix/store`.
/// - `KASTELLAN_MATRIX_WORKER_BIN` (optional) — default `exe_dir/kastellan-worker-matrix`.
/// - `KASTELLAN_MATRIX_ENFORCE_SANDBOX` (optional, default on) — `0`/`false` disables.
///
/// `password` is `None`: the daemon relies on the worker's persisted
/// `session.json` (do the one-time initial login with `kastellan-cli matrix
/// probe`). Materializing the password in-daemon needs the keyring initialized
/// outside the tokio runtime — a follow-up.
pub fn daemon_spawn_config_from_env(exe_dir: Option<&std::path::Path>) -> Option<MatrixSpawnConfig> {
    let homeserver_url = std::env::var("KASTELLAN_MATRIX_HOMESERVER_URL").ok()?;
    let user = std::env::var("KASTELLAN_MATRIX_USER").ok()?;
    let store_dir = std::env::var_os("KASTELLAN_MATRIX_STORE")
        .map(PathBuf::from)
        .or_else(|| {
            crate::audit_mirror::default_state_dir().map(|d| d.join("matrix").join("store"))
        })?;
    let worker_bin = std::env::var_os("KASTELLAN_MATRIX_WORKER_BIN")
        .map(PathBuf::from)
        .or_else(|| exe_dir.map(|d| d.join("kastellan-worker-matrix")))?;
    let enforce_sandbox = std::env::var("KASTELLAN_MATRIX_ENFORCE_SANDBOX")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    Some(MatrixSpawnConfig {
        worker_bin,
        homeserver_url,
        user,
        store_dir,
        password: None,
        device_name: Some("kastellan-daemon".to_string()),
        enforce_sandbox,
    })
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

/// Extract the bare host from a homeserver URL (e.g. `https://matrix.example.org`
/// → `matrix.example.org`) for the `Net::Allowlist` entry. Strips the scheme, any
/// path, and an explicit port.
pub fn host_from_url(url: &str) -> anyhow::Result<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        anyhow::bail!("could not parse host from homeserver url {url:?}");
    }
    Ok(host.to_string())
}

/// Everything `spawn_matrix_worker` needs to bring up the live worker. The
/// homeserver URL + user are operator config (env). The `password` is only used
/// for the *initial* login; once the worker has persisted `session.json` in the
/// store it restores from that, so `None` is correct on every restart. Callers
/// that materialize the password from the Vault must do so themselves (the
/// keyring's secret-service backend must be initialized *outside* a tokio
/// runtime — see `kastellan-cli`'s `matrix probe`).
pub struct MatrixSpawnConfig {
    /// Path to the (live-matrix) worker binary.
    pub worker_bin: PathBuf,
    /// Full homeserver URL, e.g. `https://matrix.kastellan.dev`.
    pub homeserver_url: String,
    /// Login user (localpart or full `@user:server`).
    pub user: String,
    /// Persistent encrypted E2E store dir (created if absent).
    pub store_dir: PathBuf,
    /// Bot password — `Some` only for the initial login (no persisted session
    /// yet); `None` relies on the restored session.
    pub password: Option<String>,
    /// Optional device display name.
    pub device_name: Option<String>,
    /// When `false`, the worker runs with seccomp + Landlock disabled — for
    /// first-bring-up / SDK-correctness smoke runs. Production passes `true`.
    pub enforce_sandbox: bool,
}

/// A spawned live Matrix worker: the [`Channel`] for the bus plus the bot
/// identity reported by `matrix.init` (login proof).
pub struct SpawnedMatrixWorker {
    pub channel: MatrixChannel,
    pub identity: serde_json::Value,
}

/// Bring up the sandboxed live Matrix worker: build the [`SandboxPolicy`]
/// (`Net::Allowlist` scoped to the homeserver, persistent store as `fs_write`),
/// spawn the worker, and block on `matrix.init` so the returned worker is
/// logged-in-and-synced. The caller owns teardown by dropping the
/// [`MatrixChannel`] (closes the worker's stdin → EOF → clean exit).
///
/// Egress force-routing (the per-worker sidecar) is **not** wired here yet — the
/// worker reaches the homeserver directly on `Net::Allowlist`. Coupling it to the
/// egress proxy (matrix is on the MITM-bypass list) is the immediate follow-up.
pub fn spawn_matrix_worker<B: SandboxBackend + ?Sized>(
    backend: &B,
    id: ChannelId,
    cfg: &MatrixSpawnConfig,
) -> anyhow::Result<SpawnedMatrixWorker> {
    // 1) Persistent store dir must exist before bwrap can bind it.
    std::fs::create_dir_all(&cfg.store_dir)
        .map_err(|e| anyhow::anyhow!("create matrix store dir {:?}: {e}", cfg.store_dir))?;

    // 2) Policy + worker env (the worker reads KASTELLAN_MATRIX_* from its jail env).
    let host = host_from_url(&cfg.homeserver_url)?;
    let mut policy =
        build_matrix_policy(cfg.worker_bin.clone(), &host, cfg.store_dir.clone(), None, None);
    policy
        .env
        .push(("KASTELLAN_MATRIX_HOMESERVER_URL".into(), cfg.homeserver_url.clone()));
    policy.env.push(("KASTELLAN_MATRIX_USER".into(), cfg.user.clone()));
    if let Some(password) = &cfg.password {
        policy.env.push(("KASTELLAN_MATRIX_PASSWORD".into(), password.clone()));
    }
    policy
        .env
        .push(("KASTELLAN_MATRIX_STORE".into(), cfg.store_dir.display().to_string()));
    if let Some(dev) = &cfg.device_name {
        policy.env.push(("KASTELLAN_MATRIX_DEVICE_NAME".into(), dev.clone()));
    }
    if !cfg.enforce_sandbox {
        policy.env.push(("KASTELLAN_SECCOMP_PROFILE".into(), "none".into()));
        policy.env.push(("KASTELLAN_LANDLOCK_PROFILE".into(), "none".into()));
    }

    // 4) Spawn + block on login (matrix.init) before handing the bus a channel.
    let program = cfg
        .worker_bin
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worker bin path not UTF-8: {:?}", cfg.worker_bin))?;
    let mut client = spawn_worker_client(backend, &policy, program, &[])?;
    let identity = client.init()?;

    Ok(SpawnedMatrixWorker {
        channel: MatrixChannel::new(id, Box::new(client)),
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn host_from_url_strips_scheme_path_and_port() {
        assert_eq!(host_from_url("https://matrix.kastellan.dev").unwrap(), "matrix.kastellan.dev");
        assert_eq!(host_from_url("https://matrix.example.org:8448/").unwrap(), "matrix.example.org");
        assert_eq!(host_from_url("http://127.0.0.1:6167").unwrap(), "127.0.0.1");
        assert_eq!(host_from_url("matrix.bare.host").unwrap(), "matrix.bare.host");
        assert!(host_from_url("https://").is_err());
    }

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
