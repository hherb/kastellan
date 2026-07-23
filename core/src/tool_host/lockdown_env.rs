//! Worker-prelude lock-down env derivation.
//!
//! Lifted out of `tool_host.rs` (HANDOVER Next-TODO item 5, the file-size
//! sibling-lift). [`derive_lockdown_env`] is the chokepoint for the
//! worker-side defence-in-depth layer: before any worker spawns,
//! [`crate::tool_host::spawn_worker`] augments the [`SandboxPolicy`] with the
//! `KASTELLAN_LANDLOCK_RW` / `KASTELLAN_SECCOMP_PROFILE` / `KASTELLAN_CPU_MS` env
//! entries that `kastellan-worker-prelude` reads at worker start-up. Callers
//! cannot accidentally skip it because tool_host always derives the env, and
//! the worker installs the filters from inside its own process.
//!
//! The consts and [`derive_lockdown_env`] are re-exported from
//! `crate::tool_host` (`pub use`) so the public path
//! `kastellan_core::tool_host::ENV_LANDLOCK_RW` (etc.) is unchanged by the lift.

use kastellan_sandbox::{Profile, SandboxPolicy};

/// Env var name read by `kastellan-worker-prelude::landlock_lock` for the
/// JSON-encoded list of writable scratch paths. Workers using
/// `prelude::serve_stdio` get a Landlock filter built from this.
pub const ENV_LANDLOCK_RW: &str = "KASTELLAN_LANDLOCK_RW";
/// Env var name read by `kastellan-worker-prelude::landlock_lock` for the
/// JSON-encoded list of read-only paths derived from `SandboxPolicy.fs_read`.
/// These are bind-mounted read-only by bwrap and must also be granted
/// Landlock read rights so the worker can actually access them after
/// `lock_down()` completes (e.g. `/etc/resolv.conf` for DNS in web-fetch).
pub const ENV_LANDLOCK_RO: &str = "KASTELLAN_LANDLOCK_RO";
/// Env var name read by `kastellan-worker-prelude::seccomp_lock` for the
/// per-worker seccomp profile selector.
pub const ENV_SECCOMP_PROFILE: &str = "KASTELLAN_SECCOMP_PROFILE";
/// Env var read by `kastellan-worker-prelude::landlock_lock` to disable the
/// Landlock layer (`"none"`). Source of truth for the string is the prelude;
/// mirrored here for manifests that set it (browser-driver). Not set by
/// `derive_lockdown_env` — only explicitly by a manifest that opts out.
pub const ENV_LANDLOCK_PROFILE: &str = "KASTELLAN_LANDLOCK_PROFILE";
/// Env var name read by `kastellan-worker-prelude::rlimit` for the
/// `policy.cpu_ms` budget. Plumbed cross-platform — applied via
/// `setrlimit(RLIMIT_CPU)` from the worker prelude before lock-down.
/// Omitted (not set to `"0"`) when `policy.cpu_ms == 0` so the prelude
/// can treat "unset" as the canonical `Disabled` signal.
pub const ENV_CPU_MS: &str = "KASTELLAN_CPU_MS";

/// Pure transform: clone `policy` and append the worker-prelude lockdown
/// env entries that aren't already present. Callers that explicitly set
/// either env var win — useful in tests and for future per-worker overrides
/// (e.g. a probe worker that needs `KASTELLAN_SECCOMP_PROFILE=none`).
///
/// Exposed for unit testing the env-derivation logic without spinning up
/// a real sandbox.
///
/// NOTE: this function deliberately does **not** manage
/// [`ENV_LANDLOCK_PROFILE`] (`KASTELLAN_LANDLOCK_PROFILE`). That opt-out is set
/// only by a manifest that ALSO routes the worker through the lockdown-exec
/// shim (`ToolEntry.lockdown_shim.is_some()` — today only browser-driver, #281),
/// where the shim's own `lock_down()` reads it. Do NOT add
/// `KASTELLAN_LANDLOCK_PROFILE=none` to a Rust worker's `policy.env`: a Rust
/// worker self-applies via `serve_stdio`, and the var would silently disable its
/// Landlock layer while leaving it otherwise locked down.
pub fn derive_lockdown_env(policy: &SandboxPolicy) -> SandboxPolicy {
    let mut out = policy.clone();
    let has_landlock = out.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RW);
    let has_landlock_ro = out.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RO);
    let has_seccomp = out.env.iter().any(|(k, _)| k == ENV_SECCOMP_PROFILE);
    let has_cpu_ms = out.env.iter().any(|(k, _)| k == ENV_CPU_MS);

    if !has_landlock {
        let mut rw_paths: Vec<String> = out
            .fs_write
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // Slice 5b-2: the persistent store is a writable path the worker needs
        // Landlock RW for, but it must NOT enter fs_write (the FC backend turns
        // fs_write into *ephemeral* scratch). Add only to the Landlock RW set.
        if let Some(ps) = &out.persistent_store {
            rw_paths.push(ps.guest_mount.display().to_string());
        }
        // serde_json on a Vec<String> is infallible — `unwrap` is safe here.
        let json = serde_json::to_string(&rw_paths).unwrap();
        out.env.push((ENV_LANDLOCK_RW.into(), json));
    }
    if !has_landlock_ro {
        let ro_paths: Vec<String> = out
            .fs_read
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // serde_json on a Vec<String> is infallible — `unwrap` is safe here.
        let json = serde_json::to_string(&ro_paths).unwrap();
        out.env.push((ENV_LANDLOCK_RO.into(), json));
    }
    if !has_seccomp {
        let value = match out.profile {
            Profile::WorkerStrict => "strict",
            Profile::WorkerNetClient => "net_client",
            Profile::WorkerBrowserClient => "browser_client",
            Profile::WorkerMlClient => "ml_client",
            Profile::WorkerMatrixClient => "matrix_client",
        };
        out.env.push((ENV_SECCOMP_PROFILE.into(), value.into()));
    }
    // cpu_ms == 0 means "policy didn't set it"; omit the env so the
    // prelude's apply_from_env sees no var and returns Disabled.
    if !has_cpu_ms && policy.cpu_ms > 0 {
        out.env.push((ENV_CPU_MS.into(), policy.cpu_ms.to_string()));
    }
    out
}

/// A lockdown env entry that DISABLES a sandbox layer, weakening the
/// profile-derived default (audit #12 / #388). Produced by
/// [`detect_lockdown_overrides`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockdownOverride {
    pub var: String,
    pub value: String,
}

/// Inspect a *finalized* policy for sandbox-DISABLING lockdown env entries:
/// `KASTELLAN_SECCOMP_PROFILE` set to `"none"`/`""` (the prelude parses both as
/// "no filter") or `KASTELLAN_LANDLOCK_PROFILE` set to `"none"`. Returns one
/// entry per disabled layer; empty when nothing is weakened.
///
/// [`derive_lockdown_env`] honours a manifest-supplied value verbatim, so a
/// manifest author could silently under-lock a worker. This pure detector is
/// the guard the audit asked for; the spawn paths log its output at WARN (see
/// [`warn_lockdown_overrides`]). It does NOT reject — matrix legitimately sets
/// both to `none` under the `--enforce-sandbox=false` dev opt-out — it only
/// makes a sandbox-disabled spawn loud.
pub fn detect_lockdown_overrides(policy: &SandboxPolicy) -> Vec<LockdownOverride> {
    let mut out = Vec::new();
    for (k, v) in &policy.env {
        let disabled = if k == ENV_SECCOMP_PROFILE {
            matches!(v.as_str(), "none" | "")
        } else if k == ENV_LANDLOCK_PROFILE {
            v == "none"
        } else {
            false
        };
        if disabled {
            out.push(LockdownOverride {
                var: k.clone(),
                value: v.clone(),
            });
        }
    }
    out
}

/// Detect (via [`detect_lockdown_overrides`]) and log every sandbox-disabling
/// lockdown override in `policy` at WARN, naming `worker` for context. The one
/// place the log format lives, so the spawn paths that call it
/// (`tool_host::spawn_worker`, `worker_lifecycle::persistent`) cannot drift.
pub fn warn_lockdown_overrides(worker: &str, policy: &SandboxPolicy) {
    for ov in detect_lockdown_overrides(policy) {
        tracing::warn!(
            worker,
            var = %ov.var,
            value = %ov.value,
            "worker spawns with a sandbox layer DISABLED via a policy.env override"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base_policy() -> SandboxPolicy {
        SandboxPolicy::default()
    }

    #[test]
    fn derive_adds_strict_profile_for_default() {
        let derived = derive_lockdown_env(&base_policy());
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .expect("seccomp env must be derived");
        assert_eq!(seccomp.1, "strict");
    }

    #[test]
    fn derive_adds_net_client_profile() {
        let mut p = base_policy();
        p.profile = Profile::WorkerNetClient;
        let derived = derive_lockdown_env(&p);
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .unwrap();
        assert_eq!(seccomp.1, "net_client");
    }

    #[test]
    fn derive_adds_browser_client_profile() {
        let mut p = base_policy();
        p.profile = Profile::WorkerBrowserClient;
        let derived = derive_lockdown_env(&p);
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .unwrap();
        assert_eq!(seccomp.1, "browser_client");
    }

    #[test]
    fn derive_adds_ml_client_profile() {
        let mut p = base_policy();
        p.profile = Profile::WorkerMlClient;
        let derived = derive_lockdown_env(&p);
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .expect("seccomp env must be derived");
        assert_eq!(seccomp.1, "ml_client");
    }

    #[test]
    fn derive_adds_matrix_client_profile() {
        let mut p = base_policy();
        p.profile = Profile::WorkerMatrixClient;
        let derived = derive_lockdown_env(&p);
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .expect("seccomp env must be derived");
        assert_eq!(seccomp.1, "matrix_client");
    }

    #[test]
    fn derive_serialises_fs_write_into_landlock_env() {
        let mut p = base_policy();
        p.fs_write = vec![PathBuf::from("/tmp/scratch_a"), PathBuf::from("/tmp/b")];
        let derived = derive_lockdown_env(&p);
        let landlock = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_LANDLOCK_RW)
            .unwrap();
        // Both paths must appear in the JSON. Exact-string assertion is OK
        // because serde_json on a Vec<String> is deterministic.
        assert_eq!(landlock.1, r#"["/tmp/scratch_a","/tmp/b"]"#);
    }

    #[test]
    fn derive_does_not_overwrite_caller_supplied_env() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "none".into()));
        let derived = derive_lockdown_env(&p);
        let seccomp_entries: Vec<_> = derived
            .env
            .iter()
            .filter(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .collect();
        assert_eq!(
            seccomp_entries.len(),
            1,
            "caller-supplied env must not be duplicated"
        );
        assert_eq!(seccomp_entries[0].1, "none");
    }

    #[test]
    fn derive_adds_cpu_ms_env_when_policy_sets_it() {
        let mut p = base_policy();
        p.cpu_ms = 2_500;
        let derived = derive_lockdown_env(&p);
        let cpu_ms_entry = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_CPU_MS)
            .expect("cpu_ms env must be derived when policy.cpu_ms > 0");
        assert_eq!(cpu_ms_entry.1, "2500");
    }

    #[test]
    fn derive_omits_cpu_ms_env_when_policy_is_zero() {
        // policy.cpu_ms == 0 is the "no rlimit" sentinel (matches how
        // policy.mem_mb == 0 means "omit MemoryMax" in linux_cgroup).
        // The worker prelude reads "unset" as Disabled, so omitting the
        // env is the right wire signal.
        let mut p = base_policy();
        p.cpu_ms = 0;
        let derived = derive_lockdown_env(&p);
        assert!(
            !derived.env.iter().any(|(k, _)| k == ENV_CPU_MS),
            "ENV_CPU_MS must be omitted when policy.cpu_ms == 0; env was {:?}",
            derived.env
        );
    }

    #[test]
    fn derive_serialises_fs_read_into_landlock_ro_env() {
        let mut p = base_policy();
        p.fs_read = vec![
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/ssl/certs"),
        ];
        let derived = derive_lockdown_env(&p);
        let landlock_ro = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_LANDLOCK_RO)
            .expect("KASTELLAN_LANDLOCK_RO must be derived from fs_read");
        // Exact-string assertion is OK because serde_json on a Vec<String>
        // is deterministic.
        assert_eq!(
            landlock_ro.1,
            r#"["/etc/resolv.conf","/etc/ssl/certs"]"#
        );
    }

    #[test]
    fn derive_landlock_ro_empty_when_fs_read_empty() {
        // When policy.fs_read is empty, KASTELLAN_LANDLOCK_RO should be
        // derived as "[]" (an empty JSON array) rather than omitted —
        // the worker prelude parses this as an empty Vec, which is fine.
        let p = base_policy();
        let derived = derive_lockdown_env(&p);
        let landlock_ro = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_LANDLOCK_RO)
            .expect("KASTELLAN_LANDLOCK_RO must always be derived (even when empty)");
        assert_eq!(landlock_ro.1, "[]");
    }

    #[test]
    fn derive_does_not_overwrite_caller_supplied_landlock_ro() {
        let mut p = base_policy();
        // Caller pre-supplies a custom RO path; derive must leave it alone.
        p.env.push((ENV_LANDLOCK_RO.into(), r#"["/custom/ro"]"#.into()));
        p.fs_read = vec![PathBuf::from("/etc/resolv.conf")];
        let derived = derive_lockdown_env(&p);
        let ro_entries: Vec<_> = derived
            .env
            .iter()
            .filter(|(k, _)| k == ENV_LANDLOCK_RO)
            .collect();
        assert_eq!(
            ro_entries.len(),
            1,
            "caller-supplied KASTELLAN_LANDLOCK_RO must not be duplicated"
        );
        assert_eq!(ro_entries[0].1, r#"["/custom/ro"]"#);
    }

    #[test]
    fn detect_flags_seccomp_none() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "none".into()));
        let ov = detect_lockdown_overrides(&p);
        assert_eq!(ov.len(), 1);
        assert_eq!(ov[0].var, ENV_SECCOMP_PROFILE);
        assert_eq!(ov[0].value, "none");
    }

    #[test]
    fn detect_flags_landlock_none() {
        let mut p = base_policy();
        p.env.push((ENV_LANDLOCK_PROFILE.into(), "none".into()));
        let ov = detect_lockdown_overrides(&p);
        assert_eq!(ov.len(), 1);
        assert_eq!(ov[0].var, ENV_LANDLOCK_PROFILE);
    }

    #[test]
    fn detect_flags_both_disabled() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "none".into()));
        p.env.push((ENV_LANDLOCK_PROFILE.into(), "none".into()));
        assert_eq!(detect_lockdown_overrides(&p).len(), 2);
    }

    #[test]
    fn detect_empty_for_derived_default_policy() {
        // A normal policy through derive_lockdown_env gets a real seccomp
        // profile ("strict"), never "none" → nothing flagged.
        let derived = derive_lockdown_env(&base_policy());
        assert!(detect_lockdown_overrides(&derived).is_empty());
    }

    #[test]
    fn detect_empty_for_explicit_strict() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "strict".into()));
        assert!(detect_lockdown_overrides(&p).is_empty());
    }

    #[test]
    fn lockdown_env_landlock_rw_includes_persistent_guest_mount() {
        let mut policy = base_policy();
        policy.persistent_store = Some(kastellan_sandbox::PersistentStore {
            host_backing: PathBuf::from("/var/lib/kastellan/kv/store.ext4"),
            guest_mount: PathBuf::from("/data"),
            size_mib: 64,
        });
        let derived = derive_lockdown_env(&policy);
        let rw = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_LANDLOCK_RW)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(
            rw.contains("/data"),
            "Landlock RW must include the persistent guest_mount, got {rw:?}"
        );
        // Must NOT be smuggled into fs_write (which FC would make ephemeral).
        assert!(
            !derived.fs_write.iter().any(|p| p == std::path::Path::new("/data")),
            "persistent guest_mount must NOT appear in derived.fs_write"
        );
    }
}
