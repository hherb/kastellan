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
use crate::worker_manifest::{
    discover_binary, ResolveCtx, Resolution, ToolDoc, ToolParam, WorkerManifest,
};

const TOOL_NAME: &str = "web-research";
const BIN_ENV: &str = "KASTELLAN_WEB_RESEARCH_BIN";
const DEFAULT_BIN_NAME: &str = "kastellan-worker-web-research";
const ENDPOINT_ENV: &str = "KASTELLAN_WEB_RESEARCH_ENDPOINT";
const EMBED_ENDPOINT_ENV: &str = "KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT";
const EMBED_MODEL_ENV: &str = "KASTELLAN_WEB_RESEARCH_EMBED_MODEL";
const DEFAULT_EMBED_MODEL: &str = "embeddinggemma";

/// Opt into the trusted embed-broker sidecar (Slice B). When set to `1` AND an
/// embed endpoint is configured, the worker reaches the embedding backend only
/// through a core-spawned broker over a bound UDS — the embed host is dropped
/// from the worker's `Net::Allowlist` and the direct embed-endpoint env is not
/// injected. See [`crate::broker`].
const USE_EMBED_BROKER_ENV: &str = "KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER";

/// Opt into the Linux Firecracker micro-VM backend for web-research. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_RESEARCH_USE_MICROVM";

/// In-rootfs path of the web-research worker binary (staged there by
/// `build-web-research-rootfs.sh`). Used by the micro-VM entry, not the host path.
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-research";

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

/// The #452 resolve-time guard, web-research flavour: unlike web-search there
/// is no search-broker escape hatch — this worker's only broker is the
/// embed-broker, which carries no search traffic — so a force-routed worker
/// with a `localhost`-NAME SearxNG endpoint reaches nothing in ANY broker mode
/// (the proxy range-denies what the name resolves to). A *literal* loopback
/// endpoint is NOT flagged: the proxy dials an operator-allowlisted literal
/// via its carve-out.
fn forced_localhost_misconfig(
    is_microvm: bool,
    endpoint: &str,
    get_env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    use crate::workers::endpoint_guard::{egress_will_force_route, endpoint_is_localhost_name};
    if !egress_will_force_route(is_microvm, get_env) || !endpoint_is_localhost_name(endpoint) {
        return None;
    }
    Some(format!(
        "{ENDPOINT_ENV} ({endpoint}) uses a `localhost` name, but this worker's \
         egress is force-routed through the egress proxy, which range-denies \
         what a localhost name resolves to (SSRF/DNS-rebinding defense) — every \
         search would fail at request time. web-research has no search-broker (its \
         broker carries only embed traffic); use the literal-IP form (e.g. \
         http://127.0.0.1:<port> — an allowlisted literal is dialed via the \
         proxy's carve-out) or a routable SearxNG host."
    ))
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

/// The env pairs shared by the host and micro-VM entries: the SearxNG endpoint,
/// the verbatim content allowlist JSON, and — when `embed_endpoint` is set — the
/// embed endpoint + model (model defaults to [`DEFAULT_EMBED_MODEL`]). Order is
/// stable (endpoint, allowlist, [embed endpoint, embed model]); the micro-VM
/// entry appends its `KASTELLAN_MICROVM_*` pairs after these. Pure.
fn base_env(
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> Vec<(String, String)> {
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
    env
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
    let env = base_env(endpoint, embed_endpoint, embed_model, allowlist);
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

/// Env pairs for **broker mode**: like [`base_env`] with no embed endpoint, but
/// still carrying the embed *model* — the worker's `BrokeredEmbedder` sends the
/// model per request, so it needs `KASTELLAN_WEB_RESEARCH_EMBED_MODEL`, but must
/// NOT get `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` (in broker mode the worker
/// reaches the backend only through the bound UDS, whose path core injects as
/// `KASTELLAN_EMBED_BROKER_UDS` at spawn). Order: endpoint, allowlist, embed
/// model. Pure.
fn broker_env(endpoint: &str, embed_model: Option<&str>, allowlist: &[String]) -> Vec<(String, String)> {
    // base_env with embed_endpoint=None gives just [endpoint, allowlist].
    let mut env = base_env(endpoint, None, None, allowlist);
    env.push((
        EMBED_MODEL_ENV.to_string(),
        embed_model.unwrap_or(DEFAULT_EMBED_MODEL).to_string(),
    ));
    env
}

/// Build the [`ToolEntry`] for web-research in **broker mode** (Slice B): the
/// worker embeds through a core-spawned trusted broker sidecar, so the embed
/// backend host is dropped from `Net::Allowlist` and the direct embed-endpoint
/// env is omitted. The `broker` field carries the backend the broker forwards to;
/// core's spawn chokepoint spawns the broker, binds its UDS into the jail, and
/// injects `KASTELLAN_EMBED_BROKER_UDS`. The SearxNG endpoint + content
/// allowlist are unchanged from the direct entry. `embed_model` defaults to
/// [`DEFAULT_EMBED_MODEL`] when `None`.
pub fn web_research_broker_entry(
    binary: PathBuf,
    endpoint: &str,
    embed_endpoint: &str,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let env = broker_env(endpoint, embed_model, allowlist);
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        // NO embed host in the allowlist — the worker never reaches the backend
        // directly; it goes through the broker's UDS.
        net: Net::Allowlist(net_entries(endpoint, None, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        // Set at spawn time by core (spawn_broker binds the socket and
        // core binds it into the jail); the manifest leaves it None.
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
        broker: Some(crate::broker::BrokerSpec::embed(embed_endpoint)),
    }
}

/// Build the [`ToolEntry`] for web-research running inside a Firecracker micro-VM
/// (opt-in via `KASTELLAN_WEB_RESEARCH_USE_MICROVM=1`, Linux only). Mirrors the
/// host-mode [`web_research_entry_with_embed`] but as a VM net worker:
///
/// * `Net::Allowlist` = the same union (SearxNG endpoint ∪ embed ∪ content) as
///   host mode — **not** `Net::Deny`; web-research needs egress. Force-routing sets
///   `proxy_uds` at spawn, which makes `build_launch_plan` boot the VM with no NIC
///   and tunnel egress over the slice-4a vsock channel.
/// * `fs_read: vec![]` — no NIC and no local DNS (the egress proxy resolves
///   host-side). The per-instance MITM CA is appended to `fs_read` at spawn by
///   `rewrite_worker_policy`.
/// * `env` forwards the host env ([`base_env`]) plus `KASTELLAN_MICROVM_DIR` and
///   `KASTELLAN_MICROVM_ROOTFS=web-research.ext4` so the backend boots the right rootfs.
///
/// **Localhost-embed caveat (this DIRECT VM entry only):** in VM mode all
/// egress tunnels through the host-side proxy. A **literal** embed endpoint —
/// the default local Ollama `127.0.0.1:11434` included — stays reachable (the
/// proxy dials an operator-allowlisted literal via its carve-out; it is
/// unioned into `Net::Allowlist` here). A `localhost` **name** is dead (the
/// proxy range-denies what the name resolves to) → the query embed fails and
/// the worker degrades to lexical ranking with an `embed_note` (never
/// silent); `resolve()` emits an operator warning for that name+forced
/// misconfiguration (#429), and refuses a `localhost`-name *SearxNG* endpoint
/// outright (`Misconfigured`, #452 — no search-broker exists for
/// web-research). Non-force-routed host mode is unaffected. **Remedy:** set
/// `KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1` — the VM × broker entry
/// ([`web_research_firecracker_broker_entry`]) reaches a **loopback** embed backend
/// through the host-side broker (a second vsock channel), so hybrid ranking works
/// in VM mode even against local Ollama.
///
/// Linux-only: emits the `FirecrackerVm` backend variant.
#[cfg(target_os = "linux")]
pub fn web_research_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let mut env = base_env(endpoint, embed_endpoint, embed_model, allowlist);
    env.push(("KASTELLAN_MICROVM_DIR".to_string(), image_dir));
    env.push((
        "KASTELLAN_MICROVM_ROOTFS".to_string(),
        "web-research.ext4".to_string(),
    ));
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(endpoint, embed_endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
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

/// Build the [`ToolEntry`] for web-research running inside a Firecracker micro-VM
/// **AND** reaching a host-side embed broker (VM × broker; opt-in via
/// `USE_MICROVM=1` + `USE_EMBED_BROKER=1` + an embed endpoint). Combines the VM
/// entry (empty `fs_read`, `FirecrackerVm` backend, force-routable) with broker
/// mode: the embed host is **dropped** from `Net::Allowlist`, only the embed model
/// env is injected (not the endpoint), and `broker: Some(Embed)` tells core's
/// chokepoint to spawn the broker + bind its UDS. In the VM the broker rides a
/// second vsock channel (port 1026); the FC plan rewrites the injected
/// `KASTELLAN_EMBED_BROKER_UDS` to the in-guest relay path.
///
/// Because the broker runs host-side, this is the ONLY way a VM worker reaches a
/// *loopback/local* embed backend for hybrid ranking (the egress proxy SSRF-blocks
/// loopback). Linux-only.
#[cfg(target_os = "linux")]
pub fn web_research_firecracker_broker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    embed_endpoint: &str,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let mut env = broker_env(endpoint, embed_model, allowlist);
    env.push(("KASTELLAN_MICROVM_DIR".to_string(), image_dir));
    env.push((
        "KASTELLAN_MICROVM_ROOTFS".to_string(),
        "web-research.ext4".to_string(),
    ));
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        // NO embed host — the worker reaches the backend only through the broker.
        net: Net::Allowlist(net_entries(endpoint, None, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
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
        broker: Some(crate::broker::BrokerSpec::embed(embed_endpoint)),
    }
}

/// web-research's manifest. Discovery mirrors web-search.
pub struct WebResearchManifest;

impl WorkerManifest for WebResearchManifest {
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "web.research",
            summary: "Search the web, fetch the top results, and return the passages \
                      most relevant to the query (ranked). Prefer this over web.search \
                      when you need the answer content, not just links.",
            params: &[
                ToolParam { name: "query", description: "the research question", required: true },
                ToolParam {
                    name: "max_sources",
                    description: "max pages to fetch (optional)",
                    required: false,
                },
                ToolParam {
                    name: "max_passages",
                    description: "max ranked passages to return (optional)",
                    required: false,
                },
            ],
        })
    }

    fn name(&self) -> &'static str {
        TOOL_NAME
    }
    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let endpoint = (ctx.get_env)(ENDPOINT_ENV).unwrap_or_default();
        let embed_endpoint = (ctx.get_env)(EMBED_ENDPOINT_ENV).filter(|s| !s.trim().is_empty());
        let embed_model = (ctx.get_env)(EMBED_MODEL_ENV).filter(|s| !s.trim().is_empty());
        let allowlist = (ctx.allowlist)(TOOL_NAME);

        // Broker mode (Slice B): only active when the operator opts in AND an
        // embed endpoint is configured (nothing to broker otherwise → falls
        // through to the direct/lexical entry, byte-identical to today).
        let use_broker = (ctx.get_env)(USE_EMBED_BROKER_ENV).unwrap_or_default().trim() == "1"
            && embed_endpoint.is_some();

        // Firecracker micro-VM mode (Linux) short-circuits host binary discovery:
        // the worker binary lives inside the rootfs image, not on the host.
        // Linux-only — on macOS USE_MICROVM is never read so the `FirecrackerVm`
        // variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            let use_microvm = (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
            if use_microvm {
                // #452: a Net::Allowlist VM worker force-routes unconditionally,
                // and the embed-broker can't carry search traffic, so a
                // `localhost`-name SearxNG endpoint is dead in EVERY VM
                // sub-mode (a literal loopback works via the proxy carve-out).
                if let Some(detail) = forced_localhost_misconfig(true, &endpoint, ctx.get_env) {
                    return Resolution::Misconfigured { detail };
                }
                // #429: a `localhost`-name embed endpoint without the
                // embed-broker is unreachable here → silent hybrid→lexical
                // downgrade; warn (registration proceeds — the tool works).
                if let Some(w) = crate::workers::endpoint_guard::embed_local_warning(
                    true,
                    use_broker,
                    embed_endpoint.as_deref(),
                ) {
                    tracing::warn!(target: "web_research.resolve", "{w}");
                }
                let binary = PathBuf::from(MICROVM_WORKER_BIN);
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                // VM × broker: the broker runs host-side and the VM worker reaches it
                // over the slice-4a vsock UDS (port 1026), so a loopback embed backend
                // works in VM mode. `use_broker` guarantees an embed endpoint.
                if use_broker {
                    let embed_endpoint = embed_endpoint.as_deref().expect("use_broker implies Some");
                    return Resolution::Register(web_research_firecracker_broker_entry(
                        binary,
                        image_dir,
                        &endpoint,
                        embed_endpoint,
                        embed_model.as_deref(),
                        &allowlist,
                    ));
                }
                return Resolution::Register(web_research_firecracker_entry(
                    binary,
                    image_dir,
                    &endpoint,
                    embed_endpoint.as_deref(),
                    embed_model.as_deref(),
                    &allowlist,
                ));
            }
        }

        // #452 (host path): the guard applies iff the operator enabled
        // force-routing — a plain host worker resolves localhost itself.
        if let Some(detail) = forced_localhost_misconfig(false, &endpoint, ctx.get_env) {
            return Resolution::Misconfigured { detail };
        }
        // #429 (host path): warn on a force-routed, unbrokered
        // `localhost`-name embed endpoint (hybrid→lexical downgrade);
        // never blocks registration.
        if let Some(w) = crate::workers::endpoint_guard::embed_local_warning(
            crate::workers::endpoint_guard::egress_will_force_route(false, ctx.get_env),
            use_broker,
            embed_endpoint.as_deref(),
        ) {
            tracing::warn!(target: "web_research.resolve", "{w}");
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
        if use_broker {
            // `embed_endpoint.is_some()` is guaranteed by `use_broker`.
            let embed_endpoint = embed_endpoint.as_deref().expect("use_broker implies Some");
            return Resolution::Register(web_research_broker_entry(
                binary,
                &endpoint,
                embed_endpoint,
                embed_model.as_deref(),
                &allowlist,
            ));
        }
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
    fn resolve_broker_mode_drops_embed_host_sets_spec_and_omits_endpoint_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            _ => None, // EMBED_MODEL_ENV unset -> default model
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| {
            vec![
                "searx.example.org".to_string(),
                ".docs.example.org".to_string(),
            ]
        };
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // Broker declaration carries the kind + backend endpoint.
                let spec = entry.broker.as_ref().expect("broker set in broker mode");
                assert_eq!(spec.kind, crate::broker::BrokerKind::Embed);
                assert_eq!(spec.endpoint, "http://127.0.0.1:11434/v1/embeddings");
                // Embed host is NOT in the net allowlist (the backend is loopback,
                // but even a routable embed host must be absent — it leaves egress).
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert!(
                            hosts.iter().all(|h| !h.starts_with("127.0.0.1")),
                            "embed host must be absent from net: {hosts:?}"
                        );
                        assert_eq!(
                            hosts,
                            &vec![
                                "searx.example.org:443".to_string(),
                                "docs.example.org:443".to_string(),
                            ]
                        );
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // The direct embed-endpoint env is NOT injected; the model IS.
                let has_key = |k: &str| entry.policy.env.iter().any(|(ek, _)| ek == k);
                assert!(!has_key(EMBED_ENDPOINT_ENV), "no direct embed endpoint env in broker mode");
                assert!(
                    entry
                        .policy
                        .env
                        .iter()
                        .any(|(k, v)| k == EMBED_MODEL_ENV && v == "embeddinggemma"),
                    "embed model env injected for the worker's BrokeredEmbedder"
                );
                // broker_uds is set at spawn, not the manifest.
                assert!(entry.policy.broker_uds.is_none());
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_broker_gate_without_embed_endpoint_is_direct() {
        // Gate on but no embed endpoint => nothing to broker => byte-identical to
        // the lexical-only direct entry (broker None, no broker net drop).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(entry.broker.is_none(), "no broker without an embed endpoint");
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

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_vm_broker_drops_embed_host_sets_vm_backend_and_broker_spec() {
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist =
            |_t: &str| vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // VM backend AND a broker spec (the two combined).
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                let spec = entry.broker.as_ref().expect("VM broker mode declares a broker spec");
                assert_eq!(spec.kind, crate::broker::BrokerKind::Embed);
                assert_eq!(spec.endpoint, "http://127.0.0.1:11434/v1/embeddings");
                // Embed host ABSENT from egress; VM fs_read empty.
                assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty");
                match &entry.policy.net {
                    Net::Allowlist(hosts) => assert!(
                        hosts.iter().all(|h| !h.starts_with("127.0.0.1")),
                        "embed host must be absent from net: {hosts:?}"
                    ),
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // Direct embed-endpoint env omitted; model present; broker_uds set at spawn.
                assert!(!entry.policy.env.iter().any(|(k, _)| k == EMBED_ENDPOINT_ENV));
                assert!(entry
                    .policy
                    .env
                    .iter()
                    .any(|(k, v)| k == EMBED_MODEL_ENV && v == "embeddinggemma"));
                assert!(entry.policy.broker_uds.is_none());
            }
            other => panic!("expected Register(VM broker entry), got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_vm_without_broker_stays_direct_vm_entry() {
        // USE_MICROVM without the broker gate => the existing direct/degrade VM entry.
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                assert!(entry.broker.is_none(), "no broker without the gate + endpoint");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_uses_microvm_entry_when_opted_in() {
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                // In-rootfs binary path, not a host-discovered binary.
                assert_eq!(
                    entry.binary,
                    PathBuf::from("/usr/local/bin/kastellan-worker-web-research")
                );
                let env = &entry.policy.env;
                let dir = env.iter().find(|(k, _)| k == "KASTELLAN_MICROVM_DIR").map(|(_, v)| v.as_str());
                assert_eq!(dir, Some("/var/lib/kastellan/microvm"));
            }
            other => panic!("expected Register(VM entry), got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_entry_is_vm_with_empty_fs_read_and_forwarded_env() {
        let allowlist = vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let entry = web_research_firecracker_entry(
            PathBuf::from("/usr/local/bin/kastellan-worker-web-research"),
            "/var/lib/kastellan/microvm".to_string(),
            "https://searx.example.org:8888/search",
            Some("http://embed.example.org:11434/v1/embeddings"),
            None, // default model
            &allowlist,
        );
        // VM backend, net client, no host paths shared in (the CA is added at spawn).
        assert!(matches!(
            entry.sandbox_backend,
            Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
        ));
        assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
        assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty (no NIC, no local DNS)");
        assert!(entry.policy.proxy_uds.is_none(), "proxy_uds is set at spawn, not in the manifest");
        assert_eq!(entry.policy.cpu_ms, 15_000);
        assert_eq!(entry.policy.mem_mb, 512);
        assert_eq!(entry.wall_clock_ms, Some(60_000));
        // Union Net::Allowlist: endpoint host:port, embed host:port, content host:443.
        match &entry.policy.net {
            Net::Allowlist(hosts) => {
                assert_eq!(hosts[0], "searx.example.org:8888", "endpoint host:port first");
                assert!(hosts.iter().any(|h| h == "embed.example.org:11434"), "embed host:port present: {hosts:?}");
                assert!(hosts.iter().any(|h| h == "docs.example.org:443"), "content host:443 present: {hosts:?}");
            }
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // Env forwards endpoint + verbatim allowlist + embed endpoint/model + the VM image dir + rootfs.
        let env = &entry.policy.env;
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get(ENDPOINT_ENV), Some("https://searx.example.org:8888/search"));
        assert_eq!(get("KASTELLAN_WEB_RESEARCH_ALLOWLIST"), Some(r#"["searx.example.org",".docs.example.org"]"#));
        assert_eq!(get(EMBED_ENDPOINT_ENV), Some("http://embed.example.org:11434/v1/embeddings"));
        assert_eq!(get(EMBED_MODEL_ENV), Some("embeddinggemma"), "default model forwarded");
        assert_eq!(get("KASTELLAN_MICROVM_DIR"), Some("/var/lib/kastellan/microvm"));
        assert_eq!(get("KASTELLAN_MICROVM_ROOTFS"), Some("web-research.ext4"));
    }

    #[test]
    fn resolve_forced_host_localhost_name_searxng_is_misconfigured() {
        // Host mode + force-routing flag + a `localhost`-NAME SearxNG endpoint:
        // web-research has no search-broker and the proxy range-denies what the
        // name resolves to, so this config reaches nothing (#452).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://localhost:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("KASTELLAN_WEB_RESEARCH_ENDPOINT"), "detail: {detail}");
                assert!(detail.contains("127.0.0.1"), "literal-IP remedy missing: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_forced_host_literal_loopback_searxng_still_registers() {
        // Option-A policy pin (2026-07-16 review): a LITERAL loopback SearxNG
        // endpoint is dialed via the egress proxy's allowlisted-literal
        // carve-out, so it works force-routed and must keep registering.
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(_) => {}
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_forced_host_localhost_name_embed_only_warns_and_registers() {
        // A `localhost`-NAME *embed* endpoint under force-routing degrades
        // ranking but does not break the tool: warn-only, registration
        // proceeds and the entry is unchanged (#429).
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://localhost:11434/v1/embeddings".to_string()),
            "KASTELLAN_EGRESS_FORCE_ROUTING" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist =
            |_t: &str| vec!["searx.example.org".to_string(), "localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // The embed env is still injected — the entry itself is unchanged
                // (the warning is a log line, not a policy change).
                assert!(entry.policy.env.iter().any(|(k, _)| k == EMBED_ENDPOINT_ENV));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_microvm_localhost_name_searxng_is_misconfigured() {
        // A VM worker force-routes unconditionally: a `localhost`-name SearxNG
        // is dead in VM mode regardless of the host force-routing flag (#452).
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            ENDPOINT_ENV => Some("http://localhost:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("KASTELLAN_WEB_RESEARCH_ENDPOINT"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_vm_embed_broker_does_not_rescue_localhost_name_searxng() {
        // The embed-broker carries only embed traffic — it must NOT exempt a
        // `localhost`-name SearxNG endpoint from the #452 guard.
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_RESEARCH_USE_MICROVM" => Some("1".to_string()),
            USE_EMBED_BROKER_ENV => Some("1".to_string()),
            ENDPOINT_ENV => Some("http://localhost:8888/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://127.0.0.1:11434/v1/embeddings".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["localhost".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { .. } => {}
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }
}
