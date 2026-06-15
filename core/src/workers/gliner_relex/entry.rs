//! [`ToolEntry`] construction for the gliner-relex worker.
//!
//! [`gliner_relex_entry`] is the public builder the manifest calls; it
//! branches on [`GlinerRelexEnv::use_container_backend`] between the
//! host-mode (bwrap on Linux / Seatbelt on darwin) and container-mode
//! (macOS Apple `container`, opt-in) shapes. The two private builders
//! share their env-var list and lifecycle via the helpers at the bottom
//! so a future PyTorch-hygiene or lifecycle tweak lands in both.

// `PathBuf` is only named by the macOS-only `container_mode_entry`
// (host-mode builds its paths from the already-typed `GlinerRelexEnv`
// fields), so the import is gated to match — avoids an unused-import
// warning on the Linux build (clippy `-D warnings`).
#[cfg(target_os = "macos")]
use std::path::PathBuf;

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use super::resolve::GlinerRelexEnv;
use crate::scheduler::ToolEntry;
use crate::worker_lifecycle::{Contract, IdleTimeoutCaps, Lifecycle};

/// Default image tag for the gliner-relex container backend. Operator
/// can override via `KASTELLAN_GLINER_RELEX_IMAGE` env var (read by
/// `resolve_env`). Bumping this default is a paired edit with
/// `scripts/workers/gliner-relex/build-image.sh`.
///
/// macOS-only: the Apple `container` micro-VM backend doesn't exist on
/// Linux (`SandboxBackendKind::Container` is `#[cfg(target_os = "macos")]`),
/// so gating the const avoids a dead-code warning on the Linux build
/// (issue #144).
#[cfg(target_os = "macos")]
const CONTAINER_IMAGE_DEFAULT: &str = "kastellan/gliner-relex:dev";

/// In-container path to the worker shim. Containerfile uses
/// `uv pip install --system .` which places the console-script from
/// pyproject's `[project.scripts]` at `/usr/local/bin/<name>`. Bumping
/// is a paired edit with the Containerfile's package install path.
///
/// macOS-only for the same reason as [`CONTAINER_IMAGE_DEFAULT`].
#[cfg(target_os = "macos")]
const CONTAINER_BINARY: &str = "/usr/local/bin/kastellan-worker-gliner-relex";

/// Construct the [`ToolEntry`] for the gliner-relex worker.
///
/// Branches on `env.use_container_backend`:
///
/// * `false` → host-mode entry (the existing default): worker spawns
///   from the host venv shim, FS allowlist includes weights + venv +
///   editable-install src dir, runs under Seatbelt on darwin / bwrap
///   on Linux. Byte-equivalent to the pre-Slice-2.5 shape.
///
/// * `true` → container-mode entry (macOS-only opt-in via
///   `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`): worker spawns inside
///   the `kastellan/gliner-relex:dev` image (or operator override) via
///   `MacosContainer`, FS allowlist holds only `weights_dir` (venv +
///   src baked into the image), `sandbox_backend = Some(Container)`,
///   `container_image = Some(<image>)`.
///
/// Lifecycle stays identical between modes via the shared
/// `build_idle_timeout_lifecycle()` helper.
///
/// The returned entry is registered in `core::main` when
/// `KASTELLAN_GLINER_RELEX_ENABLE=1` and the weights directory exists
/// on disk. Without those preconditions the entry is skip-registered
/// (existing deployments byte-equivalent) and calls to `gliner-relex`
/// return `UNKNOWN_TOOL` from the dispatcher.
///
/// Manifest decisions worth knowing (all match the design spec):
///
/// - **`Lifecycle::IdleTimeout`** with 10-minute idle window, 10 000
///   request cap, daily age-out, and 5 s grace. This is the
///   first-ever idle-timeout consumer in the tree (see
///   `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`).
/// - **`Contract { stateless: true }`** — required by
///   `Lifecycle::idle_timeout`'s validator. The worker is genuinely
///   stateless: each `extract` request runs the model on its own
///   text and returns; no memory of prior requests.
/// - **`cpu_ms: 0`** — disables `setrlimit(RLIMIT_CPU)`. The rlimit
///   is cumulative across the process's whole lifetime; on a warm
///   worker doing thousands of inferences it would fire even when
///   no single request is pathological. The cgroup `cpu_quota_pct`
///   ceiling + `Lifecycle::max_age_seconds` rotation handle the
///   actual safety needs; per-request hang detection is dispatcher
///   work that the worker-lifecycle spec deliberately punts.
/// - **`wall_clock_ms: None`** — same logic. Warm workers are
///   long-lived by design; `Lifecycle::max_age_seconds` (24 h) is
///   the rotation budget.
/// - **`Net::Deny`** — the worker has no business reaching the
///   network. `HF_HUB_OFFLINE=1` + `TRANSFORMERS_OFFLINE=1` are
///   defense-in-depth env hints to the libraries themselves.
/// - **`mem_mb: 4_096`** — sized for `multi-v1.0` (~2-3 GB resident)
///   with headroom. Operators picking `large-v0.5` (~4-5 GB) need
///   to bump this; flagged in the README's env-var table.
pub fn gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // The container branch is macOS-only: it constructs an entry tagged
    // `SandboxBackendKind::Container`, a variant that doesn't exist on
    // Linux. On Linux `env.use_container_backend` is forced `false` by
    // `resolve_env` (compile-time), so host mode is the only reachable
    // path and the macOS-only `container_mode_entry` is never referenced
    // — keeping the Linux build green (issue #144). bwrap is the Linux
    // containment layer; Apple `container` is the macOS opt-in.
    #[cfg(target_os = "macos")]
    if env.use_container_backend {
        return container_mode_entry(env);
    }
    host_mode_entry(env)
}

/// Host-mode entry: the existing pre-Slice-2.5 shape. Worker runs from
/// the host venv shim; FS allowlist holds weights + venv + editable
/// src dir; per-OS default sandbox backend (Seatbelt darwin / bwrap
/// linux).
fn host_mode_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // The venv uses an editable install (uv's default for hatchling
    // workspace projects); `.venv/.../_editable_impl_*.pth` points at
    // `<worker_dir>/src`. Mounting only `.venv` would let Python start
    // but fail on `from kastellan_worker_gliner_relex.__main__ import
    // main` with ModuleNotFoundError. Compute the sibling `src/` from
    // the documented `<worker_dir>/.venv` contract on `venv_dir` and
    // bind it read-only too.
    //
    // Path::parent() only returns None when the path is the root `/`
    // or a single relative component like `foo`. A venv_dir that
    // resolves to either is a wiring bug in the caller — daemon
    // startup walks `.venv/bin/<shim>` and the env-resolver always
    // anchors the venv path under at least one extra directory
    // (KASTELLAN_GLINER_RELEX_VENV_DIR is required to be absolute by
    // the operator; the KASTELLAN_DATA_DIR / HOME fallbacks tack on
    // `workers/gliner-relex/.venv`). So fail loudly here rather than
    // silently mounting the wrong path.
    let worker_src_dir = env
        .venv_dir
        .parent()
        .expect("GlinerRelexEnv.venv_dir must have a parent (got a root/relative path)")
        .join("src");

    let mut fs_read = vec![
        env.weights_dir.clone(),
        env.venv_dir.clone(),
        worker_src_dir,
    ];
    // Bind the real interpreter prefix when the venv's python lives outside the
    // venv (uv symlinks `bin/python3` to a base CPython) so the interpreter can
    // start inside the jail — and its out-of-prefix shared-lib dirs (issue #284)
    // so it can dyld-load. Both `None`/empty for a self-contained venv (and on
    // Linux the prefix is `/usr`, already bound by bwrap — a harmless redundancy).
    if let Some(root) = &env.interpreter_root {
        fs_read.push(root.clone());
    }
    fs_read.extend(env.interpreter_lib_dirs.iter().cloned());

    let policy = SandboxPolicy {
        fs_read,
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
        env: build_runtime_env(env),
        proxy_uds: None,
    };

    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: None,
        lifecycle: build_idle_timeout_lifecycle(),
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
    }
}

/// Container-mode entry: routes the worker through the macOS
/// `MacosContainer` SandboxBackend (Slice 2.5+; opt-in via
/// `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`). Only `weights_dir` mounts
/// from the host; venv + src are baked into the image. The image is
/// per-call constructed via `SandboxBackends::resolve(Some(Container),
/// Some(<image>))`.
///
/// macOS-only: emits an entry tagged `SandboxBackendKind::Container`,
/// which is `#[cfg(target_os = "macos")]`-gated in `kastellan-sandbox`.
/// Compiling this on Linux is what broke the core build before issue
/// #144 — Linux drives `use_container_backend` to a compile-time
/// `false`, so this function is never reached there and need not exist.
#[cfg(target_os = "macos")]
fn container_mode_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // Container-mode policy: fs_read mounts host weights only.
    // build_container_argv uses source=<P>,target=<P> convention, so the
    // weights mount at the SAME host path inside the container — that
    // makes the existing KASTELLAN_GLINER_RELEX_WEIGHTS_DIR env value
    // work verbatim without a path rewrite.
    //
    // Enforcement parity note (Slice 2.5):
    //   * `mem_mb: 4_096` — Apple `container` enforces. This is the
    //     payoff for opting into container mode; Seatbelt has no
    //     memory primitive so the same value was a silent no-op on
    //     darwin before Slice 2.5.
    //   * `cpu_quota_pct: Some(400)` / `tasks_max: Some(64)` —
    //     Apple `container` does NOT enforce these today (semantic
    //     gap acknowledged in HANDOVER / sandbox docs). Kept on the
    //     policy struct for parity with bwrap (Linux DGX) and so a
    //     future container-CLI version exposing the equivalent
    //     primitive picks them up without a manifest edit.
    let policy = SandboxPolicy {
        fs_read: vec![env.weights_dir.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
        env: build_runtime_env(env),
        proxy_uds: None,
    };

    let image = env
        .container_image
        .as_deref()
        .unwrap_or(CONTAINER_IMAGE_DEFAULT)
        .to_string();

    ToolEntry {
        binary: PathBuf::from(CONTAINER_BINARY),
        policy,
        wall_clock_ms: None,
        lifecycle: build_idle_timeout_lifecycle(),
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::Container),
        container_image: Some(image),
        lockdown_shim: None,
    }
}

/// Shared env-var list for both host-mode and container-mode entries.
/// Single source of truth so a future PyTorch hygiene addition lands
/// in both branches automatically.
fn build_runtime_env(env: &GlinerRelexEnv) -> Vec<(String, String)> {
    vec![
        (
            "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR".to_string(),
            env.weights_dir.to_string_lossy().into_owned(),
        ),
        (
            "KASTELLAN_GLINER_RELEX_MODEL".to_string(),
            env.model_id.clone(),
        ),
        (
            "KASTELLAN_GLINER_RELEX_DEVICE".to_string(),
            env.device.clone(),
        ),
        ("HF_HUB_OFFLINE".to_string(), "1".to_string()),
        ("TRANSFORMERS_OFFLINE".to_string(), "1".to_string()),
        // PyTorch's _dynamo (transitively imported by transformers)
        // calls getpass.getuser() at module-import time, which falls
        // back to pwd.getpwuid(os.getuid()) when no
        // LOGNAME/USER/LNAME/USERNAME is set. The sandbox has no
        // /etc/passwd, so that fallback raises KeyError and the worker
        // exits before serving any RPC. Setting USER skips the pwd
        // lookup entirely.
        ("USER".to_string(), "kastellan".to_string()),
        // TORCHINDUCTOR_CACHE_DIR pre-empts the home-dir cache
        // computation that triggers the getpass.getuser path above
        // (defense in depth — USER alone is sufficient today, but a
        // future torch refactor could re-route through getuid()).
        (
            "TORCHINDUCTOR_CACHE_DIR".to_string(),
            "/tmp/torchinductor".to_string(),
        ),
    ]
}

/// Shared lifecycle constructor: 10-minute idle window, 10 000 request
/// cap, daily age-out, 5 s grace. Identical between host-mode and
/// container-mode entries. Extracted from the inline body so both
/// branches use one source of truth; the existing
/// `entry_carries_idle_timeout_lifecycle_with_spec_caps` test pins the
/// caps.
fn build_idle_timeout_lifecycle() -> Lifecycle {
    Lifecycle::idle_timeout(
        IdleTimeoutCaps {
            idle_seconds: 600,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        },
        Contract { stateless: true },
    )
    .expect("manifest declares stateless = true; validator must accept")
}
