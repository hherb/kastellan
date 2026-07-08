//! Host-side manifest + `ToolEntry` constructor for the web-research worker.
//!
//! Composite of search + fetch: the LLM supplies only the query; the operator
//! controls the SearxNG endpoint (`KASTELLAN_WEB_RESEARCH_ENDPOINT`) and the
//! content-host allowlist (`tool_allowlists` keyed `"web-research"`). The one
//! allowlist gates both the endpoint host and every fetched result URL. The
//! `Net::Allowlist` is the union of the endpoint host:port and the content
//! host:443 entries; the egress proxy owns IP-level containment. See
//! `docs/threat-model.md` ("Network egress").

use std::path::PathBuf;

use kastellan_sandbox::{Net, Profile, SandboxPolicy};
use url::Url;

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

const TOOL_NAME: &str = "web-research";
const BIN_ENV: &str = "KASTELLAN_WEB_RESEARCH_BIN";
const DEFAULT_BIN_NAME: &str = "kastellan-worker-web-research";
const ENDPOINT_ENV: &str = "KASTELLAN_WEB_RESEARCH_ENDPOINT";

/// `host:port` for the SearxNG endpoint (port defaults: 443 https / from URL).
fn endpoint_net_entry(endpoint: &str) -> Vec<String> {
    match Url::parse(endpoint) {
        Ok(u) => match u.host_str() {
            Some(host) => vec![format!("{host}:{}", u.port_or_known_default().unwrap_or(443))],
            None => vec![],
        },
        Err(_) => vec![],
    }
}

/// Union of the endpoint host:port and the content host:443 entries, de-duped
/// (order-preserving: endpoint first). The content half reuses web-fetch's
/// canonical domain→`host:443` mapping (wildcard `.d` → `d:443`) so both fetching
/// workers share one wildcard-flattening rule.
fn net_entries(endpoint: &str, allowlist: &[String]) -> Vec<String> {
    let mut entries = endpoint_net_entry(endpoint);
    for e in crate::workers::web_fetch::allowlist_to_net_entries(allowlist) {
        if !entries.contains(&e) {
            entries.push(e);
        }
    }
    entries
}

/// Build the [`ToolEntry`] for the web-research worker. Defaults mirror web-fetch
/// (HTML/PDF parsing over several pages): `Profile::WorkerNetClient`,
/// `cpu_ms = 15_000`, `mem_mb = 512`, `wall_clock_ms = Some(60_000)` (search + N
/// sequential fetches), `SingleUse`. Resolver files in `fs_read` for DNS under
/// `--unshare-all`.
///
/// Note the wall-clock/fetch-budget interaction: fetches run sequentially with a
/// 20s per-request transport timeout, so a handful of slow/hung hosts can burn
/// the 60s budget before the worker returns even partial results. Parallel fetch
/// (which would decouple the two) is a deferred follow-up; until then the
/// `max_sources` clamp (≤ 8) keeps the worst case bounded.
pub fn web_research_entry(binary: PathBuf, endpoint: &str, allowlist: &[String]) -> ToolEntry {
    let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            ("KASTELLAN_WEB_RESEARCH_ALLOWLIST".to_string(), allow_json),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
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
    }
}

/// web-research's manifest. Discovery mirrors web-search.
pub struct WebResearchManifest;

impl WorkerManifest for WebResearchManifest {
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
        Resolution::Register(web_research_entry(binary, &endpoint, &allowlist))
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
    fn resolve_registers_union_net_and_injects_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
                assert_eq!(entry.policy.cpu_ms, 15_000);
                assert_eq!(entry.policy.mem_mb, 512);
                assert_eq!(entry.wall_clock_ms, Some(60_000));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        // endpoint host:443 first, then content docs.example.org:443.
                        assert_eq!(hosts, &vec![
                            "searx.example.org:443".to_string(),
                            "docs.example.org:443".to_string(),
                        ]);
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                assert_eq!(entry.policy.env[0].0, ENDPOINT_ENV);
                assert_eq!(entry.policy.env[0].1, "https://searx.example.org/search");
                assert_eq!(entry.policy.env[1].0, "KASTELLAN_WEB_RESEARCH_ALLOWLIST");
                assert_eq!(entry.policy.env[1].1, r#"["searx.example.org",".docs.example.org"]"#);
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
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("kastellan-worker-web-research"), "detail: {detail}");
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
