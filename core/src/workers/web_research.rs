//! Host-side manifest + `ToolEntry` constructor for the web-research worker.
//!
//! Composite of search + fetch: the LLM supplies only the query; the operator
//! controls the SearxNG endpoint (`KASTELLAN_WEB_RESEARCH_ENDPOINT`) and the
//! content-host allowlist (`tool_allowlists` keyed `"web-research"`). The one
//! allowlist gates both the endpoint host and every fetched result URL. The
//! `Net::Allowlist` is the union of the endpoint host:port and the content
//! host:443 entries — unless the search-broker is enabled
//! (`KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER=1`, #464), in which case the SearxNG
//! host is dropped (the broker holds the only route) and the worker reaches it
//! over a bound UDS with zero direct search egress. The egress proxy owns
//! IP-level containment. See `docs/threat-model.md` ("Network egress").

use std::path::PathBuf;

use kastellan_sandbox::{Net, Profile, SandboxPolicy};
use url::Url;

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{
    discover_binary, ResolveCtx, Resolution, ToolDoc, ToolParam, WorkerManifest,
};
use crate::workers::endpoint_guard;

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

/// Opt into the trusted search-broker sidecar (#464): the worker reaches SearxNG
/// only through a core-spawned search-broker over a bound UDS — the SearxNG host
/// is dropped from `Net::Allowlist` and no endpoint env is injected (core injects
/// `KASTELLAN_SEARCH_BROKER_UDS` at spawn; the worker's `choose_search_provider`
/// then selects the brokered provider). This lets a force-routed / VM worker use a
/// loopback/name-form SearxNG with zero direct search egress. Mutually exclusive
/// with [`USE_EMBED_BROKER_ENV`]: a worker binds at most ONE broker socket (single
/// `broker_uds`, one vsock channel) — search XOR embed. Choosing the search-broker
/// leaves the embed path DIRECT (a loopback-name embed endpoint then still degrades
/// to lexical, warned per #429).
const USE_SEARCH_BROKER_ENV: &str = "KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER";

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

/// The #452 resolve-time guard, web-research flavour. A force-routed worker with
/// a `localhost`-NAME SearxNG endpoint reaches nothing (the proxy range-denies
/// what the name resolves to) UNLESS the search-broker is enabled — in which case
/// the worker never dials the endpoint at all (the broker holds the route
/// host-side), so `use_search_broker` short-circuits to `None`. A *literal*
/// loopback endpoint is NOT flagged either way: the proxy dials an
/// operator-allowlisted literal via its carve-out. Predicate + shared message
/// live in [`endpoint_guard::forced_localhost_misconfig`]; the remedy tail is
/// ours, and — unlike web-search, whose worker allowlist derives from the
/// endpoint — it must say to update the `tool_allowlists` row too: the worker
/// validates the endpoint host against that row and fail-closes when it is
/// missing (the #428 lesson), so "switch the env to the literal" alone trades one
/// registered-but-dead config for another. Https for routable hosts for the same
/// reason (worker-side rule: plain http is loopback-only). The search-broker is
/// now offered as the fourth remedy.
fn forced_localhost_misconfig(
    use_search_broker: bool,
    force_routed: bool,
    endpoint: &str,
) -> Option<String> {
    if use_search_broker {
        // The search-broker owns SearxNG egress host-side; the worker never dials
        // the endpoint, so a `localhost` NAME is fine here.
        return None;
    }
    endpoint_guard::forced_localhost_misconfig(
        ENDPOINT_ENV,
        endpoint,
        force_routed,
        &format!(
            "use the literal-IP form (e.g. http://127.0.0.1:<port> — an \
             allowlisted literal is dialed via the proxy's carve-out) or an \
             https:// routable SearxNG host (plain http is loopback-only) — \
             either way the new host must also be on this tool's \
             `tool_allowlists` row (the worker validates the endpoint host \
             against it and fail-closes when missing) — or set \
             {USE_SEARCH_BROKER_ENV}=1 (the worker then reaches SearxNG only \
             through the trusted search-broker sidecar: no worker search egress, \
             no endpoint-host row needed).",
        ),
    )
}

/// `Some(warning)` iff web-research's *optional* embed endpoint is configured
/// but unreachable: egress is force-routed, the embed-broker is not enabled,
/// and the endpoint is a `localhost` name (the proxy range-denies what it
/// resolves to). Assuming the name is on the tool's allowlist, the worker
/// still functions — ranking silently degrades hybrid→lexical — so this is an
/// operator warning, not `Misconfigured` (#429). `None` when not force-routed
/// (the worker resolves `localhost` itself), when brokered (the embed-broker
/// reaches the backend over its UDS), or when the endpoint is a literal IP
/// (dialed via the proxy's allowlisted-literal carve-out) / routable / unset.
/// Lives here, not in [`endpoint_guard`]: the message cites this worker's env
/// names, so it belongs beside the consts it names.
fn embed_local_warning(
    force_routed: bool,
    use_broker: bool,
    embed_endpoint: Option<&str>,
) -> Option<String> {
    if !force_routed || use_broker {
        return None;
    }
    let embed = embed_endpoint?;
    if !endpoint_guard::endpoint_is_localhost_name(embed) {
        return None;
    }
    Some(format!(
        "web-research: {EMBED_ENDPOINT_ENV} ({embed}) uses a `localhost` name \
         while egress is force-routed: the egress proxy range-denies what the \
         name resolves to (SSRF/DNS-rebinding defense), so the query embed \
         fails and ranking degrades hybrid->lexical. Remedies — note the worker \
         validates the embed host against this tool's `tool_allowlists` row and \
         fail-closes the WHOLE worker when it is missing, so update the row to \
         match: the literal-IP form (e.g. http://127.0.0.1:11434 — an \
         allowlisted literal is dialed via the proxy's carve-out), an https:// \
         routable host (plain http is loopback-only), or \
         {USE_EMBED_BROKER_ENV}=1 (the broker path has no worker egress and \
         needs no allowlist entry)."
    ))
}

/// Union of the endpoint host:port, the optional embed-endpoint host:port, and
/// the content host:443 entries, de-duped (order-preserving: SearxNG endpoint
/// first, embed endpoint second, content hosts last). The content half reuses
/// web-fetch's canonical domain→`host:443` mapping (a wildcard `.d` keeps its
/// dot as `.d:443`, which the proxy matches as a subdomain suffix) so every
/// domain-allowlist consumer shares one mapping rule.
fn net_entries(endpoint: Option<&str>, embed_endpoint: Option<&str>, allowlist: &[String]) -> Vec<String> {
    // `None` endpoint (search-broker mode) drops the SearxNG host — the broker
    // holds the only route to it, so it never leaves the worker's egress.
    let mut entries = endpoint.map(endpoint_net_entry).unwrap_or_default();
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
    endpoint: Option<&str>,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> Vec<(String, String)> {
    let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    // `None` endpoint (search-broker mode) omits the endpoint env pair — the
    // worker reaches SearxNG only through the broker UDS core injects at spawn.
    let mut env = Vec::new();
    if let Some(ep) = endpoint {
        env.push((ENDPOINT_ENV.to_string(), ep.to_string()));
    }
    env.push(("KASTELLAN_WEB_RESEARCH_ALLOWLIST".to_string(), allow_json));
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
    let env = base_env(Some(endpoint), embed_endpoint, embed_model, allowlist);
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(Some(endpoint), embed_endpoint, allowlist)),
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
    let mut env = base_env(Some(endpoint), None, None, allowlist);
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
        net: Net::Allowlist(net_entries(Some(endpoint), None, allowlist)),
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

/// Build the [`ToolEntry`] for web-research in **search-broker mode** (#464): the
/// worker reaches SearxNG only through a core-spawned trusted search-broker over a
/// bound UDS, so the SearxNG host is dropped from `Net::Allowlist` and no endpoint
/// env is injected (core injects `KASTELLAN_SEARCH_BROKER_UDS` at spawn; the
/// worker's `choose_search_provider` selects the brokered provider). This is the
/// search XOR embed choice: unlike web-search — whose ONLY egress is SearxNG, so
/// its broker entry has an empty allowlist — web-research still fetches content
/// pages, so its `Net::Allowlist` keeps the content hosts (and a *direct* embed
/// host, if configured). Choosing the search-broker leaves the embed path DIRECT:
/// a loopback-name embed endpoint then still degrades to lexical (warned, #429);
/// point the embed endpoint at a routable/literal host, or use the embed-broker
/// instead (they are mutually exclusive — one broker socket per worker).
/// `embed_model` defaults to [`DEFAULT_EMBED_MODEL`] when `None`.
pub fn web_research_search_broker_entry(
    binary: PathBuf,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    // No endpoint env (the broker holds the route); a direct embed endpoint, if
    // set, is still injected + unioned into the allowlist.
    let env = base_env(None, embed_endpoint, embed_model, allowlist);
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        // No SearxNG host — the broker holds the only route to the search endpoint.
        net: Net::Allowlist(net_entries(None, embed_endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None, // set at spawn (rewrite_policy_for_broker)
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
    let mut env = base_env(Some(endpoint), embed_endpoint, embed_model, allowlist);
    env.push(("KASTELLAN_MICROVM_DIR".to_string(), image_dir));
    env.push((
        "KASTELLAN_MICROVM_ROOTFS".to_string(),
        "web-research.ext4".to_string(),
    ));
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(Some(endpoint), embed_endpoint, allowlist)),
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
        net: Net::Allowlist(net_entries(Some(endpoint), None, allowlist)),
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

/// Build the [`ToolEntry`] for web-research running inside a Firecracker micro-VM
/// **AND** reaching SearxNG through a host-side search-broker (VM × search-broker;
/// opt-in via `USE_MICROVM=1` + `USE_SEARCH_BROKER=1`). Combines the VM entry
/// (empty `fs_read`, `FirecrackerVm` backend, force-routable) with search-broker
/// mode: the SearxNG host is **dropped** from `Net::Allowlist`, no endpoint env is
/// injected, and `broker: Some(BrokerSpec::search(endpoint))` tells core's
/// chokepoint to spawn the broker + bind its UDS (in the VM the broker rides the
/// vsock channel, port 1026, exactly like the embed-broker). This is the way a VM
/// worker uses a **loopback/name-form** SearxNG with zero direct search egress.
/// The embed path stays DIRECT (a loopback-name embed endpoint degrades to
/// lexical, warned #429; use a routable/literal embed host in VM mode).
/// Linux-only.
#[cfg(target_os = "linux")]
pub fn web_research_firecracker_search_broker_entry(
    binary: PathBuf,
    image_dir: String,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let mut env = base_env(None, embed_endpoint, embed_model, allowlist);
    env.push(("KASTELLAN_MICROVM_DIR".to_string(), image_dir));
    env.push((
        "KASTELLAN_MICROVM_ROOTFS".to_string(),
        "web-research.ext4".to_string(),
    ));
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        // No SearxNG host — the broker holds the only route; a direct embed host,
        // if configured, stays in the union.
        net: Net::Allowlist(net_entries(None, embed_endpoint, allowlist)),
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
        broker: Some(crate::broker::BrokerSpec::search(endpoint)),
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

    fn allowlist_kind(&self) -> Option<kastellan_db::tool_allowlists::EntryKind> {
        Some(kastellan_db::tool_allowlists::EntryKind::Domain)
    }
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let endpoint = (ctx.get_env)(ENDPOINT_ENV).unwrap_or_default();
        let embed_endpoint = (ctx.get_env)(EMBED_ENDPOINT_ENV).filter(|s| !s.trim().is_empty());
        let embed_model = (ctx.get_env)(EMBED_MODEL_ENV).filter(|s| !s.trim().is_empty());
        let allowlist = (ctx.allowlist)(TOOL_NAME);

        // Single-broker XOR (#464): a worker binds at most one broker socket
        // (single `broker_uds`, one vsock channel), so the search-broker and
        // embed-broker flags are mutually exclusive — refuse the pair up front.
        let use_search_broker = ctx.flag_enabled(USE_SEARCH_BROKER_ENV);
        let use_embed_broker_flag = ctx.flag_enabled(USE_EMBED_BROKER_ENV);
        if use_search_broker && use_embed_broker_flag {
            return Resolution::Misconfigured {
                detail: format!(
                    "{USE_SEARCH_BROKER_ENV}=1 and {USE_EMBED_BROKER_ENV}=1 are \
                     mutually exclusive: a worker binds at most one broker socket \
                     (single `broker_uds`, one vsock channel — search XOR embed). \
                     Keep the broker for the backend that is local-only and make \
                     the other one routable (or unset its flag)."
                ),
            };
        }
        // Search-broker mode forwards to the SearxNG endpoint *host-side* (the
        // worker never dials it), so the endpoint is still required — with none,
        // there is nothing for the broker to reach. Fail closed at registration
        // with a clear message rather than spawning a broker pointed at an empty
        // endpoint (the search calls would then fail at runtime as an opaque
        // broker transport error). #464 review.
        if use_search_broker && endpoint.trim().is_empty() {
            return Resolution::Misconfigured {
                detail: format!(
                    "{USE_SEARCH_BROKER_ENV}=1 requires {ENDPOINT_ENV}: the \
                     search-broker forwards to that SearxNG endpoint host-side, so \
                     it must be set (a loopback/name-form URL is fine in broker \
                     mode — the broker holds the route)."
                ),
            };
        }
        // Embed-broker mode (Slice B): only active when the operator opts in AND
        // an embed endpoint is configured (nothing to broker otherwise → falls
        // through to the direct/lexical entry, byte-identical to today).
        let use_broker = use_embed_broker_flag && embed_endpoint.is_some();

        // Firecracker micro-VM mode (Linux) short-circuits host binary discovery:
        // the worker binary lives inside the rootfs image, not on the host.
        // Linux-only — on macOS USE_MICROVM is never read so the `FirecrackerVm`
        // variant is never referenced (issue #144); the guard below sees
        // `use_microvm = false` there.
        #[cfg(target_os = "linux")]
        let use_microvm = ctx.flag_enabled(USE_MICROVM_ENV);
        #[cfg(not(target_os = "linux"))]
        let use_microvm = false;

        // #452 — one guard for every path. A Net::Allowlist VM worker is never
        // given a direct NIC (plan.rs fails closed without the egress proxy), so a
        // `localhost`-name SearxNG endpoint is dead in EVERY VM sub-mode and in
        // force-routed host mode (a literal loopback works via the proxy carve-out;
        // a plain host worker resolves localhost itself) — UNLESS the search-broker
        // is enabled, which reaches SearxNG host-side so the worker never dials the
        // name (guard short-circuits on `use_search_broker`). The embed-broker does
        // NOT rescue it — that broker carries only embed traffic.
        let force_routed = endpoint_guard::egress_will_force_route(use_microvm, ctx.get_env);
        if let Some(detail) =
            forced_localhost_misconfig(use_search_broker, force_routed, &endpoint)
        {
            return Resolution::Misconfigured { detail };
        }
        // #429 — warn tier, never blocks registration: a `localhost`-name
        // embed endpoint without the embed-broker (hybrid→lexical downgrade).
        // (`localhost`-name CONTENT-allowlist entries are warned by the
        // generic #459 registry screen — see registry_build's Register arm.)
        if let Some(w) = embed_local_warning(force_routed, use_broker, embed_endpoint.as_deref())
        {
            tracing::warn!(target: "web_research.resolve", "{w}");
        }

        #[cfg(target_os = "linux")]
        if use_microvm {
            let binary = PathBuf::from(MICROVM_WORKER_BIN);
            let image_dir = ctx.microvm_image_dir();
            // VM × search-broker: the search-broker runs host-side and the VM
            // worker reaches it over the vsock UDS (port 1026), so a loopback/name
            // SearxNG works in VM mode with zero direct search egress. (XOR with the
            // embed-broker, refused above — at most one broker per worker.)
            if use_search_broker {
                return Resolution::Register(web_research_firecracker_search_broker_entry(
                    binary,
                    image_dir,
                    &endpoint,
                    embed_endpoint.as_deref(),
                    embed_model.as_deref(),
                    &allowlist,
                ));
            }
            // VM × embed-broker: the broker runs host-side and the VM worker reaches
            // it over the slice-4a vsock UDS (port 1026), so a loopback embed backend
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
        if use_search_broker {
            // Host × search-broker: SearxNG host dropped from egress, no endpoint
            // env; core spawns the broker + binds its UDS at spawn.
            return Resolution::Register(web_research_search_broker_entry(
                binary,
                &endpoint,
                embed_endpoint.as_deref(),
                embed_model.as_deref(),
                &allowlist,
            ));
        }
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
mod tests;
