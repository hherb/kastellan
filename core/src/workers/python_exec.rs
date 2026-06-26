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
//! Design: `docs/superpowers/specs/2026-06-12-python-exec-worker-design.md`.

use std::path::{Path, PathBuf};

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::tool_host::ENV_LANDLOCK_RW;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::worker_lifecycle::{Contract, IdleTimeoutCaps, Lifecycle};
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

/// Opt-in knob for the warm/idle lifecycle. `> 0` keeps the micro-VM warm for
/// that many idle seconds between calls; `0`/unset/garbage → today's per-call
/// `SingleUse` boot. Used by both the macOS Container backend and the Linux
/// Firecracker backend.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const IDLE_SECONDS_ENV: &str = "KASTELLAN_PYTHON_EXEC_IDLE_SECONDS";
/// Override for the warm worker's cumulative request cap (slow-leak hygiene).
/// Default [`DEFAULT_MAX_REQUESTS`]. Shared by macOS and Linux micro-VM paths.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const MAX_REQUESTS_ENV: &str = "KASTELLAN_PYTHON_EXEC_MAX_REQUESTS";
/// Override for the warm worker's max-age cap in seconds (drift hygiene).
/// Default [`DEFAULT_MAX_AGE_SECONDS`]. Shared by macOS and Linux micro-VM paths.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const MAX_AGE_SECONDS_ENV: &str = "KASTELLAN_PYTHON_EXEC_MAX_AGE_SECONDS";
/// Default cumulative-request cap, mirroring GLiNER-Relex's manifest.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const DEFAULT_MAX_REQUESTS: u64 = 10_000;
/// Default max-age cap (24 h), mirroring GLiNER-Relex's manifest.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const DEFAULT_MAX_AGE_SECONDS: u64 = 86_400;
/// SIGTERM grace before SIGKILL on warm-worker teardown (fixed; matches GLiNER).
/// Shared by the macOS container lifecycle and the Linux Firecracker lifecycle.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const IDLE_GRACE_SECONDS: u64 = 5;
/// Default image tag built by scripts/workers/python-exec/build-image.sh.
pub const DEFAULT_IMAGE: &str = "kastellan/python-exec:dev";
/// In-image path of the worker binary (Containerfile copies it here). The
/// `MacosContainer` backend appends this as the container's program.
pub const CONTAINER_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-python-exec";
/// In-image python interpreter the worker drives (python:3.12-slim default).
pub const CONTAINER_PYTHON: &str = "/usr/local/bin/python3";

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
/// `Net::Deny`, `Profile::WorkerStrict`, `fs_write = []` (Linux scratch is
/// the jail's ephemeral `/tmp` tmpfs via the explicit Landlock-RW grant;
/// macOS scratch is the per-spawn host dir prepared by `prepare_ephemeral_scratch`
/// and injected into `fs_write` at spawn — see `tool_host`),
/// `cpu_ms = 10_000`, `mem_mb = 512`, `wall_clock_ms = Some(30_000)`,
/// `SingleUse`, `ephemeral_scratch: true`. `fs_read` carries the worker binary,
/// the interpreter, the
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
    params_file_max: Option<String>,
) -> ToolEntry {
    let mut fs_read = vec![binary.clone(), python.clone()];
    if let Some(extra) = interpreter_extra_fs_read(&python) {
        fs_read.push(extra);
    }
    // Bind the interpreter's out-of-prefix shared-lib dirs (issue #284) so a
    // pyenv/Homebrew-linked interpreter can dyld-load in the jail. Empty for a
    // self-contained interpreter (or when the dep tool is unavailable).
    fs_read.extend(interpreter_lib_dirs);
    let mut env = vec![
        (PYTHON_ENV.to_string(), python.to_string_lossy().into_owned()),
        // Grant the jail's /tmp through the worker-side Landlock layer.
        // MUST stay out of fs_write: a /tmp entry there would bind the
        // host /tmp over bwrap's per-spawn ephemeral tmpfs (#89).
        (ENV_LANDLOCK_RW.to_string(), r#"["/tmp"]"#.to_string()),
    ];
    // Forward the operator's file-channel ceiling into the jail ONLY when set,
    // so an unset config leaves the worker env byte-identical (worker default
    // 1 MiB). Blank values are treated as unset.
    if let Some(v) = params_file_max.filter(|v| !v.trim().is_empty()) {
        env.push((PARAMS_FILE_MAX_ENV.to_string(), v));
    }
    let policy = SandboxPolicy {
        fs_read,
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerStrict,
        env,
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

/// Container-mode entry: routes python-exec through the macOS
/// `MacosContainer` micro-VM backend (opt-in via
/// `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1`). Closes the macOS `mem_mb`
/// parity gap (Seatbelt can't enforce memory; Apple `container` does via
/// `-m`) and gives arbitrary agent code a separate-kernel boundary.
///
/// Simpler than [`python_exec_entry`]: NO host interpreter discovery, NO
/// `interpreter_lib_dirs`, `fs_read` empty. Both the worker binary and the
/// interpreter live inside the image; code arrives over stdin and scratch
/// (incl. the >64 KiB `params.json` file channel) lands in the in-VM `/tmp`
/// tmpfs that `build_container_argv` mounts for `WorkerStrict`.
///
/// `mem_mb: 512` is now ENFORCED (the payoff). `cpu_quota_pct`/`tasks_max`
/// stay `None` (python-exec never set them; Apple `container` lacks the
/// primitive anyway). Latency: ~0.8 s container warm-spawn per call under
/// `SingleUse`. Pass an `IdleTimeout` `lifecycle` (operator sets
/// `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS > 0`) to keep the VM warm between calls
/// and amortise that boot; per-call freshness is preserved because the agent's
/// Python is a fresh subprocess each call and the worker wipes its scratch
/// between calls (`wipe_scratch_contents`).
///
/// macOS-only: emits `SandboxBackendKind::Container`, a
/// `#[cfg(target_os = "macos")]` variant. Compiling this on Linux is what
/// broke the core build before issue #144, so the whole fn is gated out
/// there and the resolver never reaches it.
/// Parse the warm/idle env knobs into `(idle_seconds, max_requests, max_age_seconds)`.
///
/// `idle_seconds` is `None` (→ `SingleUse`) unless
/// [`IDLE_SECONDS_ENV`] parses to a value `> 0`. The two cap overrides fall back
/// to their defaults on absent/unparseable input — fail-safe to the
/// conservative GLiNER-mirrored values.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parse_idle_caps(get_env: impl Fn(&str) -> Option<String>) -> (Option<u64>, u64, u64) {
    let parse_u64 = |key: &str| -> Option<u64> { get_env(key).and_then(|v| v.trim().parse().ok()) };
    let idle_seconds = parse_u64(IDLE_SECONDS_ENV).filter(|&n| n > 0);
    let max_requests = parse_u64(MAX_REQUESTS_ENV).unwrap_or(DEFAULT_MAX_REQUESTS);
    let max_age_seconds = parse_u64(MAX_AGE_SECONDS_ENV).unwrap_or(DEFAULT_MAX_AGE_SECONDS);
    (idle_seconds, max_requests, max_age_seconds)
}

/// Build the container-mode lifecycle from the parsed idle window.
///
/// `None`/`Some(0)` → `SingleUse` (today's per-call boot). `Some(n>0)` →
/// `IdleTimeout` keeping the warm VM for `n` idle seconds, with the request/age
/// caps and a fixed 5 s SIGTERM grace. The `Contract { stateless: true }` holds:
/// the agent's Python runs as a fresh subprocess per call and the worker wipes
/// its scratch between calls (`wipe_scratch_contents` in the worker crate).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn container_lifecycle(
    idle_seconds: Option<u64>,
    max_requests: u64,
    max_age_seconds: u64,
) -> Lifecycle {
    match idle_seconds {
        Some(n) if n > 0 => Lifecycle::idle_timeout(
            IdleTimeoutCaps {
                idle_seconds: n,
                max_requests,
                max_age_seconds,
                grace_period_seconds: IDLE_GRACE_SECONDS,
            },
            Contract { stateless: true },
        )
        .expect("stateless = true; validator must accept"),
        _ => Lifecycle::SingleUse,
    }
}

#[cfg(target_os = "macos")]
pub fn container_mode_entry(
    binary: PathBuf,
    image: String,
    params_file_max: Option<String>,
    lifecycle: Lifecycle,
) -> ToolEntry {
    let mut env = vec![(PYTHON_ENV.to_string(), CONTAINER_PYTHON.to_string())];
    if let Some(v) = params_file_max.filter(|v| !v.trim().is_empty()) {
        env.push((PARAMS_FILE_MAX_ENV.to_string(), v));
    }
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerStrict,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::Container),
        container_image: Some(image),
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}

/// Opt into the Linux Firecracker micro-VM backend. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there to avoid a dead-code error under
/// `-D warnings` (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_PYTHON_EXEC_USE_MICROVM";

/// Firecracker-mode entry: routes python-exec through the Linux
/// `FirecrackerVm` micro-VM backend (opt-in via
/// `KASTELLAN_PYTHON_EXEC_USE_MICROVM=1`). Gives arbitrary agent code a
/// separate-kernel boundary on Linux. Simpler than [`python_exec_entry`]:
/// NO host interpreter discovery, NO `interpreter_lib_dirs`, `fs_read` empty.
/// Both the worker binary and the interpreter live inside the rootfs image;
/// code arrives over stdin and scratch (incl. the >64 KiB `params.json` file
/// channel) lands in the in-VM `/tmp` tmpfs that the guest init mounts.
///
/// `mem_mb: 512` is enforced by Firecracker. `cpu_quota_pct`/`tasks_max`
/// stay `None` (python-exec never set them for the non-VM path).
///
/// Linux-only: emits `SandboxBackendKind::FirecrackerVm`, a
/// `#[cfg(target_os = "linux")]` variant. Compiling this on macOS would
/// reference the non-existent variant, breaking the macOS build (issue #144).
#[cfg(target_os = "linux")]
pub fn firecracker_mode_entry(
    binary: PathBuf,
    image_dir: String,
    params_file_max: Option<String>,
    lifecycle: Lifecycle,
) -> ToolEntry {
    let mut env = vec![
        (PYTHON_ENV.to_string(), "/usr/local/bin/python3".to_string()),
        ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
    ];
    // Forward the operator's >64 KiB params file-channel ceiling into the guest
    // ONLY when set (byte-identical to host/container default otherwise). The
    // `>64 KiB → <scratch>/params.json` write lands in the in-VM `/tmp` tmpfs
    // the guest init mounts, so the channel is fully in-guest — same posture as
    // `container_mode_entry`.
    //
    // NOTE (Slice 1): the env vars pushed here (KASTELLAN_PYTHON_PARAMS_FILE_MAX,
    // KASTELLAN_MICROVM_DIR, etc.) are provisioning-only at this stage.
    // `policy.env` is NOT yet forwarded into the guest — the guest init bakes a
    // fixed environment and execs the worker without reading policy env.  These
    // overrides will take effect in-VM only once guest env-forwarding lands
    // (tracked as a follow-up to Slice 1).
    if let Some(v) = params_file_max.filter(|v| !v.trim().is_empty()) {
        env.push((PARAMS_FILE_MAX_ENV.to_string(), v));
    }
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerStrict,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
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

        // Container mode (macOS micro-VM) short-circuits host interpreter
        // resolution: the interpreter lives in the image, not on the host.
        // macOS-only — on Linux USE_CONTAINER is never read so the
        // `Container` variant is never referenced (issue #144).
        #[cfg(target_os = "macos")]
        {
            let enabled = (ctx.get_env)(ENABLE_ENV).unwrap_or_default().trim() == "1";
            let use_container =
                (ctx.get_env)(USE_CONTAINER_ENV).unwrap_or_default().trim() == "1";
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
            let enabled = (ctx.get_env)(ENABLE_ENV).unwrap_or_default().trim() == "1";
            let use_microvm =
                (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
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
