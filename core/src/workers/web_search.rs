//! Host-side manifest + `ToolEntry` constructor for the web-search worker.
//!
//! Containment caveat: until the egress proxy lands, the host allowlist is
//! enforced *inside* the worker (scheme + host) and matches host **names**, not
//! resolved IPs — it does not contain SSRF / DNS-rebinding to internal
//! addresses. The `Net::Allowlist` data built here is populated for the future
//! proxy, which owns IP-level containment. See `docs/threat-model.md`
//! ("Network egress").

use std::path::PathBuf;

use hhagent_sandbox::{Net, Profile, SandboxPolicy};
use url::Url;

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

/// Tool name the registry keys web-search on.
const TOOL_NAME: &str = "web-search";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "HHAGENT_WEB_SEARCH_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "hhagent-worker-web-search";
/// Operator-configured SearxNG endpoint, read from the daemon's own env.
const ENDPOINT_ENV: &str = "HHAGENT_WEB_SEARCH_ENDPOINT";

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

/// Build the [`ToolEntry`] for the web-search worker.
///
/// The administrator controls both the endpoint (`HHAGENT_WEB_SEARCH_ENDPOINT`
/// on the daemon) and the host allowlist (`tool_allowlists` keyed
/// `"web-search"`); the LLM-supplied params carry only the query string and
/// cannot influence the URL. `Net::Allowlist` derives from the endpoint's
/// host:port; the allowlist gates which host the endpoint may name (the worker
/// re-checks `endpoint host ∈ allowlist` at startup).
///
/// Defaults: `Net::Allowlist`, `Profile::WorkerNetClient`, `cpu_ms = 5_000`,
/// `mem_mb = 256` (JSON parsing only — lighter than web-fetch's HTML/PDF),
/// `wall_clock_ms = Some(30_000)`, `SingleUse`. `fs_read` includes the resolver
/// config files so DNS works under the `--unshare-all` jail.
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
            ("HHAGENT_WEB_SEARCH_ALLOWLIST".to_string(), allow_json),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
}

/// web-search's manifest. Discovery mirrors web-fetch: a set
/// `HHAGENT_WEB_SEARCH_BIN` override is authoritative (honoured iff it names a
/// runnable file, else fails closed); only when unset do we fall back to the
/// exe-relative sibling `hhagent-worker-web-search`. The endpoint is read from
/// the daemon env at resolve time and injected into the worker policy.
pub struct WebSearchManifest;

impl WorkerManifest for WebSearchManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

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
        let allowlist = (ctx.allowlist)(TOOL_NAME);
        Resolution::Register(web_search_entry(binary, &endpoint, &allowlist))
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
                assert_eq!(entry.wall_clock_ms, Some(30_000));
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
                assert_eq!(entry.policy.env[1].0, "HHAGENT_WEB_SEARCH_ALLOWLIST");
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
    fn resolve_misconfigured_when_no_binary_found() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("hhagent-worker-web-search"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
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
