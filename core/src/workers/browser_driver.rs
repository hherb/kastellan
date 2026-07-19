//! Host-side manifest for the browser-driver worker (slice #1).
//!
//! A Playwright-Python worker (opt-in via `KASTELLAN_BROWSER_DRIVER_ENABLE=1`)
//! exposing `browser.render`. [`resolve_env`] is the pure core (env + fs probes
//! → [`BrowserDriverEnv`] | [`ResolveSkipReason`]); [`browser_driver_entry`]
//! builds the [`ToolEntry`]. Under the default force-routed deployment the
//! worker runs in a private netns reaching the net only via its per-worker
//! egress sidecar (in-jail loopback-TCP↔UDS shim + transparent tunnel —
//! egress slice #2); force-routing is OFF in dev, where the worker runs
//! direct-net. The real browser launch lives in the Python worker. On Linux
//! the worker is spawned through the `kastellan-worker-lockdown-exec` shim so
//! the `browser_client` seccomp filter **and** the Landlock ruleset apply
//! worker-side (#281; Landlock RO derived from `fs_read`, RW = the `/tmp`
//! scratch); macOS applies the equivalent profile via Seatbelt from the parent.

use std::path::{Path, PathBuf};

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::worker_lifecycle::force_route::env_flag_enabled;
use crate::worker_manifest::{Resolution, ResolveCtx, ToolDoc, ToolParam, WorkerManifest};

/// Tool name the registry/planner keys browser-driver on.
const TOOL_NAME: &str = "browser-driver";
/// uv console-script shim name (`<venv>/bin/<SHIM_NAME>`).
const SHIM_NAME: &str = "kastellan-worker-browser-driver";
/// Opt-in gate for the whole worker. Read in both host and micro-VM mode, so
/// `USE_MICROVM=1` alone never registers a tool the operator has not enabled.
const ENABLE_ENV: &str = "KASTELLAN_BROWSER_DRIVER_ENABLE";

/// Opt into the Linux Firecracker micro-VM backend for browser-driver.
/// Linux-only: on macOS the flag is never read (the `FirecrackerVm` variant
/// doesn't exist there), so the const is `cfg`-gated out (the issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_BROWSER_DRIVER_USE_MICROVM";

/// In-rootfs path of the browser-driver entrypoint, baked as a symlink into the
/// staged venv by `scripts/workers/microvm/build-browser-driver-rootfs.sh`
/// (via `Dockerfile.browser-driver`). This is the path PID1 `execv`s INSIDE the
/// guest — **never** a host `target/` path.
///
/// Getting this wrong is expensive and quiet: PID1 ENOENTs, panics, the VM
/// boot-loops, and the dispatch merely hangs to wall-clock, presenting as a
/// channel hang that names nothing (memory: `vm-worker-in-rootfs-binary-path`).
/// `core/tests/browser_driver_firecracker_e2e.rs` pins this const against the
/// baked path through the real `build_launch_plan`.
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-browser-driver";

/// Rootfs image filename produced by `build-browser-driver-rootfs.sh`.
#[cfg(target_os = "linux")]
const MICROVM_ROOTFS: &str = "browser-driver.ext4";

/// Playwright browser tree inside the rootfs (`ENV PLAYWRIGHT_BROWSERS_PATH` in
/// `Dockerfile.browser-driver`). Differs from host mode, where the tree lives
/// under the host venv — in the guest there is no host venv to anchor against.
///
/// **Must match the Dockerfile's `ENV` byte for byte.** `docker export` ships
/// only the filesystem — image env metadata is dropped — so the Dockerfile's
/// value positions the browsers at build time and THIS const is the sole
/// runtime source. A divergence fails loudly (Playwright: "executable doesn't
/// exist"), unlike the [`MICROVM_WORKER_BIN`] hang, and the live e2e tier
/// launches Chromium through this value for real.
#[cfg(target_os = "linux")]
const MICROVM_BROWSERS_PATH: &str = "/usr/local/lib/kastellan-browser-driver/browsers";

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
    // Opt-in gate under the one unified flag dialect (`1|true|yes|on`, trimmed,
    // case-insensitive) — #459 retired the strict `== "1"` so `…=true` can't
    // silently read as off next to a `FORCE_ROUTING=true` that reads on.
    if !env_flag_enabled(env_lookup(ENABLE_ENV)) {
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
/// self-enforces it per navigation + subresource. The same rows are mapped to
/// port-scoped `host:443` entries for `Net::Allowlist` (see the comment on the
/// `net` field) — the dual-allowlist shape web-fetch uses. `mem_mb` (1 GiB) is
/// the spike's safe cap (§3.1: headless-shell ~150-300 MB).
///
/// **Browsers live inside the venv** (`PLAYWRIGHT_BROWSERS_PATH =
/// <venv>/browsers`, set here + by `install.sh`) so only `venv_dir` needs an
/// `fs_read` bind — no separate browser-cache path. **Writable scratch** for
/// Chromium's `--user-data-dir` (Playwright places it under `$TMPDIR`) is
/// per-spawn and ephemeral (`ephemeral_scratch: true`, #283), so `fs_write` is
/// left empty in the manifest and the host grants exactly one writable dir at
/// spawn:
/// * **Linux** — bwrap's per-spawn ephemeral `/tmp` tmpfs (#89), granted to the
///   in-jail Landlock layer via `KASTELLAN_LANDLOCK_RW=["/tmp"]`. `fs_write`
///   empty keeps the host `/tmp` off the tmpfs; `TMPDIR`/`HOME` are `/tmp`.
///   `ephemeral_scratch` is a no-op here (`prepare_ephemeral_scratch` returns
///   `None`).
/// * **macOS** — Seatbelt has no tmpfs, so `prepare_ephemeral_scratch`
///   host-creates a unique per-spawn dir, adds it to `fs_write`, and injects
///   `KASTELLAN_WORKER_SCRATCH`; the worker redirects `TMPDIR`/`HOME` to it.
///   This replaces the former shared `fs_write=["/tmp"]` grant — each browser
///   spawn is now confined to its own scratch (closes #283's least-privilege
///   gap). `TMPDIR`/`HOME` are seeded to `/tmp` here too as the fail-closed
///   default; the worker overrides them once it reads the scratch env.
///   Overriding `HOME` on macOS (away from the real home that directory
///   services would resolve) is deliberate, not incidental: nothing in the
///   render path reads `~/Library/...`, and the per-spawn `HOME` keeps the
///   Playwright Node driver's `uv_os_homedir()` inside the granted scratch.
///   Verified by `browser_driver_e2e --ignored` 4/4 under the real Seatbelt
///   jail.
///
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

    // Bind the lockdown-exec shim into the jail read-only so bwrap can exec it.
    // In production it lives under /usr (bound globally), but in dev/test the
    // shim is in target/debug/ which is NOT part of the base /usr bind — without
    // this explicit entry bwrap returns "No such file or directory" before the
    // shim ever runs. Safe to skip on macOS (lockdown_shim is always None there).
    if let Some(shim) = &lockdown_shim {
        fs_read.push(shim.clone());
    }

    // macOS: /System/Library/Fonts is covered by the base profile's
    // /System/Library grant, but user/third-party fonts under /Library/Fonts
    // are not — add them so Chromium has a font to fall back on.
    #[cfg(target_os = "macos")]
    fs_read.push(PathBuf::from("/Library/Fonts"));

    // Writable scratch for Chromium's user-data-dir is per-spawn (#283; see the
    // fn doc): the manifest grants nothing, and the host adds exactly one dir at
    // spawn — bwrap's per-spawn /tmp tmpfs on Linux (via LANDLOCK_RW below), or a
    // unique macOS dir minted by `prepare_ephemeral_scratch` (added to fs_write +
    // exposed as KASTELLAN_WORKER_SCRATCH). No shared host /tmp on either OS.
    let fs_write: Vec<PathBuf> = vec![];

    let policy_env = vec![
        (
            "KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(),
            allow_json,
        ),
        // Keep Playwright's browser tree inside the already-bound venv.
        (
            "PLAYWRIGHT_BROWSERS_PATH".to_string(),
            env.venv_dir.join("browsers").display().to_string(),
        ),
        // Chromium writes its --user-data-dir under $TMPDIR. Seeded to /tmp (the
        // Linux per-spawn tmpfs); on macOS the worker redirects it to the
        // per-spawn KASTELLAN_WORKER_SCRATCH dir at startup (#283).
        ("TMPDIR".to_string(), "/tmp".to_string()),
        // Playwright's bundled Node driver calls uv_os_homedir() at startup;
        // with bwrap's --clearenv stripping HOME and no /etc/passwd bound in
        // the jail, that returns ENOENT and the driver crashes ("Connection
        // closed while reading from the driver"). Point HOME at the writable
        // tmpfs so the driver starts. (macOS resolves the real home via
        // directory services, so this is belt-and-braces there; the worker also
        // redirects HOME to the per-spawn scratch dir at startup — #283.)
        ("HOME".to_string(), "/tmp".to_string()),
        // Grant the jail's /tmp through the worker-side Landlock layer
        // (Linux; honoured by the lockdown-exec shim's apply_from_env, no-op
        // on macOS). MUST stay out of fs_write on Linux: a /tmp entry there
        // would bind the host /tmp over bwrap's per-spawn ephemeral tmpfs
        // (#89). Load-bearing now that Landlock is active for browser-driver
        // (#281 follow-up): Chromium's --user-data-dir lives under /tmp, and
        // Landlock denies writes outside the RW set.
        (crate::tool_host::ENV_LANDLOCK_RW.to_string(), r#"["/tmp"]"#.to_string()),
    ];

    // NB: browser-driver runs with Landlock ACTIVE since the #281 follow-up.
    // We deliberately do NOT set KASTELLAN_LANDLOCK_PROFILE here — its absence
    // is the default on-path, so the shim's lock_down() installs the ruleset.
    // The Landlock RO set is derived from this policy's fs_read (see
    // derive_lockdown_env) — venv, interpreter libs, /etc resolver files, the
    // shim, and (when force-routed) the per-instance CA. The RW set is the
    // /tmp scratch above. bwrap's mount namespace remains the primary FS layer;
    // Landlock is the kernel-side second gate over the same bound set. seccomp
    // (browser_client) is applied by the same shim.

    let policy = SandboxPolicy {
        fs_read,
        fs_write,
        // Port-scoped to 443 via web-fetch's canonical mapper, NOT the verbatim
        // rows. A bare-host `Net::Allowlist` entry is a grant on EVERY port at
        // the egress proxy (it logs `allowed:host-only-entry` rather than
        // blocking), so passing rows through unmapped would have made an
        // allowlisted `example.org` reach `example.org:22` as well. `validate_domain`
        // forbids an embedded port in a row, so the row itself can never narrow
        // this — the mapping has to. The worker's own Playwright-side check
        // still receives the verbatim rows (wildcards intact) via
        // `KASTELLAN_BROWSER_DRIVER_ALLOWLIST`, the same dual-allowlist shape
        // web-fetch uses. HTTPS-only, consistent with the other web workers.
        net: Net::Allowlist(crate::workers::web_fetch::allowlist_to_net_entries(allowlist)),
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
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: Some(45_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim,
        // Per-spawn writable scratch (#283): no-op on Linux (bwrap tmpfs); on
        // macOS the host mints a unique dir, grants it via fs_write, and exposes
        // it as KASTELLAN_WORKER_SCRATCH — replacing the shared /tmp grant.
        ephemeral_scratch: true,
        broker: None,
    }
}

/// Build the [`ToolEntry`] for browser-driver running inside a Firecracker
/// micro-VM (opt-in via `KASTELLAN_BROWSER_DRIVER_USE_MICROVM=1`, on top of the
/// usual `ENABLE` gate). Slice 2 of the VM-entry arc; the rootfs is slice 1.
///
/// Mirrors [`browser_driver_entry`] but as a VM net worker. What changes, and
/// why each difference is load-bearing:
///
/// * **`fs_read: vec![]`** — a VM shares no host paths in at all. The venv, the
///   interpreter and the Playwright browser tree all live inside the rootfs, so
///   none of host mode's binds (venv, interpreter prefix + lib dirs, `/etc`
///   resolver files, operator extras) has anything to point at. The worker has
///   no NIC and does no local DNS either — the egress proxy resolves host-side.
/// * **No `lockdown_shim`, no `KASTELLAN_LANDLOCK_RW`** — host mode needs the
///   `kastellan-worker-lockdown-exec` shim to apply seccomp + Landlock to a
///   pure-Python venv worker bwrap spawns directly (#281). In VM mode the
///   isolation boundary *is* the VM: a separate kernel, no host FS, no NIC. The
///   shim binary is not staged in the rootfs, so requiring it would be a boot
///   failure, not a hardening.
/// * **`mem_mb: 2048`** (host mode: 1024) — Firecracker *enforces* this as the
///   guest's total RAM, and it must cover Chromium **plus** the guest `/tmp`
///   tmpfs. `--disable-dev-shm-usage` redirects Chromium's shared memory into
///   `TMPDIR=/tmp`, so shm competes with guest RAM instead of living in a
///   separate `/dev/shm` (design spec §6, §10.4). Slice 3 should re-check this
///   budget against a real, heavy render — no real page has rendered in the VM yet.
/// * **`PLAYWRIGHT_BROWSERS_PATH`** points at the in-rootfs tree, not a venv
///   subdir. **`KASTELLAN_MICROVM_DIR` / `_ROOTFS`** tell the backend which image
///   to boot; `build_launch_plan` strips both before hex-encoding the guest env,
///   so they cost no cmdline budget.
/// * **`wall_clock_ms: 90_000`** (host mode: 45_000) — a cold VM boot precedes
///   the Playwright Node driver and a Chromium cold start.
///
/// What deliberately stays the same: `Net::Allowlist` mapped to `host:443` by
/// web-fetch's canonical mapper (a bare-host entry would be an all-port grant at
/// the proxy), the verbatim rows in `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` for the
/// worker's own per-navigation check (the dual-allowlist shape), `TMPDIR`/`HOME`
/// at `/tmp` for Playwright's `uv_os_homedir()`, `tasks_max: 512` for Chromium's
/// process tree, `Profile::WorkerBrowserClient`, `SingleUse`, and `proxy_uds:
/// None` in the manifest (force-routing sets it at spawn).
///
/// Linux-only: emits the `#[cfg(target_os = "linux")]` `FirecrackerVm` variant.
#[cfg(target_os = "linux")]
pub fn browser_driver_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    allowlist: &[String],
) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(crate::workers::web_fetch::allowlist_to_net_entries(allowlist)),
        cpu_ms: 30_000,
        mem_mb: 2048,
        profile: Profile::WorkerBrowserClient,
        env: vec![
            ("KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(), allow_json),
            (
                "PLAYWRIGHT_BROWSERS_PATH".to_string(),
                MICROVM_BROWSERS_PATH.to_string(),
            ),
            ("TMPDIR".to_string(), "/tmp".to_string()),
            ("HOME".to_string(), "/tmp".to_string()),
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            (
                "KASTELLAN_MICROVM_ROOTFS".to_string(),
                MICROVM_ROOTFS.to_string(),
            ),
        ],
        cpu_quota_pct: None,
        tasks_max: Some(512),
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(90_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: None,
    }
}

/// browser-driver's host-side manifest. Reads its operator allowlist from the
/// `tool_allowlists` table (keyed `"browser-driver"`) and injects it into the
/// worker policy; maps the resolver's skip reasons onto [`Resolution`].
pub struct BrowserDriverManifest;

impl WorkerManifest for BrowserDriverManifest {
    fn tool_doc(&self) -> Option<ToolDoc> {
        Some(ToolDoc {
            name: TOOL_NAME,
            method: "browser.render",
            summary: "Render a URL in a headless browser (executes JavaScript) and return \
                      the resulting page text. Use for pages that need JS; web.fetch is \
                      cheaper for static pages.",
            params: &[
                ToolParam { name: "url", description: "absolute https URL to render", required: true },
                ToolParam {
                    name: "timeout_ms",
                    description: "render timeout in ms (optional, clamped)",
                    required: false,
                },
                ToolParam {
                    name: "wait_until",
                    description: "load condition, e.g. \"networkidle\" (optional)",
                    required: false,
                },
            ],
        })
    }

    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

    fn allowlist_kind(&self) -> Option<kastellan_db::tool_allowlists::EntryKind> {
        Some(kastellan_db::tool_allowlists::EntryKind::Domain)
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        // Firecracker micro-VM mode (Linux) short-circuits the ENTIRE host-side
        // resolution below — not just binary discovery, as in web-fetch. Host
        // mode resolves a venv, its interpreter prefix and out-of-prefix lib
        // dirs, and then fail-closes on the missing lockdown-exec shim; in VM
        // mode every one of those lives inside the rootfs image, so none of them
        // needs to exist on this host and requiring them would make a correctly
        // configured VM deployment `Misconfigured`.
        //
        // Both flags are read: `USE_MICROVM=1` on its own must never register a
        // tool the operator has not enabled, and with ENABLE off we fall through
        // to `resolve_env`, which reports the accurate `Disabled`.
        //
        // Linux-only — on macOS `USE_MICROVM` is never read, so the
        // `FirecrackerVm` variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            if ctx.flag_enabled(ENABLE_ENV) && ctx.flag_enabled(USE_MICROVM_ENV) {
                return Resolution::Register(browser_driver_firecracker_entry(
                    PathBuf::from(MICROVM_WORKER_BIN),
                    ctx.microvm_image_dir(),
                    &(ctx.allowlist)(TOOL_NAME),
                ));
            }
        }

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
                // spawns directly, so it needs the lockdown-exec shim to apply
                // the worker-side seccomp (browser_client) + Landlock layers
                // (#281). Fail-closed if the shim is missing — never register
                // an unfilterable browser. macOS uses Seatbelt (applied from
                // the parent), so no shim.
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
                            detail: "lockdown-exec shim not found (KASTELLAN_LOCKDOWN_EXEC_BIN unset/invalid and no exe-relative sibling); browser-driver requires it for worker-side seccomp on Linux".to_string(),
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
