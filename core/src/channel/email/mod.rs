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
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use kastellan_sandbox::{Net, SandboxError, SandboxPolicy};

    use super::*;

    /// Records every policy it's asked to spawn under, then always refuses —
    /// same pattern as `worker_lifecycle::force_route::tests::PolicyCapturingBackend`.
    /// `PersistentWorker::spawn_with_backoff` calls the factory synchronously
    /// once before returning, so a refusal here surfaces as `spawn_email_worker`
    /// returning `Err` — no real process, no sandbox, fully hermetic.
    struct PolicyCapturingBackend {
        policies: Mutex<Vec<SandboxPolicy>>,
    }
    impl SandboxBackend for PolicyCapturingBackend {
        fn spawn_under_policy(
            &self,
            policy: &SandboxPolicy,
            _program: &str,
            _args: &[&str],
        ) -> Result<std::process::Child, SandboxError> {
            self.policies.lock().expect("capture mutex poisoned").push(policy.clone());
            Err(SandboxError::Backend("test: spawn refused".into()))
        }
    }

    fn test_cfg() -> EmailConfig {
        EmailConfig {
            endpoint: "https://127.0.0.1:8443".into(),
            subscription: "agent-inbox".into(),
            address: "agent@example.org".into(),
            authserv_id: "mx.example.net".into(),
            token_file: PathBuf::from("/etc/kastellan/email.token"),
            worker_bin: PathBuf::from("/bin/kastellan-worker-email-in"),
        }
    }

    #[test]
    fn egress_none_calls_the_worker_backend_directly_with_the_expected_allowlist() {
        let backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
        let cfg = test_cfg();
        let err = match spawn_email_worker(backend.clone(), ChannelId("email".into()), &cfg, None, None) {
            Err(e) => e,
            Ok(_) => panic!("PolicyCapturingBackend always refuses to spawn"),
        };
        assert!(err.to_string().contains("test: spawn refused"), "{err}");

        let policies = backend.policies.lock().unwrap();
        assert_eq!(
            policies.len(),
            1,
            "egress: None must call the worker backend directly, exactly once, \
             never a sidecar's backend"
        );
        let p = &policies[0];
        assert!(
            matches!(&p.net, Net::Allowlist(v) if v == &["127.0.0.1:8443".to_string()]),
            "net = {:?}",
            p.net
        );
        assert!(
            p.proxy_uds.is_none(),
            "the direct (egress: None) path must never force-route through a proxy UDS"
        );
    }

    /// A no-op [`crate::worker_lifecycle::force_route::DecisionSinkFactory`] —
    /// proves the wiring without a live audit sink, same convention as
    /// `force_route::tests::noop_sink_factory`.
    fn noop_sink() -> crate::worker_lifecycle::force_route::DecisionSinkFactory {
        Box::new(|| Box::new(|_row: crate::egress::audit::EgressAuditRow| {}))
    }

    /// The positive half of Task 3's wiring, exercised end to end through the
    /// public entry point (not just `upstream_ca::select_ca_for_allowlist` in
    /// isolation, which only proves the first link in the chain and is already
    /// covered by `egress::upstream_ca`'s own unit tests): a configured anchor
    /// for this worker's own endpoint must reach the intercepting SIDECAR's
    /// actual `SandboxPolicy` env, built by `proxy_policy` several calls deep
    /// (`spawn_email_worker` → factory → `spawn_net_transport` →
    /// `spawn_sidecar` → `proxy_policy`). Both backends refuse to spawn
    /// (hermetic, no real process), but the sidecar backend still captures the
    /// policy it was handed before refusing — same technique as
    /// `force_route::tests::a_selected_extra_ca_reaches_the_sidecar_policy_env_and_fs_read`.
    #[test]
    fn egress_some_intercepts_and_the_selected_anchor_reaches_the_sidecar_policy() {
        let worker_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
        let sidecar_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
        let cfg = test_cfg(); // endpoint https://127.0.0.1:8443 — host is 127.0.0.1
        let ca_map = crate::egress::upstream_ca::parse_upstream_cas(
            r#"{"127.0.0.1":"/etc/kastellan/localmail.pem"}"#,
        )
        .expect("valid config");
        let scratch_root = tempfile::tempdir().expect("scratch root");
        let routing = ForceRoutingConfig::new(
            PathBuf::from("/bin/kastellan-worker-egress-proxy"),
            scratch_root.path().to_path_buf(),
            noop_sink(),
            None,
        )
        .with_upstream_cas(Some(ca_map));
        let egress = EmailEgress {
            sidecar_backend: sidecar_backend.clone() as Arc<dyn SandboxBackend>,
            routing: Arc::new(routing),
        };

        // The sidecar backend always refuses, so this always errors — the
        // point is what got CAPTURED before the refusal, not the outcome.
        let _ = spawn_email_worker(
            worker_backend.clone() as Arc<dyn SandboxBackend>,
            ChannelId("email".into()),
            &cfg,
            Some(egress),
            None,
        );

        let sidecar_policies = sidecar_backend.policies.lock().expect("capture mutex poisoned");
        let sidecar_policy =
            sidecar_policies.first().expect("the sidecar spawn must have been attempted");
        assert!(
            sidecar_policy.env.iter().any(|(k, v)| k
                == "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA"
                && v == "/etc/kastellan/localmail.pem"),
            "the selected anchor must reach the sidecar's env: {:?}",
            sidecar_policy.env
        );
        assert!(
            !sidecar_policy.env.iter().any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_DISABLE_MITM"),
            "an intercepting sidecar must NOT carry the disable-MITM key: {:?}",
            sidecar_policy.env
        );
    }

    /// `upstream_ca::select_ca_for_allowlist`'s two `Err` arms
    /// (`MixedAllowlist`, `MultipleKeyedHosts`) both require the worker's OWN
    /// allowlist to name at least two distinct hosts: `matched` is bounded by
    /// the size of that host set, and once it has exactly one member `others`
    /// is always empty (see that function's three-way match on `matched.as_slice()`).
    /// `spawn_email_worker` always builds a single-entry allowlist
    /// (`vec![format!("{host}:{port}")]`, derived once from `cfg.endpoint`), so
    /// for THIS call site `upstream_ca_for` can only ever return `Ok(None)` or
    /// `Ok(Some(_))` — never `Err` — no matter what else is in the operator's
    /// CA map. This is not a gap: the refusal arms ARE reachable and tested
    /// where a worker's allowlist genuinely has more than one host
    /// (`egress::upstream_ca`'s own unit tests select the map in isolation;
    /// `force_route::tests` exercises the full multi-host spawn path for the
    /// general-purpose net-worker case). This test pins the email-specific
    /// half of that invariant: a CA entry for a host this worker never dials
    /// is simply irrelevant to it, never a refusal.
    #[test]
    fn an_unrelated_configured_origin_does_not_reach_or_block_the_sidecar() {
        let worker_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
        let sidecar_backend = Arc::new(PolicyCapturingBackend { policies: Mutex::new(Vec::new()) });
        let cfg = test_cfg(); // endpoint host is 127.0.0.1
        let ca_map = crate::egress::upstream_ca::parse_upstream_cas(
            r#"{"10.0.0.3":"/etc/kastellan/unrelated.pem"}"#, // a different host
        )
        .expect("valid config");
        let scratch_root = tempfile::tempdir().expect("scratch root");
        let routing = ForceRoutingConfig::new(
            PathBuf::from("/bin/kastellan-worker-egress-proxy"),
            scratch_root.path().to_path_buf(),
            noop_sink(),
            None,
        )
        .with_upstream_cas(Some(ca_map));
        let egress = EmailEgress {
            sidecar_backend: sidecar_backend.clone() as Arc<dyn SandboxBackend>,
            routing: Arc::new(routing),
        };

        let _ = spawn_email_worker(
            worker_backend.clone() as Arc<dyn SandboxBackend>,
            ChannelId("email".into()),
            &cfg,
            Some(egress),
            None,
        );

        let sidecar_policies = sidecar_backend.policies.lock().expect("capture mutex poisoned");
        let sidecar_policy = sidecar_policies.first().expect(
            "an unrelated CA entry must not block the spawn from reaching the sidecar attempt",
        );
        assert!(
            !sidecar_policy
                .env
                .iter()
                .any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_UPSTREAM_EXTRA_CA"),
            "a CA keyed to a host this worker never dials must not be handed to its sidecar: {:?}",
            sidecar_policy.env
        );
        // Load-bearing design decision, pinned by mutation: this channel
        // intercepts UNCONDITIONALLY, not "when an anchor happens to be
        // configured". This call resolves to NO anchor for this worker (the
        // map has only an unrelated host), so if `spawn_email_worker` ever
        // switched to picking the posture from `upstream_extra_ca.is_some()`
        // (i.e. `Mitm::Transparent` when anchorless), this is exactly the
        // case that would flip — and `KASTELLAN_EGRESS_PROXY_DISABLE_MITM` is
        // pushed only for the transparent posture (see `proxy_policy`), so
        // its absence here is the proof the sidecar still intercepts.
        assert!(
            !sidecar_policy
                .env
                .iter()
                .any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_DISABLE_MITM"),
            "an anchorless email channel spawn must still intercept, never fall back to \
             transparent: {:?}",
            sidecar_policy.env
        );
    }
}
