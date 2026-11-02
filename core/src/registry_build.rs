//! Shared construction of the scheduler's [`ToolRegistry`] — the host-side
//! allowlist of *which* tools the daemon may dispatch.
//!
//! Factored out of the daemon binary (`main.rs`) so the operator CLI can
//! rebuild an identical registry in-process (e.g. `memory l3 run`, which
//! re-validates an approved skill's tools against the registry *as it is
//! now* — the live TOCTOU close). The builder here has **no audit side
//! effect**: it returns the per-tool records and the caller decides whether
//! to write the `registry.loaded` row. The daemon writes it; the CLI must
//! NOT (writing a spurious row would corrupt the snapshot the approval gate
//! reads).

use crate::prompt_assembly::AdvertisedTool;
use crate::scheduler::tool_dispatch::HANDOFF_TOOL;
use crate::scheduler::ToolRegistry;
use crate::worker_manifest::{ResolveCtx, Resolution, WorkerManifest};

/// Every worker the daemon may register. Adding a worker = add its
/// `WorkerManifest` impl + one line here. Order is irrelevant (the registry
/// is a keyed map).
pub static WORKER_MANIFESTS: &[&dyn WorkerManifest] = &[
    &crate::workers::shell_exec::ShellExecManifest,
    &crate::workers::gliner_relex::GlinerRelexManifest,
    &crate::workers::python_exec::PythonExecManifest,
    &crate::workers::web_fetch::WebFetchManifest,
    &crate::workers::web_search::WebSearchManifest,
    &crate::workers::web_research::WebResearchManifest,
    &crate::workers::browser_driver::BrowserDriverManifest,
    &crate::workers::mail::MailManifest,
];

/// The kind of `tool_allowlists` entry a tool uses, discovered by scanning the
/// static manifest list. `None` for a tool that declares no allowlist or an
/// unrecognized name — the CLI treats `None` as the argv0 default, preserving
/// today's behaviour for any tool name that is not a known allowlist consumer.
/// Pure.
///
/// A declaring worker can no longer be missed here: since #545 the tool name
/// and the kind are one value, so matching the name IS having the kind.
pub fn allowlist_kind_for_tool(
    name: &str,
) -> Option<kastellan_db::tool_allowlists::EntryKind> {
    WORKER_MANIFESTS
        .iter()
        .filter_map(|m| m.allowlist())
        .find(|decl| decl.tool == name)
        .map(|decl| decl.kind)
}

/// True iff this entry runs as a Firecracker micro-VM worker — the
/// always-force-routed case for the #459 screen (`linux_firecracker/plan.rs`
/// fail-closed refuses to boot a `Net::Allowlist` VM without the egress
/// proxy, so a direct route never exists in VM mode). Non-Linux builds have
/// no VM backend variant, so the answer is statically `false` there.
#[cfg(target_os = "linux")]
fn entry_is_vm(entry: &crate::scheduler::tool_dispatch::ToolEntry) -> bool {
    matches!(
        entry.sandbox_backend,
        Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
    )
}
#[cfg(not(target_os = "linux"))]
fn entry_is_vm(_entry: &crate::scheduler::tool_dispatch::ToolEntry) -> bool {
    false
}

/// One per-tool record carried in the `registry.loaded` audit-row payload.
#[derive(serde::Serialize)]
pub struct LoadedToolRecord {
    pub name: String,
    pub binary: String,
    pub allowlist_len: usize,
    /// SHA-256 of the canonical-form allowlist: `argv0_1 || '\n' || …`
    /// (lexicographically sorted, trailing newline after the last entry;
    /// empty list → SHA-256 of the empty string).
    pub allowlist_sha256: String,
}

/// SHA-256 of the canonical-form (sorted, newline-joined) argv0 allowlist.
/// A trailing newline follows each entry including the last; an empty list
/// hashes the empty string (zero bytes fed to the hasher).
pub fn sha256_argv0_list(argv0s: &[String]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&String> = argv0s.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for argv0 in sorted {
        hasher.update(argv0.as_bytes());
        hasher.update(b"\n");
    }
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// What `assemble_registry` decided to tell the planner about one declaring
/// worker's allowlist. Built once per manifest and rendered per `ToolDoc`.
///
/// A third case exists because "the advertisable list came back empty" is
/// ambiguous, and the two readings have opposite consequences: zero stored rows
/// really does mean nothing is permitted, while rows that all failed the kind
/// filter mean the tool is as capable as ever and merely undescribable. Only
/// this site can tell them apart — it is what holds the fetched rows — so the
/// distinction is decided here and carried, never re-derived downstream.
enum Advertisement {
    /// The live permitted set, worded with the kind the manifest declares.
    Described(kastellan_db::tool_allowlists::EntryKind, Vec<String>),
    /// Rows exist and are ENFORCED, but not one is stored under the declared
    /// kind, so none can be named in this tool's wording. Carries the enforced
    /// row count — the planner is told the restriction is real without being
    /// told values it would then use in the wrong shape.
    Opaque(usize),
}

/// Keep only the rows stored under the kind this worker declares, warning about
/// any that are dropped (#541).
///
/// **Why a row's kind can disagree with its tool's.** Nothing constrains the two
/// to match: migration `0021` backfilled every pre-existing row as `argv0`
/// regardless of tool, `kastellan-cli tools allowlist add` falls back to `Argv0`
/// for a tool it does not recognise, and the runtime role holds direct `INSERT`
/// on the table. The kind is what picks the *wording* of the advertised line, so
/// an `argv0` row under `web-fetch` would otherwise be announced as
/// "only these hosts are reachable: `/usr/bin/ls`" — a permitted value the
/// planner would then try to use as a host. A wrong description of a real
/// restriction is worse than no description, which is the whole premise of
/// #533.
///
/// **Advertisement only — enforcement keeps every row.** Filtering the enforced
/// set would silently narrow what a deployed worker may do, on a host whose
/// operator did nothing wrong; withholding from the prompt only costs the
/// planner a value it must then ask about, and the warning names the row so the
/// operator can fix it. (Making enforcement kind-aware is a separate decision
/// with an upgrade hazard and a live gate — [#554], not smuggled in here.)
///
/// [#554]: https://github.com/hherb/kastellan/issues/554
///
/// An unrecognised `kind` (schema drift) matches no declared kind, so it is
/// withheld and named too — [`kastellan_db::tool_allowlists::AllowlistRow`]
/// keeps the column's raw text precisely so this warning can print it.
fn withhold_kind_mismatches(
    tool: &str,
    decl: crate::worker_manifest::AllowlistDecl,
    rows: &[kastellan_db::tool_allowlists::AllowlistRow],
) -> Vec<String> {
    let (matching, mismatched): (Vec<_>, Vec<_>) =
        rows.iter().partition(|r| r.is_kind(decl.kind));
    if !mismatched.is_empty() {
        tracing::warn!(
            tool,
            declared_kind = decl.kind.as_str(),
            mismatched = ?mismatched
                .iter()
                .map(|r| format!("{} (kind={})", r.value, r.kind))
                .collect::<Vec<_>>(),
            "tool_allowlists rows are stored under a different kind than this worker \
             declares, so they are NOT advertised to the planner — describing them with \
             this tool's wording would name a permitted value in the wrong shape. They \
             are still ENFORCED; fix each row's kind (remove and re-add it) or delete it"
        );
    }
    matching.into_iter().map(|r| r.value.clone()).collect()
}

/// Operator-facing warnings about the permitted set that is about to be
/// advertised to the planner.
///
/// Lives here rather than in the renderer because `prompt_assembly` deliberately
/// holds no `tracing` (the renderer is a pure function), while both conditions
/// below are silent from the operator's side and have a prompt consequence:
/// the rendered `allowed:` line is never persisted — `inner_loop_audit` stores
/// only `system_prompt_sha256` — so an operator cannot read back what the
/// planner was told.
fn advertisement_warnings(tool: &str, entries: &[String]) {
    use crate::prompt_assembly::allowed_values::{
        select_advertised, ADVERTISED_ALLOWLIST_MAX, ADVERTISED_ALLOWLIST_MAX_BYTES,
    };

    // The caps are announced to the MODEL ("showing 30 of 31") but were silent
    // to the operator, who would otherwise have no way to learn that adding a
    // 31st entry — or one very long one — left it permanently invisible to the
    // planner. Asks the renderer's own selection rather than re-deriving the
    // withheld set here: two computations of one number is how the operator
    // ends up with two accounts and no way to tell which is wrong.
    let selection = select_advertised(entries);
    if !selection.withheld.is_empty() {
        tracing::warn!(
            tool,
            total = selection.total(),
            advertised = selection.shown.len(),
            count_cap = ADVERTISED_ALLOWLIST_MAX,
            byte_cap = ADVERTISED_ALLOWLIST_MAX_BYTES,
            withheld = ?selection.withheld,
            "allowlist exceeds an advertised cap: these entries are ENFORCED but \
             invisible to the planner, which will never propose them"
        );
    }

    // A row carrying a comma, whitespace or a backtick cannot be a plausible
    // argv0 and does not render unambiguously in a quoted, comma-joined list.
    // Advertised anyway — withholding it would make the advertisement disagree
    // with what the worker enforces — but the operator is told, because the
    // near-certain cause is one `tools allowlist add` that meant to add several.
    let ambiguous: Vec<&str> = entries
        .iter()
        .filter(|e| e.contains(',') || e.contains('`') || e.chars().any(char::is_whitespace))
        .map(String::as_str)
        .collect();
    if !ambiguous.is_empty() {
        tracing::warn!(
            tool,
            entries = ?ambiguous,
            "tool_allowlists rows carry a comma, whitespace or a backtick, so each is ONE \
             permitted value that no plausible command matches — did a single \
             `tools allowlist add` mean to add several entries? They are advertised \
             quoted, so the planner at least sees the real value boundaries"
        );
    }
}

/// Build the registry of tools the scheduler may dispatch by resolving every
/// [`WORKER_MANIFESTS`] entry against the host environment. Pre-fetches each
/// manifest's argv allowlist from the `tool_allowlists` DB table (the only
/// async step), then delegates to the pure [`assemble_registry`].
///
/// `exe_dir` (the directory of the running `kastellan` binary, from
/// `current_exe()`) seeds the exe-relative sibling discovery default; pass
/// `None` to disable that fallback (override-env-only).
///
/// **Writes no audit row** — returns the per-tool records so the daemon can
/// write `registry.loaded` itself.
pub async fn build_tool_registry(
    pool: &sqlx::PgPool,
    exe_dir: Option<std::path::PathBuf>,
) -> Result<(ToolRegistry, Vec<LoadedToolRecord>, Vec<AdvertisedTool>), kastellan_db::DbError> {
    use std::collections::HashMap;
    use std::path::Path;

    // 1. Pre-fetch allowlists for every manifest that declares one.
    let mut allowlists: HashMap<String, Vec<kastellan_db::tool_allowlists::AllowlistRow>> =
        HashMap::new();
    for m in WORKER_MANIFESTS {
        if let Some(decl) = m.allowlist() {
            let tool = decl.tool;
            let al = kastellan_db::tool_allowlists::list_for_tool(pool, tool)
                .await
                .map_err(|e| {
                    kastellan_db::DbError::Query(format!("loading {tool} allowlist: {e}"))
                })?;
            allowlists.insert(tool.to_string(), al);
        }
    }

    // Preserve the deprecation breadcrumb for the retired env-var allowlist.
    if std::env::var_os("KASTELLAN_SHELL_EXEC_ALLOWLIST").is_some() {
        tracing::warn!(
            "KASTELLAN_SHELL_EXEC_ALLOWLIST is no longer honored; \
             use 'kastellan-cli tools allowlist add <tool> <argv0|domain>' \
             to populate the DB"
        );
    }

    // 2. Build the real ResolveCtx over std::env + the live filesystem.
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &Path| p.exists();
    let is_dir = |p: &Path| p.is_dir();
    let allowlist = |tool: &str| allowlists.get(tool).cloned().unwrap_or_default();
    let canonicalize = |p: &Path| std::fs::canonicalize(p).ok();
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &is_dir,
        exe_dir: exe_dir.as_deref(),
        canonicalize: &canonicalize,
        allowlist: &allowlist,
    };

    // 3. Pure assembly.
    Ok(assemble_registry(WORKER_MANIFESTS, &ctx))
}

/// Pure payload builder for the `registry.loaded` audit row. The daemon
/// calls this then `kastellan_db::audit::insert`; the CLI never does.
pub fn build_registry_loaded_payload(tools: &[LoadedToolRecord]) -> serde_json::Value {
    serde_json::json!({ "tools": tools })
}

/// Pure assembly: iterate a worker-manifest list against a fully-built
/// [`ResolveCtx`] and produce the registry + the per-tool records for the
/// `registry.loaded` audit row. No async, no DB — unit-testable with fakes.
///
/// `Register` ⇒ insert + record + INFO log; `Disabled` ⇒ INFO log only;
/// `Misconfigured` ⇒ ERROR log only (the daemon still starts — fail-soft).
pub fn assemble_registry(
    manifests: &[&dyn WorkerManifest],
    ctx: &ResolveCtx<'_>,
) -> (ToolRegistry, Vec<LoadedToolRecord>, Vec<AdvertisedTool>) {
    let mut reg = ToolRegistry::new();
    let mut loaded: Vec<LoadedToolRecord> = Vec::new();
    // Planner-facing tool descriptions, collected ONLY for tools that register
    // (the `Register` arm below) — a disabled/misconfigured worker is never
    // advertised, so the planner is never told of a tool it can't dispatch.
    let mut docs: Vec<AdvertisedTool> = Vec::new();
    for m in manifests {
        if m.name() == HANDOFF_TOOL || m.name() == crate::scheduler::notebook::sift::SIFT_TOOL {
            tracing::warn!(
                tool = m.name(),
                "worker manifest claims the reserved built-in name; skipping"
            );
            continue;
        }
        match m.resolve(ctx) {
            Resolution::Register(entry) => {
                let name = m.name();
                // #459 residual: a broker-declaring worker whose broker binary
                // is not discoverable would register, be advertised to the
                // planner, and then fail fail-closed on its first dispatch at
                // the spawn chokepoint ("no matching broker config"). Refuse it
                // here instead — the same drift-proof discovery the daemon runs
                // at startup (`BrokerConfigs::from_env`), keyed off this ctx
                // (main.rs feeds both the identical `exe_dir`). Unconditional: a
                // missing broker binary is dead in every mode, force-routed or not.
                if let Some(spec) = &entry.broker {
                    if !crate::broker::config::broker_bin_present(spec.kind, ctx) {
                        tracing::error!(
                            tool = name,
                            kind = ?spec.kind,
                            "worker declares a broker but its binary is not \
                             discoverable; skipping — it would register but every \
                             dispatch fails fail-closed at the spawn chokepoint"
                        );
                        continue;
                    }
                }
                // #459 generic guard: a force-routed worker whose
                // Net::Allowlist carries `localhost` NAMES is statically dead
                // for those hosts (proxy resolves the name → loopback →
                // range-denied). All entries dead ⇒ refuse exactly like
                // Misconfigured; a subset ⇒ warn and register. Per-manifest
                // guards (#452/#457) still fire first inside resolve() with
                // their more precise remedies; this screen is the generic
                // backstop covering every current and future manifest.
                let force_routed = crate::workers::endpoint_guard::egress_will_force_route(
                    entry_is_vm(&entry),
                    ctx.get_env,
                );
                // Net entries this screen proved statically undialable. Captured
                // rather than only logged because the advertisement below must
                // not tell the planner such a host is reachable.
                let mut dead_net_entries: Vec<String> = Vec::new();
                if let kastellan_sandbox::Net::Allowlist(net_entries) = &entry.policy.net {
                    use crate::workers::endpoint_guard::{screen_net_allowlist, NetScreen};
                    match screen_net_allowlist(name, net_entries, force_routed) {
                        NetScreen::Refuse { detail } => {
                            tracing::error!(tool = name, %detail, "worker misconfigured; skipping");
                            continue;
                        }
                        NetScreen::Warn { dead } => {
                            tracing::warn!(
                                tool = name,
                                dead = ?dead,
                                "Net::Allowlist entries are statically dead — either a \
                                 `localhost` name under force-routing, or a host \
                                 carrying an embedded port or path separator (dead in \
                                 any routing mode) — requests to them will fail (use \
                                 literal IPs or routable hostnames, and update the \
                                 matching tool_allowlists rows / endpoint env vars to \
                                 agree)"
                            );
                            dead_net_entries = dead;
                        }
                        NetScreen::Ok => {}
                    }
                }
                // ONE key for the fetch, the boot log, the audit record and the
                // advertisement. `AllowlistDecl::tool` is the key
                // `build_tool_registry` prefetches under, so for a worker where it
                // differs from `name()` keying the audit record on `name()` would
                // report `allowlist_len: 0` and the SHA-256 of the empty list while
                // the prompt advertised the real set — two disagreeing accounts of
                // one allowlist, with no diagnostic. Every current manifest declares
                // the same constant for both, so this is drift-proofing, not a live
                // fix.
                let declaration = m.allowlist();
                let al_key = declaration.map(|d| d.tool).unwrap_or(name);
                let rows = (ctx.allowlist)(al_key);
                // The ENFORCEMENT view: every stored row, kind-blind, exactly as
                // the worker itself receives it. The audit record describes this
                // set, not the advertised one, so `allowlist_len` keeps meaning
                // "what is enforced" even when #541's kind filter withholds a row
                // from the prompt.
                let allowlist = kastellan_db::tool_allowlists::allowlist_values(&rows);
                tracing::info!(
                    tool = name,
                    binary = %entry.binary.display(),
                    allowlist_len = allowlist.len(),
                    "registering tool"
                );
                loaded.push(LoadedToolRecord {
                    name: name.to_string(),
                    binary: entry.binary.display().to_string(),
                    allowlist_len: allowlist.len(),
                    allowlist_sha256: sha256_argv0_list(&allowlist),
                });
                reg.insert(name, entry);
                // Advertise the operator allowlist when the worker declares one.
                // Since #545 a declaration carries both halves or neither, so
                // there is no half-declared state to warn about here: a worker
                // either has a kind to word its line with or has no allowlist.
                //
                // A `Described` advertisement carries the LIVE rows only: entries
                // the screen above proved dead, and entries stored under a
                // different `kind` than this worker declares, are both withheld —
                // so the planner is never told a host is reachable that the daemon
                // has already computed is not, nor handed a value described in the
                // wrong shape. When the kind filter empties a NON-empty row set
                // there is nothing left to describe and the decision is `Opaque`
                // instead; see the guard below for why that is not the same as an
                // empty allowlist. Decided once per manifest, then rendered per
                // doc by `with_allowlist` (which is where the escaping lives —
                // this site never touches the raw DB text).
                let declared = declaration.map(|decl| {
                    let of_declared_kind = withhold_kind_mismatches(name, decl, &rows);
                    // Every row is of another kind. Falling through with an empty
                    // list would reach the renderer's zero-rows wording — "every
                    // call to this tool will be refused" / "this tool can
                    // currently reach nothing" — which is FALSE here: the rows
                    // are kind-blindly enforced, so those calls succeed. The
                    // planner would abandon a working tool, re-inventing #533's
                    // failure from the opposite side. `partition_dead_rows` guards
                    // the same inversion for its own filter (`live.is_empty() &&
                    // !rows.is_empty()`) and cannot guard this one, because by
                    // the time it runs the mismatched rows are already gone and
                    // an all-mismatch is indistinguishable from an empty table.
                    if of_declared_kind.is_empty() && !rows.is_empty() {
                        return Advertisement::Opaque(rows.len());
                    }
                    let (live, withheld) = crate::workers::endpoint_guard::partition_dead_rows(
                        &of_declared_kind,
                        &dead_net_entries,
                    );
                    if !withheld.is_empty() {
                        tracing::warn!(
                            tool = name,
                            withheld = ?withheld,
                            "tool_allowlists rows are statically dead and will NOT be \
                             advertised to the planner (they are still enforced, so a \
                             request naming one is refused rather than silently allowed) \
                             — fix or remove the rows"
                        );
                    }
                    advertisement_warnings(name, &live);
                    Advertisement::Described(decl.kind, live)
                });
                // Record what this tool advertises, so the dispatcher can
                // complete a method whose namespace the planner omitted. Taken
                // from the same `tool_docs()` the prompt is built from, so the
                // set the planner is shown and the set it may name are the same
                // set by construction.
                reg.set_methods(name, m.tool_docs().iter().map(|d| d.method).collect());
                for doc in m.tool_docs() {
                    docs.push(match &declared {
                        Some(Advertisement::Described(kind, entries)) => {
                            AdvertisedTool::with_allowlist(doc, *kind, entries)
                        }
                        Some(Advertisement::Opaque(configured)) => {
                            AdvertisedTool::with_opaque_allowlist(doc, *configured)
                        }
                        None => AdvertisedTool::without_allowlist(doc),
                    });
                }
            }
            Resolution::Disabled { detail } => {
                tracing::info!(tool = m.name(), %detail, "worker disabled; skipping");
            }
            Resolution::Misconfigured { detail } => {
                tracing::error!(tool = m.name(), %detail, "worker misconfigured; skipping");
            }
        }
    }
    (reg, loaded, docs)
}

#[cfg(test)]
mod tests;
