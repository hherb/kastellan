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
//!
//! ## Layout (2026-07-07 prod-split, Item 9b)
//!
//! The 644-LOC original was split by concern; this parent keeps the
//! [`MatrixChannel`] type + the [`spawn_matrix_worker`] orchestration and
//! delegates cohesive chunks to siblings, re-exported below so every public
//! `matrix::…` path is byte-identical:
//! - [`wire`] — the polled-driver spec + the pure poll/send codecs.
//! - [`policy`] — the pure [`SandboxPolicy`] builders + password-file helpers.
//! - [`config`] — the env-gated config parsing + homeserver-URL parsers.
//!
//! [`SandboxPolicy`]: kastellan_sandbox::SandboxPolicy

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc as tok_mpsc;

use kastellan_sandbox::SandboxBackend;

use crate::channel::polled_driver::PolledWorkerDriver;
use crate::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use crate::worker_lifecycle::force_route::ForceRoutingConfig;
use crate::worker_lifecycle::persistent::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use crate::worker_lifecycle::RestartBackoff;

use super::{Channel, ChannelId, IncomingMessage, OutgoingMessage};

mod config;
mod policy;
mod wire;

#[cfg(test)]
mod tests;

// Public API — every path preserved byte-identical via re-export (external
// callers use `kastellan_core::channel::matrix::…`).
pub use config::{
    daemon_spawn_config_from_env, host_from_url, host_port_from_url, parse_peers_csv, MatrixConfig,
    MatrixSpawnConfig,
};
pub use policy::{build_matrix_policy, build_matrix_vm_policy};
pub use wire::{encode_matrix_send, parse_matrix_poll, MATRIX_POLLED_SPEC, POLL_MS};

// Internal helpers used by the spawn factory below on every platform. The
// linux-only helpers (`matrix_vm_password_path`, `MATRIX_MICROVM_WORKER_BIN`) and
// the test-only `parse_daemon_spawn_config` are referenced via their `policy::` /
// `config::` module path instead, so they carry no cross-platform unused-import.
use policy::{write_private, LOGIN_PASSWORD_FILE};

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
///
/// [`SandboxPolicy`]: kastellan_sandbox::SandboxPolicy
pub fn spawn_matrix_worker(
    backend: Arc<dyn SandboxBackend>,
    id: ChannelId,
    cfg: &MatrixSpawnConfig,
    egress: Option<MatrixEgress>,
) -> anyhow::Result<SpawnedMatrixWorker> {
    let (host, port) = host_port_from_url(&cfg.homeserver_url)?;

    // VM mode (Linux, opt-in) vs the 5b-4a bwrap/Seatbelt path.
    #[cfg(target_os = "linux")]
    let use_microvm = cfg.use_microvm;
    #[cfg(not(target_os = "linux"))]
    let use_microvm = false;

    // `pw_write` — Some((host_path, secret)) means the factory writes a transient
    // 0600 password file before each (re)spawn (VM bootstrap only). Non-VM mode
    // writes the file once into the bwrap-bound store_dir (existing behaviour).
    // Off Linux `use_microvm` is forced `false`, so this binding is never mutated
    // there — silence the resulting `unused_mut` (the Linux VM arm needs `mut`).
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut pw_write: Option<(PathBuf, String)> = None;

    let (mut policy, program) = if use_microvm {
        #[cfg(target_os = "linux")]
        {
            // Rootfs image dir + the persistent-store ext4 backing file live in the
            // stable microvm dir (mkfs-once, outside any run dir — 5b-2).
            let image_dir = std::env::var("KASTELLAN_MICROVM_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
            let store_image = PathBuf::from(&image_dir).join("matrix-state.ext4");
            let mut policy = build_matrix_vm_policy(&host, port, image_dir, store_image);
            // The worker writes its crypto store to the /data mount, not store_dir.
            policy.env.push(("KASTELLAN_MATRIX_STORE".into(), "/data".into()));
            if let Some(pw) = &cfg.password {
                let pw_path = policy::matrix_vm_password_path(std::process::id());
                policy.fs_read.push(pw_path.clone()); // RO-shared into the guest
                policy
                    .env
                    .push(("KASTELLAN_MATRIX_PASSWORD_FILE".into(), pw_path.display().to_string()));
                pw_write = Some((pw_path, pw.clone()));
            }
            (policy, policy::MATRIX_MICROVM_WORKER_BIN.to_string())
        }
        #[cfg(not(target_os = "linux"))]
        {
            unreachable!("use_microvm is forced false off Linux")
        }
    } else {
        // 5b-4a path — unchanged.
        std::fs::create_dir_all(&cfg.store_dir)
            .map_err(|e| anyhow::anyhow!("create matrix store dir {:?}: {e}", cfg.store_dir))?;
        if let Some(password) = &cfg.password {
            let pw_path = cfg.store_dir.join(LOGIN_PASSWORD_FILE);
            write_private(&pw_path, password.as_bytes())
                .map_err(|e| anyhow::anyhow!("write matrix password file {pw_path:?}: {e}"))?;
        }
        let mut policy =
            build_matrix_policy(cfg.worker_bin.clone(), &host, port, cfg.store_dir.clone(), None, None);
        if cfg.password.is_some() {
            let pw_path = cfg.store_dir.join(LOGIN_PASSWORD_FILE);
            policy
                .env
                .push(("KASTELLAN_MATRIX_PASSWORD_FILE".into(), pw_path.display().to_string()));
        }
        policy
            .env
            .push(("KASTELLAN_MATRIX_STORE".into(), cfg.store_dir.display().to_string()));
        let program = cfg
            .worker_bin
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("worker bin path not UTF-8: {:?}", cfg.worker_bin))?
            .to_string();
        (policy, program)
    };

    // Env common to both modes.
    policy
        .env
        .push(("KASTELLAN_MATRIX_HOMESERVER_URL".into(), cfg.homeserver_url.clone()));
    policy.env.push(("KASTELLAN_MATRIX_USER".into(), cfg.user.clone()));
    if let Some(dev) = &cfg.device_name {
        policy.env.push(("KASTELLAN_MATRIX_DEVICE_NAME".into(), dev.clone()));
    }
    if !cfg.enforce_sandbox {
        policy.env.push(("KASTELLAN_SECCOMP_PROFILE".into(), "none".into()));
        policy.env.push(("KASTELLAN_LANDLOCK_PROFILE".into(), "none".into()));
    }

    // 4) PersistentFactory: each call brings up a fresh worker — force-routed
    //    through a 1:1 transparent-tunnel sidecar when `egress` is Some (the
    //    sidecar + worker respawn together; decisions flow to the audit sink),
    //    else a plain direct-allowlist spawn (dev / probe). The factory runs on
    //    the SUPERVISOR's persistent thread (PDEATHSIG-safe, #348).
    let allowlist = vec![format!("{host}:{port}")];
    let spawn_seq = AtomicU64::new(0);
    let factory: PersistentFactory = Box::new(move || {
        // VM bootstrap: (re)write the transient 0600 password file each spawn so
        // the RO-shared fs_read path always exists at spawn time (respawn-safe).
        if let Some((pw_path, secret)) = &pw_write {
            if let Some(parent) = pw_path.parent() {
                // Owner-only (0700) — the transient plaintext password lives under
                // the shared /tmp anchor, so restrict the pid-scoped dir to the
                // daemon user (matches the private posture of the old store_dir).
                use std::os::unix::fs::DirBuilderExt as _;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(|e| anyhow::anyhow!("create matrix pw dir {parent:?}: {e}"))?;
            }
            write_private(pw_path, secret.as_bytes())
                .map_err(|e| anyhow::anyhow!("write matrix pw file {pw_path:?}: {e}"))?;
        }
        match &egress {
            Some(eg) => {
                // Fresh unique scratch per spawn/respawn → fresh sidecar UDS (no
                // stale-socket reuse). RAII-cleaned by the EgressSidecar bundle.
                let seq = spawn_seq.fetch_add(1, Ordering::SeqCst);
                // Prefix shared with the startup orphan sweep (#251) so a
                // SIGKILLed daemon's leaked matrix scratch dirs are reclaimed
                // on the next boot; the sweep's round-trip test pins the
                // `{prefix}{pid}-{seq}` shape.
                let scratch = eg.routing.scratch_root.join(format!(
                    "{}{}-{seq}",
                    crate::egress::scratch_sweep::MATRIX_SCRATCH_DIR_PREFIX,
                    std::process::id()
                ));
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
