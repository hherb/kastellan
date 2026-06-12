//! Host-side manifest + `ToolEntry` constructor for the python-exec worker
//! (Phase 4 slice #1).
//!
//! The first executor for agent-authored Python: arbitrary source in,
//! `{exit_code, stdout, stderr}` out, under the strictest policy any worker
//! has — `Net::Deny`, `Profile::WorkerStrict` (the CPython child inherits the
//! seccomp filter across `execve`), no writable host path. Scratch is the
//! jail's own ephemeral `/tmp` tmpfs (#89), granted through the worker-side
//! Landlock layer by an explicit `KASTELLAN_LANDLOCK_RW=["/tmp"]` env entry
//! (`derive_lockdown_env` honours a caller-supplied value) — `fs_write` stays
//! empty so the *host* `/tmp` is never bound over the tmpfs.
//!
//! Registration is opt-in (`KASTELLAN_PYTHON_EXEC_ENABLE=1`): shell-exec is
//! deny-by-default through its empty argv allowlist, but python-exec has no
//! equivalent operational knob (arbitrary code is the point), so the
//! deny-by-default posture moves to registration itself.
//!
//! Design: `docs/superpowers/specs/2026-06-12-python-exec-worker-design.md`.

use std::path::{Path, PathBuf};

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::tool_host::ENV_LANDLOCK_RW;
use crate::worker_manifest::{discover_binary, Resolution, ResolveCtx, WorkerManifest};

/// Tool name the registry/planner keys python-exec on.
const TOOL_NAME: &str = "python-exec";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "KASTELLAN_PYTHON_EXEC_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "kastellan-worker-python-exec";
/// Opt-in gate; anything but `"1"` (trimmed) leaves the tool unregistered.
const ENABLE_ENV: &str = "KASTELLAN_PYTHON_EXEC_ENABLE";
/// Interpreter path: operator override on the daemon side, and the exact
/// var injected into the jail for the worker's fail-closed startup.
const PYTHON_ENV: &str = "KASTELLAN_PYTHON_EXEC_PYTHON";

/// Interpreter candidates probed (in order) when `KASTELLAN_PYTHON_EXEC_PYTHON`
/// is unset. Covers the stock layouts: distro/CLT (`/usr/bin`), source or
/// macOS-Intel-Homebrew installs (`/usr/local/bin`), Apple-Silicon Homebrew
/// (`/opt/homebrew/bin`).
const PYTHON_CANDIDATES: &[&str] = &[
    "/usr/bin/python3",
    "/usr/local/bin/python3",
    "/opt/homebrew/bin/python3",
];

/// Reason the resolver returned no entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveSkipReason {
    /// `KASTELLAN_PYTHON_EXEC_ENABLE` is unset/empty/anything but `"1"`.
    Disabled,
    /// `KASTELLAN_PYTHON_EXEC_PYTHON` is set but names no runnable file.
    /// Fails closed — never silently substitute a candidate for the
    /// interpreter the operator explicitly named.
    PythonOverrideInvalid { path: PathBuf },
    /// No override and no candidate interpreter found on this host.
    PythonNotFound,
}

/// Pure resolver: ENABLE gate + interpreter override/candidate cascade.
/// The worker *binary* keeps the standard [`discover_binary`] path in the
/// manifest itself.
pub fn resolve_env<E, X>(env_lookup: E, exists: X) -> Result<PathBuf, ResolveSkipReason>
where
    E: Fn(&str) -> Option<String>,
    X: Fn(&Path) -> bool,
{
    if env_lookup(ENABLE_ENV).unwrap_or_default().trim() != "1" {
        return Err(ResolveSkipReason::Disabled);
    }
    if let Some(raw) = env_lookup(PYTHON_ENV) {
        let p = PathBuf::from(raw);
        if exists(&p) {
            return Ok(p);
        }
        return Err(ResolveSkipReason::PythonOverrideInvalid { path: p });
    }
    for c in PYTHON_CANDIDATES {
        let p = PathBuf::from(c);
        if exists(&p) {
            return Ok(p);
        }
    }
    Err(ResolveSkipReason::PythonNotFound)
}

/// Build the [`ToolEntry`] for the python-exec worker.
///
/// Policy pins (the strictest of any registered worker):
/// `Net::Deny`, `Profile::WorkerStrict`, `fs_write = []` (scratch is the
/// jail's ephemeral `/tmp` tmpfs via the explicit Landlock-RW grant),
/// `cpu_ms = 10_000`, `mem_mb = 512`, `wall_clock_ms = Some(30_000)`,
/// `SingleUse`. `fs_read` carries the worker binary, the interpreter, and
/// the interpreter's `<prefix>/lib` (stdlib) — redundant under bwrap's
/// always-bound `/usr`, required for non-`/usr` prefixes under
/// Seatbelt/Landlock.
pub fn python_exec_entry(binary: PathBuf, python: PathBuf) -> ToolEntry {
    let mut fs_read = vec![binary.clone(), python.clone()];
    if let Some(lib) = prefix_lib(&python) {
        fs_read.push(lib);
    }
    let policy = SandboxPolicy {
        fs_read,
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerStrict,
        env: vec![
            (PYTHON_ENV.to_string(), python.to_string_lossy().into_owned()),
            // Grant the jail's /tmp through the worker-side Landlock layer.
            // MUST stay out of fs_write: a /tmp entry there would bind the
            // host /tmp over bwrap's per-spawn ephemeral tmpfs (#89).
            (ENV_LANDLOCK_RW.to_string(), r#"["/tmp"]"#.to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
}

/// `<prefix>/lib` for an interpreter at `<prefix>/bin/python3`, else `None`.
fn prefix_lib(python: &Path) -> Option<PathBuf> {
    let bin_dir = python.parent()?;
    if bin_dir.file_name()? != "bin" {
        return None;
    }
    Some(bin_dir.parent()?.join("lib"))
}

/// python-exec's manifest. No `allowlist_tool` (there is no argv-shaped
/// operational allowlist; the gate is `KASTELLAN_PYTHON_EXEC_ENABLE`).
pub struct PythonExecManifest;

impl WorkerManifest for PythonExecManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let is_runnable = |p: &Path| (ctx.exists)(p) && !(ctx.is_dir)(p);
        let python = match resolve_env(|k| (ctx.get_env)(k), is_runnable) {
            Ok(p) => p,
            Err(ResolveSkipReason::Disabled) => {
                return Resolution::Disabled {
                    detail: format!("{ENABLE_ENV} != 1 — python-exec not registered"),
                };
            }
            Err(ResolveSkipReason::PythonOverrideInvalid { path }) => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "{PYTHON_ENV} set to {path:?} but that is not a runnable file \
                         (fail-closed: candidates are not substituted for an explicit override)"
                    ),
                };
            }
            Err(ResolveSkipReason::PythonNotFound) => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "no python3 interpreter found: {PYTHON_ENV} unset and none of \
                         {PYTHON_CANDIDATES:?} exists"
                    ),
                };
            }
        };
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
        Resolution::Register(python_exec_entry(binary, python))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir: None,
            allowlist: &|_t| Vec::new(),
        }
    }

    #[test]
    fn resolve_disabled_without_enable_gate() {
        let get_env = |k: &str| (k == BIN_ENV).then(|| "/opt/python-exec".to_string());
        let exists = |_p: &Path| true;
        let c = ctx(&get_env, &exists);
        match PythonExecManifest.resolve(&c) {
            Resolution::Disabled { detail } => {
                assert!(detail.contains(ENABLE_ENV), "detail: {detail}");
            }
            other => panic!("expected Disabled, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_registers_with_strictest_policy() {
        let get_env = |k: &str| match k {
            "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
            "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
            _ => None,
        };
        // Only the override binary + the first interpreter candidate exist.
        let exists = |p: &Path| {
            p == Path::new("/opt/python-exec") || p == Path::new("/usr/bin/python3")
        };
        let c = ctx(&get_env, &exists);

        match PythonExecManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert_eq!(entry.binary, PathBuf::from("/opt/python-exec"));
                assert!(matches!(entry.policy.net, Net::Deny));
                assert_eq!(entry.policy.profile, Profile::WorkerStrict);
                assert_eq!(entry.policy.cpu_ms, 10_000);
                assert_eq!(entry.policy.mem_mb, 512);
                assert_eq!(entry.wall_clock_ms, Some(30_000));
                // No writable host path, ever.
                assert!(entry.policy.fs_write.is_empty());
                // fs_read: worker + interpreter + derived stdlib prefix.
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/opt/python-exec")));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/usr/bin/python3")));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/usr/lib")));
                // Env: interpreter for the worker's fail-closed startup +
                // the explicit Landlock /tmp grant (jail tmpfs scratch).
                assert!(entry
                    .policy
                    .env
                    .contains(&(PYTHON_ENV.to_string(), "/usr/bin/python3".to_string())));
                assert!(entry
                    .policy
                    .env
                    .contains(&(ENV_LANDLOCK_RW.to_string(), r#"["/tmp"]"#.to_string())));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn python_override_set_but_invalid_fails_closed() {
        let get_env = |k: &str| match k {
            "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
            "KASTELLAN_PYTHON_EXEC_PYTHON" => Some("/opt/typo/python3".to_string()),
            "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
            _ => None,
        };
        // The candidates DO exist — but the explicit override must not be
        // silently substituted.
        let exists = |p: &Path| p != Path::new("/opt/typo/python3");
        let c = ctx(&get_env, &exists);
        match PythonExecManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("/opt/typo/python3"), "detail: {detail}");
                assert!(detail.contains("fail-closed"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn no_interpreter_anywhere_is_misconfigured() {
        let get_env = |k: &str| match k {
            "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
            "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
            _ => None,
        };
        let exists = |p: &Path| p == Path::new("/opt/python-exec");
        let c = ctx(&get_env, &exists);
        match PythonExecManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("no python3 interpreter"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn candidate_cascade_picks_first_existing() {
        let get_env = |k: &str| match k {
            "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
            _ => None,
        };
        // Homebrew-only host: only /opt/homebrew/bin/python3 exists.
        let exists = |p: &Path| p == Path::new("/opt/homebrew/bin/python3");
        let python = resolve_env(get_env, |p: &Path| exists(p)).expect("resolves");
        assert_eq!(python, PathBuf::from("/opt/homebrew/bin/python3"));
        // And the derived stdlib prefix follows the prefix, not /usr.
        assert_eq!(
            prefix_lib(&python),
            Some(PathBuf::from("/opt/homebrew/lib"))
        );
    }

    #[test]
    fn missing_worker_binary_is_misconfigured() {
        let get_env = |k: &str| match k {
            "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
            _ => None,
        };
        let exists = |p: &Path| p == Path::new("/usr/bin/python3");
        let c = ctx(&get_env, &exists);
        match PythonExecManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains(DEFAULT_BIN_NAME), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn prefix_lib_requires_a_bin_parent() {
        assert_eq!(prefix_lib(Path::new("/usr/bin/python3")), Some(PathBuf::from("/usr/lib")));
        assert_eq!(prefix_lib(Path::new("/snap/python3")), None);
    }

    fn outcome_label(r: &Resolution) -> &'static str {
        match r {
            Resolution::Register(_) => "Register",
            Resolution::Disabled { .. } => "Disabled",
            Resolution::Misconfigured { .. } => "Misconfigured",
        }
    }
}
