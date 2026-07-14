//! Force-routing config + the pure decision helpers that drive the slice-#2
//! "live auto-flip" (Task 4.4).
//!
//! When an operator opts in (`KASTELLAN_EGRESS_FORCE_ROUTING=1`) and the
//! egress-proxy binary resolves, the daemon builds a [`ForceRoutingConfig`] and
//! hands it to the lifecycle managers. On every **cold spawn** of a worker whose
//! policy declares [`Net::Allowlist`], the manager then routes the worker through
//! a per-worker egress-proxy sidecar (see
//! [`crate::egress::net_worker::spawn_forced_net_worker`]) instead of spawning it
//! directly. With no config (`None`) the spawn path is **byte-identical** to the
//! pre-Task-4.4 behaviour, so a deployment that doesn't opt in is unaffected.
//!
//! Security posture: opting in is **fail-closed**. If the operator enables
//! force-routing but the proxy binary cannot be found, [`resolve_force_routing`]
//! returns `Err` rather than silently falling back to direct (unrouted) egress —
//! the daemon refuses to start rather than run net workers without their
//! containment boundary.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kastellan_sandbox::{Net, SandboxBackend, SandboxPolicy};

use crate::egress::audit::EgressAuditRow;
use crate::egress::cert_pins::{parse_cert_pins, select_pins_for_allowlist, CertPinError, CertPinMap};
use crate::egress::net_worker::{pg_decision_sink, spawn_forced_net_worker};
use crate::broker::{spawn_broker, BrokerConfigs, BrokerKind, BrokerSpec};
use crate::tool_host::{spawn_worker, SupervisedWorker, ToolHostError, WorkerSpec};
use crate::worker_manifest::{discover_binary, ResolveCtx};

/// Env var that opts the daemon into egress force-routing (slice #2 Task 4.4).
const ENV_ENABLE: &str = "KASTELLAN_EGRESS_FORCE_ROUTING";
/// Override env var for the egress-proxy binary path (mirrors `KASTELLAN_*_BIN`).
const ENV_PROXY_BIN: &str = "KASTELLAN_EGRESS_PROXY_BIN";
/// Default sibling name of the egress-proxy binary (exe-relative discovery).
const PROXY_BIN_DEFAULT: &str = "kastellan-worker-egress-proxy";
/// Optional override for the per-worker sidecar scratch root.
const ENV_SCRATCH_DIR: &str = "KASTELLAN_EGRESS_SCRATCH_DIR";
/// Optional operator cert-pin config for force-routed workers (slice #4). Same
/// `{host:["sha256/<b64>"]}` JSON the egress-proxy sidecar enforces. Validated
/// fail-closed at startup; selected per worker by allowlist host.
const ENV_CERT_PINS: &str = "KASTELLAN_EGRESS_CERT_PINS";

/// Factory that mints a fresh decision sink for each force-routed worker. Each
/// sidecar gets its own `FnMut` so its decision-ingest thread owns an
/// independent closure (the production sink clones the pool + runtime handle).
///
/// Boxed (rather than threading a `PgPool` directly) so the unit tests can pass
/// a no-op factory and exercise the routing decision without a live Postgres.
pub type DecisionSinkFactory =
    Box<dyn Fn() -> Box<dyn FnMut(EgressAuditRow) + Send + 'static> + Send + Sync>;

/// Everything a lifecycle manager needs to force-route a `Net::Allowlist` worker
/// through an egress-proxy sidecar. Built once at daemon startup and shared
/// (behind an `Arc`) across the lifecycle managers.
///
/// `None` of this is held when force-routing is disabled — the managers carry an
/// `Option<Arc<ForceRoutingConfig>>` whose `None` arm is the legacy path.
pub struct ForceRoutingConfig {
    /// Resolved, runnable path to the `kastellan-worker-egress-proxy` binary.
    pub(crate) proxy_bin: PathBuf,
    /// Directory under which each force-routed worker gets a unique scratch
    /// subdir holding its sidecar UDS. Created per spawn, removed on teardown.
    pub(crate) scratch_root: PathBuf,
    /// Mints the per-worker decision sink (see [`DecisionSinkFactory`]).
    pub(crate) make_sink: DecisionSinkFactory,
    /// Operator cert pins for force-routed workers (slice #4). `Some` ⇒
    /// non-empty (an empty/`{}` config normalizes to `None` in [`from_env`]).
    /// Selected per worker by allowlist host in [`ForceRoutingConfig::pins_for`]
    /// and handed to the sidecar via `cert_pins_json`.
    pub(crate) cert_pins: Option<CertPinMap>,
}

impl ForceRoutingConfig {
    /// Construct directly from parts. Most callers go through
    /// [`resolve_force_routing`], which adds the enable-gate + fail-closed
    /// discovery semantics; this is the bare constructor the resolver and the
    /// tests share.
    pub fn new(
        proxy_bin: PathBuf,
        scratch_root: PathBuf,
        make_sink: DecisionSinkFactory,
        cert_pins: Option<CertPinMap>,
    ) -> Self {
        Self { proxy_bin, scratch_root, make_sink, cert_pins }
    }

    /// The pin JSON to hand a force-routed worker's sidecar, given the worker's
    /// allowlist. `None` when no pins are configured or none of the worker's
    /// allowlisted hosts are pinned (→ byte-identical no-pin path).
    pub(crate) fn pins_for(&self, allowlist: &[String]) -> Option<String> {
        self.cert_pins.as_ref().and_then(|m| select_pins_for_allowlist(m, allowlist))
    }

    /// Best-effort startup reclaim of per-worker egress scratch dirs left under
    /// `scratch_root` by a prior daemon that was SIGKILLed before its
    /// `EgressSidecar::drop` RAII cleanup ran (#251). Only removes dirs whose
    /// embedded pid is neither this process nor a live one, so it is safe to run
    /// while a concurrent daemon owns its own dirs. Returns the number removed.
    pub fn sweep_stale_scratch_dirs(&self) -> usize {
        crate::egress::scratch_sweep::sweep_orphaned_scratch_dirs(
            &self.scratch_root,
            std::process::id(),
            crate::egress::scratch_sweep::pid_is_alive,
        )
    }
}

/// Error from [`resolve_force_routing`]: the operator opted into force-routing
/// but the proxy binary could not be resolved. Fail-closed — the daemon must
/// not run net workers unrouted.
#[derive(Debug, thiserror::Error)]
#[error(
    "egress force-routing is enabled (KASTELLAN_EGRESS_FORCE_ROUTING) but the \
     egress-proxy binary could not be found (set KASTELLAN_EGRESS_PROXY_BIN or \
     place kastellan-worker-egress-proxy beside the kastellan binary)"
)]
pub struct ProxyBinaryNotFound;

/// Error building the force-routing config from the environment. Either the
/// proxy binary was missing (fail-closed) or the cert-pin config was malformed
/// (fail-closed). Mapped to `anyhow` at the `main.rs` startup call site.
#[derive(Debug, thiserror::Error)]
pub enum ForceRoutingError {
    #[error(transparent)]
    ProxyBinaryNotFound(#[from] ProxyBinaryNotFound),
    #[error("invalid {env} config: {source}", env = ENV_CERT_PINS)]
    CertPins {
        #[from]
        source: CertPinError,
    },
}

/// Pure: turn the raw `KASTELLAN_EGRESS_CERT_PINS` env value into an optional
/// parsed map. Unset, blank, or `{}` → `None` (no pins); a non-empty valid map →
/// `Some(map)`; malformed → `Err` (the daemon fails closed at startup).
fn parse_cert_pins_env(value: Option<&str>) -> Result<Option<CertPinMap>, CertPinError> {
    let Some(raw) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let map = parse_cert_pins(raw)?;
    Ok(if map.is_empty() { None } else { Some(map) })
}

/// Pure: is this worker's `net` policy one the egress proxy fronts?
///
/// Only [`Net::Allowlist`] is force-routable — that's the host-allowlisted
/// egress shape the proxy enforces. [`Net::Deny`] workers have no egress to
/// route, and [`Net::ProxyEgress`] is the proxy's *own* self-enforcing policy
/// (force-routing it would be circular). So both return `false`.
pub(crate) fn policy_net_is_force_routable(net: &Net) -> bool {
    matches!(net, Net::Allowlist(_))
}

/// The browser does end-to-end TLS itself and cannot trust our per-instance MITM
/// CA, so its sidecar runs in no-MITM (transparent-tunnel) mode. The browser is
/// otherwise a normal force-routable `Net::Allowlist` worker.
pub(crate) const BROWSER_DRIVER_TOOL: &str = "browser-driver";

/// The Matrix channel worker (matrix-rust-sdk) is the second transparent-tunnel
/// worker: it does native end-to-end TLS against the self-hosted homeserver and
/// cannot trust our MITM CA either. MITM would also buy nothing — Matrix room
/// content is E2E-encrypted *before* it reaches HTTP, so an interceptor sees only
/// ciphertext (see the Phase D egress-transport spike,
/// `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md`).
/// The matrix worker's egress-coupled spawn (plan Task 5) passes this name; the
/// constant is wired here so that path inherits the transparent-tunnel decision.
pub(crate) const MATRIX_TOOL: &str = "matrix";

/// Pure: should this worker's egress sidecar disable TLS interception (run as a
/// transparent tunnel)? True for the workers that do their own end-to-end TLS and
/// cannot trust our per-instance MITM CA. The single source of truth for the
/// no-MITM decision, kept as a small exhaustively-testable predicate.
pub(crate) fn disable_mitm_for(worker_name: &str) -> bool {
    matches!(worker_name, BROWSER_DRIVER_TOOL | MATRIX_TOOL)
}

/// How a single worker spawn should be routed, given the force-routing posture.
/// This is the **single source of truth** for the routing decision;
/// [`spawn_worker_maybe_forced`] is a thin actor over it. Keeping it a pure enum
/// makes the security-relevant decision a small, exhaustively-tested truth table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForceRouteAction {
    /// Route through a per-worker egress-proxy sidecar (egress enforced at the
    /// netns boundary).
    Sidecar,
    /// Spawn directly via `spawn_worker` — force-routing off, or a
    /// non-force-routable net (`Net::Deny`/`Net::ProxyEgress`).
    Direct,
}

/// Pure: decide how to spawn a worker, given the force-routing posture.
///
/// * `force_routing_active` — is force-routing enabled for this daemon
///   (i.e. did `from_env` build a [`ForceRoutingConfig`])? This is the best
///   available **production/supervised** signal — `core_service_spec` sets
///   `KASTELLAN_EGRESS_FORCE_ROUTING=1`.
/// * `net_force_routable` — [`policy_net_is_force_routable`] of the worker's net.
///
/// Force-routing on **and** a force-routable net ⇒ [`ForceRouteAction::Sidecar`];
/// anything else ⇒ [`ForceRouteAction::Direct`].
pub(crate) fn force_route_action(
    force_routing_active: bool,
    net_force_routable: bool,
) -> ForceRouteAction {
    if force_routing_active && net_force_routable {
        ForceRouteAction::Sidecar
    } else {
        ForceRouteAction::Direct
    }
}

/// Spawn `spec`'s worker, routing it according to [`force_route_action`].
///
/// * [`ForceRouteAction::Sidecar`] — force-route through a per-worker
///   egress-proxy sidecar (force-routing on + a force-routable net).
/// * [`ForceRouteAction::Direct`] — a **byte-identical** call to
///   [`spawn_worker`] (force-routing off, or a non-force-routable net). This is
///   the legacy path, unchanged from pre-slice-#2.
///
/// This is the single chokepoint both lifecycle managers (`SingleUseLifecycle`
/// and the `IdleTimeoutLifecycle` cold-spawn) call, so the routing decision
/// lives in exactly one place.
///
/// `worker_name` is the logical tool name; it labels the sidecar's audit rows
/// and (via the proxy's `KASTELLAN_EGRESS_PROXY_WORKER` env) its decision lines.
pub(crate) fn spawn_worker_maybe_forced(
    force: Option<&ForceRoutingConfig>,
    backend: &dyn SandboxBackend,
    spec: &WorkerSpec<'_>,
    worker_name: &str,
) -> Result<SupervisedWorker, ToolHostError> {
    let action = force_route_action(
        force.is_some(),
        policy_net_is_force_routable(&spec.policy.net),
    );
    match action {
        ForceRouteAction::Direct => spawn_worker(backend, spec),
        ForceRouteAction::Sidecar => {
            let cfg = force.expect("Sidecar action implies force-routing is configured");
            let allowlist = match &spec.policy.net {
                Net::Allowlist(hosts) => hosts.clone(),
                // Unreachable: `policy_net_is_force_routable` already gated the
                // Sidecar action on `Net::Allowlist`. Fall back to the legacy
                // path rather than panic if that invariant ever changes.
                _ => return spawn_worker(backend, spec),
            };
            let pins_json = cfg.pins_for(&allowlist);
            let params = crate::egress::net_worker::NetWorkerSpawn {
                backend,
                // Host workers: the sidecar and worker share the same backend
                // (there is no VM). VM force-routing passes a distinct host
                // `sidecar_backend` at the call site (see the VM×broker e2e).
                sidecar_backend: backend,
                proxy_bin: &cfg.proxy_bin,
                spec,
                allowlist: &allowlist,
                worker_name,
                secret_fingerprints: &[],
                cert_pins_json: pins_json.as_deref(),
                // Workers that do their own end-to-end TLS + can't trust our CA
                // (browser, matrix) → their sidecar transparently tunnels.
                disable_mitm: disable_mitm_for(worker_name),
            };
            spawn_forced_net_worker(&params, &cfg.scratch_root, (cfg.make_sink)())
        }
    }
}

/// Spawn `spec`'s worker, first attaching a per-worker broker sidecar when the
/// entry declares one (`broker: Some`), then routing through
/// [`spawn_worker_maybe_forced`] as usual. This is the single cold-spawn
/// chokepoint both lifecycle managers call.
///
/// **Ordering (a worker may be BOTH broker-backed AND force-routed):**
/// 1. Spawn the broker first (fail-closed — no broker ⇒ no worker), giving its
///    bound UDS.
/// 2. Rewrite the worker's policy onto that UDS ([`rewrite_policy_for_broker`]):
///    set `broker_uds` (Slice B1 binds it into the jail) and inject the kind's
///    `uds_env()` so the worker's backend client picks the brokered path.
/// 3. Route through `spawn_worker_maybe_forced`. If force-routing is *also*
///    active, its `rewrite_worker_policy` clones this already-brokered policy, so
///    `broker_uds` + the injected env survive (a struct clone preserves
///    them; force-routing only mutates the egress fields — orthogonal per B1).
/// 4. Attach the broker sidecar to the returned worker (1:1 RAII teardown).
///
/// **Fail-closed:** a worker with `broker: Some` but no daemon
/// `BrokerConfig` for that kind (broker binary absent) is **refused** — the
/// manifest already dropped the backend host from the allowlist, so a silent
/// fallback would leave the worker with no backend route at all *and* skip the
/// containment intent.
///
/// When `broker` is `None` this is a **byte-identical** pass-through to
/// [`spawn_worker_maybe_forced`].
#[allow(clippy::too_many_arguments)] // mirrors spawn_worker_maybe_forced + the broker configs
pub(crate) fn spawn_worker_with_optional_broker(
    force: Option<&ForceRoutingConfig>,
    broker_configs: &BrokerConfigs,
    backend: &dyn SandboxBackend,
    spec: &WorkerSpec<'_>,
    broker: Option<&BrokerSpec>,
    worker_name: &str,
) -> Result<SupervisedWorker, ToolHostError> {
    let Some(broker_spec) = broker else {
        // No broker requested → legacy path, byte-identical.
        return spawn_worker_maybe_forced(force, backend, spec, worker_name);
    };
    // Fail-closed: a broker-wanting worker must not spawn without a matching config.
    let cfg = broker_configs.for_kind(broker_spec.kind).ok_or_else(|| {
        ToolHostError::Io(std::io::Error::other(format!(
            "worker {worker_name:?} requests a {:?} broker but the daemon has no \
             matching broker config (binary not found); refusing to spawn — the \
             manifest already dropped the backend host from egress",
            broker_spec.kind
        )))
    })?;
    // 1. Broker first (fail-closed on its Err — its scratch is cleaned there).
    let (sidecar, uds) = spawn_broker(cfg, broker_spec, backend)?;
    // 2. Rewrite the policy onto the broker UDS. If spawning the worker below
    //    fails, `sidecar` drops here → its Drop kills the broker + removes its
    //    scratch (fail-closed, no orphan).
    let brokered = rewrite_policy_for_broker(spec.policy.clone(), &uds, broker_spec.kind);
    let brokered_spec = WorkerSpec {
        policy: &brokered,
        program: spec.program,
        args: spec.args,
        wall_clock_ms: spec.wall_clock_ms,
    };
    // 3. Route as usual (force-routing may also apply; it preserves our fields).
    let mut worker = spawn_worker_maybe_forced(force, backend, &brokered_spec, worker_name)?;
    // 4. Attach the broker so teardown is 1:1 with the worker.
    worker.broker = Some(sidecar);
    Ok(worker)
}

/// Pure: rewrite a worker policy onto its broker's UDS. Binds the socket into the
/// jail ([`SandboxPolicy::broker_uds`], Slice B1) and injects the kind's
/// `uds_env()` so the worker's backend client selects the brokered path. The jail
/// path equals the host path (B1 binds identically), so the injected value is
/// `uds` verbatim. Any stale env value for that key is dropped first (mirrors
/// `rewrite_worker_policy`'s env handling).
///
/// `#[doc(hidden)] pub` (not stable API): exposed only so the Slice-C egress e2e
/// (`core/tests/embed_broker_egress_e2e.rs`) drives the real production rewrite
/// instead of a divergence-prone replica. In-crate this is the sole path that
/// puts a worker onto its broker UDS.
#[doc(hidden)]
pub fn rewrite_policy_for_broker(
    mut policy: SandboxPolicy,
    uds: &std::path::Path,
    kind: BrokerKind,
) -> SandboxPolicy {
    let uds_env = kind.uds_env();
    policy.broker_uds = Some(uds.to_path_buf());
    policy.env.retain(|(k, _)| k != uds_env);
    policy
        .env
        .push((uds_env.to_string(), uds.to_string_lossy().into_owned()));
    policy
}

/// Resolve the daemon's force-routing configuration from its inputs.
///
/// * `enabled` — did the operator set `KASTELLAN_EGRESS_FORCE_ROUTING`?
/// * `proxy_bin` — the discovered egress-proxy binary (or `None` if absent).
/// * `scratch_root` / `make_sink` — the remaining config parts.
///
/// Returns:
/// * `Ok(None)` — force-routing disabled (legacy byte-identical path).
/// * `Ok(Some(_))` — enabled and the proxy binary resolved.
/// * `Err(ProxyBinaryNotFound)` — enabled but no proxy binary (**fail-closed**).
pub fn resolve_force_routing(
    enabled: bool,
    proxy_bin: Option<PathBuf>,
    scratch_root: PathBuf,
    make_sink: DecisionSinkFactory,
    cert_pins: Option<CertPinMap>,
) -> Result<Option<ForceRoutingConfig>, ProxyBinaryNotFound> {
    if !enabled {
        return Ok(None);
    }
    let proxy_bin = proxy_bin.ok_or(ProxyBinaryNotFound)?;
    Ok(Some(ForceRoutingConfig::new(proxy_bin, scratch_root, make_sink, cert_pins)))
}

/// Pure: does this env value enable force-routing? Truthy spellings are
/// `1`/`true`/`yes`/`on` (case-insensitive). Anything else — including unset
/// (`None`) and an empty string — is **off**, so the security-relevant flag is
/// never enabled by accident.
fn env_flag_enabled(value: Option<String>) -> bool {
    matches!(
        value.as_deref().map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Build the daemon's force-routing config from the process environment.
///
/// Reads [`ENV_ENABLE`]; when off, returns `Ok(None)` without touching the pool.
/// When on, discovers the egress-proxy binary (override [`ENV_PROXY_BIN`], else
/// the exe-relative sibling [`PROXY_BIN_DEFAULT`]) and builds a config whose
/// decision sink persists to `audit_log` via `pool`/`handle`. Fail-closed: an
/// enabled flag with no resolvable proxy binary returns `Err(ProxyBinaryNotFound)`
/// so the daemon refuses to start rather than run net workers unrouted.
///
/// `handle` is the runtime handle the sidecar decision-ingest threads use to
/// drive the async `audit_log` insert; capture it once at startup
/// (`tokio::runtime::Handle::current()`) and pass it in.
pub fn from_env(
    pool: sqlx::PgPool,
    handle: tokio::runtime::Handle,
    exe_dir: Option<&Path>,
) -> Result<Option<Arc<ForceRoutingConfig>>, ForceRoutingError> {
    if !env_flag_enabled(std::env::var(ENV_ENABLE).ok()) {
        return Ok(None);
    }
    let cert_pins = parse_cert_pins_env(std::env::var(ENV_CERT_PINS).ok().as_deref())?;
    let proxy_bin = discover_egress_proxy_bin(exe_dir);
    let scratch_root = std::env::var_os(ENV_SCRATCH_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(default_egress_scratch_root);
    let make_sink: DecisionSinkFactory =
        Box::new(move || Box::new(pg_decision_sink(pool.clone(), handle.clone())));
    Ok(resolve_force_routing(true, proxy_bin, scratch_root, make_sink, cert_pins)?.map(Arc::new))
}

/// Default per-worker sidecar scratch root (when `KASTELLAN_EGRESS_SCRATCH_DIR`
/// is unset).
///
/// The sidecar binds its UDS at `<root>/egress-<pid>-<seq>/egress.sock`, which
/// must fit `sockaddr_un.sun_path` (104 bytes on macOS, 108 on Linux). macOS's
/// `std::env::temp_dir()` (`$TMPDIR`, e.g. `/var/folders/…/T/`) is ~50 chars
/// deep and overflows once that nesting is added — so a force-routed spawn there
/// would fail-closed at the UDS-path-length guard. Default to the short, stable
/// `/tmp` on macOS instead (operators can still override with
/// `KASTELLAN_EGRESS_SCRATCH_DIR`). On Linux `temp_dir()` is already `/tmp`, so
/// the default is unchanged there.
///
/// `pub(crate)` so the broker config ([`crate::broker::config`]) shares the
/// *same* default root — that keeps `<prefix><pid>-<seq>` broker sidecar husks
/// under the root the force-routing startup sweep (#251) already reclaims.
pub(crate) fn default_egress_scratch_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/tmp")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::temp_dir()
    }
}

/// Resolve the egress-proxy binary path the same way plain workers are found:
/// the [`ENV_PROXY_BIN`] override wins (fail-closed if set-but-invalid), else the
/// exe-relative sibling [`PROXY_BIN_DEFAULT`]. Unlike a regular worker, the proxy
/// is never registered as a callable tool — only spawned as a sidecar.
fn discover_egress_proxy_bin(exe_dir: Option<&Path>) -> Option<PathBuf> {
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &Path| p.exists();
    let is_dir = |p: &Path| p.is_dir();
    discover_egress_proxy_bin_with(&get_env, &exists, &is_dir, exe_dir)
}

/// Dependency-injected core of [`discover_egress_proxy_bin`]: the env + path
/// probes arrive as closures so the discovery semantics (override wins;
/// fail-closed if the override is set-but-not-a-runnable-file; else the
/// exe-relative sibling) are unit-testable without touching the process
/// environment or filesystem.
fn discover_egress_proxy_bin_with(
    get_env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
    is_dir: &dyn Fn(&Path) -> bool,
    exe_dir: Option<&Path>,
) -> Option<PathBuf> {
    let allowlist = |_t: &str| Vec::new();
    let ctx = ResolveCtx {
        get_env,
        exists,
        is_dir,
        exe_dir,
        // discover_binary never canonicalizes; this ctx exists only to
        // reuse its override-wins/fail-closed discovery semantics.
        canonicalize: &|_p| None,
        allowlist: &allowlist,
    };
    discover_binary(&ctx, ENV_PROXY_BIN, PROXY_BIN_DEFAULT)
}

#[cfg(test)]
mod tests;
