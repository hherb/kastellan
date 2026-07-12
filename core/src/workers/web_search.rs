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
/// (`web.search_batch`). One source of truth for the string: the `tool_docs()`
/// advertisement below and the planner-summary cap
/// (`scheduler::inner_loop::summary::ok_summary_cap`) both reference it, so a
/// rename can't silently desync the advertised method from the cap that keys on
/// it. `pub(crate)` because `summary.rs` (same crate) consumes it.
pub(crate) const WEB_SEARCH_BATCH_METHOD: &str = "web.search_batch";

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
        let endpoint = (ctx.get_env)(ENDPOINT_ENV).unwrap_or_default();
        // Broker mode: the worker reaches SearxNG only through a trusted
        // search-broker sidecar (so a force-routed worker can use a loopback
        // SearxNG). The broker owns the SearxNG allowlist; the worker gets none.
        let use_broker = (ctx.get_env)(USE_BROKER_ENV).unwrap_or_default().trim() == "1";
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
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn resolve_registers_with_net_client_policy_and_endpoint_net() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert_eq!(entry.binary, PathBuf::from("/opt/web-search"));
                assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
                assert_eq!(entry.policy.cpu_ms, 5_000);
                assert_eq!(entry.policy.mem_mb, 256);
                assert_eq!(entry.wall_clock_ms, Some(60_000));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
                // Net::Allowlist carries the endpoint host:port (loopback :8888).
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert_eq!(hosts, &vec!["127.0.0.1:8888".to_string()]);
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // Env carries the endpoint + the verbatim allowlist JSON.
                assert_eq!(entry.policy.env[0].0, ENDPOINT_ENV);
                assert_eq!(entry.policy.env[0].1, "http://127.0.0.1:8888/search");
                assert_eq!(entry.policy.env[1].0, "KASTELLAN_WEB_SEARCH_ALLOWLIST");
                assert_eq!(entry.policy.env[1].1, r#"["127.0.0.1"]"#);
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_https_endpoint_maps_to_port_443() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert_eq!(hosts, &vec!["searx.example.org:443".to_string()]);
                }
                other => panic!("expected Net::Allowlist, got {other:?}"),
            },
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn web_search_has_no_db_argv0_allowlist() {
        // web-search's host allowlist derives from its single endpoint, not the
        // argv0-path `tool_allowlists` DB table (which structurally cannot hold a
        // hostname — CLI + DB CHECK both require a leading '/').
        assert!(
            WebSearchManifest.allowlist_tool().is_none(),
            "web-search must not claim an argv0 DB allowlist"
        );
    }

    #[test]
    fn resolve_derives_worker_allowlist_from_endpoint_not_db() {
        // In the daemon the prefetched DB allowlist is ALWAYS empty for web-search
        // (the table can't hold a host), so the worker allowlist must come from the
        // endpoint host — otherwise `from_env` fails closed on an empty allowlist and
        // web-search never registers. Simulate the daemon: empty ctx.allowlist.
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("https://searx.kastellan.dev/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| Vec::<String>::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                let al = entry
                    .policy
                    .env
                    .iter()
                    .find(|(k, _)| k == "KASTELLAN_WEB_SEARCH_ALLOWLIST")
                    .map(|(_, v)| v.clone())
                    .expect("allowlist env present");
                assert_eq!(
                    al, r#"["searx.kastellan.dev"]"#,
                    "worker allowlist must derive from the endpoint host, not the empty DB allowlist"
                );
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_misconfigured_when_no_binary_found() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("kastellan-worker-web-search"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_broker_mode_drops_egress_and_declares_search_broker() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_WEB_SEARCH_USE_BROKER" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| Vec::<String>::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // Broker declared, carrying the SearxNG endpoint it forwards to.
                let spec = entry.broker.as_ref().expect("broker set in broker mode");
                assert_eq!(spec.kind, crate::broker::BrokerKind::Search);
                assert_eq!(spec.endpoint, "http://127.0.0.1:8888/search");
                // Worker has NO direct egress — empty allowlist.
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert!(hosts.is_empty(), "broker-mode worker must have no egress: {hosts:?}")
                    }
                    other => panic!("expected empty Net::Allowlist, got {other:?}"),
                }
                // No direct-endpoint env leaked to the worker in broker mode.
                assert!(
                    entry.policy.env.iter().all(|(k, _)| k != ENDPOINT_ENV),
                    "broker-mode worker must not carry the direct endpoint env"
                );
                // broker_uds is set at spawn, not by the manifest.
                assert!(entry.policy.broker_uds.is_none());
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_direct_mode_unchanged_when_use_broker_unset() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| Vec::<String>::new();
        let c = ctx(&get_env, &exists, &allowlist);
        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(entry.broker.is_none());
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert_eq!(hosts, &vec!["127.0.0.1:8888".to_string()])
                    }
                    other => panic!("got {other:?}"),
                }
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_injects_max_batch_env_when_set() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("5".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(
                    entry.policy.env.iter().any(|(k, v)| k == MAX_BATCH_QUERIES_ENV && v == "5"),
                    "cap env must be injected when set: {:?}",
                    entry.policy.env
                );
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_omits_max_batch_env_when_unset() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // Byte-identical direct-mode env: endpoint + allowlist only.
                assert_eq!(entry.policy.env.len(), 2);
                assert!(entry.policy.env.iter().all(|(k, _)| k != MAX_BATCH_QUERIES_ENV));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_skips_blank_max_batch_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("   ".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(entry.policy.env.iter().all(|(k, _)| k != MAX_BATCH_QUERIES_ENV));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    fn outcome_label(r: &Resolution) -> &'static str {
        match r {
            Resolution::Register(_) => "Register",
            Resolution::Disabled { .. } => "Disabled",
            Resolution::Misconfigured { .. } => "Misconfigured",
        }
    }
}
