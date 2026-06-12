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
    /// Absolute path to the uv console-script shim the dispatcher spawns.
    pub script_path: PathBuf,
    /// Worker venv root, mounted read-only into the jail.
    pub venv_dir: PathBuf,
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

/// Pure resolver: ENABLE gate + venv-anchor cascade + shim existence.
///
/// `is_dir` is unused today (browser-driver has no weights dir like GLiNER) but
/// kept in the signature so the manifest can thread the same `ResolveCtx`
/// probes uniformly.
pub fn resolve_env<E, D, X>(
    env_lookup: E,
    _is_dir: D,
    exists: X,
) -> Result<BrowserDriverEnv, ResolveSkipReason>
where
    E: Fn(&str) -> Option<String>,
    D: Fn(&Path) -> bool,
    X: Fn(&Path) -> bool,
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
    Ok(BrowserDriverEnv {
        script_path,
        venv_dir,
    })
}

/// Build the [`ToolEntry`] for the browser-driver worker (slice #1).
///
/// Slice #1 posture: `Net::Allowlist` on the **legacy direct-net path** (no
/// `proxy_uds` — egress-proxy force-routing is slice #2), `WorkerNetClient`
/// profile, `SingleUse` lifecycle. The operator allowlist is injected verbatim
/// as `KASTELLAN_BROWSER_DRIVER_ALLOWLIST` JSON; the worker self-enforces it per
/// navigation + subresource. `mem_mb` (1 GiB) is the spike's safe slice-1 cap
/// (§3.1); the browser-binary/font `fs_read` paths + the `Profile::BrowserClient`
/// seccomp profile are finalized from the spike findings in the Phase-2 plan.
pub fn browser_driver_entry(env: &BrowserDriverEnv, allowlist: &[String]) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![
            env.venv_dir.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(allowlist.to_vec()),
        cpu_ms: 30_000,
        mem_mb: 1024, // spike §3.1: headless-shell ~150-300 MB; 1 GiB is a safe slice-1 cap
        profile: Profile::WorkerNetClient,
        env: vec![(
            "KASTELLAN_BROWSER_DRIVER_ALLOWLIST".to_string(),
            allow_json,
        )],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None, // slice #1: legacy direct-net; force-routing is slice #2
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

    #[test]
    fn disabled_when_enable_not_set() {
        let env = |_k: &str| None;
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(matches!(
            resolve_env(env, is_dir, exists),
            Err(ResolveSkipReason::Disabled)
        ));
    }

    #[test]
    fn unresolvable_when_no_anchor() {
        let env = |k: &str| (k == "KASTELLAN_BROWSER_DRIVER_ENABLE").then(|| "1".to_string());
        let is_dir = |_p: &Path| true;
        let exists = |_p: &Path| true;
        assert!(matches!(
            resolve_env(env, is_dir, exists),
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
        match resolve_env(env, is_dir, exists) {
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
        let out = resolve_env(env, is_dir, exists).expect("resolves");
        assert_eq!(out.venv_dir, PathBuf::from("/v"));
        assert!(out.script_path.ends_with(SHIM_NAME));
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
    fn entry_has_net_client_policy_and_operator_allowlist() {
        let env = BrowserDriverEnv {
            script_path: PathBuf::from("/v/bin/kastellan-worker-browser-driver"),
            venv_dir: PathBuf::from("/v"),
        };
        let entry = browser_driver_entry(&env, &["example.com:443".to_string()]);
        assert_eq!(entry.binary, PathBuf::from("/v/bin/kastellan-worker-browser-driver"));
        assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
        // Slice #1: legacy direct-net path, no proxy_uds.
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
        assert!(entry.policy.env.iter().any(|(k, v)| k
            == "KASTELLAN_BROWSER_DRIVER_ALLOWLIST"
            && v == r#"["example.com:443"]"#));
        assert!(matches!(
            entry.lifecycle,
            crate::worker_lifecycle::Lifecycle::SingleUse
        ));
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
