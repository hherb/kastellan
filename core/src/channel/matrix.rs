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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

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

/// Filename (inside the persistent store dir) for the one-time initial-login
/// password handed to the worker out-of-band (not via argv). The worker reads it
/// via `KASTELLAN_MATRIX_PASSWORD_FILE` and consumes (deletes) it after login.
const LOGIN_PASSWORD_FILE: &str = ".login-password";

/// Write `bytes` to `path`, truncating, with `0600` permissions (owner-only) —
/// the initial-login password is a secret at rest, like the worker's session.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)
}

/// Seam over the worker RPC so the driver is unit-tested without spawning a
/// process. The real impl ([`ProtocolWorkerClient`]) wraps the blocking
/// `kastellan_protocol::Client`.
pub trait WorkerClient: Send {
    /// `matrix.poll` — return buffered inbound events (long-poll up to `timeout_ms`).
    fn poll(&mut self, timeout_ms: u64) -> anyhow::Result<Vec<Event>>;
    /// `matrix.send` — deliver an outbound message to a room.
    fn send(&mut self, conversation: &str, body: &str) -> anyhow::Result<()>;
    /// After a `poll`/`send` error signals the worker died, produce a one-line
    /// diagnostic for the daemon log — the worker's exit status + recent stderr
    /// (#348). Returns `None` when no diagnostic is available (the default; the
    /// in-process test fakes don't wrap a real process).
    fn death_report(&mut self) -> Option<String> {
        None
    }
}

/// Real [`WorkerClient`] over the blocking `kastellan_protocol` [`Client`] — the
/// synchronous JSON-RPC pipe to the spawned matrix worker.
pub struct ProtocolWorkerClient {
    client: Client,
    /// Bounded tail of the worker's recent stderr lines, retained by the drain
    /// thread so [`death_report`](WorkerClient::death_report) can surface the death
    /// cause. `None` for callers that don't drain (e.g. the e2e helper).
    stderr_tail: Option<crate::worker_stderr::StderrTail>,
}

/// How long [`ProtocolWorkerClient::death_report`] waits for the dead worker to be
/// reaped before giving up on the exit status: up to `REAP_ATTEMPTS * REAP_TICK`.
/// Bounded so a worker that is (unexpectedly) still alive can't hang the driver.
const REAP_ATTEMPTS: u32 = 10;
const REAP_TICK: Duration = Duration::from_millis(50);

impl ProtocolWorkerClient {
    /// Wrap a connected client with no stderr tail (used by the e2e helper, which
    /// owns the child's stderr itself).
    pub fn new(client: Client) -> Self {
        Self { client, stderr_tail: None }
    }

    /// Wrap a connected client together with the tail its stderr is drained into,
    /// so a death report can surface the worker's last words (#348).
    pub fn with_stderr(client: Client, stderr_tail: crate::worker_stderr::StderrTail) -> Self {
        Self { client, stderr_tail: Some(stderr_tail) }
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
    fn death_report(&mut self) -> Option<String> {
        // The driver calls this once a poll/send error indicates death. Reap the
        // child non-blockingly with a few short retries (the exit may not be
        // visible the very instant the pipe closed) so we capture the real exit
        // status — a clean `exit status: 1` (the sync-task fail-loud) vs a
        // `signal: 6` (a crypto-store SIGABRT) is exactly the signal #348 needs —
        // without risking a hang if the process is somehow still running.
        let mut status = None;
        for _ in 0..REAP_ATTEMPTS {
            match self.client.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => thread::sleep(REAP_TICK),
                Err(_) => break,
            }
        }
        let tail = self.stderr_tail.as_ref().map(|t| t.snapshot()).unwrap_or_default();
        Some(crate::worker_stderr::format_death_report(status, &tail))
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
    let mut child = backend
        .spawn_under_policy(&derived, program, args)
        .map_err(|e| anyhow::anyhow!("spawn matrix worker: {e}"))?;
    // Drain the worker's piped stderr (the JSON-RPC client reads only stdout, so an
    // undrained pipe is both discarded and a deadlock risk past ~64 KiB), retaining
    // a bounded tail so a death is diagnosable in the daemon log (#348).
    let pid = child.id();
    let stderr_tail = child
        .stderr
        .take()
        .map(|stderr| crate::worker_stderr::spawn_drain_with_tail(pid, stderr));
    let client = Client::from_child(child)
        .map_err(|e| anyhow::anyhow!("connect to matrix worker: {e}"))?;
    Ok(match stderr_tail {
        Some(tail) => ProtocolWorkerClient::with_stderr(client, tail),
        None => ProtocolWorkerClient::new(client),
    })
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

/// Produces a fresh, logged-in worker (spawns the process + `matrix.init`),
/// returning the client and its reported identity. The supervised
/// [`MatrixChannel`] driver calls this to **respawn** after a worker death.
pub type WorkerFactory =
    Box<dyn FnMut() -> anyhow::Result<(Box<dyn WorkerClient>, serde_json::Value)> + Send>;

/// Capped exponential backoff between worker respawn attempts.
const RESPAWN_BACKOFF_START: Duration = Duration::from_secs(1);
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Granularity at which the respawn backoff checks for channel shutdown, so a
/// long (up-to-30s) backoff doesn't keep a dead channel's driver thread alive.
const RESPAWN_POLL_SLICE: Duration = Duration::from_millis(200);

impl MatrixChannel {
    /// Spawn the driver thread over a [`WorkerClient`]. `id` is the channel id
    /// (e.g. `"matrix"`) stamped onto every inbound message + matched on replies.
    /// **Unsupervised:** the driver exits when the worker dies (used by tests and
    /// callers that own worker lifecycle themselves).
    pub fn new(id: ChannelId, client: Box<dyn WorkerClient>) -> Self {
        Self::spawn_driver(id, client, None)
    }

    /// Like [`new`](Self::new) but **self-healing**: when the worker dies (a
    /// `poll`/`send` error), the driver respawns it via `factory` with capped
    /// exponential backoff and resumes — so a worker crash doesn't take the
    /// channel down. Replies in flight when the worker died are retried after the
    /// respawn (no dropped replies). Inbound messages a user sends during the
    /// downtime are recovered on restart (#321): the respawned worker resumes
    /// from the SDK's persisted sync token, so its catch-up sync surfaces the
    /// messages received while it was down rather than dropping them. Only a
    /// *fresh login* (no prior token) still suppresses its catch-up backlog, to
    /// avoid replaying the whole room history.
    pub fn supervised(id: ChannelId, client: Box<dyn WorkerClient>, factory: WorkerFactory) -> Self {
        Self::spawn_driver(id, client, Some(factory))
    }

    fn spawn_driver(
        id: ChannelId,
        mut client: Box<dyn WorkerClient>,
        mut factory: Option<WorkerFactory>,
    ) -> Self {
        let (inbound_tx, inbound_rx) = tok_mpsc::channel::<IncomingMessage>(INBOUND_BUFFER);
        let (outbound_tx, outbound_rx) = std_mpsc::channel::<OutgoingMessage>();
        let cid = id.clone();
        let driver = thread::spawn(move || {
            // Replies accepted from the bus but not yet acknowledged by the worker.
            // Kept across a respawn so a death mid-send doesn't lose a reply.
            let mut pending: VecDeque<OutgoingMessage> = VecDeque::new();
            loop {
                // 1) Pull newly-queued replies into the local buffer (non-blocking).
                loop {
                    match outbound_rx.try_recv() {
                        Ok(out) => pending.push_back(out),
                        Err(std_mpsc::TryRecvError::Empty) => break,
                        Err(std_mpsc::TryRecvError::Disconnected) => {
                            tracing::info!("matrix outbound sender dropped; driver exiting");
                            return;
                        }
                    }
                }

                // 2) Flush buffered replies (front-first). Stop on the first error
                //    — the worker may have died; respawn before retrying this reply.
                let mut worker_dead = false;
                while let Some(out) = pending.front() {
                    match client.send(&out.conversation.0, &out.body) {
                        Ok(()) => {
                            pending.pop_front();
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "matrix.send failed; retrying after respawn");
                            worker_dead = true;
                            break;
                        }
                    }
                }

                // 3) Long-poll for inbound events → push to the bus.
                if !worker_dead {
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
                            tracing::warn!(error = %e, "matrix.poll failed (worker likely died)");
                            worker_dead = true;
                        }
                    }
                }

                // 4) On death: surface the cause (exit status + recent stderr) so
                //    the churn is diagnosable in the daemon log (#348), then
                //    respawn (supervised) or exit (unsupervised).
                if worker_dead {
                    if let Some(report) = client.death_report() {
                        tracing::warn!("matrix worker died: {report}");
                    }
                    let Some(factory) = factory.as_mut() else {
                        tracing::warn!("matrix worker died; driver exiting (unsupervised)");
                        return;
                    };
                    let mut delay = RESPAWN_BACKOFF_START;
                    loop {
                        // Responsive backoff: poll for shutdown in short slices
                        // rather than sleeping the full delay. If the bus dropped
                        // the inbound receiver (channel shutdown), bail instead of
                        // respawning forever against an unreachable homeserver.
                        let mut slept = Duration::ZERO;
                        while slept < delay {
                            if inbound_tx.is_closed() {
                                tracing::info!(
                                    "matrix inbound receiver closed during respawn; driver exiting"
                                );
                                return;
                            }
                            let slice = RESPAWN_POLL_SLICE.min(delay - slept);
                            thread::sleep(slice);
                            slept += slice;
                        }
                        if inbound_tx.is_closed() {
                            tracing::info!(
                                "matrix inbound receiver closed during respawn; driver exiting"
                            );
                            return;
                        }
                        match factory() {
                            Ok((fresh, _identity)) => {
                                client = fresh;
                                tracing::info!("matrix worker respawned; channel resumed");
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %format!("{e:#}"),
                                    "matrix worker respawn failed; backing off"
                                );
                                delay = (delay * 2).min(RESPAWN_BACKOFF_MAX);
                            }
                        }
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
/// - `Profile::WorkerMatrixClient` — outbound HTTPS via the proxy, plus the
///   matrix-rust-sdk SQLite-store seccomp additions (`matrix_client`).
/// - `fs_read`: the worker binary + the resolver config files (DNS in-jail) +
///   the system CA trust store (matrix-sdk 0.18 validates homeserver TLS against
///   it) + the egress CA when force-routed.
/// - `fs_write`: the **persistent** E2E store dir (NOT ephemeral scratch — the
///   SDK persists device keys + sync token there across restarts).
pub fn build_matrix_policy(
    binary: PathBuf,
    homeserver_host: &str,
    homeserver_port: u16,
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
    // matrix-sdk 0.18 validates the homeserver's TLS against the *system* trust
    // store (rustls + native certs), so the worker needs the CA bundle inside the
    // jail — without it `Client::builder().build()` fails at startup with "No CA
    // certificates were loaded from the system" and the channel never starts.
    // (matrix-sdk 0.8 used bundled webpki roots and never read these, which is why
    // this only surfaced after the 0.18 upgrade.) The worker does native
    // end-to-end TLS to the homeserver even through the egress tunnel (transparent
    // `disable_mitm`), so the system CA is needed regardless of force-routing.
    // Bind the well-known trust-store locations; `fs_read` is emitted as
    // `--ro-bind-try`, so paths absent on a given distro/OS are silently skipped.
    // `/usr/share/ca-certificates` is already covered by the `/usr` bind — these
    // are the `/etc` paths that are not.
    for ca in ["/etc/ssl/certs", "/etc/pki/tls/certs", "/etc/ssl/cert.pem"] {
        fs_read.push(PathBuf::from(ca));
    }
    if let Some(ca) = egress_ca {
        fs_read.push(ca);
    }
    SandboxPolicy {
        fs_read,
        fs_write: vec![store_dir],
        net: Net::Allowlist(vec![format!("{homeserver_host}:{homeserver_port}")]),
        cpu_ms: 0, // long-lived; no per-process CPU cap (bounded by cgroup/quota)
        mem_mb: 512,
        profile: Profile::WorkerMatrixClient,
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
/// - `KASTELLAN_MATRIX_ENFORCE_SANDBOX` (optional, default on — `matrix_client`
///   seccomp [TSYNC'd] + Landlock) — `0`/`false` is the operator debug opt-out.
///
/// `password` is `None`: the daemon relies on the worker's persisted
/// `session.json` (do the one-time initial login with `kastellan-cli matrix
/// probe`). Materializing the password in-daemon needs the keyring initialized
/// outside the tokio runtime — a follow-up.
pub fn daemon_spawn_config_from_env(exe_dir: Option<&std::path::Path>) -> Option<MatrixSpawnConfig> {
    let default_store = crate::audit_mirror::default_state_dir().map(|d| d.join("matrix").join("store"));
    parse_daemon_spawn_config(|k| std::env::var(k).ok(), exe_dir, default_store.as_deref())
}

/// Pure builder behind [`daemon_spawn_config_from_env`] over an injectable getter
/// plus resolved defaults, so the required/optional/`enforce_sandbox` contract is
/// unit-tested without mutating the process environment. `default_store` is the
/// `<state>/matrix/store` fallback; `exe_dir` sources the worker-binary fallback.
fn parse_daemon_spawn_config(
    get: impl Fn(&str) -> Option<String>,
    exe_dir: Option<&std::path::Path>,
    default_store: Option<&std::path::Path>,
) -> Option<MatrixSpawnConfig> {
    let homeserver_url = get("KASTELLAN_MATRIX_HOMESERVER_URL")?;
    let user = get("KASTELLAN_MATRIX_USER")?;
    let store_dir = get("KASTELLAN_MATRIX_STORE")
        .map(PathBuf::from)
        .or_else(|| default_store.map(|p| p.to_path_buf()))?;
    let worker_bin = get("KASTELLAN_MATRIX_WORKER_BIN")
        .map(PathBuf::from)
        .or_else(|| exe_dir.map(|d| d.join("kastellan-worker-matrix")))?;
    // Default ON (fail-safe): only an explicit `0`/`false` disables the worker's
    // seccomp + Landlock.
    let enforce_sandbox = get("KASTELLAN_MATRIX_ENFORCE_SANDBOX")
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

/// Extract `(host, port)` from a homeserver URL for the `Net::Allowlist` entry.
/// The port is the explicit `:port` if present, else the scheme default
/// (`https` → 443, `http` → 80, no scheme → 443). Strips the scheme + any path
/// and handles bracketed IPv6 literals (`https://[::1]:8448` → `("::1", 8448)`).
/// This is what scopes egress to the *actual* homeserver endpoint, so a
/// self-hosted server on a non-443 port (e.g. `:8448`) is reachable.
pub fn host_port_from_url(url: &str) -> anyhow::Result<(String, u16)> {
    let (scheme, after_scheme) = match url.split_once("://") {
        Some((s, rest)) => (Some(s), rest),
        None => (None, url),
    };
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let (host, port_str) = if let Some(rest) = authority.strip_prefix('[') {
        // [ipv6]:port → host up to the closing bracket, optional `:port` after.
        let mut parts = rest.splitn(2, ']');
        let host = parts.next().unwrap_or(rest);
        let port = parts.next().unwrap_or("").strip_prefix(':');
        (host, port)
    } else {
        // host[:port] → split on the final colon.
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        anyhow::bail!("could not parse host from homeserver url {url:?}");
    }
    let port = match port_str {
        Some(p) => p
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("invalid port in homeserver url {url:?}"))?,
        None if scheme.is_some_and(|s| s.eq_ignore_ascii_case("http")) => 80,
        None => 443,
    };
    Ok((host.to_string(), port))
}

/// Extract the bare host from a homeserver URL (e.g. `https://matrix.example.org`
/// → `matrix.example.org`), dropping the port. Thin wrapper over
/// [`host_port_from_url`].
pub fn host_from_url(url: &str) -> anyhow::Result<String> {
    Ok(host_port_from_url(url)?.0)
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
    /// When `false`, the worker runs with seccomp + Landlock disabled — an
    /// operator debug escape hatch (or SDK-correctness smoke runs). Production
    /// passes `true` (the install default): the worker then runs under the
    /// `matrix_client` seccomp profile (TSYNC'd across all threads) + Landlock.
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
/// logged-in-and-synced. The returned [`MatrixChannel`] is **supervised** — if
/// the worker later dies, the driver respawns it (capped backoff) and resumes,
/// so a worker crash doesn't take the channel down. `backend` is an [`Arc`] so
/// the respawn factory can outlive this call.
///
/// Egress force-routing (the per-worker sidecar) is **not** wired here yet — the
/// worker reaches the homeserver directly on `Net::Allowlist`. Coupling it to the
/// egress proxy (matrix is on the MITM-bypass list) is the immediate follow-up.
pub fn spawn_matrix_worker(
    backend: Arc<dyn SandboxBackend>,
    id: ChannelId,
    cfg: &MatrixSpawnConfig,
) -> anyhow::Result<SpawnedMatrixWorker> {
    // 1) Persistent store dir must exist before bwrap can bind it.
    std::fs::create_dir_all(&cfg.store_dir)
        .map_err(|e| anyhow::anyhow!("create matrix store dir {:?}: {e}", cfg.store_dir))?;

    // 2) Password (initial login only) goes via a 0600 file inside the store dir
    //    — which bwrap already binds — NOT the jail env. A `--setenv` value lands
    //    in the worker's argv (`/proc/<pid>/cmdline`, `ps`); the secret must not.
    //    The worker reads `KASTELLAN_MATRIX_PASSWORD_FILE` and consumes (deletes)
    //    it after login. `None` (the daemon's session-restore path) writes nothing.
    if let Some(password) = &cfg.password {
        let pw_path = cfg.store_dir.join(LOGIN_PASSWORD_FILE);
        write_private(&pw_path, password.as_bytes())
            .map_err(|e| anyhow::anyhow!("write matrix password file {pw_path:?}: {e}"))?;
    }

    // 3) Policy + worker env (the worker reads KASTELLAN_MATRIX_* from its jail env).
    //    The allowlist is scoped to the homeserver's actual host:port (the URL's
    //    explicit port, or the scheme default) so a non-443 server is reachable.
    let (host, port) = host_port_from_url(&cfg.homeserver_url)?;
    let mut policy =
        build_matrix_policy(cfg.worker_bin.clone(), &host, port, cfg.store_dir.clone(), None, None);
    policy
        .env
        .push(("KASTELLAN_MATRIX_HOMESERVER_URL".into(), cfg.homeserver_url.clone()));
    policy.env.push(("KASTELLAN_MATRIX_USER".into(), cfg.user.clone()));
    if cfg.password.is_some() {
        let pw_path = cfg.store_dir.join(LOGIN_PASSWORD_FILE);
        policy
            .env
            .push(("KASTELLAN_MATRIX_PASSWORD_FILE".into(), pw_path.display().to_string()));
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

    let program = cfg
        .worker_bin
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worker bin path not UTF-8: {:?}", cfg.worker_bin))?
        .to_string();

    // 4) Spawn factory: each call spawns the worker + blocks on `matrix.init`
    //    (login proof / readiness). Owns the backend + policy + program so the
    //    supervised driver can respawn after a death. NOTE: respawn relies on the
    //    persisted session — the one-time password file is consumed on first login.
    let mut spawn: WorkerFactory = Box::new(move || {
        let mut client = spawn_worker_client(&*backend, &policy, &program, &[])?;
        let identity = client.init()?;
        Ok((Box::new(client) as Box<dyn WorkerClient>, identity))
    });

    // Initial spawn (also the caller's login proof), via the same factory.
    let (client, identity) = spawn()?;

    Ok(SpawnedMatrixWorker {
        channel: MatrixChannel::supervised(id, client, spawn),
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
        // IPv6 literals: strip brackets + port.
        assert_eq!(host_from_url("https://[::1]:8448").unwrap(), "::1");
        assert_eq!(host_from_url("http://[2001:db8::1]/_matrix").unwrap(), "2001:db8::1");
        assert!(host_from_url("https://").is_err());
    }

    #[test]
    fn host_port_from_url_extracts_port_and_scheme_defaults() {
        // Scheme defaults when no explicit port.
        assert_eq!(host_port_from_url("https://matrix.kastellan.dev").unwrap(), ("matrix.kastellan.dev".into(), 443));
        assert_eq!(host_port_from_url("http://127.0.0.1").unwrap(), ("127.0.0.1".into(), 80));
        assert_eq!(host_port_from_url("matrix.bare.host").unwrap(), ("matrix.bare.host".into(), 443));
        // Explicit port wins over the scheme default — the self-hosted-on-8448 case.
        assert_eq!(host_port_from_url("https://matrix.example.org:8448/").unwrap(), ("matrix.example.org".into(), 8448));
        assert_eq!(host_port_from_url("http://127.0.0.1:6167").unwrap(), ("127.0.0.1".into(), 6167));
        // IPv6 literals, with and without an explicit port.
        assert_eq!(host_port_from_url("https://[::1]:8448").unwrap(), ("::1".into(), 8448));
        assert_eq!(host_port_from_url("http://[2001:db8::1]/_matrix").unwrap(), ("2001:db8::1".into(), 80));
        // Malformed: empty host, non-numeric port.
        assert!(host_port_from_url("https://").is_err());
        assert!(host_port_from_url("https://h:notaport").is_err());
    }

    fn daemon_get(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn daemon_cfg_none_when_required_unset() {
        let exe = std::path::Path::new("/exe");
        let st = std::path::Path::new("/st");
        // Nothing set.
        assert!(parse_daemon_spawn_config(daemon_get(&[]), Some(exe), Some(st)).is_none());
        // Homeserver set but user missing.
        let g = daemon_get(&[("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m")]);
        assert!(parse_daemon_spawn_config(g, Some(exe), Some(st)).is_none());
    }

    #[test]
    fn daemon_cfg_defaults_worker_bin_and_store_and_sandbox_on() {
        let g = daemon_get(&[
            ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m"),
            ("KASTELLAN_MATRIX_USER", "@b:m"),
        ]);
        let c = parse_daemon_spawn_config(
            g,
            Some(std::path::Path::new("/exe")),
            Some(std::path::Path::new("/st/matrix/store")),
        )
        .expect("config");
        assert_eq!(c.worker_bin, PathBuf::from("/exe/kastellan-worker-matrix"));
        assert_eq!(c.store_dir, PathBuf::from("/st/matrix/store"));
        assert!(c.enforce_sandbox, "sandbox must default ON");
        assert!(c.password.is_none(), "daemon relies on persisted session");
    }

    #[test]
    fn daemon_cfg_enforce_sandbox_off_only_for_explicit_falsey() {
        let mk = |val: &str| {
            daemon_get(&[
                ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m"),
                ("KASTELLAN_MATRIX_USER", "@b:m"),
                ("KASTELLAN_MATRIX_ENFORCE_SANDBOX", val),
            ])
        };
        let exe = std::path::Path::new("/e");
        let st = std::path::Path::new("/s");
        assert!(!parse_daemon_spawn_config(mk("0"), Some(exe), Some(st)).unwrap().enforce_sandbox);
        assert!(!parse_daemon_spawn_config(mk("false"), Some(exe), Some(st)).unwrap().enforce_sandbox);
        assert!(!parse_daemon_spawn_config(mk("FALSE"), Some(exe), Some(st)).unwrap().enforce_sandbox);
        assert!(parse_daemon_spawn_config(mk("1"), Some(exe), Some(st)).unwrap().enforce_sandbox);
    }

    #[test]
    fn daemon_cfg_env_overrides_worker_bin_and_store() {
        let g = daemon_get(&[
            ("KASTELLAN_MATRIX_HOMESERVER_URL", "https://m"),
            ("KASTELLAN_MATRIX_USER", "@b:m"),
            ("KASTELLAN_MATRIX_WORKER_BIN", "/opt/w"),
            ("KASTELLAN_MATRIX_STORE", "/data/store"),
        ]);
        // No exe_dir / default_store needed when both are overridden.
        let c = parse_daemon_spawn_config(g, None, None).expect("config");
        assert_eq!(c.worker_bin, PathBuf::from("/opt/w"));
        assert_eq!(c.store_dir, PathBuf::from("/data/store"));
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
    fn death_report_surfaces_exit_status_and_stderr() {
        // Drive a real short-lived child (writes to stderr, exits non-zero — the
        // shape of a worker death) through `ProtocolWorkerClient::with_stderr`
        // exactly as `spawn_worker_client` does, and assert the death report names
        // both the exit status and the captured stderr (#348). Hermetic: no
        // sandbox, no PG, no homeserver — just the protocol pipe + stderr drain.
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("echo worker-boom >&2; exit 3")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let tail =
            crate::worker_stderr::spawn_drain_with_tail(pid, child.stderr.take().expect("stderr"));
        let client = Client::from_child(child).expect("wrap child");
        let mut worker = ProtocolWorkerClient::with_stderr(client, tail);

        // The stderr drain runs on a background thread; poll the (idempotent)
        // report until the captured line lands, bounded so a regression can't hang.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let report = loop {
            let report = worker.death_report().expect("real client yields a report");
            if report.contains("worker-boom") || std::time::Instant::now() >= deadline {
                break report;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        assert!(report.contains("exit status: 3"), "exit status missing: {report}");
        assert!(report.contains("worker-boom"), "stderr tail missing: {report}");
    }

    #[tokio::test]
    async fn supervised_driver_respawns_after_worker_death() {
        // First worker dies after its first poll; the factory hands back a fresh
        // worker carrying a queued event. The supervised driver must respawn and
        // surface that event — i.e. the channel survives a worker death.
        let dying = FakeWorker::new();
        *dying.fail_after.lock().unwrap() = Some(1); // poll #2 errors → "death"

        let replacement = FakeWorker::new();
        replacement.push(ev("after-respawn"));

        // Factory yields the replacement exactly once, then errors (so a runaway
        // respawn loop can't mask a bug — we only expect one respawn here).
        let replacement_cell = std::sync::Mutex::new(Some(replacement));
        let factory: WorkerFactory = Box::new(move || match replacement_cell.lock().unwrap().take() {
            Some(w) => Ok((Box::new(w) as Box<dyn WorkerClient>, serde_json::json!({}))),
            None => anyhow::bail!("factory exhausted"),
        });

        let mut ch =
            MatrixChannel::supervised(ChannelId("matrix".into()), Box::new(dying), factory);

        // Bounded wait: respawn backoff is 1s, so allow a couple of seconds.
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), ch.recv())
            .await
            .expect("channel should resume within the respawn window")
            .expect("event surfaced after respawn");
        assert_eq!(got.body, "after-respawn");
    }

    #[test]
    fn policy_builder_shape() {
        let p = build_matrix_policy(
            PathBuf::from("/opt/kastellan/kastellan-worker-matrix"),
            "matrix.example.org",
            443,
            PathBuf::from("/var/lib/kastellan/matrix/store"),
            Some(PathBuf::from("/run/egress.sock")),
            Some(PathBuf::from("/run/ca.pem")),
        );
        assert!(matches!(p.net, Net::Allowlist(ref v) if v == &["matrix.example.org:443"]));
        assert!(matches!(p.profile, Profile::WorkerMatrixClient));
        assert_eq!(p.fs_write, vec![PathBuf::from("/var/lib/kastellan/matrix/store")]);
        assert!(p.fs_read.contains(&PathBuf::from("/run/ca.pem")));
        assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        // System CA trust store must be bound regardless of force-routing —
        // matrix-sdk 0.18 validates homeserver TLS against it (transparent tunnel,
        // not MITM), so its absence fails the client build at startup.
        assert!(p.fs_read.contains(&PathBuf::from("/etc/ssl/certs")));
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
            443,
            PathBuf::from("/store"),
            None,
            None,
        );
        assert!(p.proxy_uds.is_none());
        assert!(!p.fs_read.iter().any(|x| x.to_string_lossy().contains("ca.pem")));
    }
}
