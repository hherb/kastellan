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

use crate::scheduler::tool_dispatch::HANDOFF_TOOL;
use crate::scheduler::ToolRegistry;
use crate::worker_manifest::{ResolveCtx, Resolution, ToolDoc, WorkerManifest};

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
];

/// The kind of `tool_allowlists` entry a tool uses, discovered by scanning the
/// static manifest list. `None` for a tool that declares no allowlist or an
/// unrecognized name — the CLI treats `None` as the argv0 default, preserving
/// today's behaviour for any tool name that is not a known allowlist consumer.
/// Pure.
pub fn allowlist_kind_for_tool(
    name: &str,
) -> Option<kastellan_db::tool_allowlists::EntryKind> {
    WORKER_MANIFESTS
        .iter()
        .find(|m| m.allowlist_tool() == Some(name))
        .and_then(|m| m.allowlist_kind())
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
) -> Result<(ToolRegistry, Vec<LoadedToolRecord>, Vec<ToolDoc>), kastellan_db::DbError> {
    use std::collections::HashMap;
    use std::path::Path;

    // 1. Pre-fetch allowlists for every manifest that declares one.
    let mut allowlists: HashMap<String, Vec<String>> = HashMap::new();
    for m in WORKER_MANIFESTS {
        if let Some(tool) = m.allowlist_tool() {
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
             use 'kastellan-cli tools allowlist add <tool> <argv0>' to populate the DB"
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
) -> (ToolRegistry, Vec<LoadedToolRecord>, Vec<ToolDoc>) {
    let mut reg = ToolRegistry::new();
    let mut loaded: Vec<LoadedToolRecord> = Vec::new();
    // Planner-facing tool descriptions, collected ONLY for tools that register
    // (the `Register` arm below) — a disabled/misconfigured worker is never
    // advertised, so the planner is never told of a tool it can't dispatch.
    let mut docs: Vec<ToolDoc> = Vec::new();
    for m in manifests {
        if m.name() == HANDOFF_TOOL {
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
                                "Net::Allowlist entries use `localhost` names that are \
                                 statically dead under force-routing — requests to them \
                                 will fail (use literal IPs or routable hostnames, and \
                                 update the matching tool_allowlists rows / endpoint \
                                 env vars to agree)"
                            );
                        }
                        NetScreen::Ok => {}
                    }
                }
                let allowlist = (ctx.allowlist)(name);
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
                for doc in m.tool_docs() {
                    docs.push(doc);
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
mod tests {
    use super::*;
    use crate::worker_manifest::{ResolveCtx, Resolution, WorkerManifest};
    use std::path::{Path, PathBuf};

    /// A fake worker for assembly tests. `outcome` selects which arm
    /// `resolve` returns; `allowlist_name` (if Some) is reported from
    /// `allowlist_tool()` so the prefetch-keying path is exercised.
    struct FakeManifest {
        name: &'static str,
        outcome: FakeOutcome,
        allowlist_name: Option<&'static str>,
    }
    enum FakeOutcome {
        Register,
        /// Register, but with `policy.net = Net::Allowlist(these entries)` —
        /// exercises the #459 generic screen.
        RegisterWithNet(Vec<String>),
        /// Linux-gated: like `RegisterWithNet` but the entry is a Firecracker
        /// micro-VM worker (`sandbox_backend = FirecrackerVm`) — pins the
        /// VM-is-always-force-routed screen composition.
        #[cfg(target_os = "linux")]
        RegisterVmWithNet(Vec<String>),
        /// Register with `entry.broker = Some(BrokerSpec::search(..))` and an
        /// EMPTY Net::Allowlist (the broker/zero-egress posture) — exercises the
        /// #459 resolve-time broker-presence refuse.
        RegisterBrokerSearch,
        Disabled,
        Misconfigured,
    }
    impl WorkerManifest for FakeManifest {
        fn name(&self) -> &'static str {
            self.name
        }
        fn allowlist_tool(&self) -> Option<&'static str> {
            self.allowlist_name
        }
        fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
            match &self.outcome {
                FakeOutcome::Register => Resolution::Register(
                    crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    ),
                ),
                FakeOutcome::RegisterWithNet(entries) => {
                    let mut entry = crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    );
                    entry.policy.net = kastellan_sandbox::Net::Allowlist(entries.clone());
                    Resolution::Register(entry)
                }
                #[cfg(target_os = "linux")]
                FakeOutcome::RegisterVmWithNet(entries) => {
                    let mut entry = crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    );
                    entry.policy.net = kastellan_sandbox::Net::Allowlist(entries.clone());
                    entry.sandbox_backend =
                        Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm);
                    Resolution::Register(entry)
                }
                FakeOutcome::RegisterBrokerSearch => {
                    let mut entry = crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    );
                    entry.policy.net = kastellan_sandbox::Net::Allowlist(Vec::new());
                    entry.broker = Some(crate::broker::BrokerSpec::search(
                        "https://searx.example.org/search",
                    ));
                    Resolution::Register(entry)
                }
                FakeOutcome::Disabled => Resolution::Disabled { detail: "off".into() },
                FakeOutcome::Misconfigured => {
                    Resolution::Misconfigured { detail: "broken".into() }
                }
            }
        }
    }

    fn test_ctx<'a>(allowlist: &'a dyn Fn(&str) -> Vec<String>) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env: &|_k| None,
            exists: &|_p: &Path| false,
            is_dir: &|_p: &Path| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    /// Build a ResolveCtx whose env has KASTELLAN_EGRESS_FORCE_ROUTING=1
    /// (the test_ctx helper pins get_env to None, so these build their own).
    fn forced_ctx<'a>(allowlist: &'a dyn Fn(&str) -> Vec<String>) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env: &|k| (k == "KASTELLAN_EGRESS_FORCE_ROUTING").then(|| "1".to_string()),
            exists: &|_p: &Path| false,
            is_dir: &|_p: &Path| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn force_routed_all_localhost_allowlist_is_refused_like_misconfigured() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = forced_ctx(&allow);
        let m = FakeManifest {
            name: "deadtool",
            outcome: FakeOutcome::RegisterWithNet(vec![
                "localhost:443".to_string(),
                "svc.localhost:8080".to_string(),
            ]),
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("deadtool").is_none(), "statically dead tool must not register");
        assert!(loaded.is_empty(), "no LoadedToolRecord for a refused tool");
    }

    #[test]
    fn force_routed_subset_localhost_allowlist_warns_but_registers() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = forced_ctx(&allow);
        let m = FakeManifest {
            name: "mixedtool",
            outcome: FakeOutcome::RegisterWithNet(vec![
                "docs.example.org:443".to_string(),
                "localhost:443".to_string(),
            ]),
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("mixedtool").is_some(), "subset-dead tool still registers");
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn unforced_localhost_allowlist_registers_exactly_as_today() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow); // get_env is None ⇒ not force-routed
        let m = FakeManifest {
            name: "hosttool",
            outcome: FakeOutcome::RegisterWithNet(vec!["localhost:443".to_string()]),
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("hosttool").is_some(), "no force-routing ⇒ untouched");
        assert_eq!(loaded.len(), 1);
    }

    /// Linux-gated: a Firecracker-VM entry is ALWAYS force-routed
    /// (`plan.rs` refuses a `Net::Allowlist` VM without the egress proxy),
    /// so the screen fires even with `KASTELLAN_EGRESS_FORCE_ROUTING` unset.
    #[cfg(target_os = "linux")]
    #[test]
    fn vm_entry_all_localhost_allowlist_is_refused_even_unforced() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow); // get_env is None ⇒ host flag off
        let m = FakeManifest {
            name: "vmdead",
            outcome: FakeOutcome::RegisterVmWithNet(vec!["localhost:443".to_string()]),
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("vmdead").is_none(), "VM ⇒ always forced ⇒ all-dead refused");
        assert!(loaded.is_empty());
    }

    #[test]
    fn force_routed_non_allowlist_net_is_not_screened() {
        // shell_exec_entry's policy is Net::Deny — the screen only inspects
        // Net::Allowlist, so this registers exactly as before.
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = forced_ctx(&allow);
        let m = FakeManifest {
            name: "denytool",
            outcome: FakeOutcome::Register,
            allowlist_name: None,
        };
        let (reg, _loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("denytool").is_some());
    }

    #[test]
    fn broker_worker_registers_when_broker_binary_present() {
        // exists=true + exe_dir set ⇒ the search-broker sibling resolves ⇒
        // broker_bin_present is true ⇒ the broker worker registers.
        let allow = |_t: &str| Vec::<String>::new();
        let exe_dir = PathBuf::from("/install/bin");
        let ctx = ResolveCtx {
            get_env: &|_k| None,
            exists: &|_p: &Path| true,
            is_dir: &|_p: &Path| false,
            exe_dir: Some(exe_dir.as_path()),
            canonicalize: &|_p| None,
            allowlist: &allow,
        };
        let m = FakeManifest {
            name: "brokertool",
            outcome: FakeOutcome::RegisterBrokerSearch,
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("brokertool").is_some(), "broker present ⇒ registers");
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn broker_worker_refused_when_broker_binary_absent() {
        // test_ctx: exists=|_|false ⇒ no broker binary discoverable ⇒ refuse.
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow);
        let m = FakeManifest {
            name: "brokerdead",
            outcome: FakeOutcome::RegisterBrokerSearch,
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("brokerdead").is_none(), "absent broker binary ⇒ refused");
        assert!(loaded.is_empty(), "no LoadedToolRecord for a refused broker worker");
    }

    #[test]
    fn broker_worker_refused_even_when_not_force_routed() {
        // test_ctx has get_env=None ⇒ NOT force-routed. The broker refuse is
        // unconditional (independent of force-routing), so it still fires.
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow);
        let m = FakeManifest {
            name: "brokerdead2",
            outcome: FakeOutcome::RegisterBrokerSearch,
            allowlist_name: None,
        };
        let (reg, _loaded, _docs) = assemble_registry(&[&m], &ctx);
        assert!(reg.lookup("brokerdead2").is_none(), "unconditional broker refuse");
    }

    #[test]
    fn assemble_inserts_registered_and_records_allowlist_hash() {
        let allowlist = |t: &str| {
            if t == "alpha" {
                vec!["ls".to_string()]
            } else {
                Vec::new()
            }
        };
        let ctx = test_ctx(&allowlist);
        let m_alpha = FakeManifest {
            name: "alpha",
            outcome: FakeOutcome::Register,
            allowlist_name: Some("alpha"),
        };
        let manifests: &[&dyn WorkerManifest] = &[&m_alpha];

        let (reg, loaded, _docs) = assemble_registry(manifests, &ctx);

        assert!(reg.lookup("alpha").is_some(), "alpha should be registered");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "alpha");
        assert_eq!(loaded[0].allowlist_len, 1);
        assert_eq!(loaded[0].allowlist_sha256, sha256_argv0_list(&["ls".to_string()]));
        assert_eq!(loaded[0].binary, "/fake/alpha");
    }

    #[test]
    fn assemble_skips_disabled_and_misconfigured_without_recording() {
        let allowlist = |_t: &str| Vec::new();
        let ctx = test_ctx(&allowlist);
        let m_off = FakeManifest {
            name: "off",
            outcome: FakeOutcome::Disabled,
            allowlist_name: None,
        };
        let m_bad = FakeManifest {
            name: "bad",
            outcome: FakeOutcome::Misconfigured,
            allowlist_name: None,
        };
        let manifests: &[&dyn WorkerManifest] = &[&m_off, &m_bad];

        let (reg, loaded, _docs) = assemble_registry(manifests, &ctx);

        assert!(reg.lookup("off").is_none());
        assert!(reg.lookup("bad").is_none());
        assert!(loaded.is_empty(), "skipped workers produce no records");
    }

    #[test]
    fn sha256_argv0_list_is_order_independent_and_empty_is_empty_string_sha() {
        let a = sha256_argv0_list(&["ls".into(), "cat".into()]);
        let b = sha256_argv0_list(&["cat".into(), "ls".into()]);
        assert_eq!(a, b, "canonical form sorts before hashing");
        // SHA-256 of "" (no entries → no bytes fed).
        assert_eq!(
            sha256_argv0_list(&[]),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn build_registry_loaded_payload_wraps_tools_array() {
        let recs = vec![LoadedToolRecord {
            name: "shell-exec".into(),
            binary: "/x".into(),
            allowlist_len: 1,
            allowlist_sha256: "deadbeef".into(),
        }];
        let v = build_registry_loaded_payload(&recs);
        assert_eq!(v["tools"][0]["name"], "shell-exec");
        assert_eq!(v["tools"][0]["allowlist_len"], 1);
    }

    #[test]
    fn manifest_claiming_reserved_handoff_name_is_skipped() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow);
        let reserved = FakeManifest {
            name: "handoff",
            outcome: FakeOutcome::Register,
            allowlist_name: None,
        };
        let (reg, loaded, _docs) = assemble_registry(&[&reserved], &ctx);
        assert!(reg.lookup("handoff").is_none(), "reserved name must not register");
        assert!(loaded.is_empty(), "reserved name must not appear in loaded records");
    }

    #[test]
    fn shell_exec_registers_with_no_override_env_via_exe_sibling() {
        let exe_dir = PathBuf::from("/install/bin");
        let sibling = exe_dir.join("kastellan-worker-shell-exec");
        // No KASTELLAN_SHELL_EXEC_BIN; only the sibling exists.
        let get_env = |_k: &str| None;
        let exists = {
            let sibling = sibling.clone();
            move |p: &Path| p == sibling.as_path()
        };
        let allowlist = |_t: &str| Vec::new();
        let ctx = ResolveCtx {
            get_env: &get_env,
            exists: &exists,
            is_dir: &|_p: &Path| false,
            exe_dir: Some(exe_dir.as_path()),
            canonicalize: &|_p| None,
            allowlist: &allowlist,
        };

        // Real manifest list. gliner is Disabled (no enable flag) and skipped.
        let (reg, loaded, _docs) = assemble_registry(WORKER_MANIFESTS, &ctx);

        let entry = reg
            .lookup("shell-exec")
            .expect("shell-exec must register from the exe-relative sibling with no env override");
        assert_eq!(entry.binary, sibling);
        assert!(reg.lookup("gliner-relex").is_none(), "gliner disabled → not registered");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "shell-exec");
    }

    #[test]
    fn every_registered_worker_docs_name_matches_registry_key() {
        // A ToolDoc's name must equal its manifest's name(), else the planner is
        // told a tool name it can't dispatch. Guards against copy-paste drift.
        for m in WORKER_MANIFESTS {
            for doc in m.tool_docs() {
                assert_eq!(doc.name, m.name(), "tool_doc name drift for {}", m.name());
                assert!(!doc.method.is_empty(), "{} has empty method", m.name());
                assert!(!doc.summary.is_empty(), "{} has empty summary", m.name());
            }
        }
    }

    #[test]
    fn core_web_and_shell_workers_advertise_a_tool_doc() {
        let by_name = |want: &str| {
            WORKER_MANIFESTS
                .iter()
                .find(|m| m.name() == want)
                .and_then(|m| m.tool_doc())
        };
        assert_eq!(by_name("web-search").expect("web-search doc").method, "web.search");
        assert_eq!(by_name("web-research").expect("web-research doc").method, "web.research");
        assert_eq!(by_name("web-fetch").expect("web-fetch doc").method, "web.fetch");
        assert_eq!(by_name("shell-exec").expect("shell-exec doc").method, "shell.exec");
        assert_eq!(by_name("python-exec").expect("python-exec doc").method, "python.exec");
        assert_eq!(by_name("browser-driver").expect("browser-driver doc").method, "browser.render");
        assert_eq!(by_name("gliner-relex").expect("gliner-relex doc").method, "extract");
    }

    #[test]
    fn web_search_advertises_the_batch_method() {
        let m = WORKER_MANIFESTS
            .iter()
            .find(|m| m.name() == "web-search")
            .expect("web-search manifest");
        let docs = m.tool_docs();
        assert!(docs.iter().any(|d| d.method == "web.search"), "web.search missing");
        let batch = docs
            .iter()
            .find(|d| d.method == "web.search_batch")
            .expect("web.search_batch advertised");
        assert_eq!(batch.name, "web-search");
        assert!(batch.params.iter().any(|p| p.name == "queries" && p.required));
    }

    #[test]
    fn assemble_collects_docs_only_for_registered_tools() {
        // Register a real worker (shell-exec has a ToolDoc) via the exe-sibling
        // path, alongside the other workers which are Disabled in this ctx. Only
        // the registered one's doc is collected.
        let exe_dir = PathBuf::from("/install/bin");
        let sibling = exe_dir.join("kastellan-worker-shell-exec");
        let get_env = |_k: &str| None;
        let exists = {
            let s = sibling.clone();
            move |p: &Path| p == s.as_path()
        };
        let allowlist = |_t: &str| Vec::new();
        let ctx = ResolveCtx {
            get_env: &get_env,
            exists: &exists,
            is_dir: &|_p: &Path| false,
            exe_dir: Some(exe_dir.as_path()),
            canonicalize: &|_p| None,
            allowlist: &allowlist,
        };
        let (_reg, _loaded, docs) = assemble_registry(WORKER_MANIFESTS, &ctx);
        assert!(docs.iter().any(|d| d.name == "shell-exec"), "shell-exec doc collected");
        assert!(
            !docs.iter().any(|d| d.name == "web-search"),
            "disabled web-search must not be advertised"
        );
    }

    #[test]
    fn allowlist_kind_for_tool_maps_argv0_and_domain_tools() {
        use kastellan_db::tool_allowlists::EntryKind;
        assert_eq!(allowlist_kind_for_tool("shell-exec"), Some(EntryKind::Argv0));
        assert_eq!(allowlist_kind_for_tool("web-fetch"), Some(EntryKind::Domain));
        assert_eq!(allowlist_kind_for_tool("web-research"), Some(EntryKind::Domain));
        assert_eq!(allowlist_kind_for_tool("browser-driver"), Some(EntryKind::Domain));
        // A worker with no allowlist, and an unknown name, both map to None.
        assert_eq!(allowlist_kind_for_tool("python-exec"), None);
        assert_eq!(allowlist_kind_for_tool("nonexistent-tool"), None);
    }
}
