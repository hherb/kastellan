//! Uniform, declarative worker self-description.
//!
//! Each worker implements [`WorkerManifest`]; the daemon iterates a static
//! list of them at startup (see [`crate::registry_build`]) to build the
//! [`crate::scheduler::ToolRegistry`], replacing the hardcoded per-worker
//! branches that used to live in `registry_build.rs`.
//!
//! Design: `docs/superpowers/specs/2026-06-05-worker-manifest-plumbing-design.md`.

use std::path::{Path, PathBuf};

use crate::scheduler::ToolEntry;

/// A worker's planner-facing self-description (name + JSON-RPC method + params).
/// Rendered into the `<tools>` block by the prompt assembler so the planner
/// knows the tool exists and how to call it. All-`'static` so each worker
/// declares it as a `const`-style literal. Compiled-in ⇒ trusted (no escaping
/// at the render site).
pub struct ToolDoc {
    /// Tool name; MUST equal [`WorkerManifest::name`] (drift-guarded by a test).
    pub name: &'static str,
    /// JSON-RPC method the planner emits for this tool (e.g. `"web.search"`).
    pub method: &'static str,
    /// One line: what the tool does and when to reach for it.
    pub summary: &'static str,
    /// Ordered parameter descriptions.
    pub params: &'static [ToolParam],
}

/// One parameter of a [`ToolDoc`].
pub struct ToolParam {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// A worker's self-description. One impl per worker, living in that worker's
/// host-side module. `resolve` is **pure** — every input arrives via
/// [`ResolveCtx`], so each impl is unit-testable with fakes (no `std::env`,
/// no real filesystem access inside the impl).
pub trait WorkerManifest: Sync {
    /// Tool name the registry/planner keys on (e.g. `"shell-exec"`).
    fn name(&self) -> &'static str;

    /// If this worker needs the operational argv allowlist from the
    /// `tool_allowlists` DB table, the tool name to query (usually
    /// `== name()`). `None` ⇒ no allowlist. The async fetch stays in the
    /// builder; the result is threaded into [`ResolveCtx::allowlist`].
    fn allowlist_tool(&self) -> Option<&'static str> {
        None
    }

    /// Optional planner-facing description used to advertise this tool in the
    /// `<tools>` prompt block. `None` (the default) ⇒ dispatchable but not
    /// advertised. Only collected for workers that reach
    /// [`Resolution::Register`], so a disabled/misconfigured worker is never
    /// advertised. Static compiled-in text ⇒ trusted (no escaping at render).
    fn tool_doc(&self) -> Option<ToolDoc> {
        None
    }

    /// All planner-facing tool docs for this worker. Defaults to wrapping the
    /// single [`WorkerManifest::tool_doc`], so single-method workers need no
    /// change. A worker that serves several JSON-RPC methods (e.g. web-search:
    /// `web.search` + `web.search_batch`) overrides this to advertise each. Every
    /// returned doc's `name` must still equal [`WorkerManifest::name`]
    /// (drift-guarded).
    fn tool_docs(&self) -> Vec<ToolDoc> {
        self.tool_doc().into_iter().collect()
    }

    /// Pure resolution: host env + fs probes + pre-fetched allowlist → outcome.
    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution;
}

/// The three outcomes every worker produces, unified so the builder logs each
/// at one consistent severity.
///
/// `clippy::large_enum_variant` is allowed deliberately: a `Resolution` is
/// constructed and immediately matched inside the registry-build loop — it is
/// never stored in a collection — so boxing the (large) `ToolEntry` would only
/// add ceremony at every call site for no real stack-size benefit.
#[allow(clippy::large_enum_variant)]
pub enum Resolution {
    /// Resolved → insert this entry into the registry.
    Register(ToolEntry),
    /// Intentionally absent (e.g. feature flag off). Logged at INFO.
    Disabled { detail: String },
    /// Wanted to register but its environment is broken (missing binary,
    /// missing weights dir). Logged at ERROR; the daemon still starts
    /// (fail-soft — same posture as today).
    Misconfigured { detail: String },
}

/// Minimal, *universal* resolve inputs — deliberately not a per-worker kitchen
/// sink. Arbitrary worker-specific config arrives through `get_env` (the
/// universal extension point), so adding an exotic worker never widens this
/// struct.
pub struct ResolveCtx<'a> {
    /// Read an environment variable. Injected (not `std::env`) so resolvers
    /// are pure and unit-testable with a fake env.
    pub get_env: &'a dyn Fn(&str) -> Option<String>,
    /// Probe: does this path exist?
    pub exists: &'a dyn Fn(&Path) -> bool,
    /// Probe: is this path a directory?
    pub is_dir: &'a dyn Fn(&Path) -> bool,
    /// Directory of the running `kastellan` binary, for `current_exe()`-relative
    /// worker discovery. `None` when it can't be determined (fail-soft).
    pub exe_dir: Option<&'a Path>,
    /// Probe: resolve symlinks to the real path (`std::fs::canonicalize`
    /// in production). `None` when resolution fails (broken link, missing
    /// path) — callers fall back to the raw path. Exists because a policy
    /// built around a symlink can break *inside* the jail: e.g. an
    /// interpreter at `/usr/bin/python3 → /etc/alternatives/python3` is
    /// unreachable in-jail when `/etc/alternatives` isn't bound, even
    /// though `/usr` is.
    pub canonicalize: &'a dyn Fn(&Path) -> Option<PathBuf>,
    /// Operational argv allowlist, pre-fetched from the DB by the builder,
    /// keyed by tool name. A worker that declared `allowlist_tool()` looks
    /// itself up here; absent ⇒ empty.
    pub allowlist: &'a dyn Fn(&str) -> Vec<String>,
}

/// Locate a worker binary. Precedence:
///   1. **If the override env var (e.g. `"KASTELLAN_SHELL_EXEC_BIN"`) is set, it
///      is authoritative** — return it iff it names a runnable file, otherwise
///      return `None`. A set-but-invalid override **fails closed**: we never
///      silently substitute a *different* binary for the one the operator
///      explicitly named (that would subvert their intent and is a footgun in a
///      security-first daemon). This also matches the pre-manifest behaviour
///      (`KASTELLAN_SHELL_EXEC_BIN` set but not a file ⇒ not registered).
///   2. **Only when the override is unset**, fall back to the exe-relative
///      sibling default `<exe_dir>/<default_name>`, if it is a runnable file.
///
/// "Runnable file" means a path that exists and is *not* a directory. This
/// preserves the prior `binary.is_file()` gate for the realistic cases (regular
/// file accepted, directory rejected) without adding a third probe to
/// [`ResolveCtx`]. It is *not* a bit-for-bit reimplementation of
/// [`std::path::Path::is_file`]: a FIFO/socket/device special file (which
/// `is_file()` rejects) would pass `exists && !is_dir`. That divergence is
/// inert in practice — nobody points the override at a named pipe — and the
/// case that actually bit us (a directory) is handled identically.
///
/// Returns `None` when no runnable binary is found (the caller maps that to
/// [`Resolution::Misconfigured`]).
pub fn discover_binary(
    ctx: &ResolveCtx<'_>,
    override_env: &str,
    default_name: &str,
) -> Option<PathBuf> {
    // A path is "runnable" if it exists and is not a directory.
    let is_runnable_file = |p: &Path| (ctx.exists)(p) && !(ctx.is_dir)(p);

    // An explicit override is authoritative: honour it or reject it, but never
    // fall through to a different binary.
    if let Some(raw) = (ctx.get_env)(override_env) {
        let p = PathBuf::from(raw);
        return is_runnable_file(&p).then_some(p);
    }
    // No override set: try the exe-relative sibling default.
    if let Some(dir) = ctx.exe_dir {
        let p = dir.join(default_name);
        if is_runnable_file(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a ResolveCtx from simple closures for discovery tests. The
    /// allowlist closure is unused here (returns empty).
    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        exe_dir: Option<&'a Path>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir,
            canonicalize: &|_p| None,
            allowlist: &|_t| Vec::new(),
        }
    }

    #[test]
    fn override_env_pointing_at_existing_file_wins_over_sibling() {
        let get_env = |k: &str| (k == "OVERRIDE").then(|| "/opt/custom/worker".to_string());
        // Both the override path AND the sibling exist; override must win.
        let exists = |_p: &Path| true;
        let exe = PathBuf::from("/usr/bin");
        let c = ctx(&get_env, &exists, Some(&exe));
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            Some(PathBuf::from("/opt/custom/worker"))
        );
    }

    #[test]
    fn override_pointing_at_a_directory_fails_closed_without_substitution() {
        // Override is set and the path exists, but it's a directory — the old
        // `is_file()` posture rejected this; we must too. An explicit override
        // is authoritative: rather than silently substituting the sibling
        // (which would run a *different* binary than the operator named), we
        // fail closed → None → Misconfigured, EVEN THOUGH a valid sibling
        // exists.
        let get_env = |k: &str| (k == "OVERRIDE").then(|| "/some/dir".to_string());
        let exists = |_p: &Path| true; // everything "exists", incl. the sibling
        let is_dir = |p: &Path| p == Path::new("/some/dir"); // …but the override is a dir
        let exe = PathBuf::from("/usr/bin");
        let c = ResolveCtx {
            get_env: &get_env,
            exists: &exists,
            is_dir: &is_dir,
            exe_dir: Some(exe.as_path()),
            canonicalize: &|_p| None,
            allowlist: &|_t| Vec::new(),
        };
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            None,
            "a directory override must fail closed, NOT fall through to the sibling"
        );
    }

    #[test]
    fn override_set_but_missing_fails_closed_even_when_sibling_exists() {
        // Override names a path that does not exist. The sibling DOES exist and
        // is runnable — but an explicit override is authoritative, so we must
        // not silently run the sibling. Fail closed → None.
        let get_env = |k: &str| (k == "OVERRIDE").then(|| "/opt/typo/worker".to_string());
        let exe = PathBuf::from("/usr/bin");
        let sibling = exe.join("worker");
        // Only the sibling exists; the override path does not.
        let exists = {
            let sibling = sibling.clone();
            move |p: &Path| p == sibling.as_path()
        };
        let c = ctx(&get_env, &exists, Some(&exe));
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            None,
            "a set-but-missing override must fail closed, not substitute the sibling"
        );
    }

    #[test]
    fn no_override_falls_back_to_exe_relative_sibling() {
        let get_env = |_k: &str| None;
        let exe = PathBuf::from("/usr/bin");
        let sibling = exe.join("worker");
        let exists = move |p: &Path| p == sibling.as_path();
        let c = ctx(&get_env, &exists, Some(&exe));
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            Some(PathBuf::from("/usr/bin/worker"))
        );
    }

    #[test]
    fn neither_override_nor_sibling_exists_returns_none() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let exe = PathBuf::from("/usr/bin");
        let c = ctx(&get_env, &exists, Some(&exe));
        assert_eq!(discover_binary(&c, "OVERRIDE", "worker"), None);
    }

    #[test]
    fn missing_exe_dir_uses_override_only_and_does_not_panic() {
        let get_env = |k: &str| (k == "OVERRIDE").then(|| "/opt/worker".to_string());
        let exists = |_p: &Path| true;
        let c = ctx(&get_env, &exists, None);
        assert_eq!(
            discover_binary(&c, "OVERRIDE", "worker"),
            Some(PathBuf::from("/opt/worker"))
        );

        // And with no override + no exe_dir → None, still no panic.
        let get_env2 = |_k: &str| None;
        let c2 = ctx(&get_env2, &exists, None);
        assert_eq!(discover_binary(&c2, "OVERRIDE", "worker"), None);
    }
}

#[cfg(test)]
mod tool_doc_tests {
    use super::*;

    struct BareManifest;
    impl WorkerManifest for BareManifest {
        fn name(&self) -> &'static str {
            "bare"
        }
        fn resolve(&self, _ctx: &ResolveCtx<'_>) -> Resolution {
            Resolution::Disabled { detail: "n/a".into() }
        }
    }

    struct DocManifest;
    impl WorkerManifest for DocManifest {
        fn name(&self) -> &'static str {
            "documented"
        }
        fn resolve(&self, _ctx: &ResolveCtx<'_>) -> Resolution {
            Resolution::Disabled { detail: "n/a".into() }
        }
        fn tool_doc(&self) -> Option<ToolDoc> {
            Some(ToolDoc {
                name: "documented",
                method: "doc.run",
                summary: "does a thing",
                params: &[ToolParam { name: "q", description: "the query", required: true }],
            })
        }
    }

    #[test]
    fn default_tool_doc_is_none() {
        assert!(BareManifest.tool_doc().is_none());
    }

    #[test]
    fn overridden_tool_doc_carries_fields() {
        let d = DocManifest.tool_doc().expect("Some");
        assert_eq!(d.name, "documented");
        assert_eq!(d.method, "doc.run");
        assert_eq!(d.params.len(), 1);
        assert!(d.params[0].required);
    }

    #[test]
    fn default_tool_docs_wraps_single_doc() {
        assert!(BareManifest.tool_docs().is_empty());
        let docs = DocManifest.tool_docs();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].method, "doc.run");
    }
}
