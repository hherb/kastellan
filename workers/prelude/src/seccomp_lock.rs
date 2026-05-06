//! seccomp-bpf syscall filter, applied from inside the worker process.
//!
//! ## Stage 1: deny-list of catastrophic syscalls
//!
//! This is *Phase 0 hardening, stage 1*. We use a **deny-list** rather than
//! the more rigorous allow-list because:
//!
//!   * an allow-list big enough to keep `tokio`, `serde_json`, libc malloc
//!     paths, and the dynamic linker working is ~200 syscalls and brittle
//!     to upgrade;
//!   * the handover plan explicitly says "start permissive and tighten
//!     incrementally with negative tests".
//!
//! Stage 2 (tracked in `docs/devel/ROADMAP.md`) will migrate to a
//! per-profile allow-list.
//!
//! ## What we deny
//!
//! Syscalls in `KILL_LIST` are catastrophic: namespace manipulation
//! (`unshare`, `setns`), filesystem-mount changes (`mount`, `umount2`,
//! `pivot_root`), kernel module loading (`init_module`, `finit_module`,
//! `delete_module`), debugging (`ptrace`), kernel reload (`kexec_load`,
//! `kexec_file_load`, `reboot`), BPF program loading (`bpf`),
//! perf-counters (`perf_event_open`), kernel keyring manipulation
//! (`keyctl`, `add_key`, `request_key`), clock changes
//! (`settimeofday`, `clock_settime`, `clock_adjtime`, `adjtimex`),
//! swap control (`swapon`, `swapoff`), and personality changes.
//!
//! A worker hitting any of these is killed by the kernel via SIGSYS.
//!
//! ## Why this also runs *under* bwrap
//!
//! `bwrap` already gives us namespace isolation. Defence in depth: a
//! kernel bug (or a future relaxation of bwrap config) does not get the
//! worker `unshare(CLONE_NEWUSER)` privileges it could use to escape into
//! a fresh user namespace. Same for `mount`: bwrap removed the
//! capability, but seccomp removes the syscall entry point too.

use std::collections::BTreeMap;

use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch,
};

use crate::{LockdownError, SeccompReport};

/// Profile selector exposed to workers via the `HHAGENT_SECCOMP_PROFILE`
/// env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// `"strict"` — denies the catastrophic syscall set. Suitable for
    /// workers that have no legitimate need for namespace, mount, BPF,
    /// kernel-module, or debug syscalls. This is every Phase 0 worker.
    Strict,
    /// `"net_client"` — same as `Strict` for stage 1. Reserved for future
    /// stages where we may relax some networking-adjacent syscalls.
    NetClient,
}

impl Profile {
    fn parse(s: &str) -> Result<Option<Self>, LockdownError> {
        match s {
            "strict" => Ok(Some(Profile::Strict)),
            "net_client" => Ok(Some(Profile::NetClient)),
            "none" | "" => Ok(None),
            other => Err(LockdownError::Env(format!(
                "HHAGENT_SECCOMP_PROFILE must be 'strict' | 'net_client' | 'none', got {other:?}"
            ))),
        }
    }
}

/// Read [`HHAGENT_SECCOMP_PROFILE`] and apply the corresponding filter.
pub fn apply_from_env() -> Result<SeccompReport, LockdownError> {
    let raw = std::env::var("HHAGENT_SECCOMP_PROFILE").unwrap_or_else(|_| "none".to_string());
    match Profile::parse(&raw)? {
        None => Ok(SeccompReport::Disabled),
        Some(p) => apply(p).map(|()| SeccompReport::Installed),
    }
}

/// Install the seccomp filter for `profile`. Sets `PR_SET_NO_NEW_PRIVS`
/// first, which is required for unprivileged seccomp loading.
pub fn apply(profile: Profile) -> Result<(), LockdownError> {
    set_no_new_privs()?;
    let bpf = build_bpf(profile)?;
    apply_filter(&bpf).map_err(|e| LockdownError::Seccomp(format!("apply_filter: {e}")))?;
    Ok(())
}

/// Pure builder: produce the BPF program for `profile`. Exposed so unit
/// tests can assert on filter shape without actually installing it
/// (installation is one-way and would poison other tests in the same
/// process).
pub fn build_bpf(profile: Profile) -> Result<BpfProgram, LockdownError> {
    let kill_syscalls = kill_list_for(profile);
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in kill_syscalls {
        // Empty rule vec = unconditional match for that syscall number.
        rules.insert(*nr, Vec::new());
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,        // default for un-listed syscalls
        SeccompAction::KillProcess,  // for listed syscalls
        target_arch()?,
    )
    .map_err(|e| LockdownError::Seccomp(format!("SeccompFilter::new: {e}")))?;
    BpfProgram::try_from(filter)
        .map_err(|e| LockdownError::Seccomp(format!("BpfProgram::try_from: {e}")))
}

fn kill_list_for(_profile: Profile) -> &'static [i64] {
    // Stage 1: same deny-list for both Strict and NetClient. Diverges in
    // stage 2 once allow-listing lands.
    KILL_LIST
}

/// The catastrophic-syscall deny-list. Same set is enforced by both
/// profiles in stage 1.
///
/// Each entry carries a brief justification so a future maintainer
/// reading the diff knows *why* it's here, not just *what* it does.
pub const KILL_LIST: &[i64] = &[
    // Namespace manipulation — bwrap already unshared, no need for more.
    libc::SYS_unshare,
    libc::SYS_setns,
    // Mount / FS table manipulation.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    // Kernel module loading.
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    // Debug / process inspection of others.
    libc::SYS_ptrace,
    // BPF and perf — kernel-level program loading.
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    // Kernel reload / shutdown.
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_reboot,
    // Swap control.
    libc::SYS_swapon,
    libc::SYS_swapoff,
    // Wall-clock manipulation.
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_adjtimex,
    // Kernel keyring.
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    // Personality changes (can disable ASLR or switch syscall semantics).
    libc::SYS_personality,
];

/// Map the build target architecture to seccompiler's enum. Returns an
/// error on unsupported arches so we never silently install a filter for
/// the wrong arch (which is a foot-gun: filters are arch-specific BPF).
fn target_arch() -> Result<TargetArch, LockdownError> {
    #[cfg(target_arch = "x86_64")]
    {
        Ok(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(TargetArch::aarch64)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(LockdownError::Seccomp(format!(
            "seccomp filter not built for target_arch {}",
            std::env::consts::ARCH
        )))
    }
}

/// Set `PR_SET_NO_NEW_PRIVS = 1`. Required for unprivileged seccomp
/// loading: without it, `seccomp(SECCOMP_SET_MODE_FILTER, ...)` returns
/// EACCES unless the caller has `CAP_SYS_ADMIN`. Setting it is also
/// one-way for this process.
fn set_no_new_privs() -> Result<(), LockdownError> {
    // SAFETY: `prctl` with PR_SET_NO_NEW_PRIVS takes a single immediate
    // value and modifies process-wide state — there is no buffer aliasing
    // or pointer-validity concern. The call is documented to never fail
    // on Linux >= 3.5, but we still check the return code.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(LockdownError::Seccomp(format!(
            "prctl(PR_SET_NO_NEW_PRIVS) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_parse_recognises_known_values() {
        assert_eq!(Profile::parse("strict").unwrap(), Some(Profile::Strict));
        assert_eq!(
            Profile::parse("net_client").unwrap(),
            Some(Profile::NetClient)
        );
        assert_eq!(Profile::parse("none").unwrap(), None);
        assert_eq!(Profile::parse("").unwrap(), None);
    }

    #[test]
    fn profile_parse_rejects_unknown() {
        assert!(Profile::parse("garbage").is_err());
    }

    #[test]
    fn build_bpf_strict_succeeds() {
        // Just verifies the rule construction + BPF compilation works on
        // the test host's arch. Doesn't actually load the filter (which
        // would poison subsequent tests).
        let bpf = build_bpf(Profile::Strict).expect("strict bpf must build");
        assert!(!bpf.is_empty(), "expected non-empty BPF program");
    }

    #[test]
    fn kill_list_contains_unshare() {
        // Sanity check: the most important syscall in our threat model
        // (escape to a new user namespace) must be in the list.
        assert!(KILL_LIST.contains(&libc::SYS_unshare));
    }
}
