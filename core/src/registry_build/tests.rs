//! Tests for the registry builder: manifest resolution outcomes, the #459
//! static-death screens, and what does or does not reach the planner-facing
//! advertisement.
//!
//! Lifted out of `registry_build.rs` verbatim (only the module wrapper and its
//! indentation went) so the movement diff stays reviewable on its own, and the
//! production half of the file drops back under the 500-line guideline.

use super::*;
use crate::worker_manifest::{
    argv0_rows, domain_rows, AllowlistDecl, ResolveCtx, Resolution, WorkerManifest,
};
use kastellan_db::tool_allowlists::EntryKind;
use std::path::{Path, PathBuf};

/// A fake worker for assembly tests. `outcome` selects which arm `resolve`
/// returns; `allowlist` (if Some) is what the manifest declares, so the
/// allowlist-lookup path is exercised (the *prefetch* itself lives in
/// `build_tool_registry`, which only ever iterates the real
/// `WORKER_MANIFESTS` and never sees a fake).
struct FakeManifest {
    name: &'static str,
    outcome: FakeOutcome,
    /// This fake's allowlist declaration, or `None`. ONE field, mirroring the
    /// trait since #545: a fake cannot express a half-declared manifest either,
    /// which is exactly the point of the collapse.
    allowlist: Option<AllowlistDecl>,
    /// When true, `tool_doc()` returns a synthetic doc, so this fake reaches
    /// the ADVERTISED surface (the returned `Vec<AdvertisedTool>`) and not
    /// only the registry. Without it a fake contributes no docs at all and
    /// any assertion over `docs` silently has nothing to look at.
    advertise_doc: bool,
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
    fn allowlist(&self) -> Option<AllowlistDecl> {
        self.allowlist
    }
    fn tool_doc(&self) -> Option<crate::worker_manifest::ToolDoc> {
        self.advertise_doc.then_some(crate::worker_manifest::ToolDoc {
            name: self.name,
            method: "fake.run",
            summary: "A fake tool.",
            params: &[],
        })
    }
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        match &self.outcome {
            FakeOutcome::Register => Resolution::Register(
                crate::workers::shell_exec::shell_exec_entry(
                    PathBuf::from(format!("/fake/{}", self.name)),
                    &kastellan_db::tool_allowlists::allowlist_values(
                        &(ctx.allowlist)(self.name),
                    ),
                ),
            ),
            FakeOutcome::RegisterWithNet(entries) => {
                let mut entry = crate::workers::shell_exec::shell_exec_entry(
                    PathBuf::from(format!("/fake/{}", self.name)),
                    &kastellan_db::tool_allowlists::allowlist_values(
                        &(ctx.allowlist)(self.name),
                    ),
                );
                entry.policy.net = kastellan_sandbox::Net::Allowlist(entries.clone());
                Resolution::Register(entry)
            }
            #[cfg(target_os = "linux")]
            FakeOutcome::RegisterVmWithNet(entries) => {
                let mut entry = crate::workers::shell_exec::shell_exec_entry(
                    PathBuf::from(format!("/fake/{}", self.name)),
                    &kastellan_db::tool_allowlists::allowlist_values(
                        &(ctx.allowlist)(self.name),
                    ),
                );
                entry.policy.net = kastellan_sandbox::Net::Allowlist(entries.clone());
                entry.sandbox_backend =
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm);
                Resolution::Register(entry)
            }
            FakeOutcome::RegisterBrokerSearch => {
                let mut entry = crate::workers::shell_exec::shell_exec_entry(
                    PathBuf::from(format!("/fake/{}", self.name)),
                    &kastellan_db::tool_allowlists::allowlist_values(
                        &(ctx.allowlist)(self.name),
                    ),
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

fn test_ctx<'a>(allowlist: &'a dyn Fn(&str) -> Vec<kastellan_db::tool_allowlists::AllowlistRow>) -> ResolveCtx<'a> {
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
fn forced_ctx<'a>(allowlist: &'a dyn Fn(&str) -> Vec<kastellan_db::tool_allowlists::AllowlistRow>) -> ResolveCtx<'a> {
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
    let allow = |_t: &str| Vec::new();
    let ctx = forced_ctx(&allow);
    let m = FakeManifest {
        name: "deadtool",
        outcome: FakeOutcome::RegisterWithNet(vec![
            "localhost:443".to_string(),
            "svc.localhost:8080".to_string(),
        ]),
        allowlist: None,
        advertise_doc: false,
    };
    let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
    assert!(reg.lookup("deadtool").is_none(), "statically dead tool must not register");
    assert!(loaded.is_empty(), "no LoadedToolRecord for a refused tool");
}

#[test]
fn force_routed_subset_localhost_allowlist_warns_but_registers() {
    let allow = |_t: &str| Vec::new();
    let ctx = forced_ctx(&allow);
    let m = FakeManifest {
        name: "mixedtool",
        outcome: FakeOutcome::RegisterWithNet(vec![
            "docs.example.org:443".to_string(),
            "localhost:443".to_string(),
        ]),
        allowlist: None,
        advertise_doc: false,
    };
    let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
    assert!(reg.lookup("mixedtool").is_some(), "subset-dead tool still registers");
    assert_eq!(loaded.len(), 1);
}

#[test]
fn unforced_localhost_allowlist_registers_exactly_as_today() {
    let allow = |_t: &str| Vec::new();
    let ctx = test_ctx(&allow); // get_env is None ⇒ not force-routed
    let m = FakeManifest {
        name: "hosttool",
        outcome: FakeOutcome::RegisterWithNet(vec!["localhost:443".to_string()]),
        allowlist: None,
        advertise_doc: false,
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
    let allow = |_t: &str| Vec::new();
    let ctx = test_ctx(&allow); // get_env is None ⇒ host flag off
    let m = FakeManifest {
        name: "vmdead",
        outcome: FakeOutcome::RegisterVmWithNet(vec!["localhost:443".to_string()]),
        allowlist: None,
        advertise_doc: false,
    };
    let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
    assert!(reg.lookup("vmdead").is_none(), "VM ⇒ always forced ⇒ all-dead refused");
    assert!(loaded.is_empty());
}

#[test]
fn force_routed_non_allowlist_net_is_not_screened() {
    // shell_exec_entry's policy is Net::Deny — the screen only inspects
    // Net::Allowlist, so this registers exactly as before.
    let allow = |_t: &str| Vec::new();
    let ctx = forced_ctx(&allow);
    let m = FakeManifest {
        name: "denytool",
        outcome: FakeOutcome::Register,
        allowlist: None,
        advertise_doc: false,
    };
    let (reg, _loaded, _docs) = assemble_registry(&[&m], &ctx);
    assert!(reg.lookup("denytool").is_some());
}

#[test]
fn broker_worker_registers_when_broker_binary_present() {
    // exists=true + exe_dir set ⇒ the search-broker sibling resolves ⇒
    // broker_bin_present is true ⇒ the broker worker registers.
    let allow = |_t: &str| Vec::new();
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
        allowlist: None,
        advertise_doc: false,
    };
    let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
    assert!(reg.lookup("brokertool").is_some(), "broker present ⇒ registers");
    assert_eq!(loaded.len(), 1);
}

#[test]
fn broker_worker_refused_when_broker_binary_absent() {
    // test_ctx: exists=|_|false ⇒ no broker binary discoverable ⇒ refuse.
    let allow = |_t: &str| Vec::new();
    let ctx = test_ctx(&allow);
    let m = FakeManifest {
        name: "brokerdead",
        outcome: FakeOutcome::RegisterBrokerSearch,
        allowlist: None,
        advertise_doc: false,
    };
    let (reg, loaded, _docs) = assemble_registry(&[&m], &ctx);
    assert!(reg.lookup("brokerdead").is_none(), "absent broker binary ⇒ refused");
    assert!(loaded.is_empty(), "no LoadedToolRecord for a refused broker worker");
}

#[test]
fn broker_worker_refused_even_when_not_force_routed() {
    // test_ctx has get_env=None ⇒ NOT force-routed. The broker refuse is
    // unconditional (independent of force-routing), so it still fires.
    let allow = |_t: &str| Vec::new();
    let ctx = test_ctx(&allow);
    let m = FakeManifest {
        name: "brokerdead2",
        outcome: FakeOutcome::RegisterBrokerSearch,
        allowlist: None,
        advertise_doc: false,
    };
    let (reg, _loaded, _docs) = assemble_registry(&[&m], &ctx);
    assert!(reg.lookup("brokerdead2").is_none(), "unconditional broker refuse");
}

#[test]
fn assemble_inserts_registered_and_records_allowlist_hash() {
    let allowlist = |t: &str| {
        if t == "alpha" {
            argv0_rows(&["ls"])
        } else {
            Vec::new()
        }
    };
    let ctx = test_ctx(&allowlist);
    let m_alpha = FakeManifest {
        name: "alpha",
        outcome: FakeOutcome::Register,
        allowlist: Some(AllowlistDecl { tool: "alpha", kind: EntryKind::Argv0 }),
        advertise_doc: false,
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
        allowlist: None,
        advertise_doc: false,
    };
    let m_bad = FakeManifest {
        name: "bad",
        outcome: FakeOutcome::Misconfigured,
        allowlist: None,
        advertise_doc: false,
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
fn manifest_claiming_reserved_sift_name_is_skipped() {
    let allow = |_t: &str| Vec::new();
    let ctx = test_ctx(&allow);
    let reserved = FakeManifest {
        name: "sift",
        outcome: FakeOutcome::Register,
        allowlist: None,
        advertise_doc: true,
    };
    let (reg, loaded, docs) = assemble_registry(&[&reserved], &ctx);
    assert!(reg.lookup("sift").is_none(), "reserved name must not register");
    assert!(loaded.is_empty(), "reserved name must not appear in loaded records");
    assert!(docs.is_empty(), "reserved name must not be advertised");
}

#[test]
fn manifest_claiming_reserved_handoff_name_is_skipped() {
    let allow = |_t: &str| Vec::new();
    let ctx = test_ctx(&allow);
    let reserved = FakeManifest {
        name: "handoff",
        outcome: FakeOutcome::Register,
        allowlist: None,
        advertise_doc: false,
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
    assert!(docs.iter().any(|d| d.doc.name == "shell-exec"), "shell-exec doc collected");
    assert!(
        !docs.iter().any(|d| d.doc.name == "web-search"),
        "disabled web-search must not be advertised"
    );
}

/// A registered tool's advertised methods reach the registry, because the
/// dispatcher completes an omitted namespace against exactly this set — and a
/// registry that recorded none would silently qualify nothing, restoring the
/// live `-32601 unknown method get_attachment_text` failure with every unit
/// test still green (`method_qualify`'s own tests pass a literal array).
///
/// Pinned against a *disabled* tool as well: recording methods for a worker
/// that never registered would let the dispatcher rewrite a method into a tool
/// the planner cannot dispatch.
#[test]
fn assemble_records_advertised_methods_for_registered_tools_only() {
    let exe_dir = PathBuf::from("/install/bin");
    let sibling = shell_exec_sibling(&exe_dir);
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
    let (reg, _loaded, _docs) = assemble_registry(WORKER_MANIFESTS, &ctx);

    let shell = reg.methods_for("shell-exec");
    assert!(
        shell.contains(&"shell.exec"),
        "shell-exec's advertised methods must reach the registry; got {shell:?}"
    );
    assert!(
        reg.methods_for("web-search").is_empty(),
        "a disabled tool must record no methods"
    );
    assert!(reg.methods_for("no-such-tool").is_empty(), "unknown tool → empty, not a panic");
}

#[test]
fn allowlist_kind_for_tool_maps_argv0_and_domain_tools() {
    assert_eq!(allowlist_kind_for_tool("shell-exec"), Some(EntryKind::Argv0));
    assert_eq!(allowlist_kind_for_tool("web-fetch"), Some(EntryKind::Domain));
    assert_eq!(allowlist_kind_for_tool("web-research"), Some(EntryKind::Domain));
    assert_eq!(allowlist_kind_for_tool("browser-driver"), Some(EntryKind::Domain));
    // A worker with no allowlist, and an unknown name, both map to None.
    assert_eq!(allowlist_kind_for_tool("python-exec"), None);
    assert_eq!(allowlist_kind_for_tool("nonexistent-tool"), None);
}

/// The exe-relative sibling path shell-exec resolves to when no override
/// env is set — the one path a ctx must report as existing for shell-exec
/// (and only shell-exec) to register. Mirrors the existing
/// `shell_exec_registers_with_no_override_env_via_exe_sibling` fixture.
fn shell_exec_sibling(exe_dir: &Path) -> PathBuf {
    exe_dir.join("kastellan-worker-shell-exec")
}

/// A declared allowlist reaches the advertised surface: sorted, and worded by
/// the declared `EntryKind`. This is the whole point of #533 — the planner was
/// never shown this set and guessed it one value per plan iteration.
#[test]
fn a_declared_allowlist_is_advertised_sorted_and_worded_by_kind() {
    let exe_dir = PathBuf::from("/install/bin");
    let sibling = shell_exec_sibling(&exe_dir);
    let get_env = |_k: &str| None;
    let exists = {
        let s = sibling.clone();
        move |p: &Path| p == s.as_path()
    };
    // Deliberately NOT in sorted order — the renderer must sort.
    let allowlist = |t: &str| {
        if t == "shell-exec" {
            argv0_rows(&["/usr/bin/ls", "/usr/bin/cat"])
        } else {
            Vec::new()
        }
    };
    let ctx = ResolveCtx {
        get_env: &get_env,
        exists: &exists,
        is_dir: &|_p: &Path| false,
        exe_dir: Some(exe_dir.as_path()),
        canonicalize: &|_p| None,
        allowlist: &allowlist,
    };

    let (_reg, _loaded, docs) = assemble_registry(WORKER_MANIFESTS, &ctx);
    let shell = docs.iter().find(|d| d.doc.name == "shell-exec").expect("shell-exec advertised");
    let allowed = shell.allowed().expect("shell-exec declares an allowlist");
    assert!(
        allowed.contains("`/usr/bin/cat`, `/usr/bin/ls`"),
        "sorted, individually quoted permitted set: {allowed}"
    );
    assert!(allowed.contains("argv[0]"), "argv0 wording: {allowed}");
}

/// Whether a permitted set is advertised follows the manifest's DECLARATION,
/// never the contents of the list — in **both** directions.
///
/// Built from fakes rather than `WORKER_MANIFESTS` deliberately. The earlier
/// version of this test used a ctx in which only `shell-exec` resolved, so its
/// loop ran exactly once, over a tool that *does* declare: the `false == false`
/// direction the test is named for never executed, and a mutation that
/// advertised an `allowed:` line for EVERY registered tool (telling the planner
/// `web.search` and `mail.*` refuse everything) survived the whole suite.
///
/// Three registering, advertised workers cover the matrix:
///   * `argvy`  — declares both halves, list EMPTY ⇒ `Some(refusal warning)`
///   * `domainy` — declares both halves, `Domain` kind, non-empty ⇒ `Some(hosts)`
///   * `silent` — advertised but declares no allowlist ⇒ `None`
#[test]
fn advertising_a_permitted_set_follows_the_declaration_not_the_contents() {
    let allowlist = |t: &str| match t {
        // `argvy` declares an allowlist that is EMPTY — the state in which
        // every dispatch fails, and precisely the live 2026-06-20/21 regime
        // that produced 15 of 15 failures with the planner told nothing.
        "domainy" => domain_rows(&["example.org"]),
        _ => Vec::new(),
    };
    let ctx = test_ctx(&allowlist);
    let argvy = FakeManifest {
        name: "argvy",
        outcome: FakeOutcome::Register,
        allowlist: Some(AllowlistDecl { tool: "argvy", kind: EntryKind::Argv0 }),
        advertise_doc: true,
    };
    let domainy = FakeManifest {
        name: "domainy",
        outcome: FakeOutcome::Register,
        allowlist: Some(AllowlistDecl { tool: "domainy", kind: EntryKind::Domain }),
        advertise_doc: true,
    };
    let silent = FakeManifest {
        name: "silent",
        outcome: FakeOutcome::Register,
        allowlist: None,
        advertise_doc: true,
    };
    let manifests: &[&dyn WorkerManifest] = &[&argvy, &domainy, &silent];

    let (_reg, _loaded, docs) = assemble_registry(manifests, &ctx);
    assert_eq!(docs.len(), 3, "all three fakes must be advertised: {}", docs.len());

    let find = |n: &str| docs.iter().find(|d| d.doc.name == n).expect("advertised");
    // Declared but empty ⇒ still advertised, with the refusal warning.
    let argv0_line = find("argvy").allowed().expect("declared ⇒ advertised even when empty");
    assert!(argv0_line.contains("refused"), "empty argv0 ⇒ refusal warning: {argv0_line}");
    // A Domain-kind worker reaches the advertised surface — the three real
    // domain workers (web-fetch/web-research/browser-driver) take this path.
    let domain_line = find("domainy").allowed().expect("declared ⇒ advertised");
    assert!(domain_line.contains("`example.org`"), "host advertised: {domain_line}");
    assert!(domain_line.contains("reachable"), "domain wording: {domain_line}");
    // The direction the old test never reached: no declaration ⇒ no line.
    assert!(
        find("silent").allowed().is_none(),
        "a worker declaring no allowlist must advertise no permitted set"
    );
}

/// A `tool_allowlists` row the #459 screen has already computed to be
/// statically dead must NOT be advertised as reachable: telling the planner a
/// host works when the daemon knows it does not is the INVERTED form of #533,
/// and costs a plan iteration per attempt. The live rows stay advertised.
#[test]
fn statically_dead_rows_are_withheld_from_the_advertisement() {
    // Force-routed (the supervised default), so a `localhost` NAME is dead —
    // the proxy range-denies what it resolves to.
    let allowlist = |_t: &str| domain_rows(&["example.org", "localhost"]);
    let ctx = forced_ctx(&allowlist);
    let m = FakeManifest {
        name: "domainy",
        outcome: FakeOutcome::RegisterWithNet(vec![
            "example.org:443".to_string(),
            "localhost:443".to_string(),
        ]),
        allowlist: Some(AllowlistDecl { tool: "domainy", kind: EntryKind::Domain }),
        advertise_doc: true,
    };
    let (_reg, loaded, docs) = assemble_registry(&[&m], &ctx);

    let line = docs[0].allowed().expect("declared ⇒ advertised");
    assert!(line.contains("`example.org`"), "live row still advertised: {line}");
    assert!(
        !line.contains("localhost"),
        "statically-dead row must not be advertised as reachable: {line}"
    );
    // Enforcement is untouched — the audit record still counts BOTH rows, so
    // a request naming `localhost` is refused rather than silently allowed.
    assert_eq!(loaded[0].allowlist_len, 2, "withholding is advertisement-only");
}

/// A row stored under a different `kind` than its tool declares must not be
/// advertised: the kind is what picks the WORDING, so an argv0 path under a
/// domain worker would be announced as "only these hosts are reachable:
/// `/usr/bin/ls`" — a permitted value the planner would then try to use as a
/// host. Nothing constrains the two to agree (migration `0021` backfilled every
/// pre-existing row as `argv0` regardless of tool, the CLI falls back to `Argv0`
/// for an unrecognised tool, and the runtime role holds direct INSERT), so the
/// disagreement has to be handled rather than assumed away (#541).
#[test]
fn a_row_of_another_kind_is_not_advertised_under_this_tools_wording() {
    let allowlist = |_t: &str| {
        let mut rows = domain_rows(&["example.org"]);
        // The mismatch: an argv0-kind row sitting under a Domain-kind tool.
        rows.extend(argv0_rows(&["/usr/bin/ls"]));
        rows
    };
    let ctx = test_ctx(&allowlist);
    let m = FakeManifest {
        name: "domainy",
        outcome: FakeOutcome::Register,
        allowlist: Some(AllowlistDecl { tool: "domainy", kind: EntryKind::Domain }),
        advertise_doc: true,
    };
    let (_reg, loaded, docs) = assemble_registry(&[&m], &ctx);

    let line = docs[0].allowed().expect("declared ⇒ advertised");
    assert!(line.contains("`example.org`"), "the matching row is advertised: {line}");
    assert!(
        !line.contains("/usr/bin/ls"),
        "a row of another kind must not be advertised in this tool's wording: {line}"
    );
    // Advertisement-only, exactly like the statically-dead case above:
    // narrowing what a deployed worker may do — on a host whose operator did
    // nothing wrong — is a bigger harm than a value the planner must ask about.
    assert_eq!(loaded[0].allowlist_len, 2, "enforcement keeps both rows");
}

/// Schema drift is a mismatch too, and must not be an outage. A `kind` this
/// build does not recognise (a third kind added by a later migration) reads as
/// "not the declared kind" — withheld and named — rather than failing the
/// registry build, which is what parsing the column into `EntryKind` here would
/// have done to a daemon holding one such row.
#[test]
fn a_row_with_an_unrecognised_kind_is_withheld_rather_than_fatal() {
    let allowlist = |_t: &str| {
        let mut rows = domain_rows(&["example.org"]);
        rows.push(kastellan_db::tool_allowlists::AllowlistRow {
            value: "future.example.org".to_string(),
            kind: "kind-from-a-later-migration".to_string(),
        });
        rows
    };
    let ctx = test_ctx(&allowlist);
    let m = FakeManifest {
        name: "domainy",
        outcome: FakeOutcome::Register,
        allowlist: Some(AllowlistDecl { tool: "domainy", kind: EntryKind::Domain }),
        advertise_doc: true,
    };
    let (reg, _loaded, docs) = assemble_registry(&[&m], &ctx);

    assert!(reg.lookup("domainy").is_some(), "an unknown kind must not refuse the tool");
    let line = docs[0].allowed().expect("declared ⇒ advertised");
    assert!(line.contains("`example.org`"), "the recognised row is advertised: {line}");
    assert!(!line.contains("future.example.org"), "the drifted row is withheld: {line}");
}

/// When the kind filter withholds EVERY row, the advertisement must not fall
/// through to the zero-rows wording.
///
/// The two states reach the renderer looking identical (an empty entry list)
/// and mean opposite things: zero stored rows genuinely permits nothing, while
/// an all-mismatch permits exactly what it always did, because enforcement is
/// kind-blind. Rendering the second as the first tells the planner "every call
/// to this tool will be refused" about a tool whose calls succeed — #533's
/// failure re-invented from the other side, and worse than #533 because the
/// planner abandons the tool instead of retrying.
///
/// Companion to the sibling above: that one pins the mixed case (a matching
/// row survives), this one pins the total case, which is the one the two
/// existing filters cannot see (`partition_dead_rows`' own all-withheld guard
/// runs after the mismatched rows are already gone).
#[test]
fn an_all_mismatched_kind_allowlist_is_advertised_as_configured_but_undescribable() {
    // Every row is argv0-kind under a Domain-kind worker: nothing to word.
    let allowlist = |_t: &str| argv0_rows(&["/usr/bin/ls", "/usr/bin/cat"]);
    let ctx = test_ctx(&allowlist);
    let m = FakeManifest {
        name: "domainy",
        outcome: FakeOutcome::Register,
        allowlist: Some(AllowlistDecl { tool: "domainy", kind: EntryKind::Domain }),
        advertise_doc: true,
    };
    let (_reg, loaded, docs) = assemble_registry(&[&m], &ctx);

    let line = docs[0].allowed().expect("declared ⇒ advertised");
    // The load-bearing half: the strong claim must NOT appear. Both kind
    // wordings are checked because the renderer picks one by kind and either
    // would be the same lie here.
    assert!(
        !line.contains("can currently reach nothing") && !line.contains("will be refused until"),
        "an all-mismatch must not read as 'nothing is permitted': {line}"
    );
    // Nor may it name a value: describing an argv0 path under domain wording is
    // exactly what #541 withholds it for.
    assert!(!line.contains("/usr/bin/"), "no value may be named: {line}");
    // What it must say instead: the restriction is real, and how big it is.
    assert!(line.contains("2 permitted values"), "the enforced count is stated: {line}");
    assert!(line.contains("still enforced"), "enforcement is stated: {line}");
    assert_eq!(loaded[0].allowlist_len, 2, "and enforcement really is untouched");
}

/// A genuinely EMPTY allowlist must keep the strong wording — the regression
/// guard for the fix above.
///
/// This is not a hypothetical branch: the three domain workers sit at zero rows
/// on the live DGX today, so this line is what the deployed planner reads. If
/// the all-mismatch fix had been written by softening the empty wording instead
/// of adding a state, this test is what would have caught it.
#[test]
fn a_genuinely_empty_allowlist_still_says_nothing_is_permitted() {
    let allowlist = |_t: &str| Vec::new();
    let ctx = test_ctx(&allowlist);
    let m = FakeManifest {
        name: "domainy",
        outcome: FakeOutcome::Register,
        allowlist: Some(AllowlistDecl { tool: "domainy", kind: EntryKind::Domain }),
        advertise_doc: true,
    };
    let (_reg, loaded, docs) = assemble_registry(&[&m], &ctx);

    let line = docs[0].allowed().expect("declared ⇒ advertised");
    assert!(
        line.contains("can currently reach nothing"),
        "zero rows keeps the strong (and true) claim: {line}"
    );
    assert!(!line.contains("still enforced"), "and is not the opaque wording: {line}");
    assert_eq!(loaded[0].allowlist_len, 0, "zero rows is zero rows");
}

/// The dead-row screen judges the rows of the DECLARED kind, not every stored
/// row — pinning a decision, because the answer changed with #541.
///
/// `partition_dead_rows` degrades to withholding nothing when its whole input
/// is dead, on the premise that a `NetScreen::Warn` is a proper subset and so
/// an all-dead match means the net entries and the rows do not correspond.
/// Feeding it the kind-filtered subset makes that premise *tighter*, not
/// looser: a domain worker's net entries are built from its domain rows, so an
/// argv0 row propping up the "some row is alive" count was never evidence
/// about reachability at all. The visible consequence is that the degradation
/// now fires here, where before #541 the unrelated argv0 row would have been
/// advertised as a host instead — both readings are lossy, and this is the one
/// that does not name a value in the wrong shape.
#[test]
fn the_dead_row_screen_judges_only_the_rows_of_the_declared_kind() {
    let allowlist = |_t: &str| {
        let mut rows = domain_rows(&["localhost"]);
        rows.extend(argv0_rows(&["/usr/bin/ls"]));
        rows
    };
    let ctx = forced_ctx(&allowlist);
    let m = FakeManifest {
        name: "domainy",
        outcome: FakeOutcome::RegisterWithNet(vec![
            "example.org:443".to_string(),
            "localhost:443".to_string(),
        ]),
        allowlist: Some(AllowlistDecl { tool: "domainy", kind: EntryKind::Domain }),
        advertise_doc: true,
    };
    let (_reg, loaded, docs) = assemble_registry(&[&m], &ctx);

    let line = docs[0].allowed().expect("declared ⇒ advertised");
    assert!(
        line.contains("`localhost`"),
        "all-dead within the declared kind degrades to withholding nothing: {line}"
    );
    assert!(
        !line.contains("/usr/bin/ls"),
        "the argv0 row is still never worded as a host: {line}"
    );
    assert_eq!(loaded[0].allowlist_len, 2, "enforcement stays kind-blind either way");
}

/// The CLI and the registry must agree about every declaring worker.
///
/// #545 made the half-declared manifest unrepresentable, which retired the
/// guard test that used to watch for it — the tool name and the kind are now
/// one value, so neither half can go missing. What that does NOT make
/// impossible is the lookup drifting: `allowlist_kind_for_tool` is what
/// `kastellan-cli tools allowlist add` validates against, and it finds a
/// manifest by scanning for a matching `tool` key. If that scan and the
/// declaration ever disagree, the operator is told their tool "is not a known
/// allowlist consumer" and their domain entry is validated as an argv0 path —
/// the exact footgun #545 set out to remove, one layer down.
///
/// Guards that lookup and NOTHING adjacent to it (the standing #516/#524/#525
/// lesson): it does not check the rendered wording.
#[test]
fn the_cli_resolves_the_declared_kind_for_every_allowlist_worker() {
    for m in WORKER_MANIFESTS {
        let Some(decl) = m.allowlist() else { continue };
        assert_eq!(
            allowlist_kind_for_tool(decl.tool),
            Some(decl.kind),
            "{}: `tools allowlist add {}` must validate against the declared kind",
            m.name(),
            decl.tool
        );
    }
}
