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
use crate::worker_manifest::{ResolveCtx, Resolution, WorkerManifest};

/// Every worker the daemon may register. Adding a worker = add its
/// `WorkerManifest` impl + one line here. Order is irrelevant (the registry
/// is a keyed map).
pub static WORKER_MANIFESTS: &[&dyn WorkerManifest] = &[
    &crate::workers::shell_exec::ShellExecManifest,
    &crate::workers::gliner_relex::GlinerRelexManifest,
    &crate::workers::web_fetch::WebFetchManifest,
];

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
/// `exe_dir` (the directory of the running `hhagent` binary, from
/// `current_exe()`) seeds the exe-relative sibling discovery default; pass
/// `None` to disable that fallback (override-env-only).
///
/// **Writes no audit row** — returns the per-tool records so the daemon can
/// write `registry.loaded` itself.
pub async fn build_tool_registry(
    pool: &sqlx::PgPool,
    exe_dir: Option<std::path::PathBuf>,
) -> Result<(ToolRegistry, Vec<LoadedToolRecord>), hhagent_db::DbError> {
    use std::collections::HashMap;
    use std::path::Path;

    // 1. Pre-fetch allowlists for every manifest that declares one.
    let mut allowlists: HashMap<String, Vec<String>> = HashMap::new();
    for m in WORKER_MANIFESTS {
        if let Some(tool) = m.allowlist_tool() {
            let al = hhagent_db::tool_allowlists::list_for_tool(pool, tool)
                .await
                .map_err(|e| {
                    hhagent_db::DbError::Query(format!("loading {tool} allowlist: {e}"))
                })?;
            allowlists.insert(tool.to_string(), al);
        }
    }

    // Preserve the deprecation breadcrumb for the retired env-var allowlist.
    if std::env::var_os("HHAGENT_SHELL_EXEC_ALLOWLIST").is_some() {
        tracing::warn!(
            "HHAGENT_SHELL_EXEC_ALLOWLIST is no longer honored; \
             use 'hhagent-cli tools allowlist add <tool> <argv0>' to populate the DB"
        );
    }

    // 2. Build the real ResolveCtx over std::env + the live filesystem.
    let get_env = |k: &str| std::env::var(k).ok();
    let exists = |p: &Path| p.exists();
    let is_dir = |p: &Path| p.is_dir();
    let allowlist = |tool: &str| allowlists.get(tool).cloned().unwrap_or_default();
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &is_dir,
        exe_dir: exe_dir.as_deref(),
        allowlist: &allowlist,
    };

    // 3. Pure assembly.
    Ok(assemble_registry(WORKER_MANIFESTS, &ctx))
}

/// Pure payload builder for the `registry.loaded` audit row. The daemon
/// calls this then `hhagent_db::audit::insert`; the CLI never does.
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
) -> (ToolRegistry, Vec<LoadedToolRecord>) {
    let mut reg = ToolRegistry::new();
    let mut loaded: Vec<LoadedToolRecord> = Vec::new();
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
            }
            Resolution::Disabled { detail } => {
                tracing::info!(tool = m.name(), %detail, "worker disabled; skipping");
            }
            Resolution::Misconfigured { detail } => {
                tracing::error!(tool = m.name(), %detail, "worker misconfigured; skipping");
            }
        }
    }
    (reg, loaded)
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
            match self.outcome {
                FakeOutcome::Register => Resolution::Register(
                    crate::workers::shell_exec::shell_exec_entry(
                        PathBuf::from(format!("/fake/{}", self.name)),
                        &(ctx.allowlist)(self.name),
                    ),
                ),
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
            allowlist,
        }
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

        let (reg, loaded) = assemble_registry(manifests, &ctx);

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

        let (reg, loaded) = assemble_registry(manifests, &ctx);

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
        let (reg, loaded) = assemble_registry(&[&reserved], &ctx);
        assert!(reg.lookup("handoff").is_none(), "reserved name must not register");
        assert!(loaded.is_empty(), "reserved name must not appear in loaded records");
    }

    #[test]
    fn shell_exec_registers_with_no_override_env_via_exe_sibling() {
        let exe_dir = PathBuf::from("/install/bin");
        let sibling = exe_dir.join("hhagent-worker-shell-exec");
        // No HHAGENT_SHELL_EXEC_BIN; only the sibling exists.
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
            allowlist: &allowlist,
        };

        // Real manifest list. gliner is Disabled (no enable flag) and skipped.
        let (reg, loaded) = assemble_registry(WORKER_MANIFESTS, &ctx);

        let entry = reg
            .lookup("shell-exec")
            .expect("shell-exec must register from the exe-relative sibling with no env override");
        assert_eq!(entry.binary, sibling);
        assert!(reg.lookup("gliner-relex").is_none(), "gliner disabled → not registered");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "shell-exec");
    }
}
