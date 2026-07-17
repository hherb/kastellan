//! Host-side manifest + `ToolEntry` constructor for the web-search worker.
//!
//! Containment caveat: until the egress proxy lands, the host allowlist is
//! enforced *inside* the worker (scheme + host) and matches host **names**, not
//! resolved IPs — it does not contain SSRF / DNS-rebinding to internal
//! addresses. The `Net::Allowlist` data built here is populated for the future
//! proxy, which owns IP-level containment. See `docs/threat-model.md`
//! ("Network egress").

use std::path::PathBuf;

use kastellan_sandbox::{Net, Profile, SandboxPolicy};
use url::Url;

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{
    discover_binary, ResolveCtx, Resolution, ToolDoc, ToolParam, WorkerManifest,
};
use crate::workers::endpoint_guard;

/// Tool name the registry keys web-search on.
const TOOL_NAME: &str = "web-search";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "KASTELLAN_WEB_SEARCH_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "kastellan-worker-web-search";
/// Operator-configured SearxNG endpoint, read from the daemon's own env.
const ENDPOINT_ENV: &str = "KASTELLAN_WEB_SEARCH_ENDPOINT";
/// Operator opt-in: route web-search through a trusted search-broker sidecar (so a
/// force-routed worker can reach a loopback SearxNG). `=1` enables broker mode.
const USE_BROKER_ENV: &str = "KASTELLAN_WEB_SEARCH_USE_BROKER";
/// Operator override for the `web.search_batch` size cap, read from the daemon
/// env and injected into the jail only when set. This is the same env-var name
/// the web-search worker reads (`kastellan_worker_web_common::
/// WEB_SEARCH_MAX_BATCH_QUERIES_ENV`) — `web-common` is only a dev-dependency of
/// core, so the lib can't import it; the two are pinned equal by the
/// `web_search_batch_cap_env_matches_worker_contract` integration test instead.
pub const MAX_BATCH_QUERIES_ENV: &str = "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES";
/// JSON-RPC method the web-search worker exposes for batched search
/// (`web.search_batch`). One source of truth within core: the `tool_docs()`
/// advertisement below and the planner-summary cap
/// (`scheduler::inner_loop::summary::ok_summary_cap`) both reference it, so a
/// rename can't silently desync the advertised method from the cap that keys on
/// it. The worker (a separate crate core can't import in its lib) matches on
/// `web-common`'s `WEB_SEARCH_BATCH_METHOD`; the two are pinned equal by the
/// `web_search_batch_method_matches_worker_contract` integration test, so a
/// rename can't route every batch call to `METHOD_NOT_FOUND` either. `pub` for
/// that cross-crate test to observe it.
pub const WEB_SEARCH_BATCH_METHOD: &str = "web.search_batch";

/// Opt into the Linux Firecracker micro-VM backend for web-search. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_SEARCH_USE_MICROVM";

/// In-rootfs path of the web-search worker binary (staged there by
/// `build-web-search-rootfs.sh`). Used by the micro-VM entries, not the host path.
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-search";

/// Derive the `Net::Allowlist` `host:port` entry from the endpoint URL. Returns
/// an empty list if the endpoint is unset or unparseable — the worker fails
/// closed at startup in that case, so an empty net policy is safe.
fn net_entries_from_endpoint(endpoint: &str) -> Vec<String> {
    match Url::parse(endpoint) {
        Ok(u) => match u.host_str() {
            Some(host) => {
                let port = u.port_or_known_default().unwrap_or(443);
                vec![format!("{host}:{port}")]
            }
            None => vec![],
        },
        Err(_) => vec![],
    }
}

/// Derive the worker's host allowlist (`["<host>"]`) from the endpoint URL. The
/// worker's `from_env` re-checks `endpoint host ∈ allowlist`, and there is only
/// ever one endpoint, so the allowlist *is* the endpoint host — deriving it here
/// keeps the two from drifting and needs no separate operator config. Empty when
/// the endpoint is unset or unparseable (the worker then fails closed — correct,
/// web-search is disabled without an endpoint).
fn host_allowlist_from_endpoint(endpoint: &str) -> Vec<String> {
    match Url::parse(endpoint) {
        Ok(u) => u.host_str().map(|h| vec![h.to_string()]).unwrap_or_default(),
        Err(_) => vec![],
    }
}

/// The #452 resolve-time guard, web-search flavour: `Some(detail)` iff broker
/// mode is off (the search-broker owns SearxNG egress host-side, so worker
/// egress never carries search traffic), this worker's egress will be
/// force-routed (micro-VM always; host iff `KASTELLAN_EGRESS_FORCE_ROUTING`),
/// and the operator endpoint uses a `localhost` NAME — the one endpoint class
/// the egress proxy can never serve. A *literal* loopback endpoint
/// (`http://127.0.0.1:…`) is NOT flagged: the proxy dials an
/// operator-allowlisted literal via its carve-out. Predicate + shared message
/// live in [`endpoint_guard::forced_localhost_misconfig`]; only the remedy
/// tail is per-worker. The remedy names https for routable hosts because the
/// worker's own `validate_endpoint` allows plain http for loopback hosts only
/// — an `http://` routable endpoint would pass this guard and then die
/// SchemeDenied at worker startup.
fn forced_localhost_misconfig(
    use_broker: bool,
    is_microvm: bool,
    endpoint: &str,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if use_broker {
        return None;
    }
    endpoint_guard::forced_localhost_misconfig(
        ENDPOINT_ENV,
        endpoint,
        endpoint_guard::egress_will_force_route(is_microvm, get_env),
        &format!(
            "Use the literal-IP form (e.g. http://127.0.0.1:<port> — an \
             allowlisted literal is dialed via the proxy's carve-out), an \
             https:// routable host (the worker allows plain http for loopback \
             hosts only), or set {USE_BROKER_ENV}=1."
        ),
    )
}

/// Build the [`ToolEntry`] for the web-search worker.
///
/// The administrator controls the endpoint (`KASTELLAN_WEB_SEARCH_ENDPOINT` on
/// the daemon); the LLM-supplied params carry only the query string and cannot
/// influence the URL. Both `Net::Allowlist` (host:port) and the worker's host
/// allowlist derive from that endpoint (see [`net_entries_from_endpoint`] /
/// [`host_allowlist_from_endpoint`]) — there is one endpoint, so the allowlist
/// is its host, and the worker re-checks `endpoint host ∈ allowlist` at startup.
///
/// Defaults: `Net::Allowlist`, `Profile::WorkerNetClient`, `cpu_ms = 5_000`,
/// `mem_mb = 256` (JSON parsing only — lighter than web-fetch's HTML/PDF),
/// `wall_clock_ms = Some(60_000)`, `SingleUse`. `fs_read` includes the resolver
/// config files so DNS works under the `--unshare-all` jail.
///
/// The 60 s wall (matching the sibling multi-op `web-research` worker, not the
/// 30 s of a single-request worker) gives `web.search_batch` headroom for
/// several sequential searches; the worker itself soft-bounds a batch below this
/// wall (`batch::BATCH_SOFT_DEADLINE`) so it returns per-query results rather
/// than being SIGKILLed mid-batch.
pub fn web_search_entry(binary: PathBuf, endpoint: &str, allowlist: &[String]) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries_from_endpoint(endpoint)),
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        env: vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            ("KASTELLAN_WEB_SEARCH_ALLOWLIST".to_string(), allow_json),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
    }
}

/// Build the web-search [`ToolEntry`] in **broker mode**: the worker reaches
/// SearxNG only through a core-spawned trusted search-broker, so its
/// `Net::Allowlist` is empty and the direct endpoint/allowlist env is omitted.
/// `entry.broker` carries the SearxNG endpoint the broker forwards to; core's
/// cold-spawn chokepoint spawns the broker, binds its UDS into the jail via
/// `SandboxPolicy::broker_uds`, and injects `KASTELLAN_SEARCH_BROKER_UDS` so the
/// worker's `choose_search_provider` selects `BrokeredSearchProvider`.
pub fn web_search_broker_entry(binary: PathBuf, endpoint: &str) -> ToolEntry {
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        // No direct egress — the broker holds the only route to SearxNG.
        net: Net::Allowlist(vec![]),
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        // No direct endpoint/allowlist env: the worker never reaches SearxNG itself.
        env: vec![],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: Some(crate::broker::BrokerSpec::search(endpoint)),
    }
}

/// Build the [`ToolEntry`] for web-search running inside a Firecracker micro-VM
/// (opt-in via `KASTELLAN_WEB_SEARCH_USE_MICROVM=1`). Mirrors the sibling
/// `web_research_firecracker_entry` (same `15_000`/`512` sizing + endpoint-derived
/// allowlist): empty `fs_read` (no NIC / local DNS — the
/// per-instance MITM CA is appended at spawn by `rewrite_worker_policy`), the
/// in-rootfs worker binary, `sandbox_backend = FirecrackerVm`, and the
/// `KASTELLAN_MICROVM_DIR` / `KASTELLAN_MICROVM_ROOTFS=web-search.ext4` env. Keeps
/// the endpoint-derived `Net::Allowlist` + endpoint/allowlist env, so the worker
/// reaches a **routable** SearxNG through the host MITM egress sidecar.
///
/// Loopback-SearxNG caveat: in VM mode egress force-routes through the host
/// proxy. A **literal** loopback endpoint (`http://127.0.0.1:…`) stays reachable
/// — the proxy dials an operator-allowlisted literal via its carve-out
/// (`egress-proxy::proxy::decide`) — but a `localhost` **name** is dead (the
/// proxy range-denies what the name resolves to), and `resolve()` refuses that
/// config (`Misconfigured`, #452). The broker VM entry
/// ([`web_search_firecracker_broker_entry`], `USE_BROKER=1`) removes SearxNG
/// from worker egress entirely (the stronger-containment option). Linux-only:
/// emits the `FirecrackerVm` backend variant.
#[cfg(target_os = "linux")]
pub fn web_search_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    allowlist: &[String],
) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(net_entries_from_endpoint(endpoint)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            ("KASTELLAN_WEB_SEARCH_ALLOWLIST".to_string(), allow_json),
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-search.ext4".to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
    }
}

/// Build the web-search [`ToolEntry`] running inside a Firecracker micro-VM **AND**
/// reaching a host-side search-broker (VM × broker; opt-in via `USE_MICROVM=1` +
/// `USE_BROKER=1`). Combines the VM entry (empty `fs_read`, `FirecrackerVm`
/// backend) with broker mode: `Net::Allowlist` is **empty** (zero direct egress),
/// no endpoint env is injected, and `broker: Some(BrokerSpec::search(endpoint))`
/// tells core's cold-spawn chokepoint to spawn the broker + bind its UDS. In the
/// VM the broker rides a second vsock channel (port 1026); the FC plan rewrites the
/// injected `KASTELLAN_SEARCH_BROKER_UDS` to the in-guest relay path.
///
/// Because the broker runs host-side, a VM web-search worker reaches a loopback
/// SearxNG with ZERO direct egress (a literal-loopback endpoint is also reachable
/// in the direct entry via the proxy's allowlisted-literal carve-out; the broker
/// is the stronger-containment option and the only path for a `localhost`-name
/// endpoint). Linux-only.
#[cfg(target_os = "linux")]
pub fn web_search_firecracker_broker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
) -> ToolEntry {
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        // No direct egress — the broker holds the only route to SearxNG.
        net: Net::Allowlist(vec![]),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-search.ext4".to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,  // set at spawn (force-routing)
        broker_uds: None, // set at spawn (rewrite_policy_for_broker)
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: Some(crate::broker::BrokerSpec::search(endpoint)),
    }
}

/// Append the operator's `web.search_batch` size-cap override to a worker
/// entry's env, but only when it is present and non-blank. Leaving it off keeps
/// the worker on its built-in default (8) and the entry's env byte-identical to
/// the pre-batch behaviour. The worker (`batch::resolve_max_batch`) is the
/// authoritative parser/clamper — core passes the raw trimmed value through.
fn maybe_inject_max_batch(mut entry: ToolEntry, val: Option<String>) -> ToolEntry {
    if let Some(v) = val {
        let v = v.trim();
        if !v.is_empty() {
            entry.policy.env.push((MAX_BATCH_QUERIES_ENV.to_string(), v.to_string()));
        }
    }
    entry
}

/// web-search's manifest. Discovery mirrors web-fetch: a set
/// `KASTELLAN_WEB_SEARCH_BIN` override is authoritative (honoured iff it names a
/// runnable file, else fails closed); only when unset do we fall back to the
/// exe-relative sibling `kastellan-worker-web-search`. The endpoint is read from
/// the daemon env at resolve time and injected into the worker policy.
pub struct WebSearchManifest;

impl WorkerManifest for WebSearchManifest {
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "web.search",
            summary: "Search the web via a SearxNG backend; returns a list of result \
                      titles, URLs, and snippets. Use for questions needing current or \
                      external facts.",
            params: &[
                ToolParam { name: "query", description: "the search query", required: true },
                ToolParam {
                    name: "count",
                    description: "max results, default 10 (cap 20)",
                    required: false,
                },
            ],
        })
    }

    fn tool_docs(&self) -> Vec<ToolDoc> {
        // Reuse the single-query doc, then append the batch method. Both docs
        // carry `name == TOOL_NAME` so the drift guard (doc.name == name())
        // still holds — same worker, two methods. No numeric ceiling is
        // advertised: the batch size is an operator-tunable runtime value
        // (KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES); an over-cap batch is rejected
        // fail-closed with INVALID_PARAMS and surfaced to the planner.
        let mut docs: Vec<ToolDoc> = self.tool_doc().into_iter().collect();
        docs.push(ToolDoc {
            name: TOOL_NAME,
            method: WEB_SEARCH_BATCH_METHOD,
            summary: "Run several INDEPENDENT web searches in one call; returns a \
                      per-query result group for each. Prefer this over multiple \
                      web.search steps when the queries do not depend on each other.",
            params: &[
                ToolParam {
                    name: "queries",
                    description: "list of independent search queries to run in one batch",
                    required: true,
                },
                ToolParam {
                    name: "count",
                    description: "max results per query, default 10 (cap 20)",
                    required: false,
                },
            ],
        });
        docs
    }

    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    // No `allowlist_tool`: web-search has a single operator-configured endpoint,
    // so its host allowlist is the endpoint's own host — derived below, not read
    // from the argv0-path `tool_allowlists` DB table (which cannot hold a
    // hostname: the CLI and a DB CHECK both require a leading '/').

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let endpoint = (ctx.get_env)(ENDPOINT_ENV).unwrap_or_default();
        // Broker mode: the worker reaches SearxNG only through a trusted
        // search-broker sidecar (so a force-routed worker can reach a loopback
        // SearxNG). The broker owns the SearxNG allowlist; the worker gets none.
        let use_broker = ctx.flag_enabled(USE_BROKER_ENV);

        // Firecracker micro-VM mode (Linux) short-circuits host binary discovery:
        // the worker binary lives inside the rootfs image, not on the host.
        // Linux-only — on macOS USE_MICROVM is never read so the `FirecrackerVm`
        // variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            let use_microvm = ctx.flag_enabled(USE_MICROVM_ENV);
            if use_microvm {
                // #452: a Net::Allowlist VM worker is never given a direct NIC
                // (plan.rs refuses to boot one without the egress proxy), so a
                // direct (non-broker) `localhost`-name endpoint can never be
                // reached in VM mode (a literal loopback IS reached via the
                // proxy's allowlisted-literal carve-out).
                if let Some(detail) =
                    forced_localhost_misconfig(use_broker, true, &endpoint, ctx.get_env)
                {
                    return Resolution::Misconfigured { detail };
                }
                let binary = PathBuf::from(MICROVM_WORKER_BIN);
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                // VM × broker: the broker runs host-side and the VM worker reaches
                // it over the vsock UDS (port 1026), so a loopback SearxNG works in
                // VM mode.
                let entry = if use_broker {
                    web_search_firecracker_broker_entry(binary, image_dir, &endpoint)
                } else {
                    let allowlist = host_allowlist_from_endpoint(&endpoint);
                    web_search_firecracker_entry(binary, image_dir, &endpoint, &allowlist)
                };
                let entry = maybe_inject_max_batch(entry, (ctx.get_env)(MAX_BATCH_QUERIES_ENV));
                return Resolution::Register(entry);
            }
        }

        // #452: same guard for the host path — force-routing is the operator
        // flag there, not unconditional like the VM branch above.
        if let Some(detail) = forced_localhost_misconfig(use_broker, false, &endpoint, ctx.get_env)
        {
            return Resolution::Misconfigured { detail };
        }

        let binary = match discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME) {
            Some(b) => b,
            None => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "could not resolve worker binary: {BIN_ENV} set but not a \
                         runnable file, or unset with no sibling {DEFAULT_BIN_NAME} found"
                    ),
                };
            }
        };
        let entry = if use_broker {
            web_search_broker_entry(binary, &endpoint)
        } else {
            let allowlist = host_allowlist_from_endpoint(&endpoint);
            web_search_entry(binary, &endpoint, &allowlist)
        };
        let entry = maybe_inject_max_batch(entry, (ctx.get_env)(MAX_BATCH_QUERIES_ENV));
        Resolution::Register(entry)
    }
}

#[cfg(test)]
mod tests;
