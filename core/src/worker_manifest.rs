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
    /// Directory of the running `hhagent` binary, for `current_exe()`-relative
    /// worker discovery. `None` when it can't be determined (fail-soft).
    pub exe_dir: Option<&'a Path>,
    /// Operational argv allowlist, pre-fetched from the DB by the builder,
    /// keyed by tool name. A worker that declared `allowlist_tool()` looks
    /// itself up here; absent ⇒ empty.
    pub allowlist: &'a dyn Fn(&str) -> Vec<String>,
}

/// Locate a worker binary. Precedence:
///   1. the explicit override env var (e.g. `"HHAGENT_SHELL_EXEC_BIN"`) if it
///      names an existing file — preserves every current deployment/test;
///   2. else the exe-relative sibling default `<exe_dir>/<default_name>`, if
///      it exists.
///
/// Returns `None` when neither yields an existing file (the caller maps that
/// to [`Resolution::Misconfigured`]).
pub fn discover_binary(
    ctx: &ResolveCtx<'_>,
    override_env: &str,
    default_name: &str,
) -> Option<PathBuf> {
    if let Some(raw) = (ctx.get_env)(override_env) {
        let p = PathBuf::from(raw);
        if (ctx.exists)(&p) {
            return Some(p);
        }
        // Override set but missing: fall through to the sibling default
        // rather than hard-failing (design §4 precedence).
    }
    if let Some(dir) = ctx.exe_dir {
        let p = dir.join(default_name);
        if (ctx.exists)(&p) {
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
