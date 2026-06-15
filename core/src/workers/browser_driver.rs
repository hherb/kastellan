//! Host-side manifest for the browser-driver worker (slice #1).
//!
//! A Playwright-Python worker (opt-in via `KASTELLAN_BROWSER_DRIVER_ENABLE=1`)
//! exposing `browser.render`. [`resolve_env`] is the pure core (env + fs probes
//! → [`BrowserDriverEnv`] | [`ResolveSkipReason`]); [`browser_driver_entry`]
//! builds the [`ToolEntry`]. Under the default force-routed deployment the
//! worker runs in a private netns reaching the net only via its per-worker
//! egress sidecar (in-jail loopback-TCP↔UDS shim + transparent tunnel —
//! egress slice #2); force-routing is OFF in dev, where the worker runs
//! direct-net. The real browser launch lives in the Python worker; the prelude
//! seccomp/Landlock additions + real-sandbox e2e land in the Phase-2 plan
//! (their exact shape was settled by the spike — see the design spec §3.1).

use std::path::{Path, PathBuf};

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{Resolution, ResolveCtx, WorkerManifest};

/// Tool name the registry/planner keys browser-driver on.
const TOOL_NAME: &str = "browser-driver";
/// uv console-script shim name (`<venv>/bin/<SHIM_NAME>`).
const SHIM_NAME: &str = "kastellan-worker-browser-driver";

/// Resolved config for the browser-driver worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserDriverEnv {
    /// Absolute path to the console-script shim the dispatcher spawns.
    pub script_path: PathBuf,
    /// Worker venv root, mounted read-only into the jail.
    pub venv_dir: PathBuf,
    /// Real interpreter prefix root (e.g. `~/.pyenv/versions/3.12.3` or
    /// `/usr`), when the venv's `python3` symlinks to a CPython whose
    /// `libpython`/stdlib live **outside** `venv_dir`. Mounted read-only so the
    /// interpreter starts inside the jail (the spike's `py_root` finding §3.1;
    /// mirrors `python-exec`'s interpreter binding). `None` for a fully
    /// self-contained venv (nothing extra to bind).
    pub interpreter_root: Option<PathBuf>,
    /// Read-only directories of the interpreter's out-of-prefix shared-library
    /// dependencies (e.g. a Homebrew `libintl` dir a pyenv CPython links). Bound
    /// so the interpreter can dyld-load inside the jail — without them it
    /// SIGABRTs before the worker runs (issue #284). Empty when the interpreter
    /// is self-contained or the dep tool is unavailable.
    pub interpreter_lib_dirs: Vec<PathBuf>,
    /// Operator-supplied extra read-only paths
    /// (`KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ`, a JSON array of absolute
    /// paths). An escape hatch for host-specific dependencies the resolver
    /// can't infer — e.g. a non-self-contained interpreter that links a system
    /// library outside its prefix (a pyenv CPython built against Homebrew
    /// `/opt/homebrew/...`), or extra font dirs. Empty by default.
    pub extra_fs_read: Vec<PathBuf>,
}

/// Reason the resolver returned no entry (mirrors GLiNER's skip taxonomy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveSkipReason {
    /// `KASTELLAN_BROWSER_DRIVER_ENABLE` is unset/empty/anything but `"1"`.
    Disabled,
    /// None of `KASTELLAN_BROWSER_DRIVER_VENV_DIR`, `KASTELLAN_DATA_DIR`, or
    /// `HOME` is set — no anchor to default the venv path against.
    VenvDirUnresolvable,
    /// Resolved `<venv>/bin/kastellan-worker-browser-driver` is absent on disk.
    ScriptShimMissing { path: PathBuf },
}

/// Pure resolver: ENABLE gate + venv-anchor cascade + shim existence +
/// interpreter-root resolution.
///
/// `is_dir` is unused today (browser-driver has no weights dir like GLiNER) but
/// kept in the signature so the manifest can thread the same `ResolveCtx`
/// probes uniformly. `canonicalize` resolves the venv's `python3` symlink to
/// the real interpreter so its prefix can be bound into the jail (see
/// [`BrowserDriverEnv::interpreter_root`]).
pub fn resolve_env<E, D, X, C, R>(
    env_lookup: E,
    _is_dir: D,
    exists: X,
    canonicalize: C,
    resolve_deps: R,
) -> Result<BrowserDriverEnv, ResolveSkipReason>
where
    E: Fn(&str) -> Option<String>,
    D: Fn(&Path) -> bool,
    X: Fn(&Path) -> bool,
    C: Fn(&Path) -> Option<PathBuf>,
    R: Fn(&Path) -> Vec<PathBuf>,
{
    // `trim` so a stray newline from `echo "1" > envfile` doesn't fail the
    // opt-in silently. Strict on the value: only `"1"` counts.
    if env_lookup("KASTELLAN_BROWSER_DRIVER_ENABLE")
        .unwrap_or_default()
        .trim()
        != "1"
    {
        return Err(ResolveSkipReason::Disabled);
    }

    // Anchor priority: explicit override > data-dir > home. No `/tmp` fallback.
    let venv_dir = if let Some(v) = env_lookup("KASTELLAN_BROWSER_DRIVER_VENV_DIR") {
        PathBuf::from(v)
    } else if let Some(d) = env_lookup("KASTELLAN_DATA_DIR") {
        PathBuf::from(d).join("workers/browser-driver/.venv")
    } else if let Some(h) = env_lookup("HOME") {
        PathBuf::from(h).join(".local/share/kastellan/workers/browser-driver/.venv")
    } else {
        return Err(ResolveSkipReason::VenvDirUnresolvable);
    };
    let script_path = venv_dir.join("bin").join(SHIM_NAME);
    if !exists(&script_path) {
        return Err(ResolveSkipReason::ScriptShimMissing { path: script_path });
    }
    let interpreter_root = crate::workers::interpreter_deps::resolve_interpreter_root(
        &venv_dir,
        &exists,
        &canonicalize,
    );
    let interpreter_lib_dirs = crate::workers::interpreter_deps::interpreter_lib_dirs(
        &venv_dir,
        interpreter_root.as_deref(),
        &exists,
        &canonicalize,
        &resolve_deps,
    );
    let extra_fs_read = env_lookup("KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ")
        .as_deref()
        .map(parse_extra_fs_read)
        .unwrap_or_default();
    Ok(BrowserDriverEnv {
        script_path,
        venv_dir,
        interpreter_root,
        interpreter_lib_dirs,
        extra_fs_read,
    })
}

/// Parse the `KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ` JSON array into absolute
/// paths. Lenient: a blank value or malformed JSON yields no extra paths
/// (the worker simply gets fewer reads — fail-closed, never a parse panic);
/// relative entries are dropped (the policy requires absolute paths).
fn parse_extra_fs_read(raw: &str) -> Vec<PathBuf> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect()
}

/// Build the [`ToolEntry`] for the browser-driver worker (Phase 2).
///
/// Posture: `Net::Allowlist`; `proxy_uds` is left `None` in the manifest and
/// SET AT SPAWN by the force-routing path (`rewrite_worker_policy`), exactly
/// like web-fetch — so under the default force-routed deployment the browser
/// runs in a private netns reaching the net only via its egress sidecar (the
/// browser does end-to-end TLS; the sidecar transparently tunnels — egress
/// slice #2). `Profile::WorkerBrowserClient` (the spike's seccomp + Seatbelt browser
/// widening, §3.1), `SingleUse` lifecycle. The operator allowlist is injected
/// verbatim as `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` JSON; the worker
/// self-enforces it per navigation + subresource. `mem_mb` (1 GiB) is the
/// spike's safe cap (§3.1: headless-shell ~150-300 MB).
///
/// **Browsers live inside the venv** (`PLAYWRIGHT_BROWSERS_PATH =
/// <venv>/browsers`, set here + by `install.sh`) so only `venv_dir` needs an
/// `fs_read` bind — no separate browser-cache path. **Writable scratch** for
/// Chromium's `--user-data-dir` (Playwright places it under `$TMPDIR`):
/// `TMPDIR=/tmp` on both OSes; on Linux that's bwrap's per-spawn ephemeral
/// `/tmp` tmpfs (#89), granted to the in-jail Landlock layer via
/// `KASTELLAN_LANDLOCK_RW=["/tmp"]` with `fs_write` empty (keeps the host `/tmp`
/// off the tmpfs); on macOS Seatbelt has no tmpfs, so `fs_write=["/tmp"]` grants
/// the writable dir (a per-spawn scratch dir is the deferred follow-up — #283).
/// Fonts:
/// `/usr` (Linux) and `/System/Library` (macOS) are already readable from the
/// base sandbox; macOS additionally needs `/Library/Fonts`.
pub fn browser_driver_entry(
    env: &BrowserDriverEnv,
    allowlist: &[String],
    lockdown_shim: Option<PathBuf>,
) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");

    let mut fs_read = vec![
        env.venv_dir.clone(),
        PathBuf::from("/etc/resolv.conf"),
        PathBuf::from("/etc/hosts"),
        PathBuf::from("/etc/nsswitch.conf"),
    ];
    // Bind the real interpreter prefix when the venv's python lives outside
    // venv_dir (pyenv/uv venvs) so CPython can start inside the jail.
    if let Some(root) = &env.interpreter_root {
        fs_read.push(root.clone());
    }
    // Bind the interpreter's out-of-prefix shared-lib dirs (issue #284) so a
    // pyenv/Homebrew-linked interpreter can dyld-load in the jail.
    fs_read.extend(env.interpreter_lib_dirs.iter().cloned());
    // Operator-supplied host-specific extra reads (interpreter system-lib deps,
    // fonts, …) — see BrowserDriverEnv::extra_fs_read.
    fs_read.extend(env.extra_fs_read.iter().cloned());

    // macOS: /System/Library/Fonts is covered by the base profile's
    // /System/Library grant, but user/third-party fonts under /Library/Fonts
    // are not — add them so Chromium has a font to fall back on.
    #[cfg(target_os = "macos")]
    fs_read.push(PathBuf::from("/Library/Fonts"));

    // Writable scratch for Chromium's user-data-dir (see the fn doc).
    #[cfg(target_os = "linux")]
    let fs_write = vec![]; // bwrap per-spawn /tmp tmpfs (#89), granted via LANDLOCK_RW below
    #[cfg(not(target_os = "linux"))]
    let fs_write = vec![PathBuf::from("/tmp")]; // macOS Seatbelt needs an explicit writable dir

    let mut policy_env = vec![
        (
            "KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(),
            allow_json,
        ),
        // Keep Playwright's browser tree inside the already-bound venv.
        (
            "PLAYWRIGHT_BROWSERS_PATH".to_string(),
            env.venv_dir.join("browsers").display().to_string(),
        ),
        // Chromium writes its --user-data-dir under $TMPDIR.
        ("TMPDIR".to_string(), "/tmp".to_string()),
        // Playwright's bundled Node driver calls uv_os_homedir() at startup;
        // with bwrap's --clearenv stripping HOME and no /etc/passwd bound in
        // the jail, that returns ENOENT and the driver crashes ("Connection
        // closed while reading from the driver"). Point HOME at the writable
        // tmpfs so the driver starts. (macOS resolves the real home via
        // directory services, so this is belt-and-braces there.)
        ("HOME".to_string(), "/tmp".to_string()),
        // Grant the jail's /tmp through the worker-side Landlock layer
        // (Linux; honoured by derive_lockdown_env, no-op on macOS). MUST
        // stay out of fs_write on Linux: a /tmp entry there would bind the
        // host /tmp over bwrap's per-spawn ephemeral tmpfs (#89).
        (crate::tool_host::ENV_LANDLOCK_RW.to_string(), r#"["/tmp"]"#.to_string()),
    ];

    // When spawned through the lockdown shim (Linux), disable the shim's
    // Landlock layer: browser-driver's Chromium FS surface isn't validated
    // against a Landlock ruleset yet, and bwrap's mount namespace already
    // contains it. seccomp (browser_client) still applies. (#281; Landlock is
    // a tracked follow-up.) macOS passes None here and adds nothing.
    if lockdown_shim.is_some() {
        policy_env.push((
            crate::tool_host::ENV_LANDLOCK_PROFILE.to_string(),
            "none".to_string(),
        ));
    }

    let policy = SandboxPolicy {
        fs_read,
        fs_write,
        net: Net::Allowlist(allowlist.to_vec()),
        cpu_ms: 30_000,
        mem_mb: 1024, // spike §3.1: headless-shell ~150-300 MB; 1 GiB is a safe cap
        profile: Profile::WorkerBrowserClient,
        env: policy_env,
        cpu_quota_pct: None,
        // Chromium spawns a process tree (zygote + renderer + gpu + utility),
        // each multi-threaded — easily >100 tasks. The default cgroup
        // TasksMax=64 throttles it into a hang (DGX-confirmed: 64 fails, 512
        // renders). 512 is generous headroom for a single-page render.
        tasks_max: Some(512),
        proxy_uds: None, // set at spawn by force-routing (rewrite_worker_policy); same as web-fetch
    };
    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: Some(45_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim,
    }
}

/// browser-driver's host-side manifest. Reads its operator allowlist from the
/// `tool_allowlists` table (keyed `"browser-driver"`) and injects it into the
/// worker policy; maps the resolver's skip reasons onto [`Resolution`].
pub struct BrowserDriverManifest;

impl WorkerManifest for BrowserDriverManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        match resolve_env(
            |k| (ctx.get_env)(k),
            |p| (ctx.is_dir)(p),
            |p| (ctx.exists)(p),
            |p| (ctx.canonicalize)(p),
            crate::workers::interpreter_deps::resolve_deps_via_tool,
        ) {
            Ok(env) => {
                let allowlist = (ctx.allowlist)(TOOL_NAME);
                // Linux: browser-driver is a pure-Python venv worker bwrap
                // spawns directly, so it needs the lockdown-exec shim to get the
                // browser_client seccomp filter. Fail-closed if the shim is
                // missing — never register an unfilterable browser. macOS uses
                // Seatbelt (applied from the parent), so no shim.
                #[cfg(target_os = "linux")]
                {
                    match crate::worker_manifest::discover_binary(
                        ctx,
                        "KASTELLAN_LOCKDOWN_EXEC_BIN",
                        "kastellan-worker-lockdown-exec",
                    ) {
                        Some(shim) => {
                            Resolution::Register(browser_driver_entry(&env, &allowlist, Some(shim)))
                        }
                        None => Resolution::Misconfigured {
                            detail: "lockdown-exec shim not found \
                                     (KASTELLAN_LOCKDOWN_EXEC_BIN unset/invalid and no \
                                     exe-relative sibling); browser-driver requires it \
                                     for worker-side seccomp on Linux"
                                .to_string(),
                        },
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Resolution::Register(browser_driver_entry(&env, &allowlist, None))
                }
            }
            Err(ResolveSkipReason::Disabled) => Resolution::Disabled {
                detail: "KASTELLAN_BROWSER_DRIVER_ENABLE != \"1\"".to_string(),
            },
            Err(ResolveSkipReason::VenvDirUnresolvable) => Resolution::Misconfigured {
                detail: "venv dir unresolvable (KASTELLAN_BROWSER_DRIVER_VENV_DIR, \
                         KASTELLAN_DATA_DIR, and HOME all unset)"
                    .to_string(),
            },
            Err(ResolveSkipReason::ScriptShimMissing { path }) => Resolution::Misconfigured {
                detail: format!("venv shim missing: {}", path.display()),
            },
        }
    }
}

#[cfg(test)]
mod tests;
