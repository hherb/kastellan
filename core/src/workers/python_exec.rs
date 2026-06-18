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
pub(crate) const TOOL_NAME: &str = "python-exec";
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
/// is unset: distro python (`/usr/bin`), then source installs
/// (`/usr/local/bin`). `pub` so the e2e suite probes the identical cascade.
#[cfg(not(target_os = "macos"))]
pub const PYTHON_CANDIDATES: &[&str] = &["/usr/bin/python3", "/usr/local/bin/python3"];

/// macOS interpreter candidates. `/usr/bin/python3` is deliberately
/// ABSENT: on every Mac that path is Apple's xcrun shim (`/usr/bin` is
/// SIP-protected — nothing else can live there), which locates the real
/// interpreter by `dlopen()`ing `libxcrun.dylib` from the Xcode/CLT tree.
/// That tree is not readable inside the Seatbelt jail, so the shim always
/// dies with exit 1 (observed 2026-06-13 in `python_exec_e2e`). The
/// candidates below all canonicalize to a self-contained framework
/// python: Apple-Silicon Homebrew, Intel-Homebrew / python.org installer,
/// then the Command Line Tools framework python. `pub` so the e2e suite
/// probes the identical cascade.
#[cfg(target_os = "macos")]
pub const PYTHON_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/python3",
    "/usr/local/bin/python3",
    "/Library/Developer/CommandLineTools/usr/bin/python3",
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
/// `SingleUse`. `fs_read` carries the worker binary, the interpreter, the
/// derived stdlib path from [`interpreter_extra_fs_read`] (`<prefix>/lib`,
/// or the framework version root for macOS framework pythons) — redundant
/// under bwrap's always-bound `/usr`, required for non-`/usr` prefixes
/// under Seatbelt/Landlock — and `interpreter_lib_dirs`, the interpreter's
/// out-of-prefix shared-library dirs (issue #284; see
/// [`interpreter_extra_lib_dirs`]). Pass an empty vec when the caller hasn't
/// resolved the dep graph (the manual `*_EXTRA_FS_READ` hatch stays the backstop).
pub fn python_exec_entry(
    binary: PathBuf,
    python: PathBuf,
    interpreter_lib_dirs: Vec<PathBuf>,
) -> ToolEntry {
    let mut fs_read = vec![binary.clone(), python.clone()];
    if let Some(extra) = interpreter_extra_fs_read(&python) {
        fs_read.push(extra);
    }
    // Bind the interpreter's out-of-prefix shared-lib dirs (issue #284) so a
    // pyenv/Homebrew-linked interpreter can dyld-load in the jail. Empty for a
    // self-contained interpreter (or when the dep tool is unavailable).
    fs_read.extend(interpreter_lib_dirs);
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
        lockdown_shim: None,
        ephemeral_scratch: true,
    }
}

/// Extra read-only path the jailed interpreter needs beyond its own binary.
///
/// * **macOS framework layout** (`…/Python*.framework/Versions/<v>/bin/<exe>`,
///   which every working macOS python canonicalizes into — Homebrew,
///   python.org, CLT): grant the whole version root. The interpreter
///   dylib (`<root>/Python`) and `Resources/` are *siblings* of `bin/`
///   and `lib/`, so a `lib`-only grant cannot even load the binary.
/// * **POSIX prefix layout** (`<prefix>/bin/<exe>`): grant `<prefix>/lib`
///   (the stdlib). Redundant under bwrap's always-bound `/usr`, required
///   for non-`/usr` prefixes under Seatbelt/Landlock.
/// * Anything else (no `bin/` parent): `None`.
fn interpreter_extra_fs_read(python: &Path) -> Option<PathBuf> {
    let bin_dir = python.parent()?;
    if bin_dir.file_name()? != "bin" {
        return None;
    }
    let prefix = bin_dir.parent()?;
    let is_framework_version_root = prefix
        .parent() // …/Versions
        .and_then(|v| v.parent()) // …/Python*.framework
        .and_then(|f| f.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".framework"));
    if is_framework_version_root {
        Some(prefix.to_path_buf())
    } else {
        Some(prefix.join("lib"))
    }
}

/// Out-of-prefix shared-library dirs the resolved interpreter needs to
/// dyld-load inside the jail (issue #284).
///
/// `python` must be the **canonical** interpreter path. The dep-graph walk
/// treats [`interpreter_extra_fs_read`] — the prefix `lib` / framework version
/// root this worker already binds — as the in-jail-readable region; anything
/// the interpreter links *outside* it (e.g. a pyenv CPython's Homebrew
/// `libintl`) is returned for an extra read-only bind. When the interpreter has
/// no `bin/` parent (so no derived bound region), the binary path itself is used
/// as the prefix — nothing lies under a file path, so every non-system dep is
/// bound (safe over-approximation). Empty when the interpreter is self-contained
/// or the dep tool is unavailable (fail-safe — the manual `*_EXTRA_FS_READ`
/// hatch stays the backstop).
///
/// `pub` so the e2e suite computes the identical dirs the manifest does (the
/// seed logic lives in [`crate::workers::interpreter_deps`], so the two can't
/// drift). Pure: `exists`, `canonicalize`, and `resolve_deps` are injected.
pub fn interpreter_extra_lib_dirs(
    python: &Path,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
    resolve_deps: &dyn Fn(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    let prefix = interpreter_extra_fs_read(python).unwrap_or_else(|| python.to_path_buf());
    crate::workers::interpreter_deps::interpreter_lib_dirs_for_binary(
        python,
        &prefix,
        exists,
        canonicalize,
        resolve_deps,
    )
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
            // Canonicalize host-side: a symlink-chain interpreter (e.g.
            // `/usr/bin/python3 → /etc/alternatives/python3` on
            // update-alternatives distros) is unreachable *inside* the jail
            // when the link's intermediate dir isn't bound. The policy and
            // the injected env must carry the real path. Best-effort: when
            // canonicalization fails we keep the raw path (it passed the
            // existence probe, so the common direct-file case still works).
            Ok(p) => (ctx.canonicalize)(&p).unwrap_or(p),
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
        // Resolve the interpreter's out-of-prefix shared-lib dirs (issue #284)
        // via the real linker-introspection tool. Fail-safe: an unavailable
        // tool yields no extra binds (the manual EXTRA_FS_READ hatch backstops).
        let interpreter_lib_dirs = interpreter_extra_lib_dirs(
            &python,
            &|p| (ctx.exists)(p),
            &|p| (ctx.canonicalize)(p),
            &crate::workers::interpreter_deps::resolve_deps_via_tool,
        );
        Resolution::Register(python_exec_entry(binary, python, interpreter_lib_dirs))
    }
}

#[cfg(test)]
mod tests;
