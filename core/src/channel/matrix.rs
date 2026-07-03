//! Core-side Matrix channel: wraps the channel-generic [`PolledWorkerDriver`]
//! (poll/send/identity plumbing) over a [`PersistentWorker`]-supervised
//! transport to the sandboxed `kastellan-worker-matrix`, bridged to the async
//! [`Channel`] trait via the driver's tokio mpsc endpoints.
//!
//! Why a driver thread at all: `kastellan_protocol::client::Client` is
//! synchronous, blocking, and one-request-at-a-time (strict request→response,
//! no server-initiated notifications). A Matrix client must *push* inbound
//! events, so the driver thread serializes `matrix.poll` + `matrix.send` on the
//! single pipe, while the mpsc endpoints give the bus a cancellation-safe
//! `recv()` and a non-blocking `send()`. See
//! `docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md`.
//!
//! Spawn/respawn/backoff/alarm is owned by [`PersistentWorker`] (shared across
//! every long-lived worker, not just Matrix); this module supplies the
//! matrix-specific wire codecs ([`parse_matrix_poll`] / [`encode_matrix_send`]),
//! the [`MATRIX_POLLED_SPEC`], the [`SandboxPolicy`] builder, and the transport
//! factory — including the optional egress-sidecar force-routing
//! ([`MatrixEgress`]). Proven end-to-end by `core/tests/matrix_channel_e2e.rs`
//! against a fake-worker stub.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc as tok_mpsc;

use kastellan_matrix_wire::PollResult;
use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

use crate::channel::polled_driver::{PolledEvent, PolledWorkerDriver, PolledWorkerSpec};
use crate::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use crate::worker_lifecycle::force_route::ForceRoutingConfig;
use crate::worker_lifecycle::persistent::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use crate::worker_lifecycle::RestartBackoff;

use super::{Channel, ChannelId, IncomingMessage, OutgoingMessage, PeerId};

/// How long the driver waits in one `matrix.poll` before looping to check the
/// outbound queue. Outbound latency is bounded by this; a few seconds is fine for
/// a single-user assistant.
pub const POLL_MS: u64 = 2000;

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

/// A live Matrix channel: owns the driver thread; implements the [`Channel`]
/// trait the [`super::bus::ChannelBus`] consumes.
pub struct MatrixChannel {
    id: ChannelId,
    inbound_rx: tok_mpsc::Receiver<IncomingMessage>,
    outbound_tx: std_mpsc::Sender<OutgoingMessage>,
    // Kept for ownership clarity only (dropping a JoinHandle detaches, it does
    // not join): the driver thread exits on its own once both channel endpoints
    // above are dropped, and its RAII drop of the PersistentHandle then tears
    // down the supervisor + worker (+ sidecar).
    _driver: thread::JoinHandle<()>,
}

impl MatrixChannel {
    /// Wrap a running [`PolledWorkerDriver`]'s endpoints as the bus-facing
    /// [`Channel`]. The driver (and the supervisor + worker + sidecar under
    /// it) shuts down via RAII when this channel is dropped.
    pub fn from_driver(id: ChannelId, driver: PolledWorkerDriver) -> Self {
        let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
        Self { id, inbound_rx, outbound_tx, _driver: join }
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
        persistent_store: None,
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

/// Egress force-routing context for the matrix worker (5b-4 spec decision 2:
/// matrix rides the global `KASTELLAN_EGRESS_FORCE_ROUTING`). `None` ⇒
/// legacy direct `Net::Allowlist` (dev / CLI probe). Carries the daemon's
/// resolved [`ForceRoutingConfig`] (proxy binary, scratch root, decision-sink
/// factory) plus the HOST backend the sidecar runs under — the sidecar is the
/// real-network egress boundary; under 5b-4b the WORKER backend becomes a VM,
/// the sidecar backend never does.
pub struct MatrixEgress {
    pub sidecar_backend: Arc<dyn SandboxBackend>,
    pub routing: Arc<ForceRoutingConfig>,
}

/// Matrix respawn backoff: 1s → 30s doubling (the channel's historical envelope).
fn matrix_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_secs(1),
        factor_num: 2,
        factor_den: 1,
        cap: Duration::from_secs(30),
    }
}

/// Bring up the sandboxed live Matrix worker: build the [`SandboxPolicy`]
/// (`Net::Allowlist` scoped to the homeserver, persistent store as `fs_write`),
/// spawn the worker (via [`PersistentWorker`], respawning on death with capped
/// backoff), and block on `matrix.init` so the returned worker is
/// logged-in-and-synced. `backend` is an [`Arc`] so the respawn factory can
/// outlive this call.
///
/// `egress` is `Some` when the daemon opted into egress force-routing
/// (`KASTELLAN_EGRESS_FORCE_ROUTING`): every (re)spawn brings up a fresh
/// per-worker transparent-tunnel sidecar alongside the worker and audits its
/// routing decisions through the daemon's sink. `None` spawns the worker
/// directly on `Net::Allowlist` (the legacy path — used by the `kastellan-cli
/// matrix probe` diagnostic).
pub fn spawn_matrix_worker(
    backend: Arc<dyn SandboxBackend>,
    id: ChannelId,
    cfg: &MatrixSpawnConfig,
    egress: Option<MatrixEgress>,
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
            // On the fail-closed path the sidecar's Drop removes only the UDS,
            // not the dir (see spawn_net_transport's contract) — reclaim it
            // here, else every failed respawn in the supervisor's retry loop
            // leaks one unique scratch dir on a long-lived daemon.
            match spawn_net_transport(&params, &scratch, sink) {
                Ok(t) => Ok(Box::new(t) as Box<dyn PersistentTransport>),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&scratch);
                    Err(e)
                }
            }
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
}

#[cfg(test)]
mod tests;
