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
const EMBED_ENDPOINT_ENV: &str = "KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT";
const EMBED_MODEL_ENV: &str = "KASTELLAN_WEB_RESEARCH_EMBED_MODEL";
const DEFAULT_EMBED_MODEL: &str = "embeddinggemma";

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

/// Union of the endpoint host:port, the optional embed-endpoint host:port, and
/// the content host:443 entries, de-duped (order-preserving: SearxNG endpoint
/// first, embed endpoint second, content hosts last). The content half reuses
/// web-fetch's canonical domain→`host:443` mapping (wildcard `.d` → `d:443`) so
/// both fetching workers share one wildcard-flattening rule.
fn net_entries(endpoint: &str, embed_endpoint: Option<&str>, allowlist: &[String]) -> Vec<String> {
    let mut entries = endpoint_net_entry(endpoint);
    if let Some(embed) = embed_endpoint {
        for e in endpoint_net_entry(embed) {
            if !entries.contains(&e) {
                entries.push(e);
            }
        }
    }
    for e in crate::workers::web_fetch::allowlist_to_net_entries(allowlist) {
        if !entries.contains(&e) {
            entries.push(e);
        }
    }
    entries
}

/// Build the [`ToolEntry`] for the web-research worker. Defaults mirror web-fetch
/// (HTML/PDF parsing over several pages): `Profile::WorkerNetClient`,
/// `cpu_ms = 15_000`, `mem_mb = 512`, `wall_clock_ms = Some(60_000)` (search +
/// bounded-parallel fetches), `SingleUse`. Resolver files in `fs_read` for DNS
/// under `--unshare-all`.
///
/// Fetches are now bounded-parallel (web-research `MAX_CONCURRENT_FETCHES`
/// scoped-thread waves), so every allowlisted candidate page (up to the search
/// count, not just the `max_sources` kept) is fetched concurrently rather than
/// serially — wall-clock is ~⌈candidates / cap⌉ × the 20s per-request transport
/// timeout, not the sum, so a handful of slow/hung hosts burn much less of the
/// 60s budget than under the old sequential pass. The result is byte-identical
/// to the old sequential pass; the `max_sources` clamp (≤ 8) still keeps the
/// worst case bounded.
pub fn web_research_entry(binary: PathBuf, endpoint: &str, allowlist: &[String]) -> ToolEntry {
    web_research_entry_with_embed(binary, endpoint, None, None, allowlist)
}

/// Like [`web_research_entry`] but also, when `embed_endpoint` is set, unions that
/// host:port into the egress allowlist and injects the embed endpoint + model env
/// so the jailed worker may reach an embedding-only endpoint for hybrid ranking.
/// When `embed_endpoint` is `None` the result is identical to the lexical-only
/// worker (no extra net entry, no extra env). `embed_model` defaults to
/// [`DEFAULT_EMBED_MODEL`] when `None`.
pub fn web_research_entry_with_embed(
    binary: PathBuf,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let mut env = vec![
        (ENDPOINT_ENV.to_string(), endpoint.to_string()),
        ("KASTELLAN_WEB_RESEARCH_ALLOWLIST".to_string(), allow_json),
    ];
    if let Some(embed) = embed_endpoint {
        env.push((EMBED_ENDPOINT_ENV.to_string(), embed.to_string()));
        env.push((
            EMBED_MODEL_ENV.to_string(),
            embed_model.unwrap_or(DEFAULT_EMBED_MODEL).to_string(),
        ));
    }
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(endpoint, embed_endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
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
        let embed_endpoint = (ctx.get_env)(EMBED_ENDPOINT_ENV).filter(|s| !s.trim().is_empty());
        let embed_model = (ctx.get_env)(EMBED_MODEL_ENV).filter(|s| !s.trim().is_empty());
        let allowlist = (ctx.allowlist)(TOOL_NAME);
        Resolution::Register(web_research_entry_with_embed(
            binary,
            &endpoint,
            embed_endpoint.as_deref(),
            embed_model.as_deref(),
            &allowlist,
        ))
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
                assert_eq!(entry.policy.env.len(), 2, "no embed env when endpoint unset");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_unions_embed_endpoint_into_net_and_injects_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://embed.example.org:11434/v1/embeddings".to_string()),
            _ => None, // EMBED_MODEL_ENV unset -> default model
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec![
            "searx.example.org".to_string(),
            "embed.example.org".to_string(),
            ".docs.example.org".to_string(),
        ];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert!(hosts.iter().any(|h| h == "embed.example.org:11434"),
                            "embed host:port missing from net: {hosts:?}");
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                let has = |k: &str, v: &str| entry.policy.env.iter().any(|(ek, ev)| ek == k && ev == v);
                assert!(has(EMBED_ENDPOINT_ENV, "http://embed.example.org:11434/v1/embeddings"));
                assert!(has(EMBED_MODEL_ENV, "embeddinggemma"), "default model injected");
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
