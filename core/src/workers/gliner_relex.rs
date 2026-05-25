//! GLiNER-Relex worker manifest + wire-shape types.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! for the design, and the Slice 2 section of
//! `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md` for the
//! task-level breakdown this module implements.
//!
//! What this module owns:
//!
//! - [`GlinerRelexEnv`] — daemon-startup builder; carries the resolved
//!   weights/venv paths + model id + device selector.
//! - [`gliner_relex_entry`] — produces the [`crate::scheduler::ToolEntry`]
//!   that the dispatcher's [`crate::scheduler::ToolRegistry`] holds.
//! - [`ExtractRequest`] / [`ExtractResponse`] / [`Entity`] /
//!   [`TripleEntity`] / [`Triple`] — serde shape types matching the
//!   Python worker's wire contract (see
//!   `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`
//!   for the producing side + `workers/gliner-relex/README.md` for the
//!   field-by-field shape table).
//!
//! What this module deliberately does NOT own:
//!
//! - **A typed Rust client wrapping [`crate::tool_host::dispatch`]**.
//!   The dispatcher's `report_crash` chokepoint between `dispatch` and
//!   `map_dispatch_result` makes a standalone client either duplicate
//!   crash-classifier logic or couple to a lifecycle manager; the v2
//!   entity-extraction consumer slice will pick the right shape around
//!   its actual call site. See HANDOVER's design-spec section for the
//!   rationale.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hhagent_protocol::client::ClientError as ProtocolClientError;
use hhagent_sandbox::{Net, Profile, SandboxPolicy};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::scheduler::ToolEntry;
use crate::tool_host::{self, ToolHostError};
use crate::worker_lifecycle::{Contract, IdleTimeoutCaps, Lifecycle, WorkerLifecycleManager};

/// Resolved paths + config for the GLiNER-Relex worker.
///
/// Populated by the daemon's startup code from environment variables
/// (see `core/src/main.rs::build_gliner_relex_entry`) and passed into
/// [`gliner_relex_entry`] to build the manifest.
///
/// Production callers should construct this via the daemon helper;
/// tests build it directly to pin manifest shape without touching the
/// real filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlinerRelexEnv {
    /// Absolute path to the uv-generated console-script shim:
    /// `<worker_dir>/.venv/bin/hhagent-worker-gliner-relex`. This is
    /// the binary the dispatcher spawns under sandbox; `pyproject.toml`
    /// declares `[project.scripts] hhagent-worker-gliner-relex` so
    /// `uv sync` creates the file.
    pub script_path: PathBuf,
    /// Absolute path to the worker venv root: `<worker_dir>/.venv/`.
    /// Mounted read-only into the sandbox via `policy.fs_read` so the
    /// Python interpreter + site-packages are visible from inside the
    /// jail.
    pub venv_dir: PathBuf,
    /// Absolute path to the model snapshot directory; operator stages
    /// this via `scripts/workers/gliner-relex/install.sh`. Mounted
    /// read-only via `policy.fs_read`. Daemon refuses to register the
    /// worker if this path doesn't exist on disk at startup.
    pub weights_dir: PathBuf,
    /// HF repo ID matching the on-disk snapshot. One of
    /// `knowledgator/gliner-relex-multi-v1.0` (default) or
    /// `knowledgator/gliner-relex-large-v0.5`. Forwarded via env var
    /// to the worker for its own startup-time logging only — the
    /// worker loads from `weights_dir` directly.
    pub model_id: String,
    /// One of `auto` / `cpu` / `cuda` / `mps`. Forwarded verbatim
    /// via `HHAGENT_GLINER_RELEX_DEVICE`; per-platform legality is
    /// enforced by the Python `__main__._resolve_device` helper, not
    /// here.
    ///
    /// * `auto`:
    ///   - **Linux**: probe `torch.cuda.mem_get_info(0)` for >= 3 GiB
    ///     free (per spike correction #4); pick `cuda` if so, else
    ///     fall back to `cpu` silently.
    ///   - **darwin**: resolve directly to `cpu`. The macOS MPS spike
    ///     found MPS regresses ~5x vs CPU on realistic ~600-char
    ///     paragraph input and worst-case cold dispatch is 4 s. MPS
    ///     is opt-in only.
    ///
    /// * `cpu` / `cuda` / `mps`: explicit overrides. `cuda` is
    ///   rejected on darwin; `mps` is rejected on non-darwin; both
    ///   produce a `MODEL_LOAD_FAILED` / `UNSUPPORTED_DEVICE` exit
    ///   from the worker at startup so the operator sees the
    ///   misconfig immediately.
    pub device: String,
    /// True when the operator set `HHAGENT_GLINER_RELEX_USE_CONTAINER=1`
    /// (strict: only `"1"` after trim counts). `gliner_relex_entry`
    /// branches on this field to emit the container-mode `ToolEntry`
    /// shape (in-container binary, weights-only `fs_read`,
    /// `sandbox_backend = Some(Container)`, `container_image` populated)
    /// instead of the host-mode one.
    ///
    /// In container mode `resolve_env` also skips the host-venv
    /// existence check — the worker shim lives inside the image at
    /// `/usr/local/bin/hhagent-worker-gliner-relex`, so requiring a
    /// host venv would be a footgun for container-mode-only operators.
    pub use_container_backend: bool,
    /// Operator-supplied container image tag override, read from
    /// `HHAGENT_GLINER_RELEX_IMAGE`. `None` (default) falls back to
    /// the `CONTAINER_IMAGE_DEFAULT` constant at the
    /// `gliner_relex_entry` callsite. Symmetric to
    /// `HHAGENT_GLINER_RELEX_MODEL` override behaviour.
    ///
    /// This field is **orthogonal to `use_container_backend`** — the
    /// env var is read unconditionally so that operators can stage an
    /// image tag preference ahead of flipping `USE_CONTAINER=1`. In
    /// host mode the value is carried on the struct but ignored at
    /// `ToolEntry` construction (only `container_mode_entry` reads it).
    /// Setting `IMAGE=` alone (without `USE_CONTAINER=1`) does NOT
    /// switch the worker to container mode.
    pub container_image: Option<String>,
}

/// Default image tag for the gliner-relex container backend. Operator
/// can override via `HHAGENT_GLINER_RELEX_IMAGE` env var (read by
/// `resolve_env`). Bumping this default is a paired edit with
/// `scripts/workers/gliner-relex/build-image.sh`.
const CONTAINER_IMAGE_DEFAULT: &str = "hhagent/gliner-relex:dev";

/// In-container path to the worker shim. Containerfile uses
/// `uv pip install --system .` which places the console-script from
/// pyproject's `[project.scripts]` at `/usr/local/bin/<name>`. Bumping
/// is a paired edit with the Containerfile's package install path.
const CONTAINER_BINARY: &str = "/usr/local/bin/hhagent-worker-gliner-relex";

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
///   `HHAGENT_GLINER_RELEX_USE_CONTAINER=1`): worker spawns inside
///   the `hhagent/gliner-relex:dev` image (or operator override) via
///   `MacosContainer`, FS allowlist holds only `weights_dir` (venv +
///   src baked into the image), `sandbox_backend = Some(Container)`,
///   `container_image = Some(<image>)`.
///
/// Lifecycle stays identical between modes via the shared
/// `build_idle_timeout_lifecycle()` helper.
///
/// The returned entry is registered in `core::main` when
/// `HHAGENT_GLINER_RELEX_ENABLE=1` and the weights directory exists
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
    if env.use_container_backend {
        container_mode_entry(env)
    } else {
        host_mode_entry(env)
    }
}

/// Host-mode entry: the existing pre-Slice-2.5 shape. Worker runs from
/// the host venv shim; FS allowlist holds weights + venv + editable
/// src dir; per-OS default sandbox backend (Seatbelt darwin / bwrap
/// linux).
fn host_mode_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // The venv uses an editable install (uv's default for hatchling
    // workspace projects); `.venv/.../_editable_impl_*.pth` points at
    // `<worker_dir>/src`. Mounting only `.venv` would let Python start
    // but fail on `from hhagent_worker_gliner_relex.__main__ import
    // main` with ModuleNotFoundError. Compute the sibling `src/` from
    // the documented `<worker_dir>/.venv` contract on `venv_dir` and
    // bind it read-only too.
    //
    // Path::parent() only returns None when the path is the root `/`
    // or a single relative component like `foo`. A venv_dir that
    // resolves to either is a wiring bug in the caller — daemon
    // startup walks `.venv/bin/<shim>` and the env-resolver always
    // anchors the venv path under at least one extra directory
    // (HHAGENT_GLINER_RELEX_VENV_DIR is required to be absolute by
    // the operator; the HHAGENT_DATA_DIR / HOME fallbacks tack on
    // `workers/gliner-relex/.venv`). So fail loudly here rather than
    // silently mounting the wrong path.
    let worker_src_dir = env
        .venv_dir
        .parent()
        .expect("GlinerRelexEnv.venv_dir must have a parent (got a root/relative path)")
        .join("src");

    let policy = SandboxPolicy {
        fs_read: vec![
            env.weights_dir.clone(),
            env.venv_dir.clone(),
            worker_src_dir,
        ],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
        env: build_runtime_env(env),
    };

    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: None,
        lifecycle: build_idle_timeout_lifecycle(),
        sandbox_backend: None,
        container_image: None,
    }
}

/// Container-mode entry: routes the worker through the macOS
/// `MacosContainer` SandboxBackend (Slice 2.5+; opt-in via
/// `HHAGENT_GLINER_RELEX_USE_CONTAINER=1`). Only `weights_dir` mounts
/// from the host; venv + src are baked into the image. The image is
/// per-call constructed via `SandboxBackends::resolve(Some(Container),
/// Some(<image>))`.
fn container_mode_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // Container-mode policy: fs_read mounts host weights only.
    // build_container_argv uses source=<P>,target=<P> convention, so the
    // weights mount at the SAME host path inside the container — that
    // makes the existing HHAGENT_GLINER_RELEX_WEIGHTS_DIR env value
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
        sandbox_backend: Some(hhagent_sandbox::SandboxBackendKind::Container),
        container_image: Some(image),
    }
}

/// Shared env-var list for both host-mode and container-mode entries.
/// Single source of truth so a future PyTorch hygiene addition lands
/// in both branches automatically.
fn build_runtime_env(env: &GlinerRelexEnv) -> Vec<(String, String)> {
    vec![
        (
            "HHAGENT_GLINER_RELEX_WEIGHTS_DIR".to_string(),
            env.weights_dir.to_string_lossy().into_owned(),
        ),
        (
            "HHAGENT_GLINER_RELEX_MODEL".to_string(),
            env.model_id.clone(),
        ),
        (
            "HHAGENT_GLINER_RELEX_DEVICE".to_string(),
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
        ("USER".to_string(), "hhagent".to_string()),
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

/// Reason the daemon's [`GlinerRelexEnv`] resolver returned no entry.
///
/// `resolve_env` either yields a populated [`GlinerRelexEnv`] or one of
/// these structured variants. The daemon turns each variant into a
/// `tracing::info!` / `tracing::error!` line at startup so operators
/// can tell at a glance which precondition isn't met. Tests exercise
/// each branch directly without touching process-wide environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveSkipReason {
    /// `HHAGENT_GLINER_RELEX_ENABLE` is unset, empty, or anything other
    /// than `"1"` (after trim). This is the production default — every
    /// deployment that hasn't run `scripts/workers/gliner-relex/install.sh`
    /// and explicitly enabled the worker lands here.
    Disabled,
    /// `HHAGENT_GLINER_RELEX_ENABLE=1` but
    /// `HHAGENT_GLINER_RELEX_WEIGHTS_DIR` is unset.
    WeightsDirEnvMissing,
    /// `HHAGENT_GLINER_RELEX_WEIGHTS_DIR` is set but the path doesn't
    /// resolve to a directory on disk at daemon-startup time.
    WeightsDirNotADir { path: PathBuf },
    /// None of `HHAGENT_GLINER_RELEX_VENV_DIR`, `HHAGENT_DATA_DIR`, or
    /// `HOME` is set — there is no anchor to default the venv path
    /// against. This is the failure mode that previously fell through
    /// to `/tmp` silently; surfacing it explicitly so the operator log
    /// shows the misconfiguration.
    VenvDirUnresolvable,
    /// Resolved `<venv_dir>/bin/hhagent-worker-gliner-relex` doesn't
    /// exist on disk.
    ScriptShimMissing { path: PathBuf },
}

/// Resolve a [`GlinerRelexEnv`] from a generic env lookup + filesystem
/// predicates.
///
/// This is the pure core of `core::main::build_gliner_relex_entry`. The
/// daemon passes [`std::env::var`] + [`Path::is_dir`] + [`Path::exists`];
/// tests pass in-memory fakes to exercise each skip-register branch
/// without touching the process environment or filesystem.
///
/// Env vars consulted (same names + semantics as the production helper):
///
/// - `HHAGENT_GLINER_RELEX_ENABLE` — must be `"1"` (whitespace-trimmed)
///   to register the worker. Anything else (unset / `0` / `true` / `on`)
///   returns [`ResolveSkipReason::Disabled`].
/// - `HHAGENT_GLINER_RELEX_WEIGHTS_DIR` — required; absolute path to the
///   model snapshot.
/// - `HHAGENT_GLINER_RELEX_MODEL` — optional; default
///   `knowledgator/gliner-relex-multi-v1.0`.
/// - `HHAGENT_GLINER_RELEX_DEVICE` — optional; default `auto`.
/// - `HHAGENT_GLINER_RELEX_USE_CONTAINER` — optional; `"1"` (strict,
///   whitespace-trimmed) opts into container mode. Anything else (unset /
///   `0` / `true` / `on`) uses host mode (the default). In container mode
///   the venv-anchor cascade below is skipped — the worker shim lives
///   inside the container image at `/usr/local/bin/...`.
/// - `HHAGENT_GLINER_RELEX_IMAGE` — optional container image tag override;
///   `None` defers to the `CONTAINER_IMAGE_DEFAULT` constant at the
///   `gliner_relex_entry` callsite (added in Task 6).
/// - `HHAGENT_GLINER_RELEX_VENV_DIR` — optional; if set, used verbatim.
/// - `HHAGENT_DATA_DIR` — optional anchor for the venv default
///   (`<data>/workers/gliner-relex/.venv`).
/// - `HOME` — last-resort anchor (`<home>/.local/share/hhagent/...`).
///   If neither `HHAGENT_DATA_DIR` nor `HOME` is set and the operator
///   didn't pass `HHAGENT_GLINER_RELEX_VENV_DIR`, returns
///   [`ResolveSkipReason::VenvDirUnresolvable`] rather than silently
///   defaulting to `/tmp` — that earlier silent fallback hid
///   misconfiguration on minimal-env hosts (containers, system
///   services) where the operator usually meant `HHAGENT_DATA_DIR` to
///   be set.
pub fn resolve_env<EnvLookup, IsDir, Exists>(
    env_lookup: EnvLookup,
    is_dir: IsDir,
    exists: Exists,
) -> Result<GlinerRelexEnv, ResolveSkipReason>
where
    EnvLookup: Fn(&str) -> Option<String>,
    IsDir: Fn(&Path) -> bool,
    Exists: Fn(&Path) -> bool,
{
    let enable = env_lookup("HHAGENT_GLINER_RELEX_ENABLE").unwrap_or_default();
    // `trim` so a stray newline from `echo "1" > envfile` doesn't fail
    // the opt-in silently. Strict on the value itself: only `"1"`
    // counts. Inviting `true` / `yes` / `on` would surface the next
    // operator's dialect debate; the README documents `=1` explicitly.
    if enable.trim() != "1" {
        return Err(ResolveSkipReason::Disabled);
    }

    let weights_dir = match env_lookup("HHAGENT_GLINER_RELEX_WEIGHTS_DIR") {
        Some(v) => PathBuf::from(v),
        None => return Err(ResolveSkipReason::WeightsDirEnvMissing),
    };
    if !is_dir(&weights_dir) {
        return Err(ResolveSkipReason::WeightsDirNotADir { path: weights_dir });
    }

    let model_id = env_lookup("HHAGENT_GLINER_RELEX_MODEL")
        .unwrap_or_else(|| "knowledgator/gliner-relex-multi-v1.0".to_string());
    let device = env_lookup("HHAGENT_GLINER_RELEX_DEVICE")
        .unwrap_or_else(|| "auto".to_string());

    // New env knobs (Slice 2.5):
    //   * `HHAGENT_GLINER_RELEX_USE_CONTAINER=1` → container-mode (strict on "1").
    //   * `HHAGENT_GLINER_RELEX_IMAGE=<tag>` → operator-supplied image override.
    let use_container_backend = env_lookup("HHAGENT_GLINER_RELEX_USE_CONTAINER")
        .map(|v| {
            // trim() rationale: matches HHAGENT_GLINER_RELEX_ENABLE strictness; see comment above.
            v.trim() == "1"
        })
        .unwrap_or(false);
    let container_image = env_lookup("HHAGENT_GLINER_RELEX_IMAGE");

    // Host venv resolution is skipped in container mode — the worker
    // shim lives inside the image, so no host venv is required.
    let (venv_dir, script_path) = if use_container_backend {
        (PathBuf::new(), PathBuf::new())
    } else {
        // Anchor priority: explicit override > data-dir > home. No
        // `/tmp` fallback — see ResolveSkipReason::VenvDirUnresolvable
        // for the rationale.
        let venv_dir = if let Some(v) = env_lookup("HHAGENT_GLINER_RELEX_VENV_DIR") {
            PathBuf::from(v)
        } else if let Some(data_dir) = env_lookup("HHAGENT_DATA_DIR") {
            PathBuf::from(data_dir).join("workers/gliner-relex/.venv")
        } else if let Some(home) = env_lookup("HOME") {
            PathBuf::from(home)
                .join(".local/share/hhagent/workers/gliner-relex/.venv")
        } else {
            return Err(ResolveSkipReason::VenvDirUnresolvable);
        };
        let script_path = venv_dir.join("bin").join("hhagent-worker-gliner-relex");
        if !exists(&script_path) {
            return Err(ResolveSkipReason::ScriptShimMissing { path: script_path });
        }
        (venv_dir, script_path)
    };

    Ok(GlinerRelexEnv {
        script_path,
        venv_dir,
        weights_dir,
        model_id,
        device,
        use_container_backend,
        container_image,
    })
}

/// Maximum number of distinct entity labels per `extract` request.
///
/// Pinned to the matching `MAX_ENTITY_LABELS` constant on the Python
/// side at
/// `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`.
/// Bumping either side requires bumping both: the Python validator
/// will reject inputs the Rust caller could otherwise generate.
pub const MAX_ENTITY_LABELS: usize = 64;

/// Maximum number of distinct relation labels per `extract` request.
/// Empty is valid and signals entity-only mode (no relations returned).
pub const MAX_RELATION_LABELS: usize = 64;

/// Maximum UTF-8 byte length of the `text` field.
pub const MAX_TEXT_BYTES: usize = 8192;

/// Wire shape of an `extract` request's `params`.
///
/// `threshold` and `max_entities` are optional on the wire (the Python
/// server applies defaults of 0.5 and 64). `relation_threshold` is
/// captured separately per spike correction #3 — the GLiNER-Relex
/// model is noisy at low thresholds and production callers should pass
/// ≥ 0.5 for relations to suppress dense candidate-triple noise from
/// overlapping entity subspans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractRequest {
    pub text: String,
    pub entity_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entities: Option<u32>,
}

/// Wire shape of an `extract` response's `result`.
///
/// `entities` carries top-level entity dicts (see [`Entity`]); `triples`
/// carries relations whose `head` and `tail` are *nested* entity refs
/// (see [`TripleEntity`]) — a deliberately different shape with `type`
/// instead of `label` and an `entity_idx` back-pointer, no nested
/// `score`. The smoke test on real `multi-v1.0` weights established
/// this naming (see `workers/gliner-relex/README.md` "Field-key naming
/// observed on real `multi-v1.0` output" for the table).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractResponse {
    pub entities: Vec<Entity>,
    pub triples: Vec<Triple>,
}

/// A top-level entity in [`ExtractResponse::entities`].
///
/// Distinct from [`TripleEntity`] because the upstream GLiNER-Relex
/// envelope uses different field names + a different field set for the
/// two positions: top-level entities carry `label` + `score`; nested
/// triple head/tail carry `type` + `entity_idx` (and no `score`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub text: String,
    pub label: String,
    pub start: u32,
    pub end: u32,
    pub score: f32,
}

/// A nested entity reference inside [`Triple::head`] / [`Triple::tail`].
///
/// Real `knowledgator/gliner-relex-multi-v1.0` output uses `type` (NOT
/// `label`) for the entity category and adds an `entity_idx`
/// back-pointer into the top-level [`ExtractResponse::entities`]
/// array. There is no per-position `score`; consumers wanting the
/// score look up `entities[entity_idx].score`. See
/// `workers/gliner-relex/README.md` "Field-key naming observed on
/// real `multi-v1.0` output" for the empirical confirmation (smoke
/// test 2026-05-18, fixed in `1c36f56`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TripleEntity {
    pub text: String,
    /// The entity type. Named `type` on the wire (matching upstream)
    /// but Rust requires the `r#` raw-identifier prefix for the
    /// keyword. Serde's `rename` keeps the wire side clean.
    #[serde(rename = "type")]
    pub r#type: String,
    pub start: u32,
    pub end: u32,
    /// Index back into the top-level [`ExtractResponse::entities`]
    /// array. Stable for a single response only.
    pub entity_idx: u32,
}

/// A relation triple in [`ExtractResponse::triples`].
///
/// Field names match upstream's [GLiNER-Relex inference envelope][gr]:
/// `head` and `tail` (NOT `subject` / `object`) carry full nested
/// entity dicts via [`TripleEntity`]; `relation` is the predicate
/// label; `score` is the model's confidence. See spike correction #2
/// at `docs/superpowers/specs/2026-05-18-gliner-relex-spike-notes.md`.
///
/// [gr]: https://github.com/urchade/GLiNER
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Triple {
    pub head: TripleEntity,
    pub tail: TripleEntity,
    pub relation: String,
    pub score: f32,
}

/// Typed client wrapping [`crate::tool_host::dispatch`] for the
/// gliner-relex worker's `extract` method.
///
/// One [`Client`] per daemon — holds the
/// [`Arc<dyn WorkerLifecycleManager>`][WorkerLifecycleManager] shared
/// with the step dispatcher (so the client lands on the SAME warm slot
/// that scheduled steps land on, when `entry.lifecycle ==
/// Lifecycle::IdleTimeout`), plus a snapshot of the worker's
/// [`ToolEntry`]. The entry is the same one registered in the tool
/// registry; cloning the manifest into the client avoids exposing the
/// registry's internals to non-dispatch callers.
///
/// ## Why this exists
///
/// Slice 2 deliberately did NOT ship a typed client (see this module's
/// header doc, "What this module deliberately does NOT own"). The v2
/// entity-extraction consumer slice (Task 11's `GlinerRelexExtractor`)
/// is the first non-dispatcher caller that needs to land an `extract`
/// request as a typed function call rather than wiring a `PlannedStep`
/// through the scheduler. This client is the chokepoint for that path
/// — it funnels every consumer through the same `acquire` →
/// `tool_host::dispatch` → crash-classify shape the step dispatcher
/// uses, so audit rows, warm-slot bookkeeping, and crash recovery all
/// behave identically.
///
/// ## What it does NOT do
///
/// - **No batching.** One [`extract`][Self::extract] call = one
///   JSON-RPC round trip. Higher-level batchers compose this client.
/// - **No retry on RPC errors.** `INVALID_INPUT` / `INFERENCE_FAILED`
///   are surfaced as [`ClientError::RpcError`] for the caller to
///   classify; the worker stays alive (per
///   [`dispatch_indicates_worker_dead`][cd]'s `Rpc(_)` → alive
///   classification).
/// - **No retry on worker death.** Crashes report through to the
///   lifecycle manager via
///   [`WorkerHandle::report_crash`][rc], which bumps the restart
///   backoff; the caller sees [`ClientError::WorkerDead`] and decides
///   whether to retry. This matches the step dispatcher's behaviour.
///
/// [cd]: crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead
/// [rc]: crate::worker_lifecycle::WorkerHandle::report_crash
pub struct Client {
    lifecycle: Arc<dyn WorkerLifecycleManager>,
    pool: PgPool,
    entry: ToolEntry,
    tool_name: &'static str,
}

impl Client {
    /// Logical tool name registered for the gliner-relex worker. This
    /// is the same string `core::main::build_gliner_relex_entry` uses
    /// when registering the entry in the [`ToolRegistry`][reg], so the
    /// warm-cache key in [`IdleTimeoutLifecycle`][itl] matches whether
    /// the call originates from the step dispatcher or this client.
    ///
    /// [reg]: crate::scheduler::ToolRegistry
    /// [itl]: crate::worker_lifecycle::IdleTimeoutLifecycle
    pub const TOOL_NAME: &'static str = "gliner-relex";

    /// Construct a client. Production callers (Task 15) pass the
    /// `Arc<dyn WorkerLifecycleManager>` shared with the step
    /// dispatcher and a snapshot of the registered [`ToolEntry`].
    pub fn new(
        lifecycle: Arc<dyn WorkerLifecycleManager>,
        pool: PgPool,
        entry: ToolEntry,
    ) -> Self {
        Self {
            lifecycle,
            pool,
            entry,
            tool_name: Self::TOOL_NAME,
        }
    }

    /// Single round-trip extract. Wraps acquire → dispatch → crash-
    /// classify → decode.
    ///
    /// The audit row for the dispatch is written automatically by
    /// [`tool_host::dispatch`]; the caller does not need to log
    /// anything separately for SQL-queryable history.
    ///
    /// On RPC-level errors (worker reachable, request rejected) the
    /// numeric `-32xxx` code is preserved in
    /// [`ClientError::RpcError`] so callers can branch on the
    /// wire-stable code (e.g. `-32001 INVALID_INPUT` retries are
    /// pointless; `-32003 INFERENCE_FAILED` retries may help).
    /// On worker-death errors (`Io`, `Protocol(EarlyExit|Io|Decode|IdMismatch)`)
    /// the lifecycle manager is notified via
    /// [`WorkerHandle::report_crash`][rc] before the error returns, so
    /// the next acquire on the same warm slot waits behind the
    /// restart-backoff.
    ///
    /// [rc]: crate::worker_lifecycle::WorkerHandle::report_crash
    pub async fn extract(
        &self,
        req: ExtractRequest,
    ) -> Result<ExtractResponse, ClientError> {
        let req_value = serde_json::to_value(&req)
            .map_err(|e| ClientError::EncodeError(e.to_string()))?;

        let mut handle = self
            .lifecycle
            .acquire(self.tool_name, &self.entry)
            .await
            .map_err(|e| ClientError::WorkerSpawnFailed(e.to_string()))?;

        let result = tool_host::dispatch(
            &self.pool,
            handle.worker_mut(),
            self.tool_name,
            "extract",
            req_value,
        )
        .await;

        // Crash classification — same chokepoint the step dispatcher
        // uses (`scheduler::tool_dispatch::ToolHostStepDispatcher::dispatch_step`).
        // Keeping the call here means warm-slot bookkeeping for client
        // calls and scheduler calls converges in `idle_timeout.rs`.
        if crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead(
            &result,
        ) {
            handle.report_crash();
        }

        match result {
            Ok(v) => serde_json::from_value::<ExtractResponse>(v)
                .map_err(|e| ClientError::DecodeError(e.to_string())),
            // RPC-level error: the worker is alive and rejected the
            // call. Preserve the wire-stable numeric code + message so
            // callers can branch on `-32001 INVALID_INPUT` /
            // `-32002 MODEL_LOAD_FAILED` / `-32003 INFERENCE_FAILED`
            // without re-parsing the message string.
            Err(ToolHostError::Protocol(ProtocolClientError::Rpc(rpc))) => {
                Err(ClientError::RpcError {
                    code: rpc.code,
                    message: rpc.message,
                })
            }
            // Everything else (Sandbox spawn failure already converted
            // above by the acquire arm; Io; Protocol(EarlyExit|Io|
            // Decode|IdMismatch)) means the worker is gone. The
            // crash-classifier already flipped `died = true` on the
            // handle so the lifecycle manager will not return it to
            // the warm slot.
            Err(e) => Err(ClientError::WorkerDead(e.to_string())),
        }
    }
}

/// Errors returned by [`Client::extract`].
///
/// Split into five disjoint variants so callers can branch without
/// stringly-typed matching:
///
/// - [`EncodeError`][Self::EncodeError]: serialising the
///   [`ExtractRequest`] to JSON failed. Practically unreachable —
///   `ExtractRequest`'s fields all serialise infallibly — but kept as
///   a typed variant rather than `unwrap()` so the failure surface is
///   explicit.
/// - [`WorkerSpawnFailed`][Self::WorkerSpawnFailed]: the lifecycle
///   manager's `acquire` returned an error (sandbox couldn't spawn,
///   restart-backoff still active, …). The worker never started for
///   this call.
/// - [`WorkerDead`][Self::WorkerDead]: dispatch returned an error
///   variant classified as "worker died" by
///   [`dispatch_indicates_worker_dead`][cd]
///   (Io / Protocol::{EarlyExit, Io, Decode, IdMismatch}).
///   [`Client::extract`] has already notified the handle via
///   [`report_crash`][rc] before returning this.
/// - [`RpcError`][Self::RpcError]: worker is alive and rejected the
///   call. The numeric `code` is wire-stable per the JSON-RPC error
///   table in the [worker README][readme].
/// - [`DecodeError`][Self::DecodeError]: dispatch succeeded but the
///   response did not deserialise into [`ExtractResponse`]. Indicates
///   a worker/client wire-shape drift bug.
///
/// [cd]: crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead
/// [rc]: crate::worker_lifecycle::WorkerHandle::report_crash
/// [readme]: https://github.com/hherb/hhagent/blob/main/workers/gliner-relex/README.md
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("encode error: {0}")]
    EncodeError(String),
    #[error("worker spawn failed: {0}")]
    WorkerSpawnFailed(String),
    #[error("worker dead mid-call: {0}")]
    WorkerDead(String),
    #[error("rpc error code={code}: {message}")]
    RpcError { code: i32, message: String },
    #[error("decode error: {0}")]
    DecodeError(String),
}

#[cfg(test)]
mod tests;
