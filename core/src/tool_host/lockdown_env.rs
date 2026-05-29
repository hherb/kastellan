//! Worker-prelude lock-down env derivation.
//!
//! Lifted out of `tool_host.rs` (HANDOVER Next-TODO item 5, the file-size
//! sibling-lift). [`derive_lockdown_env`] is the chokepoint for the
//! worker-side defence-in-depth layer: before any worker spawns,
//! [`crate::tool_host::spawn_worker`] augments the [`SandboxPolicy`] with the
//! `HHAGENT_LANDLOCK_RW` / `HHAGENT_SECCOMP_PROFILE` / `HHAGENT_CPU_MS` env
//! entries that `hhagent-worker-prelude` reads at worker start-up. Callers
//! cannot accidentally skip it because tool_host always derives the env, and
//! the worker installs the filters from inside its own process.
//!
//! The consts and [`derive_lockdown_env`] are re-exported from
//! `crate::tool_host` (`pub use`) so the public path
//! `hhagent_core::tool_host::ENV_LANDLOCK_RW` (etc.) is unchanged by the lift.

use hhagent_sandbox::{Profile, SandboxPolicy};

/// Env var name read by `hhagent-worker-prelude::landlock_lock` for the
/// JSON-encoded list of writable scratch paths. Workers using
/// `prelude::serve_stdio` get a Landlock filter built from this.
pub const ENV_LANDLOCK_RW: &str = "HHAGENT_LANDLOCK_RW";
/// Env var name read by `hhagent-worker-prelude::seccomp_lock` for the
/// per-worker seccomp profile selector.
pub const ENV_SECCOMP_PROFILE: &str = "HHAGENT_SECCOMP_PROFILE";
/// Env var name read by `hhagent-worker-prelude::rlimit` for the
/// `policy.cpu_ms` budget. Plumbed cross-platform — applied via
/// `setrlimit(RLIMIT_CPU)` from the worker prelude before lock-down.
/// Omitted (not set to `"0"`) when `policy.cpu_ms == 0` so the prelude
/// can treat "unset" as the canonical `Disabled` signal.
pub const ENV_CPU_MS: &str = "HHAGENT_CPU_MS";

/// Pure transform: clone `policy` and append the worker-prelude lockdown
/// env entries that aren't already present. Callers that explicitly set
/// either env var win — useful in tests and for future per-worker overrides
/// (e.g. a probe worker that needs `HHAGENT_SECCOMP_PROFILE=none`).
///
/// Exposed for unit testing the env-derivation logic without spinning up
/// a real sandbox.
pub fn derive_lockdown_env(policy: &SandboxPolicy) -> SandboxPolicy {
    let mut out = policy.clone();
    let has_landlock = out.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RW);
    let has_seccomp = out.env.iter().any(|(k, _)| k == ENV_SECCOMP_PROFILE);
    let has_cpu_ms = out.env.iter().any(|(k, _)| k == ENV_CPU_MS);

    if !has_landlock {
        let rw_paths: Vec<String> = out
            .fs_write
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // serde_json on a Vec<String> is infallible — `unwrap` is safe here.
        let json = serde_json::to_string(&rw_paths).unwrap();
        out.env.push((ENV_LANDLOCK_RW.into(), json));
    }
    if !has_seccomp {
        let value = match out.profile {
            Profile::WorkerStrict => "strict",
            Profile::WorkerNetClient => "net_client",
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
}
