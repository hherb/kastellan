//! Host-side manifest + `ToolEntry` constructor for the web-fetch worker.
//!
//! Containment caveat: until the egress proxy lands, the host allowlist is
//! enforced *inside* the worker and matches host **names**, not resolved IPs —
//! it does not contain SSRF / DNS-rebinding to internal addresses. The
//! `Net::Allowlist` data built here is populated for the future proxy, which
//! owns IP-level containment. See `docs/threat-model.md` ("Network egress").

use std::path::PathBuf;

use kastellan_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

/// Map the operator domain allowlist to `Net::Allowlist` `host:443` entries:
/// a wildcard `.domain` maps to its bare `domain:443` (the egress proxy refines
/// wildcard semantics). HTTPS-only, so port 443. Pure — shared by the host and
/// micro-VM entries here, and reused by the web-research manifest's content-host
/// half of its union allowlist.
pub(crate) fn allowlist_to_net_entries(allowlist: &[String]) -> Vec<String> {
    allowlist
        .iter()
        .map(|d| {
            let host = d.strip_prefix('.').unwrap_or(d);
            format!("{host}:443")
        })
        .collect()
}

/// Tool name the registry keys web-fetch on.
const TOOL_NAME: &str = "web-fetch";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "KASTELLAN_WEB_FETCH_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "kastellan-worker-web-fetch";

/// Opt into the Linux Firecracker micro-VM backend for web-fetch. Linux-only;
/// on macOS the flag is never read (the `FirecrackerVm` variant doesn't exist),
/// so the const is `cfg`-gated out there (issue-#144 rule).
#[cfg(target_os = "linux")]
const USE_MICROVM_ENV: &str = "KASTELLAN_WEB_FETCH_USE_MICROVM";

/// In-rootfs path of the web-fetch worker binary (staged there by
/// `build-web-fetch-rootfs.sh`). Used by the micro-VM entry, not the host path.
#[cfg(target_os = "linux")]
const MICROVM_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-web-fetch";

/// Build the [`ToolEntry`] for the web-fetch worker.
///
/// The administrator controls the domain allowlist (sourced from the
/// `tool_allowlists` DB table by the daemon, keyed `"web-fetch"`); the
/// LLM-supplied `step.parameters` cannot widen it. The same allowlist is
/// represented twice from one source:
///   - injected verbatim as the `KASTELLAN_WEB_FETCH_ALLOWLIST` env JSON for the
///     worker's own per-hop check (which understands the `.domain` wildcard), and
///   - mapped to `host:443` entries for `Net::Allowlist`, so the policy is
///     correct for the future egress proxy. (Wildcard `.domain` entries map to
///     their bare `domain:443`; the egress-proxy slice refines wildcard egress
///     semantics.) Port 80 is intentionally excluded: the worker enforces
///     HTTPS-only fetches.
///
/// Defaults: `Net::Allowlist`, `Profile::WorkerNetClient` (permits `socket(2)`),
/// `cpu_ms = 10_000`, `mem_mb = 512` (HTML/PDF parsing is heavier than argv
/// exec), `wall_clock_ms = Some(30_000)`, `SingleUse`. `fs_read` includes the
/// resolver config files so DNS works under the `--unshare-all` jail.
pub fn web_fetch_entry(binary: PathBuf, allowlist: &[String]) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let net_entries = allowlist_to_net_entries(allowlist);
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries),
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![("KASTELLAN_WEB_FETCH_ALLOWLIST".to_string(), allow_json)],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
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
        ephemeral_scratch: false,
    }
}

/// Build the [`ToolEntry`] for web-fetch running inside a Firecracker micro-VM
/// (opt-in via `KASTELLAN_WEB_FETCH_USE_MICROVM=1`). Mirrors the host-mode
/// [`web_fetch_entry`] but as a VM net worker:
///
/// * `Net::Allowlist(host:443…)` (derived from the operator allowlist exactly as
///   host mode) — **not** `Net::Deny`; web-fetch needs egress. Force-routing sets
///   `proxy_uds` at spawn, which makes `build_launch_plan` boot the VM with no NIC
///   and tunnel egress over the slice-4a vsock channel.
/// * `Profile::WorkerNetClient`, `proxy_uds: None` in the manifest (set at spawn).
/// * `fs_read: vec![]` — the worker has no NIC and does no local DNS (the egress
///   proxy resolves host-side), so no `/etc/resolv.conf` etc. The per-instance CA
///   is appended to `fs_read` at spawn by `rewrite_worker_policy`.
/// * `env` forwards the verbatim allowlist plus `KASTELLAN_MICROVM_DIR` (shared
///   image dir) and `KASTELLAN_MICROVM_ROOTFS=web-fetch.ext4` so the backend boots
///   the right rootfs. All three ride the #360 `kastellan.env` cmdline token.
///
/// `mem_mb: 512` is enforced by Firecracker. Linux-only: emits the
/// `#[cfg(target_os = "linux")]` `FirecrackerVm` backend variant.
#[cfg(target_os = "linux")]
pub fn web_fetch_firecracker_entry(
    binary: PathBuf,
    image_dir: String,
    allowlist: &[String],
) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let net_entries = allowlist_to_net_entries(allowlist);
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Allowlist(net_entries),
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            ("KASTELLAN_WEB_FETCH_ALLOWLIST".to_string(), allow_json),
            ("KASTELLAN_MICROVM_DIR".to_string(), image_dir),
            ("KASTELLAN_MICROVM_ROOTFS".to_string(), "web-fetch.ext4".to_string()),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm),
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}

/// web-fetch's manifest. Discovery mirrors shell-exec: a set
/// `KASTELLAN_WEB_FETCH_BIN` override is authoritative (honoured iff it names a
/// runnable file, else fails closed); only when unset do we fall back to the
/// exe-relative sibling `kastellan-worker-web-fetch`. See [`discover_binary`].
pub struct WebFetchManifest;

impl WorkerManifest for WebFetchManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let allowlist = (ctx.allowlist)(TOOL_NAME);

        // Firecracker micro-VM mode (Linux) short-circuits host binary discovery:
        // the worker binary lives inside the rootfs image, not on the host.
        // Linux-only — on macOS USE_MICROVM is never read so the `FirecrackerVm`
        // variant is never referenced (issue #144).
        #[cfg(target_os = "linux")]
        {
            let use_microvm =
                (ctx.get_env)(USE_MICROVM_ENV).unwrap_or_default().trim() == "1";
            if use_microvm {
                let binary = PathBuf::from(MICROVM_WORKER_BIN);
                let image_dir = (ctx.get_env)("KASTELLAN_MICROVM_DIR")
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "/var/lib/kastellan/microvm".to_string());
                return Resolution::Register(web_fetch_firecracker_entry(
                    binary, image_dir, &allowlist,
                ));
            }
        }

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
        Resolution::Register(web_fetch_entry(binary, &allowlist))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir: None,
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn resolve_registers_with_net_client_policy_and_dual_allowlist() {
        let get_env = |k: &str| (k == BIN_ENV).then(|| "/opt/web-fetch".to_string());
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["en.wikipedia.org".to_string(), ".example.com".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebFetchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert_eq!(entry.binary, PathBuf::from("/opt/web-fetch"));
                assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
                assert_eq!(entry.policy.cpu_ms, 10_000);
                assert_eq!(entry.policy.mem_mb, 512);
                assert_eq!(entry.wall_clock_ms, Some(30_000));
                // fs_read carries the binary + resolver files.
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/opt/web-fetch")));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/hosts")));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/nsswitch.conf")));
                // Net::Allowlist derived from the domains (wildcard → bare host).
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert_eq!(
                            hosts,
                            &vec![
                                "en.wikipedia.org:443".to_string(),
                                "example.com:443".to_string()
                            ]
                        );
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // Env carries the verbatim domain list (wildcard preserved).
                let (k, v) = &entry.policy.env[0];
                assert_eq!(k, "KASTELLAN_WEB_FETCH_ALLOWLIST");
                assert_eq!(v, r#"["en.wikipedia.org",".example.com"]"#);
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_misconfigured_when_no_binary_found() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match WebFetchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("kastellan-worker-web-fetch"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    fn outcome_label(r: &Resolution) -> &'static str {
        match r {
            Resolution::Register(_) => "Register",
            Resolution::Disabled { .. } => "Disabled",
            Resolution::Misconfigured { .. } => "Misconfigured",
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolve_uses_microvm_entry_when_opted_in() {
        let get_env = |k: &str| match k {
            "KASTELLAN_WEB_FETCH_USE_MICROVM" => Some("1".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["en.wikipedia.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebFetchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(
                    entry.sandbox_backend,
                    Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
                ));
                // In-rootfs binary path, not a host-discovered binary.
                assert_eq!(
                    entry.binary,
                    PathBuf::from("/usr/local/bin/kastellan-worker-web-fetch")
                );
                // Shared default image dir.
                let env = &entry.policy.env;
                let dir = env.iter().find(|(k, _)| k == "KASTELLAN_MICROVM_DIR").map(|(_, v)| v.as_str());
                assert_eq!(dir, Some("/var/lib/kastellan/microvm"));
            }
            other => panic!("expected Register(VM entry), got {}", outcome_label(&other)),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn firecracker_entry_is_net_allowlist_vm_with_empty_fs_read() {
        let allowlist = vec!["en.wikipedia.org".to_string(), ".example.com".to_string()];
        let entry = web_fetch_firecracker_entry(
            PathBuf::from("/usr/local/bin/kastellan-worker-web-fetch"),
            "/var/lib/kastellan/microvm".to_string(),
            &allowlist,
        );
        // VM backend, net client, no host paths shared in (the CA is added at spawn).
        assert!(matches!(
            entry.sandbox_backend,
            Some(kastellan_sandbox::SandboxBackendKind::FirecrackerVm)
        ));
        assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
        assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty (no NIC, no local DNS)");
        assert!(entry.policy.proxy_uds.is_none(), "proxy_uds is set at spawn, not in the manifest");
        // Net::Allowlist derived from the domains (wildcard → bare host:443).
        match &entry.policy.net {
            Net::Allowlist(hosts) => assert_eq!(
                hosts,
                &vec!["en.wikipedia.org:443".to_string(), "example.com:443".to_string()]
            ),
            other => panic!("expected Net::Allowlist, got {other:?}"),
        }
        // Env forwards the verbatim allowlist + the image dir + the rootfs filename.
        let env = &entry.policy.env;
        let get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());
        assert_eq!(get("KASTELLAN_WEB_FETCH_ALLOWLIST"), Some(r#"["en.wikipedia.org",".example.com"]"#));
        assert_eq!(get("KASTELLAN_MICROVM_DIR"), Some("/var/lib/kastellan/microvm"));
        assert_eq!(get("KASTELLAN_MICROVM_ROOTFS"), Some("web-fetch.ext4"));
    }
}
