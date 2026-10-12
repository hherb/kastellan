//! Email fallback channel (Phase 2, slice #5). Inbound only in this slice.
//!
//! Design: `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`.
//!
//! Email cannot authenticate its own senders the way Matrix can (E2E +
//! homeserver auth), so this module supplies the evidence the bus needs to
//! decide: a DMARC verdict from our own MX, and a per-pairing shared token.
//! Both are computed by pure functions in [`gate`] — in core, not in the
//! worker, so every rejection still lands in `audit_log`.
//!
//! ## Layout
//!
//! Mirrors `channel/matrix.rs`'s split (each file under the project's
//! 500-LOC guideline):
//! - [`gate`] — the pure DMARC + token checks (unchanged by this module).
//! - [`wire`] — the polled-driver spec + the pure poll/send/ack codecs that
//!   turn `email-in`'s raw material into [`super::PeerEvidence`].
//! - [`policy`] — the pure [`SandboxPolicy`] builder for the email-in worker.
//! - [`config`] — the env-gated [`config::EmailConfig`] parsing.
//! - this parent — [`EmailChannel`] (the [`super::Channel`] impl) and
//!   [`spawn_email_worker`] (the orchestration that wires the two above +
//!   [`PolledWorkerDriver`] together).
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

use crate::channel::polled_driver::{AckOnlyAudit, DriverAudit, PolledWorkerDriver};
use crate::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use crate::egress::spawn::Mitm;
use crate::worker_lifecycle::force_route::ForceRoutingConfig;
use crate::worker_lifecycle::persistent::{
    ClientTransport, PersistentFactory, PersistentTransport, PersistentWorker,
};
use crate::worker_lifecycle::RestartBackoff;

use super::{Channel, ChannelId, IncomingMessage, OutgoingMessage};

mod authres_parse;
pub mod config;
pub mod gate;
pub mod policy;
pub mod wire;

use config::EmailConfig;

/// A live email channel: owns the driver thread; implements the [`Channel`]
/// trait the [`super::bus::ChannelBus`] consumes. Structurally identical to
/// `matrix::MatrixChannel` — see that type's docs for why dropping a
/// [`thread::JoinHandle`] here is fine (it detaches, doesn't join; the driver
/// thread exits on its own once both channel endpoints are dropped).
pub struct EmailChannel {
    id: ChannelId,
    inbound_rx: tok_mpsc::Receiver<IncomingMessage>,
    // Never sent through in slice 1 (`send` always bails — no outbound
    // worker yet), but must stay owned here regardless: dropping BOTH driver
    // endpoints together is what stops the driver thread (see
    // `polled_driver::run`'s step 1 `Disconnected` arm), so keeping this
    // field alive for RAII is the point even though nothing ever calls
    // `.send()` on it. Slice 2 starts reading it.
    _outbound_tx: std_mpsc::Sender<OutgoingMessage>,
    // Kept for ownership clarity only (dropping a JoinHandle detaches, it
    // does not join) — see `matrix::MatrixChannel`'s identical field.
    _driver: thread::JoinHandle<()>,
}

impl EmailChannel {
    /// Wrap a running [`PolledWorkerDriver`]'s endpoints as the bus-facing
    /// [`Channel`]. The driver (and the supervisor + worker under it) shuts
    /// down via RAII when this channel is dropped.
    pub fn from_driver(id: ChannelId, driver: PolledWorkerDriver) -> Self {
        let PolledWorkerDriver { inbound_rx, outbound_tx, join } = driver;
        Self { id, inbound_rx, _outbound_tx: outbound_tx, _driver: join }
    }
}

#[async_trait::async_trait]
impl Channel for EmailChannel {
    fn id(&self) -> ChannelId {
        self.id.clone()
    }

    async fn recv(&mut self) -> Option<IncomingMessage> {
        // Cancellation-safe: a dropped `recv()` future (the bus `select!`
        // losing the race to an outbound) leaves any buffered event in the
        // channel for the next call — same contract as MatrixChannel::recv.
        self.inbound_rx.recv().await
    }

    /// Slice 1 has no outbound worker (design §8: "Slice 2 — outbound").
    /// Always errors; the bus logs + audits the failure, same as any other
    /// delivery failure, rather than silently dropping the reply.
    async fn send(&self, _msg: OutgoingMessage) -> anyhow::Result<()> {
        anyhow::bail!("email outbound not configured (slice 2)")
    }
}

/// A spawned live email worker: the [`Channel`] for the bus plus the
/// identity `email.init` reported (configured address + subscription name).
pub struct SpawnedEmailWorker {
    pub channel: EmailChannel,
    pub identity: serde_json::Value,
}

/// Egress force-routing context for the email worker — same two fields, same
/// semantics as `matrix::MatrixEgress`, deliberately not reused as one shared
/// type (channels stay decoupled; see `matrix::MatrixEgress`'s own docs for
/// why each field is what it is). `None` ⇒ the legacy direct `Net::Allowlist`
/// path (dev / no operator opt-in). `Some` ⇒ every (re)spawn goes through a
/// 1:1 intercepting sidecar (`Mitm::Intercept` — see [`spawn_email_worker`]'s
/// docs for why, unlike Matrix's, this one always intercepts), audited
/// through the daemon's sink, AND —
/// this is the part that is not cosmetic — the worker runs in a PRIVATE netns
/// with no route out except the sidecar's UDS. A `Net::Allowlist` policy with
/// `proxy_uds: None` takes the legacy `--share-net` path instead (see
/// `linux_bwrap::build_argv`'s doc), i.e. the worker shares the HOST network
/// namespace — silently, with no error, no log line calling it out. Whatever
/// wires up the live daemon deployment of this channel MUST pass `Some` for
/// production use; only a dev/CLI probe should ever pass `None`.
pub struct EmailEgress {
    pub sidecar_backend: Arc<dyn SandboxBackend>,
    pub routing: Arc<ForceRoutingConfig>,
}

/// Email respawn backoff: 1s → 30s doubling, the same envelope
/// `matrix::matrix_backoff` uses — no reason for the two long-lived polled
/// channels to behave differently here.
fn email_backoff() -> RestartBackoff {
    RestartBackoff {
        base: Duration::from_secs(1),
        factor_num: 2,
        factor_den: 1,
        cap: Duration::from_secs(30),
    }
}

/// Bring up the sandboxed email-in worker: build the [`SandboxPolicy`]
/// (`Net::Allowlist` scoped to the localmail endpoint), spawn it (via
/// [`PersistentWorker`], respawning on death with capped backoff), and block
/// on `email.init` so the returned worker is confirmed live. `backend` is an
/// [`Arc`] so the respawn factory can outlive this call.
///
/// `egress` mirrors `matrix::spawn_matrix_worker`'s parameter of the same
/// name exactly (see [`EmailEgress`]'s docs for why `None` is unsafe in
/// production): `Some` brings up a fresh per-worker sidecar alongside the
/// worker on every (re)spawn and routes the worker through it (private
/// netns); `None` spawns the worker directly on `Net::Allowlist` (the legacy
/// path, e.g. a future `kastellan-cli email probe` diagnostic). Unlike
/// Matrix's sidecar, this one ALWAYS INTERCEPTS (`Mitm::Intercept`): it
/// terminates the worker's TLS and re-originates upstream, so an operator
/// anchor — selected once here via [`ForceRoutingConfig::upstream_ca_for`],
/// before the respawn factory is built — reaches a self-signed localmail when
/// one is configured for this worker's origin, and the plaintext leg becomes
/// MITM-visible. Interception is the *precondition* for the egress boundary's
/// credential-leak scanner, not coverage by it: this persistent-transport
/// path (unlike `egress::net_worker`'s per-tool-call one) provisions no
/// `secret_fingerprints`, so no `secret_hashes.json` ever lands in this
/// sidecar's scratch dir and the proxy's `load_patterns` finds none — it
/// fails OPEN (scans nothing), not closed. With no anchor configured the
/// upstream leg is plain webpki, the same posture every other force-routed
/// tool worker already has; the posture itself is never conditional on the
/// anchor being present.
///
/// Records `cfg.authserv_id` via [`wire::set_authserv_id`] *before* starting
/// the driver, so the very first `email.poll` result is parsed against the
/// correct trust root.
///
/// `audit_ack_only` is the same optional-hook shape as `egress`: `None` when
/// the caller has no durable sink to write to (e.g. a `kastellan-cli email
/// probe` diagnostic), `Some` to record every acked-but-never-an-event id
/// (localmail's `skipped` list) as an `audit_log` row via
/// [`crate::channel::polled_driver::AckOnlyAudit`] — see that type's docs for
/// why it is a boxed closure rather than a `PgPool` parameter, and why it must
/// not block (this module stays DB-free; the daemon wiring supplies the
/// closure, which spawns its insert — `audit_sink::SinkWriter::spawn` in the
/// daemon binary — rather than `block_on`ing it, #789).
///
/// [`SandboxPolicy`]: kastellan_sandbox::SandboxPolicy
pub fn spawn_email_worker(
    backend: Arc<dyn SandboxBackend>,
    id: ChannelId,
    cfg: &EmailConfig,
    egress: Option<EmailEgress>,
    audit_ack_only: Option<AckOnlyAudit>,
) -> anyhow::Result<SpawnedEmailWorker> {
    let (host, port) = crate::channel::matrix::host_port_from_url(&cfg.endpoint)?;

    let mut policy =
        policy::build_email_policy(cfg.worker_bin.clone(), &host, port, cfg.token_file.clone());
    policy.env.push(("KASTELLAN_EMAIL_ENDPOINT".into(), cfg.endpoint.clone()));
    policy.env.push(("KASTELLAN_EMAIL_SUBSCRIPTION".into(), cfg.subscription.clone()));
    policy.env.push(("KASTELLAN_EMAIL_ADDRESS".into(), cfg.address.clone()));
    policy
        .env
        .push(("KASTELLAN_EMAIL_TOKEN_FILE".into(), cfg.token_file.display().to_string()));

    let program = cfg
        .worker_bin
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("worker bin path not UTF-8: {:?}", cfg.worker_bin))?
        .to_string();

    // PersistentFactory: each call brings up a fresh worker — force-routed
    // through a 1:1 intercepting sidecar when `egress` is Some (the
    // sidecar + worker respawn together; decisions flow to the audit sink),
    // else a plain direct-allowlist spawn (dev / probe). The factory runs on
    // the SUPERVISOR's persistent thread (PDEATHSIG-safe, #348). Follows
    // `matrix::spawn_matrix_worker`'s branching exactly (minus the VM /
    // password-bootstrap concerns, which do not apply to this worker).
    let allowlist = vec![format!("{host}:{port}")];

    // #492's selector, reused verbatim so the channel inherits the
    // single-private-origin rule: exactly one configured origin, and it must be
    // the only host this worker can dial. Selected HERE rather than inside the
    // factory so a configuration disagreement disables the email channel once,
    // loudly, at startup — instead of failing forever inside the supervisor's
    // respawn backoff. Owned so the closure holds no borrow into the Arc.
    let upstream_extra_ca: Option<PathBuf> = match &egress {
        Some(eg) => eg
            .routing
            .upstream_ca_for(&allowlist)
            .map_err(|e| {
                anyhow::anyhow!(
                    "email channel: refusing to start — KASTELLAN_EGRESS_UPSTREAM_EXTRA_CA: {e}"
                )
            })?
            .map(|p| p.to_path_buf()),
        None => None,
    };

    let spawn_seq = AtomicU64::new(0);
    let factory: PersistentFactory = Box::new(move || match &egress {
        Some(eg) => {
            // Fresh unique scratch per spawn/respawn → fresh sidecar UDS (no
            // stale-socket reuse). RAII-cleaned by the EgressSidecar bundle.
            let seq = spawn_seq.fetch_add(1, Ordering::SeqCst);
            // Prefix shared with the startup orphan sweep (#251) so a
            // SIGKILLed daemon's leaked email scratch dirs are reclaimed on
            // the next boot; the sweep's round-trip test pins the
            // `{prefix}{pid}-{seq}` shape.
            let scratch = eg.routing.scratch_root.join(format!(
                "{}{}-{seq}",
                crate::egress::scratch_sweep::EMAIL_SCRATCH_DIR_PREFIX,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)
                .map_err(|e| anyhow::anyhow!("create email egress scratch {scratch:?}: {e}"))?;
            let params = NetTransportSpawn {
                backend: &*backend,
                sidecar_backend: &*eg.sidecar_backend,
                proxy_bin: &eg.routing.proxy_bin,
                program: &program,
                args: &[],
                base_policy: policy.clone(),
                allowlist: &allowlist,
                worker_name: "email",
                // The email channel ALWAYS intercepts: the sidecar terminates
                // the worker's TLS and re-originates upstream, so an operator
                // anchor (when configured) reaches a self-signed localmail and
                // the plaintext leg becomes MITM-visible. That is the
                // precondition for the #3b credential-leak scanner, not
                // coverage by it — this persistent-transport path provisions
                // no secret_fingerprints (only net_worker.rs's per-tool-call
                // path does), so the proxy finds no secret_hashes.json here
                // and load_patterns fails OPEN (scans nothing). With no
                // anchor the upstream leg is plain webpki — the same posture
                // every force-routed tool worker has.
                mitm: Mitm::Intercept {
                    upstream_extra_ca: upstream_extra_ca.as_deref(),
                },
                worker_extra_ca: None,
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

    // Must happen before PolledWorkerDriver::spawn: the driver thread starts
    // polling immediately, and parse_email_poll reads AUTHSERV_ID.
    wire::set_authserv_id(&cfg.authserv_id);

    let handle = PersistentWorker::spawn_with_backoff("email", factory, email_backoff())?;
    let (driver, identity) = PolledWorkerDriver::spawn(
        wire::EMAIL_POLLED_SPEC,
        Box::new(handle),
        wire::parse_email_poll,
        wire::encode_email_send,
        Some(wire::encode_email_ack),
        Some(wire::parse_email_skipped),
        DriverAudit {
            // A skipped id is always LOGGED (driver `tracing::warn!`, id +
            // reason, never body) regardless of this hook — `audit_ack_only`
            // only controls whether it ALSO becomes a durable `audit_log` row.
            ack_only: audit_ack_only,
            // No reply ever reaches this driver's send: `EmailChannel::send`
            // refuses every reply itself (outbound is slice 2), and the bus
            // audits that refusal. Wire a sink here when slice 2 routes
            // replies through `email.send`.
            reply_undelivered: None,
            // An ack channel: a batch the bus never took is not acked, so the
            // worker redelivers it, and the driver never calls this hook (#826).
            inbound_dropped: None,
        },
        id.clone(),
    )?;
    Ok(SpawnedEmailWorker { channel: EmailChannel::from_driver(id, driver), identity })
}

#[cfg(test)]
mod tests;
