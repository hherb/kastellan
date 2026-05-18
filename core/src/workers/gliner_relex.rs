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
    /// `auto` / `cuda` / `cpu`. `auto` lets the worker probe
    /// `torch.cuda.mem_get_info(0)` for >= 3 GiB free (per spike
    /// correction #4) and pick CUDA or fall back to CPU silently.
    /// `mps` is reserved for the macOS follow-up plan.
    pub device: String,
}

/// Construct the [`ToolEntry`] for the gliner-relex worker.
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
    // The venv uses an editable install (uv's default for hatchling
    // workspace projects); `.venv/.../_editable_impl_*.pth` points at
    // `<worker_dir>/src`. Mounting only `.venv` would let Python start
    // but fail on `from hhagent_worker_gliner_relex.__main__ import
    // main` with ModuleNotFoundError. Compute the sibling `src/` from
    // the documented `<worker_dir>/.venv` contract on `venv_dir` and
    // bind it read-only too.
    //
    // `Path::parent()` only returns `None` when the path is the root
    // `/` or a single relative component like `foo`. A `venv_dir` that
    // resolves to either is a wiring bug in the caller — daemon
    // startup walks `.venv/bin/<shim>` and the env-resolver always
    // anchors the venv path under at least one extra directory
    // (`HHAGENT_GLINER_RELEX_VENV_DIR` is required to be absolute by
    // the operator; the `HHAGENT_DATA_DIR` / `HOME` fallbacks tack on
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
        env: vec![
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
            // calls getpass.getuser() at module-import time, which
            // falls back to pwd.getpwuid(os.getuid()) when no
            // LOGNAME/USER/LNAME/USERNAME is set. The sandbox has no
            // /etc/passwd, so that fallback raises KeyError and the
            // worker exits before serving any RPC. Setting USER skips
            // the pwd lookup entirely (getpass picks the first
            // non-empty env var). The value is arbitrary; we use
            // "hhagent" as a marker that this is the worker, not a
            // real user account.
            ("USER".to_string(), "hhagent".to_string()),
            // TORCHINDUCTOR_CACHE_DIR pre-empts the home-dir cache
            // computation that triggers the getpass.getuser path
            // above (defense in depth — the USER env var alone is
            // sufficient today, but a future torch refactor could
            // re-route through getuid()). /tmp is tmpfs inside the
            // sandbox so this is ephemeral per-spawn; no leakage to
            // the host. Slice 2 doesn't use torch.compile so the
            // cache stays effectively empty.
            (
                "TORCHINDUCTOR_CACHE_DIR".to_string(),
                "/tmp/torchinductor".to_string(),
            ),
        ],
    };

    let lifecycle = Lifecycle::idle_timeout(
        IdleTimeoutCaps {
            idle_seconds: 600,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        },
        Contract { stateless: true },
    )
    .expect("manifest declares stateless = true; validator must accept");

    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: None,
        lifecycle,
    }
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

    Ok(GlinerRelexEnv {
        script_path,
        venv_dir,
        weights_dir,
        model_id,
        device,
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
mod tests {
    use super::*;

    #[test]
    fn extract_request_serialises_with_expected_keys() {
        let req = ExtractRequest {
            text: "Smith treats asthma.".to_string(),
            entity_labels: vec!["person".to_string(), "disease".to_string()],
            relation_labels: vec!["treats".to_string()],
            threshold: Some(0.5),
            relation_threshold: Some(0.5),
            max_entities: Some(64),
        };
        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();
        let keys: std::collections::BTreeSet<&str> =
            obj.keys().map(|s| s.as_str()).collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "text",
                "entity_labels",
                "relation_labels",
                "threshold",
                "relation_threshold",
                "max_entities",
            ]),
        );
    }

    #[test]
    fn extract_request_omits_optional_fields_when_none() {
        let req = ExtractRequest {
            text: "x".to_string(),
            entity_labels: vec!["x".to_string()],
            relation_labels: vec![],
            threshold: None,
            relation_threshold: None,
            max_entities: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("threshold"));
        assert!(!obj.contains_key("relation_threshold"));
        assert!(!obj.contains_key("max_entities"));
    }

    #[test]
    fn extract_response_round_trips_real_wire_shape() {
        // Sampled from the operator smoke test of 2026-05-18 against
        // real `knowledgator/gliner-relex-multi-v1.0` weights — the
        // shape that landed the install.sh + README fix (commit
        // `1c36f56`). Nested head/tail use `type` (not `label`) +
        // `entity_idx`; no nested `score`.
        let canned = serde_json::json!({
            "entities": [
                {"text": "Dr Smith", "label": "person",  "start": 0,  "end": 8,  "score": 0.999},
                {"text": "asthma",   "label": "disease", "start": 16, "end": 22, "score": 0.999}
            ],
            "triples":  [
                {
                    "head":     {"text": "Dr Smith", "type": "person",  "start": 0,  "end": 8,  "entity_idx": 0},
                    "tail":     {"text": "asthma",   "type": "disease", "start": 16, "end": 22, "entity_idx": 1},
                    "relation": "treats",
                    "score":    0.995
                }
            ],
        });
        let resp: ExtractResponse =
            serde_json::from_value(canned.clone()).expect("decode real wire shape");
        assert_eq!(resp.entities.len(), 2);
        assert_eq!(resp.entities[0].text, "Dr Smith");
        assert_eq!(resp.entities[0].label, "person");
        assert_eq!(resp.triples[0].head.text, "Dr Smith");
        // CRITICAL: nested head/tail use `type`, not `label`. If a
        // future refactor renames `TripleEntity::r#type` to `label`,
        // this assertion would still compile but the from_value above
        // would fail to decode.
        assert_eq!(resp.triples[0].head.r#type, "person");
        assert_eq!(resp.triples[0].head.entity_idx, 0);
        assert_eq!(resp.triples[0].relation, "treats");
        // Round-trip back through Rust types is shape-identical
        // (`PartialEq` on the structs). We don't compare against the
        // raw `canned` Value: f32 → JSON Number → f32 widens through
        // the json::Number f64 carrier (`0.999_f32` round-trips as
        // `0.9990000128746033`), which is a serde_json artifact, not
        // a real shape drift. The decode-then-decode equality below
        // catches every field-rename or field-add bug we care about.
        let re_serialised = serde_json::to_value(&resp).unwrap();
        let resp_again: ExtractResponse = serde_json::from_value(re_serialised).unwrap();
        assert_eq!(resp, resp_again);
    }

    #[test]
    fn label_caps_match_python_side() {
        // Pinned at the values used by the Python validators (see
        // workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py
        // MAX_TEXT_BYTES / MAX_ENTITY_LABELS / MAX_RELATION_LABELS).
        // A drift here would let the Rust caller generate inputs the
        // Python side immediately rejects with INVALID_INPUT.
        assert_eq!(MAX_ENTITY_LABELS, 64);
        assert_eq!(MAX_RELATION_LABELS, 64);
        assert_eq!(MAX_TEXT_BYTES, 8192);
    }

    /// Shared test fixture: a GlinerRelexEnv pointing at /tmp paths
    /// that won't actually be touched (the manifest constructor is
    /// pure — no filesystem access). Path strings are visible in
    /// assertions below so a refactor that changes them gets caught.
    fn test_env() -> GlinerRelexEnv {
        GlinerRelexEnv {
            script_path: PathBuf::from("/tmp/fake/.venv/bin/hhagent-worker-gliner-relex"),
            venv_dir: PathBuf::from("/tmp/fake/.venv"),
            weights_dir: PathBuf::from("/tmp/fake/weights/multi-v1.0"),
            model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
            device: "auto".to_string(),
        }
    }

    #[test]
    fn entry_carries_idle_timeout_lifecycle_with_spec_caps() {
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        match entry.lifecycle {
            Lifecycle::IdleTimeout { caps, contract } => {
                assert!(
                    contract.stateless,
                    "must declare stateless=true for idle_timeout"
                );
                assert_eq!(caps.idle_seconds, 600);
                assert_eq!(caps.max_requests, 10_000);
                assert_eq!(caps.max_age_seconds, 86_400);
                assert_eq!(caps.grace_period_seconds, 5);
            }
            Lifecycle::SingleUse => panic!("expected IdleTimeout, got SingleUse"),
        }
    }

    #[test]
    fn entry_disables_per_request_kill_switches_for_warm_worker() {
        // The two knobs that are *deliberately* off for warm workers
        // — see the design spec + the per-field rationale on
        // gliner_relex_entry. Pinning here so a future "harden the
        // worker" pass doesn't quietly re-enable either without an
        // explicit revisit of the lifecycle semantics.
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        assert_eq!(
            entry.policy.cpu_ms, 0,
            "cpu_ms must be 0; RLIMIT_CPU is cumulative and would fire across many warm calls"
        );
        assert!(
            entry.wall_clock_ms.is_none(),
            "wall_clock_ms must be None; lifecycle.max_age_seconds is the rotation budget"
        );
    }

    #[test]
    fn entry_denies_network() {
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        match entry.policy.net {
            Net::Deny => {}
            other => panic!("expected Net::Deny, got {other:?}"),
        }
    }

    #[test]
    fn entry_uses_strict_profile() {
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        match entry.policy.profile {
            Profile::WorkerStrict => {}
            other => panic!("expected Profile::WorkerStrict, got {other:?}"),
        }
    }

    #[test]
    fn entry_mounts_weights_and_venv_and_src_read_only_no_writes() {
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        assert!(
            entry.policy.fs_read.contains(&env.weights_dir),
            "weights dir must be in fs_read so the model can load"
        );
        assert!(
            entry.policy.fs_read.contains(&env.venv_dir),
            "venv dir must be in fs_read so the Python interpreter + site-packages are visible"
        );
        // Editable-install source dir, computed as <worker_dir>/src
        // where <worker_dir> == venv_dir.parent(). The venv ships a
        // `.pth` file that points Python here; without the mount, the
        // worker fails to import its own package inside the sandbox.
        let expected_src = env
            .venv_dir
            .parent()
            .expect("test_env venv_dir has a parent")
            .join("src");
        assert!(
            entry.policy.fs_read.contains(&expected_src),
            "editable-install src dir must be in fs_read; got {:?}",
            entry.policy.fs_read
        );
        assert!(
            entry.policy.fs_write.is_empty(),
            "stateless worker writes nothing; fs_write must stay empty"
        );
    }

    #[test]
    fn entry_carries_offline_and_routing_env_vars() {
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        // Build a map view; the order in the Vec<(K, V)> is incidental.
        let env_map: std::collections::HashMap<&str, &str> = entry
            .policy
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        assert_eq!(env_map.get("HF_HUB_OFFLINE"), Some(&"1"));
        assert_eq!(env_map.get("TRANSFORMERS_OFFLINE"), Some(&"1"));
        assert_eq!(
            env_map.get("HHAGENT_GLINER_RELEX_MODEL"),
            Some(&env.model_id.as_str())
        );
        assert_eq!(
            env_map.get("HHAGENT_GLINER_RELEX_DEVICE"),
            Some(&env.device.as_str())
        );
        // The weights path is plumbed via env so the worker's
        // __main__.py knows where to load from. Compare the stringified
        // form because the policy env stores `String`, not `PathBuf`.
        let expected_weights = env.weights_dir.to_string_lossy().into_owned();
        assert_eq!(
            env_map.get("HHAGENT_GLINER_RELEX_WEIGHTS_DIR"),
            Some(&expected_weights.as_str())
        );
        // USER + TORCHINDUCTOR_CACHE_DIR are sandbox-hygiene shims
        // that keep PyTorch's _dynamo import from blowing up on the
        // missing /etc/passwd. See the long comment on
        // gliner_relex_entry for the failure mode they avoid.
        assert!(
            env_map.contains_key("USER"),
            "USER env var must be set; otherwise getpass.getuser() in torch._dynamo crashes on missing /etc/passwd"
        );
        assert_eq!(
            env_map.get("TORCHINDUCTOR_CACHE_DIR"),
            Some(&"/tmp/torchinductor")
        );
    }

    #[test]
    fn entry_sets_cgroup_ceilings_for_warm_inference() {
        // cpu_quota_pct=400 (4 CPUs) and tasks_max=64 are
        // worker-specific defaults; explicit pin so a global default
        // tweak doesn't silently widen what the gliner-relex worker
        // gets.
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        assert_eq!(entry.policy.cpu_quota_pct, Some(400));
        assert_eq!(entry.policy.tasks_max, Some(64));
        assert_eq!(
            entry.policy.mem_mb, 4_096,
            "4 GiB sized for multi-v1.0; large-v0.5 operators must bump"
        );
    }

    #[test]
    fn entry_binary_points_at_the_venv_shim() {
        let env = test_env();
        let entry = gliner_relex_entry(&env);
        assert_eq!(entry.binary, env.script_path);
    }

    // ---- resolve_env unit tests --------------------------------------
    //
    // The resolver is the pure core of `core::main::build_gliner_relex_entry`.
    // Tests pass in-memory env-var + filesystem fakes so every skip-register
    // branch is reachable without touching the process environment or the
    // real filesystem. Production behaviour is exercised by the e2e tests
    // in `core/tests/gliner_relex_e2e.rs`.

    use std::collections::{HashMap, HashSet};

    /// Build an env-lookup closure backed by a fixed map.
    fn env_map_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Match-anything fs predicate (every path exists / is a dir).
    fn always_true(_: &Path) -> bool {
        true
    }

    /// Match-nothing fs predicate (no path exists / is a dir).
    fn always_false(_: &Path) -> bool {
        false
    }

    #[test]
    fn resolve_env_disabled_when_enable_unset() {
        let env = env_map_of(&[]);
        let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
        assert_eq!(r, Err(ResolveSkipReason::Disabled));
    }

    #[test]
    fn resolve_env_disabled_when_enable_is_zero_or_truthy_alias() {
        for v in ["0", "true", "yes", "on", ""] {
            let env = env_map_of(&[("HHAGENT_GLINER_RELEX_ENABLE", v)]);
            let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
            assert_eq!(
                r,
                Err(ResolveSkipReason::Disabled),
                "enable={v:?} must be Disabled (strict on the value, only \"1\" enables)"
            );
        }
    }

    #[test]
    fn resolve_env_trims_whitespace_on_enable() {
        // Common operator footgun: `echo "1" > /etc/hhagent/env` yields
        // a value ending in `\n`. The README documents `=1` but trimming
        // is cheap insurance.
        let env = env_map_of(&[
            ("HHAGENT_GLINER_RELEX_ENABLE", " 1\n"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
            ("HHAGENT_DATA_DIR", "/srv/data"),
        ]);
        let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
        assert!(
            matches!(r, Ok(_)),
            "trimmed \" 1\\n\" must be accepted, got {r:?}"
        );
    }

    #[test]
    fn resolve_env_returns_weights_env_missing() {
        let env = env_map_of(&[("HHAGENT_GLINER_RELEX_ENABLE", "1")]);
        let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
        assert_eq!(r, Err(ResolveSkipReason::WeightsDirEnvMissing));
    }

    #[test]
    fn resolve_env_returns_weights_dir_not_a_dir() {
        let env = env_map_of(&[
            ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/missing"),
            ("HHAGENT_DATA_DIR", "/srv/data"),
        ]);
        let r = resolve_env(|k| env.get(k).cloned(), always_false, always_true);
        match r {
            Err(ResolveSkipReason::WeightsDirNotADir { path }) => {
                assert_eq!(path, PathBuf::from("/srv/missing"));
            }
            other => panic!("expected WeightsDirNotADir, got {other:?}"),
        }
    }

    #[test]
    fn resolve_env_returns_venv_unresolvable_when_no_anchor() {
        // Enable + weights set + dir exists, but none of the three venv
        // anchors set. Pre-refactor this would silently fall through to
        // `/tmp/.local/share/hhagent/...`; now it surfaces a structured
        // skip reason so the operator log says exactly what's missing.
        let env = env_map_of(&[
            ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
        ]);
        let r = resolve_env(|k| env.get(k).cloned(), always_true, always_true);
        assert_eq!(r, Err(ResolveSkipReason::VenvDirUnresolvable));
    }

    #[test]
    fn resolve_env_returns_script_shim_missing() {
        // Weights dir exists but the venv shim doesn't (operator
        // staged the weights but forgot `uv sync`).
        let env = env_map_of(&[
            ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
            ("HHAGENT_GLINER_RELEX_VENV_DIR", "/opt/glr/.venv"),
        ]);
        // weights dir is a dir; script doesn't exist.
        let r = resolve_env(
            |k| env.get(k).cloned(),
            |p| p == Path::new("/srv/weights"),
            always_false,
        );
        match r {
            Err(ResolveSkipReason::ScriptShimMissing { path }) => {
                assert_eq!(
                    path,
                    PathBuf::from("/opt/glr/.venv/bin/hhagent-worker-gliner-relex")
                );
            }
            other => panic!("expected ScriptShimMissing, got {other:?}"),
        }
    }

    #[test]
    fn resolve_env_happy_path_explicit_venv_dir_wins() {
        // Explicit `HHAGENT_GLINER_RELEX_VENV_DIR` must override the
        // `HHAGENT_DATA_DIR`-derived default, even when both are set.
        let env = env_map_of(&[
            ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
            ("HHAGENT_GLINER_RELEX_VENV_DIR", "/opt/explicit/.venv"),
            ("HHAGENT_DATA_DIR", "/srv/data"),
        ]);
        let exists_paths: HashSet<PathBuf> = ["/srv/weights", "/opt/explicit/.venv/bin/hhagent-worker-gliner-relex"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let r = resolve_env(
            |k| env.get(k).cloned(),
            |p| exists_paths.contains(p),
            |p| exists_paths.contains(p),
        )
        .expect("happy path");
        assert_eq!(r.venv_dir, PathBuf::from("/opt/explicit/.venv"));
        assert_eq!(
            r.script_path,
            PathBuf::from("/opt/explicit/.venv/bin/hhagent-worker-gliner-relex")
        );
        assert_eq!(r.weights_dir, PathBuf::from("/srv/weights"));
        assert_eq!(r.model_id, "knowledgator/gliner-relex-multi-v1.0");
        assert_eq!(r.device, "auto");
    }

    #[test]
    fn resolve_env_happy_path_uses_hhagent_data_dir() {
        let env = env_map_of(&[
            ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
            ("HHAGENT_DATA_DIR", "/srv/data"),
            ("HHAGENT_GLINER_RELEX_MODEL", "knowledgator/gliner-relex-large-v0.5"),
            ("HHAGENT_GLINER_RELEX_DEVICE", "cuda"),
        ]);
        let exists_paths: HashSet<PathBuf> = [
            "/srv/weights",
            "/srv/data/workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let r = resolve_env(
            |k| env.get(k).cloned(),
            |p| exists_paths.contains(p),
            |p| exists_paths.contains(p),
        )
        .expect("happy path");
        assert_eq!(r.venv_dir, PathBuf::from("/srv/data/workers/gliner-relex/.venv"));
        assert_eq!(r.model_id, "knowledgator/gliner-relex-large-v0.5");
        assert_eq!(r.device, "cuda");
    }

    #[test]
    fn resolve_env_happy_path_home_fallback_when_no_data_dir() {
        let env = env_map_of(&[
            ("HHAGENT_GLINER_RELEX_ENABLE", "1"),
            ("HHAGENT_GLINER_RELEX_WEIGHTS_DIR", "/srv/weights"),
            ("HOME", "/home/op"),
        ]);
        let exists_paths: HashSet<PathBuf> = [
            "/srv/weights",
            "/home/op/.local/share/hhagent/workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let r = resolve_env(
            |k| env.get(k).cloned(),
            |p| exists_paths.contains(p),
            |p| exists_paths.contains(p),
        )
        .expect("happy path");
        assert_eq!(
            r.venv_dir,
            PathBuf::from("/home/op/.local/share/hhagent/workers/gliner-relex/.venv")
        );
    }

    #[test]
    fn client_error_display_pins_format() {
        // The `Display` impl is wire-stable: operator-facing log
        // messages and audit-row error strings rely on these exact
        // forms. A refactor that shuffles the `#[error(...)]`
        // attributes will trip these assertions before it can land.
        let e = ClientError::EncodeError("bad json".into());
        assert_eq!(e.to_string(), "encode error: bad json");

        let e = ClientError::WorkerSpawnFailed("no venv".into());
        assert_eq!(e.to_string(), "worker spawn failed: no venv");

        let e = ClientError::WorkerDead("EOF".into());
        assert_eq!(e.to_string(), "worker dead mid-call: EOF");

        let e = ClientError::RpcError {
            code: -32001,
            message: "INVALID_INPUT".into(),
        };
        assert_eq!(e.to_string(), "rpc error code=-32001: INVALID_INPUT");

        let e = ClientError::DecodeError("not an ExtractResponse".into());
        assert_eq!(e.to_string(), "decode error: not an ExtractResponse");
    }

    #[test]
    fn client_error_variants_are_distinct() {
        // Compile-time exhaustiveness pin: every variant must be
        // reachable by an explicit arm. If a future variant is added
        // to `ClientError` without updating this classifier, the
        // build fails with a non-exhaustive-match error — forcing the
        // caller-side branch logic to be revisited.
        fn classify(e: &ClientError) -> &'static str {
            match e {
                ClientError::EncodeError(_) => "encode",
                ClientError::WorkerSpawnFailed(_) => "spawn",
                ClientError::WorkerDead(_) => "dead",
                ClientError::RpcError { .. } => "rpc",
                ClientError::DecodeError(_) => "decode",
            }
        }
        assert_eq!(classify(&ClientError::EncodeError("x".into())), "encode");
        assert_eq!(
            classify(&ClientError::WorkerSpawnFailed("x".into())),
            "spawn"
        );
        assert_eq!(classify(&ClientError::WorkerDead("x".into())), "dead");
        assert_eq!(
            classify(&ClientError::RpcError {
                code: 0,
                message: "x".into()
            }),
            "rpc"
        );
        assert_eq!(classify(&ClientError::DecodeError("x".into())), "decode");
    }
}
