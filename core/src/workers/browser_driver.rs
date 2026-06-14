//! Host-side manifest for the browser-driver worker (slice #1).
//!
//! A Playwright-Python worker (opt-in via `KASTELLAN_BROWSER_DRIVER_ENABLE=1`)
//! exposing `browser.render`. [`resolve_env`] is the pure core (env + fs probes
//! → [`BrowserDriverEnv`] | [`ResolveSkipReason`]); [`browser_driver_entry`]
//! builds the [`ToolEntry`]. Slice #1 runs on the legacy direct-net
//! `Net::Allowlist` path (no `proxy_uds`); egress-proxy force-routing is slice
//! #2. The real browser launch lives in the Python worker; the prelude
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
pub fn resolve_env<E, D, X, C>(
    env_lookup: E,
    _is_dir: D,
    exists: X,
    canonicalize: C,
) -> Result<BrowserDriverEnv, ResolveSkipReason>
where
    E: Fn(&str) -> Option<String>,
    D: Fn(&Path) -> bool,
    X: Fn(&Path) -> bool,
    C: Fn(&Path) -> Option<PathBuf>,
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
    let interpreter_root = resolve_interpreter_root(&venv_dir, &exists, &canonicalize);
    let extra_fs_read = env_lookup("KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ")
        .as_deref()
        .map(parse_extra_fs_read)
        .unwrap_or_default();
    Ok(BrowserDriverEnv {
        script_path,
        venv_dir,
        interpreter_root,
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

/// Resolve the real interpreter prefix to bind into the jail.
///
/// The venv's `bin/python3` (or `bin/python`) is canonicalized to the real
/// CPython, whose **prefix** (`<bin>/..`) is returned — that root holds the
/// interpreter binary + `libpython` + the stdlib. `None` when the interpreter
/// can't be found/canonicalized or already lives under the venv (self-contained
/// — nothing extra to bind). Pure: all I/O arrives via the closures.
fn resolve_interpreter_root(
    venv_dir: &Path,
    exists: &dyn Fn(&Path) -> bool,
    canonicalize: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    let bin = venv_dir.join("bin");
    let candidate = ["python3", "python"]
        .iter()
        .map(|n| bin.join(n))
        .find(|p| exists(p))?;
    let real = canonicalize(&candidate)?;
    let prefix = real.parent()?.parent()?; // <prefix>/bin/python → <prefix>
    // Self-contained: the real interpreter is already under venv_dir, so the
    // venv fs_read covers it — nothing extra to bind.
    if prefix.starts_with(venv_dir) {
        return None;
    }
    Some(prefix.to_path_buf())
}

/// Build the [`ToolEntry`] for the browser-driver worker (Phase 2).
///
/// Posture: `Net::Allowlist` on the **legacy direct-net path** (no `proxy_uds`
/// — see the #263 decision in the Phase-2 plan; force-routing is egress slice
/// #2), `Profile::WorkerBrowserClient` (the spike's seccomp + Seatbelt browser
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
pub fn browser_driver_entry(env: &BrowserDriverEnv, allowlist: &[String]) -> ToolEntry {
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

    let policy = SandboxPolicy {
        fs_read,
        fs_write,
        net: Net::Allowlist(allowlist.to_vec()),
        cpu_ms: 30_000,
        mem_mb: 1024, // spike §3.1: headless-shell ~150-300 MB; 1 GiB is a safe cap
        profile: Profile::WorkerBrowserClient,
        env: vec![
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
        ],
        cpu_quota_pct: None,
        // Chromium spawns a process tree (zygote + renderer + gpu + utility),
        // each multi-threaded — easily >100 tasks. The default cgroup
        // TasksMax=64 throttles it into a hang (DGX-confirmed: 64 fails, 512
        // renders). 512 is generous headroom for a single-page render.
        tasks_max: Some(512),
        proxy_uds: None, // dev-only legacy direct-net (#263); force-routing is slice #2
    };
    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: Some(45_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
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
        ) {
            Ok(env) => {
                let allowlist = (ctx.allowlist)(TOOL_NAME);
                Resolution::Register(browser_driver_entry(&env, &allowlist))
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
mod tests {
    use super::*;

    /// No interpreter canonicalization in most tests — a self-contained venv.
    fn no_canon(_p: &Path) -> Option<PathBuf> {
        None
    }

    #[test]
    fn disabled_when_enable_not_set() {
        let env = |_k: &str| None;
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(matches!(
            resolve_env(env, is_dir, exists, no_canon),
            Err(ResolveSkipReason::Disabled)
        ));
    }

    #[test]
    fn unresolvable_when_no_anchor() {
        let env = |k: &str| (k == "KASTELLAN_BROWSER_DRIVER_ENABLE").then(|| "1".to_string());
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(matches!(
            resolve_env(env, is_dir, exists, no_canon),
            Err(ResolveSkipReason::VenvDirUnresolvable)
        ));
    }

    #[test]
    fn shim_missing_surfaces_path() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| false; // shim absent
        match resolve_env(env, is_dir, exists, no_canon) {
            Err(ResolveSkipReason::ScriptShimMissing { path }) => {
                assert!(path.ends_with(SHIM_NAME), "path: {}", path.display());
            }
            other => panic!("expected ScriptShimMissing, got {other:?}"),
        }
    }

    #[test]
    fn resolves_when_enabled_and_shim_present() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        let out = resolve_env(env, is_dir, exists, no_canon).expect("resolves");
        assert_eq!(out.venv_dir, PathBuf::from("/v"));
        assert!(out.script_path.ends_with(SHIM_NAME));
        // Self-contained (canonicalize → None) ⇒ no extra interpreter bind.
        assert_eq!(out.interpreter_root, None);
    }

    #[test]
    fn resolves_interpreter_root_for_external_venv() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        // The venv's python3 symlinks to an EXTERNAL interpreter (pyenv-style):
        // /home/u/.pyenv/versions/3.12.3/bin/python3.12 → prefix
        // /home/u/.pyenv/versions/3.12.3 must be bound.
        let canon = |p: &Path| {
            if p == Path::new("/v/bin/python3") {
                Some(PathBuf::from(
                    "/home/u/.pyenv/versions/3.12.3/bin/python3.12",
                ))
            } else {
                None
            }
        };
        let out = resolve_env(env, is_dir, exists, canon).expect("resolves");
        assert_eq!(
            out.interpreter_root,
            Some(PathBuf::from("/home/u/.pyenv/versions/3.12.3"))
        );
        // And the entry binds that root read-only.
        let entry = browser_driver_entry(&out, &[]);
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/home/u/.pyenv/versions/3.12.3")));
    }

    #[test]
    fn extra_fs_read_env_is_parsed_and_bound() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            "KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ" => {
                Some(r#"["/opt/homebrew", "relative/dropped"]"#.to_string())
            }
            _ => None,
        };
        let out = resolve_env(env, |_p| true, |_p| true, no_canon).expect("resolves");
        // Absolute entry kept; relative one dropped (policy needs absolute paths).
        assert_eq!(out.extra_fs_read, vec![PathBuf::from("/opt/homebrew")]);
        let entry = browser_driver_entry(&out, &[]);
        assert!(entry.policy.fs_read.contains(&PathBuf::from("/opt/homebrew")));
    }

    #[test]
    fn malformed_extra_fs_read_yields_no_extra_paths() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            "KASTELLAN_BROWSER_DRIVER_EXTRA_FS_READ" => Some("not json".to_string()),
            _ => None,
        };
        let out = resolve_env(env, |_p| true, |_p| true, no_canon).expect("resolves");
        assert!(out.extra_fs_read.is_empty());
    }

    #[test]
    fn interpreter_under_venv_needs_no_extra_bind() {
        let env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        // Self-contained venv: python3 resolves to within /v.
        let canon = |_p: &Path| Some(PathBuf::from("/v/bin/python3.12"));
        let out = resolve_env(env, is_dir, exists, canon).expect("resolves");
        assert_eq!(
            out.interpreter_root, None,
            "interpreter already under venv_dir ⇒ no extra bind"
        );
    }

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| true,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn entry_has_browser_client_policy_and_operator_allowlist() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
            interpreter_root: None,
            extra_fs_read: vec![],
        };
        let entry = browser_driver_entry(&env, &["example.com:443".to_string()]);
        assert_eq!(entry.binary, PathBuf::from("/v/bin/kastellan-worker-browser-driver"));
        // Phase 2: the browser-specific seccomp/Seatbelt profile.
        assert!(matches!(entry.policy.profile, Profile::WorkerBrowserClient));
        // Dev-only legacy direct-net path, no proxy_uds (#263).
        assert!(entry.policy.proxy_uds.is_none());
        match &entry.policy.net {
            Net::Allowlist(hosts) => assert_eq!(hosts, &vec!["example.com:443".to_string()]),
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // venv mounted RO; resolver config present for in-jail DNS.
        assert!(entry.policy.fs_read.contains(&PathBuf::from("/v")));
        assert!(entry
            .policy
            .fs_read
            .contains(&PathBuf::from("/etc/resolv.conf")));
        // operator allowlist injected as env JSON.
        let env_get = |key: &str| {
            entry
                .policy
                .env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            env_get("KASTELLAN_BROWSER_DRIVER_ALLOWLIST").as_deref(),
            Some(r#"["example.com:443"]"#)
        );
        // Browsers staged inside the (already-bound) venv; TMPDIR scratch wired.
        assert_eq!(
            env_get("PLAYWRIGHT_BROWSERS_PATH").as_deref(),
            Some("/v/browsers")
        );
        assert_eq!(env_get("TMPDIR").as_deref(), Some("/tmp"));
        // HOME must be set so Playwright's Node driver's uv_os_homedir() works
        // under bwrap's --clearenv (no /etc/passwd in the jail).
        assert_eq!(env_get("HOME").as_deref(), Some("/tmp"));
        assert_eq!(
            env_get(crate::tool_host::ENV_LANDLOCK_RW).as_deref(),
            Some(r#"["/tmp"]"#)
        );
        assert!(matches!(
            entry.lifecycle,
            crate::worker_lifecycle::Lifecycle::SingleUse
        ));
        // TasksMax must be raised above the default 64 — Chromium's process
        // tree needs it (DGX-confirmed).
        assert_eq!(entry.policy.tasks_max, Some(512));
    }

    #[test]
    fn manifest_registers_when_enabled() {
        let get_env = |k: &str| match k {
            "KASTELLAN_BROWSER_DRIVER_ENABLE" => Some("1".to_string()),
            "KASTELLAN_BROWSER_DRIVER_VENV_DIR" => Some("/v".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["example.com:443".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        assert_eq!(BrowserDriverManifest.name(), "browser-driver");
        assert_eq!(BrowserDriverManifest.allowlist_tool(), Some("browser-driver"));
        assert!(matches!(
            BrowserDriverManifest.resolve(&c),
            Resolution::Register(_)
        ));
    }

    #[test]
    fn manifest_disabled_by_default() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);
        assert!(matches!(
            BrowserDriverManifest.resolve(&c),
            Resolution::Disabled { .. }
        ));
    }
}
