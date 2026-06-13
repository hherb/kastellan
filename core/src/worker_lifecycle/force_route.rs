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

use kastellan_sandbox::{Net, SandboxBackend};

use crate::egress::audit::EgressAuditRow;
use crate::egress::net_worker::{pg_decision_sink, spawn_forced_net_worker};
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
}

impl ForceRoutingConfig {
    /// Construct directly from parts. Most callers go through
    /// [`resolve_force_routing`], which adds the enable-gate + fail-closed
    /// discovery semantics; this is the bare constructor the resolver and the
    /// tests share.
    pub fn new(proxy_bin: PathBuf, scratch_root: PathBuf, make_sink: DecisionSinkFactory) -> Self {
        Self {
            proxy_bin,
            scratch_root,
            make_sink,
        }
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

/// Pure: is this worker's `net` policy one the egress proxy fronts?
///
/// Only [`Net::Allowlist`] is force-routable — that's the host-allowlisted
/// egress shape the proxy enforces. [`Net::Deny`] workers have no egress to
/// route, and [`Net::ProxyEgress`] is the proxy's *own* self-enforcing policy
/// (force-routing it would be circular). So both return `false`.
pub(crate) fn policy_net_is_force_routable(net: &Net) -> bool {
    matches!(net, Net::Allowlist(_))
}

/// Spawn `spec`'s worker, force-routing it through an egress-proxy sidecar iff
/// `force` is `Some` **and** the policy declares a force-routable [`Net`]
/// (currently [`Net::Allowlist`] — see [`policy_net_is_force_routable`]).
///
/// Otherwise this is a **byte-identical** call to [`spawn_worker`] — both the
/// `None`-config (force-routing disabled) and the not-force-routable-net
/// (`Net::Deny` / `Net::ProxyEgress`) arms take the legacy path unchanged. This
/// is the single chokepoint both lifecycle managers (`SingleUseLifecycle` and
/// the `IdleTimeoutLifecycle` cold-spawn) call, so the routing decision lives in
/// exactly one place.
///
/// `worker_name` is the logical tool name; it labels the sidecar's audit rows
/// and (via the proxy's `KASTELLAN_EGRESS_PROXY_WORKER` env) its decision lines.
pub(crate) fn spawn_worker_maybe_forced(
    force: Option<&ForceRoutingConfig>,
    backend: &dyn SandboxBackend,
    spec: &WorkerSpec<'_>,
    worker_name: &str,
) -> Result<SupervisedWorker, ToolHostError> {
    match force {
        Some(cfg) if policy_net_is_force_routable(&spec.policy.net) => {
            // Force-routable ⇒ the net is `Net::Allowlist`; the allowlisted
            // host:port endpoints become the proxy's own allowlist.
            let allowlist = match &spec.policy.net {
                Net::Allowlist(hosts) => hosts.clone(),
                // Unreachable: `policy_net_is_force_routable` already gated on
                // `Net::Allowlist`. Fall back to the legacy path rather than
                // panic if that invariant ever changes.
                _ => return spawn_worker(backend, spec),
            };
            let params = crate::egress::net_worker::NetWorkerSpawn {
                backend,
                proxy_bin: &cfg.proxy_bin,
                spec,
                allowlist: &allowlist,
                worker_name,
                secret_fingerprints: &[], // dispatch-time provisioning deferred (#268)
                cert_pins_json: None,     // operator frontier-pin wiring deferred (slice #4 follow-up)
            };
            spawn_forced_net_worker(&params, &cfg.scratch_root, (cfg.make_sink)())
        }
        _ => spawn_worker(backend, spec),
    }
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
) -> Result<Option<ForceRoutingConfig>, ProxyBinaryNotFound> {
    if !enabled {
        return Ok(None);
    }
    let proxy_bin = proxy_bin.ok_or(ProxyBinaryNotFound)?;
    Ok(Some(ForceRoutingConfig::new(
        proxy_bin,
        scratch_root,
        make_sink,
    )))
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
) -> Result<Option<Arc<ForceRoutingConfig>>, ProxyBinaryNotFound> {
    if !env_flag_enabled(std::env::var(ENV_ENABLE).ok()) {
        return Ok(None);
    }
    let proxy_bin = discover_egress_proxy_bin(exe_dir);
    let scratch_root = std::env::var_os(ENV_SCRATCH_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(default_egress_scratch_root);
    let make_sink: DecisionSinkFactory = Box::new(move || {
        Box::new(pg_decision_sink(pool.clone(), handle.clone()))
    });
    Ok(resolve_force_routing(true, proxy_bin, scratch_root, make_sink)?.map(Arc::new))
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
fn default_egress_scratch_root() -> PathBuf {
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
mod tests {
    use super::*;
    use crate::tool_host::{ToolHostError, WorkerSpec};
    use kastellan_sandbox::{SandboxBackend, SandboxError, SandboxPolicy};

    /// A no-op sink factory — proves the routing decision without a live pool.
    fn noop_sink_factory() -> DecisionSinkFactory {
        Box::new(|| Box::new(|_row| {}))
    }

    /// Backend whose spawn always fails. The point of these tests is *which*
    /// spawn path runs, told apart by the error variant: the plain
    /// `spawn_worker` path surfaces `ToolHostError::Sandbox`, while the
    /// force-routed path maps its sidecar-spawn failure to `ToolHostError::Io`
    /// (see `egress::net_worker::spawn_net_worker`).
    struct FailBackend;
    impl SandboxBackend for FailBackend {
        fn spawn_under_policy(
            &self,
            _policy: &SandboxPolicy,
            _program: &str,
            _args: &[&str],
        ) -> Result<std::process::Child, SandboxError> {
            Err(SandboxError::Backend("test: spawn refused".into()))
        }
    }

    fn config_with(scratch_root: PathBuf) -> ForceRoutingConfig {
        ForceRoutingConfig::new(
            PathBuf::from("/nonexistent/egress-proxy"),
            scratch_root,
            noop_sink_factory(),
        )
    }

    fn spec_for<'a>(policy: &'a SandboxPolicy) -> WorkerSpec<'a> {
        WorkerSpec {
            policy,
            program: "/bin/worker",
            args: &[],
            wall_clock_ms: None,
        }
    }

    #[test]
    fn none_config_uses_plain_spawn_worker_even_for_allowlist() {
        let policy = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            ..SandboxPolicy::default()
        };
        let res = spawn_worker_maybe_forced(None, &FailBackend, &spec_for(&policy), "web-fetch");
        // Plain spawn_worker surfaces the backend error as Sandbox.
        assert!(
            matches!(res, Err(ToolHostError::Sandbox(_))),
            "None config must take the legacy spawn_worker path (Sandbox error)"
        );
    }

    #[test]
    fn some_config_allowlist_routes_through_forced_spawn() {
        let policy = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            ..SandboxPolicy::default()
        };
        let scratch = tempfile::tempdir().expect("scratch root");
        let cfg = config_with(scratch.path().to_path_buf());
        let res =
            spawn_worker_maybe_forced(Some(&cfg), &FailBackend, &spec_for(&policy), "web-fetch");
        // The forced path maps the sidecar-spawn failure to Io (fail-closed).
        assert!(
            matches!(res, Err(ToolHostError::Io(_))),
            "Some config + Allowlist must force-route (Io fail-closed error)"
        );
    }

    #[test]
    fn some_config_deny_net_uses_plain_spawn_worker() {
        let policy = SandboxPolicy {
            net: Net::Deny,
            ..SandboxPolicy::default()
        };
        let scratch = tempfile::tempdir().expect("scratch root");
        let cfg = config_with(scratch.path().to_path_buf());
        let res = spawn_worker_maybe_forced(Some(&cfg), &FailBackend, &spec_for(&policy), "shell");
        // Net::Deny is not force-routable → legacy spawn_worker (Sandbox error).
        assert!(
            matches!(res, Err(ToolHostError::Sandbox(_))),
            "Net::Deny must take the legacy path even with config (Sandbox error)"
        );
        // And it must not have created a scratch subdir (no force-route happened).
        let entries: Vec<_> = std::fs::read_dir(scratch.path())
            .expect("read scratch root")
            .collect();
        assert!(entries.is_empty(), "Net::Deny path must not touch scratch_root");
    }

    #[test]
    fn allowlist_net_is_force_routable() {
        assert!(policy_net_is_force_routable(&Net::Allowlist(vec![
            "api.example.com:443".into()
        ])));
    }

    #[test]
    fn deny_and_proxy_egress_nets_are_not_force_routable() {
        assert!(!policy_net_is_force_routable(&Net::Deny));
        assert!(!policy_net_is_force_routable(&Net::ProxyEgress));
    }

    #[test]
    fn disabled_resolves_to_none_even_with_a_binary() {
        let out = resolve_force_routing(
            false,
            Some(PathBuf::from("/opt/egress-proxy")),
            PathBuf::from("/tmp"),
            noop_sink_factory(),
        )
        .expect("disabled never errors");
        assert!(out.is_none(), "disabled => None (legacy path)");
    }

    #[test]
    fn enabled_with_binary_resolves_to_some() {
        let out = resolve_force_routing(
            true,
            Some(PathBuf::from("/opt/egress-proxy")),
            PathBuf::from("/tmp"),
            noop_sink_factory(),
        )
        .expect("enabled + binary => Ok(Some)");
        let cfg = out.expect("Some");
        assert_eq!(cfg.proxy_bin, PathBuf::from("/opt/egress-proxy"));
        assert_eq!(cfg.scratch_root, PathBuf::from("/tmp"));
    }

    #[test]
    fn enabled_without_binary_fails_closed() {
        let out = resolve_force_routing(true, None, PathBuf::from("/tmp"), noop_sink_factory());
        assert!(
            out.is_err(),
            "enabled but no proxy binary must fail closed, not fall back to unrouted egress"
        );
    }

    #[test]
    fn env_flag_truthy_and_falsy_values() {
        // Enabled only for explicit truthy spellings; everything else (incl.
        // unset) is off so force-routing is never enabled by accident.
        for v in ["1", "true", "TRUE", "yes", "on"] {
            assert!(env_flag_enabled(Some(v.to_string())), "{v:?} should enable");
        }
        for v in ["0", "false", "no", "off", ""] {
            assert!(!env_flag_enabled(Some(v.to_string())), "{v:?} should disable");
        }
        assert!(!env_flag_enabled(None), "unset should disable");
    }

    /// macOS: the default scratch root must be short enough that the nested
    /// `egress-<pid>-<seq>/egress.sock` still fits the 104-byte
    /// `sockaddr_un.sun_path`. macOS's `$TMPDIR` is too deep, so we default to
    /// `/tmp`; pin that so a regression back to `temp_dir()` (which would make
    /// every force-routed spawn fail-closed on macOS) trips here.
    #[cfg(target_os = "macos")]
    #[test]
    fn default_scratch_root_is_short_on_macos() {
        let root = default_egress_scratch_root();
        assert_eq!(root, PathBuf::from("/tmp"));
        // Worst-case projected UDS (max pid + max seq), plus the NUL
        // terminator, must fit the 104-byte `sun_path` — i.e. the path itself
        // must be < 104 bytes (`len + 1 <= 104`).
        let projected = root
            .join("egress-4294967295-18446744073709551615")
            .join("egress.sock");
        assert!(
            projected.as_os_str().len() < 104,
            "default macOS scratch root too deep for sockaddr_un.sun_path: {}",
            projected.display()
        );
    }

    #[test]
    fn proxy_bin_override_pointing_at_a_runnable_file_wins() {
        // KASTELLAN_EGRESS_PROXY_BIN set to a runnable file is authoritative,
        // even when a sibling default also exists.
        let get_env =
            |k: &str| (k == ENV_PROXY_BIN).then(|| "/opt/custom/egress-proxy".to_string());
        let exists = |_p: &Path| true;
        let is_dir = |_p: &Path| false;
        let exe = PathBuf::from("/usr/lib/kastellan");
        let out = discover_egress_proxy_bin_with(&get_env, &exists, &is_dir, Some(exe.as_path()));
        assert_eq!(out, Some(PathBuf::from("/opt/custom/egress-proxy")));
    }

    #[test]
    fn proxy_bin_override_set_but_invalid_fails_closed_without_sibling_fallback() {
        // The override is set but names a non-existent path. A set-but-invalid
        // override is fail-closed: we must NOT silently substitute the
        // exe-relative sibling (which would route through a *different* binary
        // than the operator named) — `from_env` then maps the `None` to
        // ProxyBinaryNotFound and the daemon refuses to start.
        let get_env =
            |k: &str| (k == ENV_PROXY_BIN).then(|| "/opt/typo/egress-proxy".to_string());
        // The override path does NOT exist; the sibling default WOULD.
        let exists = |p: &Path| p != Path::new("/opt/typo/egress-proxy");
        let is_dir = |_p: &Path| false;
        let exe = PathBuf::from("/usr/lib/kastellan");
        let out = discover_egress_proxy_bin_with(&get_env, &exists, &is_dir, Some(exe.as_path()));
        assert_eq!(
            out, None,
            "set-but-invalid override must fail closed, not fall through to the sibling"
        );
    }

    #[test]
    fn proxy_bin_falls_back_to_exe_sibling_when_override_unset() {
        // No override → the exe-relative `kastellan-worker-egress-proxy` sibling
        // is used iff it is a runnable file.
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| true;
        let is_dir = |_p: &Path| false;
        let exe = PathBuf::from("/usr/lib/kastellan");
        let out = discover_egress_proxy_bin_with(&get_env, &exists, &is_dir, Some(exe.as_path()));
        assert_eq!(
            out,
            Some(exe.join(PROXY_BIN_DEFAULT)),
            "unset override must use the exe-relative sibling default"
        );
    }
}
