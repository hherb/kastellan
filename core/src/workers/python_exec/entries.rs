//! `ToolEntry` builders for the python-exec worker and the helpers they need.
//!
//! Lifted out of the parent `python_exec` module (issue #363) so the manifest +
//! resolver stay readable under the rule-4 500-LOC cap. Three backends build an
//! entry here:
//!
//! * [`python_exec_entry`] — the default host-sandbox entry (bwrap/Seatbelt),
//!   strictest policy of any worker.
//! * [`container_mode_entry`] — the macOS `MacosContainer` micro-VM (opt-in).
//! * [`firecracker_mode_entry`] — the Linux Firecracker micro-VM (opt-in).
//!
//! The warm/idle lifecycle helpers ([`parse_idle_caps`], [`container_lifecycle`])
//! and the interpreter dep-graph helpers ([`interpreter_extra_fs_read`],
//! [`interpreter_extra_lib_dirs`]) live here too — they only serve entry
//! construction. All items are re-exported from the parent so existing
//! `python_exec::<name>` call sites (the resolver + the e2e suites) are
//! unchanged by the lift.

use std::path::{Path, PathBuf};

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use super::{PARAMS_FILE_MAX_ENV, PYTHON_ENV};
use crate::scheduler::ToolEntry;
use crate::tool_host::ENV_LANDLOCK_RW;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::worker_lifecycle::{Contract, IdleTimeoutCaps, Lifecycle};

/// Default image tag built by scripts/workers/python-exec/build-image.sh.
pub const DEFAULT_IMAGE: &str = "kastellan/python-exec:dev";
/// In-image path of the worker binary (Containerfile copies it here). The
/// `MacosContainer` backend appends this as the container's program.
pub const CONTAINER_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-python-exec";
/// In-image python interpreter the worker drives (python:3.12-slim default).
pub const CONTAINER_PYTHON: &str = "/usr/local/bin/python3";

/// Opt-in knob for the warm/idle lifecycle. `> 0` keeps the micro-VM warm for
/// that many idle seconds between calls; `0`/unset/garbage → today's per-call
/// `SingleUse` boot. Used by both the macOS Container backend and the Linux
/// Firecracker backend.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const IDLE_SECONDS_ENV: &str = "KASTELLAN_PYTHON_EXEC_IDLE_SECONDS";
/// Override for the warm worker's cumulative request cap (slow-leak hygiene).
/// Default [`DEFAULT_MAX_REQUESTS`]. Shared by macOS and Linux micro-VM paths.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const MAX_REQUESTS_ENV: &str = "KASTELLAN_PYTHON_EXEC_MAX_REQUESTS";
/// Override for the warm worker's max-age cap in seconds (drift hygiene).
/// Default [`DEFAULT_MAX_AGE_SECONDS`]. Shared by macOS and Linux micro-VM paths.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const MAX_AGE_SECONDS_ENV: &str = "KASTELLAN_PYTHON_EXEC_MAX_AGE_SECONDS";
/// Default cumulative-request cap, mirroring GLiNER-Relex's manifest.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const DEFAULT_MAX_REQUESTS: u64 = 10_000;
/// Default max-age cap (24 h), mirroring GLiNER-Relex's manifest.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const DEFAULT_MAX_AGE_SECONDS: u64 = 86_400;
/// SIGTERM grace before SIGKILL on warm-worker teardown (fixed; matches GLiNER).
/// Shared by the macOS container lifecycle and the Linux Firecracker lifecycle.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const IDLE_GRACE_SECONDS: u64 = 5;

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
        embed_broker_uds: None,
        persistent_store: None,
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

/// Parse the warm/idle env knobs into `(idle_seconds, max_requests, max_age_seconds)`.
///
/// `idle_seconds` is `None` (→ `SingleUse`) unless
/// [`IDLE_SECONDS_ENV`] parses to a value `> 0`. The two cap overrides fall back
/// to their defaults on absent/unparseable input — fail-safe to the
/// conservative GLiNER-mirrored values.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn parse_idle_caps(get_env: impl Fn(&str) -> Option<String>) -> (Option<u64>, u64, u64) {
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
pub(crate) fn container_lifecycle(
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
        embed_broker_uds: None,
        persistent_store: None,
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
        // The in-guest interpreter path — `/usr/bin/python3` is the rootfs
        // reality (`build-rootfs.sh` copies the stdlib at the native `/usr`
        // prefix; the guest init bakes the same path as its fail-safe fallback).
        // This value is FORWARDED into the guest now (#360), so it must match the
        // rootfs — the old `/usr/local/bin/python3` was a latent mismatch that
        // only stayed harmless while env was provisioning-only.
        (PYTHON_ENV.to_string(), "/usr/bin/python3".to_string()),
        ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
    ];
    // Forward the operator's >64 KiB params file-channel ceiling into the guest
    // ONLY when set (byte-identical to host/container default otherwise). The
    // `>64 KiB → <scratch>/params.json` write lands in the in-VM `/tmp` tmpfs
    // the guest init mounts, so the channel is fully in-guest — same posture as
    // `container_mode_entry`.
    //
    // All of `policy.env` is forwarded into the guest via the hex
    // `kastellan.env=` kernel-cmdline token the sandbox backend bakes into
    // `boot_args` (#360); the guest init decodes it and applies it over its baked
    // defaults before exec'ing the worker. So the ceiling set here is live in-VM.
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
        embed_broker_uds: None,
        persistent_store: None,
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
pub(crate) fn interpreter_extra_fs_read(python: &Path) -> Option<PathBuf> {
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
