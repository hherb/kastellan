//! Environment resolution for the gliner-relex worker.
//!
//! [`resolve_env`] is the pure core: it maps a generic env-lookup +
//! filesystem predicates onto either a populated [`GlinerRelexEnv`] or a
//! structured [`ResolveSkipReason`]. The daemon passes [`std::env::var`],
//! [`Path::is_dir`], and [`Path::exists`]; tests pass in-memory fakes to
//! exercise each skip-register branch without touching the process
//! environment or filesystem.

use std::path::{Path, PathBuf};

/// Resolved paths + config for the GLiNER-Relex worker.
///
/// Populated by `GlinerRelexManifest::resolve` from environment variables
/// (via [`resolve_env`]) and passed into
/// [`gliner_relex_entry`](super::entry::gliner_relex_entry) to build the
/// manifest's [`crate::scheduler::ToolEntry`].
///
/// Production callers should construct this via the daemon helper;
/// tests build it directly to pin manifest shape without touching the
/// real filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlinerRelexEnv {
    /// Absolute path to the uv-generated console-script shim:
    /// `<worker_dir>/.venv/bin/kastellan-worker-gliner-relex`. This is
    /// the binary the dispatcher spawns under sandbox; `pyproject.toml`
    /// declares `[project.scripts] kastellan-worker-gliner-relex` so
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
    /// via `KASTELLAN_GLINER_RELEX_DEVICE`; per-platform legality is
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
    /// True when the operator set `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`
    /// (strict: only `"1"` after trim counts).
    /// [`gliner_relex_entry`](super::entry::gliner_relex_entry) branches
    /// on this field to emit the container-mode `ToolEntry` shape
    /// (in-container binary, weights-only `fs_read`,
    /// `sandbox_backend = Some(Container)`, `container_image` populated)
    /// instead of the host-mode one.
    ///
    /// In container mode `resolve_env` also skips the host-venv
    /// existence check — the worker shim lives inside the image at
    /// `/usr/local/bin/kastellan-worker-gliner-relex`, so requiring a
    /// host venv would be a footgun for container-mode-only operators.
    pub use_container_backend: bool,
    /// Operator-supplied container image tag override, read from
    /// `KASTELLAN_GLINER_RELEX_IMAGE`. `None` (default) falls back to
    /// the `CONTAINER_IMAGE_DEFAULT` constant at the
    /// `gliner_relex_entry` callsite. Symmetric to
    /// `KASTELLAN_GLINER_RELEX_MODEL` override behaviour.
    ///
    /// This field is **orthogonal to `use_container_backend`** — the
    /// env var is read unconditionally so that operators can stage an
    /// image tag preference ahead of flipping `USE_CONTAINER=1`. In
    /// host mode the value is carried on the struct but ignored at
    /// `ToolEntry` construction (only `container_mode_entry` reads it).
    /// Setting `IMAGE=` alone (without `USE_CONTAINER=1`) does NOT
    /// switch the worker to container mode.
    pub container_image: Option<String>,
    /// Host-mode only: the real interpreter prefix the venv's `bin/python3`
    /// symlinks to, when it lives **outside** `venv_dir` (a uv venv symlinks
    /// to a base CPython whose `libpython` + stdlib are external). Mounted
    /// read-only so the interpreter starts inside the jail. `None` for a
    /// self-contained venv (or container mode). Populated by the manifest via
    /// [`resolve_host_interpreter_binds`] (NOT by [`resolve_env`], which has no
    /// other `canonicalize` need) — same external-interpreter binding the
    /// browser-driver worker does.
    pub interpreter_root: Option<PathBuf>,
    /// Host-mode only: read-only directories of the interpreter's out-of-prefix
    /// shared-library dependencies (e.g. a Homebrew `libintl` a pyenv/Homebrew
    /// CPython links). Bound so the interpreter can dyld-load inside the jail —
    /// without them it SIGABRTs before the worker runs (issue #284). Empty when
    /// the interpreter is self-contained / all-system, the dep tool is
    /// unavailable, or in container mode.
    pub interpreter_lib_dirs: Vec<PathBuf>,
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
    /// `KASTELLAN_GLINER_RELEX_ENABLE` is unset, empty, or anything other
    /// than `"1"` (after trim). This is the production default — every
    /// deployment that hasn't run `scripts/workers/gliner-relex/install.sh`
    /// and explicitly enabled the worker lands here.
    Disabled,
    /// `KASTELLAN_GLINER_RELEX_ENABLE=1` but
    /// `KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` is unset.
    WeightsDirEnvMissing,
    /// `KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` is set but the path doesn't
    /// resolve to a directory on disk at daemon-startup time.
    WeightsDirNotADir { path: PathBuf },
    /// None of `KASTELLAN_GLINER_RELEX_VENV_DIR`, `KASTELLAN_DATA_DIR`, or
    /// `HOME` is set — there is no anchor to default the venv path
    /// against. This is the failure mode that previously fell through
    /// to `/tmp` silently; surfacing it explicitly so the operator log
    /// shows the misconfiguration.
    VenvDirUnresolvable,
    /// Resolved `<venv_dir>/bin/kastellan-worker-gliner-relex` doesn't
    /// exist on disk.
    ScriptShimMissing { path: PathBuf },
}

/// Resolve a [`GlinerRelexEnv`] from a generic env lookup + filesystem
/// predicates.
///
/// This is the pure core wrapped by `GlinerRelexManifest::resolve`. The
/// daemon passes [`std::env::var`] + [`Path::is_dir`] + [`Path::exists`];
/// tests pass in-memory fakes to exercise each skip-register branch
/// without touching the process environment or filesystem.
///
/// Env vars consulted (same names + semantics as the production helper):
///
/// - `KASTELLAN_GLINER_RELEX_ENABLE` — must be `"1"` (whitespace-trimmed)
///   to register the worker. Anything else (unset / `0` / `true` / `on`)
///   returns [`ResolveSkipReason::Disabled`].
/// - `KASTELLAN_GLINER_RELEX_WEIGHTS_DIR` — required; absolute path to the
///   model snapshot.
/// - `KASTELLAN_GLINER_RELEX_MODEL` — optional; default
///   `knowledgator/gliner-relex-multi-v1.0`.
/// - `KASTELLAN_GLINER_RELEX_DEVICE` — optional; default `auto`.
/// - `KASTELLAN_GLINER_RELEX_USE_CONTAINER` — optional; `"1"` (strict,
///   whitespace-trimmed) opts into container mode. Anything else (unset /
///   `0` / `true` / `on`) uses host mode (the default). In container mode
///   the venv-anchor cascade below is skipped — the worker shim lives
///   inside the container image at `/usr/local/bin/...`.
/// - `KASTELLAN_GLINER_RELEX_IMAGE` — optional container image tag override;
///   `None` defers to the `CONTAINER_IMAGE_DEFAULT` constant at the
///   `gliner_relex_entry` callsite (added in Task 6).
/// - `KASTELLAN_GLINER_RELEX_VENV_DIR` — optional; if set, used verbatim.
/// - `KASTELLAN_DATA_DIR` — optional anchor for the venv default
///   (`<data>/workers/gliner-relex/.venv`).
/// - `HOME` — last-resort anchor (`<home>/.local/share/kastellan/...`).
///   If neither `KASTELLAN_DATA_DIR` nor `HOME` is set and the operator
///   didn't pass `KASTELLAN_GLINER_RELEX_VENV_DIR`, returns
///   [`ResolveSkipReason::VenvDirUnresolvable`] rather than silently
///   defaulting to `/tmp` — that earlier silent fallback hid
///   misconfiguration on minimal-env hosts (containers, system
///   services) where the operator usually meant `KASTELLAN_DATA_DIR` to
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
    let enable = env_lookup("KASTELLAN_GLINER_RELEX_ENABLE").unwrap_or_default();
    // `trim` so a stray newline from `echo "1" > envfile` doesn't fail
    // the opt-in silently. Strict on the value itself: only `"1"`
    // counts. Inviting `true` / `yes` / `on` would surface the next
    // operator's dialect debate; the README documents `=1` explicitly.
    if enable.trim() != "1" {
        return Err(ResolveSkipReason::Disabled);
    }

    let weights_dir = match env_lookup("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR") {
        Some(v) => PathBuf::from(v),
        None => return Err(ResolveSkipReason::WeightsDirEnvMissing),
    };
    if !is_dir(&weights_dir) {
        return Err(ResolveSkipReason::WeightsDirNotADir { path: weights_dir });
    }

    let model_id = env_lookup("KASTELLAN_GLINER_RELEX_MODEL")
        .unwrap_or_else(|| "knowledgator/gliner-relex-multi-v1.0".to_string());
    let device = env_lookup("KASTELLAN_GLINER_RELEX_DEVICE")
        .unwrap_or_else(|| "auto".to_string());

    // New env knobs (Slice 2.5):
    //   * `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1` → container-mode (strict on "1").
    //   * `KASTELLAN_GLINER_RELEX_IMAGE=<tag>` → operator-supplied image override.
    //
    // Container mode is macOS-only — the Apple `container` micro-VM
    // backend (`SandboxBackendKind::Container`) doesn't exist on Linux.
    // On non-macOS targets the flag is forced `false` at compile time so
    // the build never references the macOS-only variant (issue #144); an
    // operator who sets `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1` on Linux
    // gets host-mode + bwrap silently (the env var isn't even read).
    #[cfg(target_os = "macos")]
    let use_container_backend = env_lookup("KASTELLAN_GLINER_RELEX_USE_CONTAINER")
        .map(|v| {
            // trim() rationale: matches KASTELLAN_GLINER_RELEX_ENABLE strictness; see comment above.
            v.trim() == "1"
        })
        .unwrap_or(false);
    #[cfg(not(target_os = "macos"))]
    let use_container_backend = false;
    let container_image = env_lookup("KASTELLAN_GLINER_RELEX_IMAGE");

    // Host venv resolution is skipped in container mode — the worker
    // shim lives inside the image, so no host venv is required.
    let (venv_dir, script_path) = if use_container_backend {
        (PathBuf::new(), PathBuf::new())
    } else {
        // Anchor priority: explicit override > data-dir > home. No
        // `/tmp` fallback — see ResolveSkipReason::VenvDirUnresolvable
        // for the rationale.
        let venv_dir = if let Some(v) = env_lookup("KASTELLAN_GLINER_RELEX_VENV_DIR") {
            PathBuf::from(v)
        } else if let Some(data_dir) = env_lookup("KASTELLAN_DATA_DIR") {
            PathBuf::from(data_dir).join("workers/gliner-relex/.venv")
        } else if let Some(home) = env_lookup("HOME") {
            PathBuf::from(home)
                .join(".local/share/kastellan/workers/gliner-relex/.venv")
        } else {
            return Err(ResolveSkipReason::VenvDirUnresolvable);
        };
        let script_path = venv_dir.join("bin").join("kastellan-worker-gliner-relex");
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
        // Interpreter binds are resolved by the manifest (host mode only) —
        // they need `canonicalize` + the otool/ldd dep tool, which this pure
        // env resolver doesn't take. Default to "nothing extra" here.
        interpreter_root: None,
        interpreter_lib_dirs: Vec::new(),
    })
}

/// Resolve the host-mode interpreter binds for the worker venv (issue #284).
///
/// `uv` creates the worker venv by symlinking `bin/python3` to a base
/// interpreter; that interpreter's `libpython`, stdlib, and any out-of-prefix
/// shared libraries (e.g. a Homebrew `libintl`) live **outside** the venv, so
/// binding only `.venv` leaves CPython unable to dyld-load inside the jail.
/// Returns `(interpreter_root, interpreter_lib_dirs)` for
/// [`GlinerRelexEnv`]:
///
/// * `interpreter_root` — the external interpreter prefix to bind read-only;
///   `None` for a self-contained venv. (On Linux the base python lives under
///   `/usr`, which bwrap already binds — it is still surfaced here; binding it
///   again is a harmless redundancy, exactly as the browser-driver worker does.)
/// * `interpreter_lib_dirs` — out-of-prefix shared-lib dirs the interpreter
///   links; empty when self-contained / all-system, or the dep tool is
///   unavailable (fail-safe — the manual `*_EXTRA_FS_READ` hatch backstops).
///
/// Delegates to the shared [`crate::workers::interpreter_deps`] helpers so the
/// "where's the real interpreter + what does it link" logic is byte-identical
/// to the browser-driver worker. Pure: `exists`, `canonicalize`, and
/// `resolve_deps` are injected. Call for host mode only — container-mode workers
/// bake the interpreter into the image.
pub fn resolve_host_interpreter_binds(
    venv_dir: &Path,
    exists: impl Fn(&Path) -> bool,
    canonicalize: impl Fn(&Path) -> Option<PathBuf>,
    resolve_deps: impl Fn(&Path) -> Vec<PathBuf>,
) -> (Option<PathBuf>, Vec<PathBuf>) {
    let interpreter_root = crate::workers::interpreter_deps::resolve_interpreter_root(
        venv_dir,
        &exists,
        &canonicalize,
    );
    let interpreter_lib_dirs = crate::workers::interpreter_deps::interpreter_lib_dirs(
        venv_dir,
        interpreter_root.as_deref(),
        &exists,
        &canonicalize,
        &resolve_deps,
    );
    (interpreter_root, interpreter_lib_dirs)
}
