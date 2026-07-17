//! Host-side manifest + `ToolEntry` constructor for the python-exec worker
//! (Phase 4 slice #1).
//!
//! The first executor for agent-authored Python: arbitrary source in,
//! `{exit_code, stdout, stderr}` out, under the strictest policy any worker
//! has — `Net::Deny`, `Profile::WorkerStrict` (the CPython child inherits the
//! seccomp filter across `execve`), no writable host path.
//!
//! **Writable scratch** is per-spawn and ephemeral (`ephemeral_scratch: true`):
//! * **Linux** — the jail's own `/tmp` tmpfs (#89), granted through the
//!   worker-side Landlock layer via `KASTELLAN_LANDLOCK_RW=["/tmp"]`.
//!   `fs_write` stays empty so the *host* `/tmp` is never bound over the
//!   tmpfs.
//! * **macOS** — a host-created per-spawn directory under
//!   `KASTELLAN_WORKER_SCRATCH`, produced by `prepare_ephemeral_scratch` in
//!   `tool_host`, added to `fs_write` at spawn time, and RAII-cleaned after
//!   the worker exits.  No longer "not writable on macOS".
//!
//! Registration is opt-in (`KASTELLAN_PYTHON_EXEC_ENABLE=1`): shell-exec is
//! deny-by-default through its empty argv allowlist, but python-exec has no
//! equivalent operational knob (arbitrary code is the point), so the
//! deny-by-default posture moves to registration itself.
//!
//! The `ToolEntry` builders (host / macOS-container / Linux-Firecracker) and the
//! warm/idle + interpreter helpers they need live in the sibling
//! [`entries`] module (issue #363); they are re-exported here so
//! `python_exec::<name>` call sites are unchanged. This module keeps the
//! manifest + resolver.
//!
//! Design: `docs/superpowers/specs/2026-06-12-python-exec-worker-design.md`.

use std::path::{Path, PathBuf};

use crate::worker_lifecycle::force_route::env_flag_enabled;
use crate::worker_manifest::{
    discover_binary, Resolution, ResolveCtx, ToolDoc, ToolParam, WorkerManifest,
};

mod entries;
// Re-export the entry builders + helpers so external call sites (the e2e suites)
// and the resolver below keep referring to them as `python_exec::<name>` — the
// #363 lift is invisible to callers. Visibility mirrors the pre-split surface:
// the e2e-consumed items stay `pub`; crate-internal helpers stay `pub(crate)`.
pub use entries::{
    interpreter_extra_lib_dirs, python_exec_entry, CONTAINER_PYTHON, CONTAINER_WORKER_BIN,
    DEFAULT_IMAGE,
};
#[cfg(target_os = "macos")]
pub use entries::container_mode_entry;
#[cfg(target_os = "linux")]
pub use entries::firecracker_mode_entry;
// `parse_idle_caps` + `container_lifecycle` are the only entry helpers the
// resolver below calls directly; the remaining entry-internal consts/helpers are
// consumed only by `tests`, which reaches them via `super::entries::` to avoid
// unused-import warnings on the non-test lib build.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use entries::{container_lifecycle, parse_idle_caps};

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
/// Operator-config ceiling for the >64 KiB params file channel, forwarded into
/// the jail when set. Worker-side default + clamp live in
/// `workers/python-exec/src/exec.rs::params_file_max`; keep the name in sync.
const PARAMS_FILE_MAX_ENV: &str = "KASTELLAN_PYTHON_PARAMS_FILE_MAX";

/// Opt into the macOS micro-VM (`MacosContainer`) backend. macOS-only;
/// on Linux the flag is never read (the `Container` variant doesn't exist),
/// so the const is `cfg`-gated out there to avoid a dead-code error under
/// `-D warnings` (issue-#144 rule — its only use site is the macOS resolver
/// branch).
#[cfg(target_os = "macos")]
const USE_CONTAINER_ENV: &str = "KASTELLAN_PYTHON_EXEC_USE_CONTAINER";
/// Operator override for the container image tag. macOS-only; same `cfg`-gate
/// rationale as [`USE_CONTAINER_ENV`].
#[cfg(target_os = "macos")]
const IMAGE_ENV: &str = "KASTELLAN_PYTHON_EXEC_IMAGE";

/// Opt into the Linux Firecracker micro-VM backend. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there to avoid a dead-code error under
/// `-D warnings` (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_PYTHON_EXEC_USE_MICROVM";

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
    if !env_flag_enabled(env_lookup(ENABLE_ENV)) {
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

/// python-exec's manifest. No `allowlist_tool` (there is no argv-shaped
/// operational allowlist; the gate is `KASTELLAN_PYTHON_EXEC_ENABLE`).
pub struct PythonExecManifest;

impl WorkerManifest for PythonExecManifest {
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "python.exec",
            summary: "Execute a short Python program in a sandboxed interpreter and \
                      capture stdout/stderr/exit code.",
            params: &[
                ToolParam { name: "code", description: "the Python source to run", required: true },
                ToolParam {
                    name: "params",
                    description: "optional JSON object exposed to the program",
                    required: false,
                },
            ],
        })
    }

    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let is_runnable = |p: &Path| (ctx.exists)(p) && !(ctx.is_dir)(p);

        // Container mode (macOS micro-VM) short-circuits host interpreter
        // resolution: the interpreter lives in the image, not on the host.
        // macOS-only — on Linux USE_CONTAINER is never read so the
        // `Container` variant is never referenced (issue #144).
        #[cfg(target_os = "macos")]
        {
            let enabled = ctx.flag_enabled(ENABLE_ENV);
            let use_container = ctx.flag_enabled(USE_CONTAINER_ENV);
            if enabled && use_container {
                let binary = PathBuf::from(CONTAINER_WORKER_BIN);
                let image = (ctx.get_env)(IMAGE_ENV)
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_IMAGE.to_string());
                let params_file_max = (ctx.get_env)(PARAMS_FILE_MAX_ENV);
                let (idle, max_req, max_age) = parse_idle_caps(|k| (ctx.get_env)(k));
                return Resolution::Register(container_mode_entry(
                    binary,
                    image,
                    params_file_max,
                    container_lifecycle(idle, max_req, max_age),
                ));
            }
            // enabled && !use_container, or !enabled: fall through to the
            // existing host-mode logic (which re-checks the ENABLE gate).
        }

        // Firecracker micro-VM mode (Linux) short-circuits host interpreter
        // resolution: the interpreter lives inside the rootfs image.
        // Linux-only — on macOS USE_MICROVM is never read so the
        // `FirecrackerVm` variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            let enabled = ctx.flag_enabled(ENABLE_ENV);
            let use_microvm = ctx.flag_enabled(USE_MICROVM_ENV);
            if enabled && use_microvm {
                let binary =
                    PathBuf::from("/usr/local/bin/kastellan-worker-python-exec");
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                let params_file_max = (ctx.get_env)(PARAMS_FILE_MAX_ENV);
                let (idle, max_req, max_age) = parse_idle_caps(|k| (ctx.get_env)(k));
                return Resolution::Register(firecracker_mode_entry(
                    binary,
                    image_dir,
                    params_file_max,
                    container_lifecycle(idle, max_req, max_age),
                ));
            }
            // enabled && !use_microvm, or !enabled: fall through to the
            // existing host-mode logic (which re-checks the ENABLE gate).
        }

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
        let params_file_max = (ctx.get_env)(PARAMS_FILE_MAX_ENV);
        Resolution::Register(python_exec_entry(
            binary,
            python,
            interpreter_lib_dirs,
            params_file_max,
        ))
    }
}

#[cfg(test)]
mod tests;
