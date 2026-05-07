//! seccomp-bpf syscall filter, applied from inside the worker process.
//!
//! ## Stage 2: per-profile allow-list
//!
//! This is *Phase 0 hardening, stage 2*. We use an **allow-list** — every
//! syscall not on the list triggers `SECCOMP_RET_KILL_PROCESS` (SIGSYS) and
//! the worker dies. Two profiles diverge: [`Profile::Strict`] permits the
//! base set only; [`Profile::NetClient`] also permits the BSD-socket family
//! (`socket`, `connect`, `sendmsg`, …) so workers that need outbound HTTP
//! (egress proxy, web-fetch) can reach the network.
//!
//! Why an allow-list instead of the original stage-1 deny-list:
//!
//!   * Defence in depth: a deny-list is one CVE away from being defeated by
//!     a syscall variant we forgot. The allow-list fails closed — a
//!     newly-exposed catastrophic syscall (e.g. some future namespace API)
//!     is killed by default.
//!   * Capability differentiation: the deny-list could not distinguish
//!     `Strict` from `NetClient` because both denied the same set. The
//!     allow-list lets us draw a precise line between profiles.
//!
//! ## Coverage
//!
//! The base set was derived empirically from `strace -f` of a real
//! shell-exec round-trip plus the tokio/std runtime requirements (futex,
//! rseq, clone3, epoll_*). It currently lists ~110 syscalls on aarch64 plus
//! ~20 x86_64-only legacy variants (open/stat/pipe/dup2/poll/select/…) so
//! the same crate compiles on both arches.
//!
//! Catastrophic syscalls deliberately *omitted* (and therefore killed by
//! the default action):
//!
//!   * Namespace manipulation: `unshare`, `setns`
//!   * Mount table changes: `mount`, `umount2`, `pivot_root`,
//!     `move_mount`, `open_tree`, `fsopen`, `fsmount`, `fsconfig`
//!   * Kernel module loading: `init_module`, `finit_module`, `delete_module`
//!   * Process inspection of others: `ptrace`, `process_vm_readv`,
//!     `process_vm_writev`
//!   * BPF and perf: `bpf`, `perf_event_open`, `kcmp`
//!   * Kernel reload: `kexec_load`, `kexec_file_load`, `reboot`
//!   * Swap: `swapon`, `swapoff`
//!   * Wall-clock manipulation: `settimeofday`, `clock_settime`,
//!     `clock_adjtime`, `adjtimex`
//!   * Kernel keyring: `keyctl`, `add_key`, `request_key`
//!   * Personality / quotactl / sysfs / acct / iopl / ioperm
//!   * io_uring: `io_uring_setup`, `io_uring_enter`, `io_uring_register`
//!     (we do not use it; if a future worker needs it, the right move is a
//!     dedicated `Profile::IoUringClient` rather than a global allow)
//!
//! ## Why this also runs under bwrap
//!
//! `bwrap` already gives namespace isolation. Defence in depth: a kernel
//! bug (or a future relaxation of bwrap config) does not get the worker
//! `unshare(CLONE_NEWUSER)` privileges it could use to escape into a fresh
//! user namespace. Same for `mount`: bwrap removed the capability, but
//! seccomp removes the syscall entry point too.

use std::collections::BTreeMap;

use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch,
};

use crate::{LockdownError, SeccompReport};

/// Profile selector exposed to workers via the `HHAGENT_SECCOMP_PROFILE`
/// env var.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// `"strict"` — base allow-list only. Suitable for workers that have
    /// no legitimate need for the BSD-socket family (e.g. shell-exec,
    /// python-exec without net).
    Strict,
    /// `"net_client"` — base allow-list **plus** the BSD-socket family
    /// (`socket`, `connect`, `bind`, `listen`, `accept`/`accept4`,
    /// `setsockopt`, `getsockopt`, `getpeername`, `getsockname`,
    /// `recvfrom`, `sendto`, `recvmsg`/`sendmsg`, `recvmmsg`/`sendmmsg`,
    /// `shutdown`, `socketpair`). Suitable for the egress proxy worker
    /// and any future net-using worker. Note: this *only* lifts the
    /// syscall-entry restriction — actual network reach is still gated by
    /// `bwrap --share-net`/`unshare --net` and (Phase 3) the egress proxy.
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

/// Read `HHAGENT_SECCOMP_PROFILE` and apply the corresponding filter.
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
    let allow_syscalls = allow_list_for(profile);
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in allow_syscalls {
        // Empty rule vec = unconditional match for that syscall number.
        // The match action is the filter's `match_action` (Allow, below).
        rules.insert(nr, Vec::new());
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,  // default for un-listed syscalls
        SeccompAction::Allow,        // for listed syscalls
        target_arch()?,
    )
    .map_err(|e| LockdownError::Seccomp(format!("SeccompFilter::new: {e}")))?;
    BpfProgram::try_from(filter)
        .map_err(|e| LockdownError::Seccomp(format!("BpfProgram::try_from: {e}")))
}

/// Build the allow-list for `profile`. Returns a freshly-allocated `Vec`
/// because the contents are arch-dependent — see the `cfg` blocks below.
pub fn allow_list_for(profile: Profile) -> Vec<i64> {
    let mut out: Vec<i64> = BASE_ALLOW.to_vec();
    #[cfg(target_arch = "x86_64")]
    out.extend_from_slice(BASE_ALLOW_X86_64_LEGACY);
    if matches!(profile, Profile::NetClient) {
        out.extend_from_slice(NET_CLIENT_ADDITIONS);
    }
    out
}

// libc 0.2 doesn't expose `SYS_sendfile` and `SYS_fadvise64` on aarch64
// even though the kernel implements both at stable ABI numbers. Define
// them locally so [`BASE_ALLOW`] compiles unchanged on both arches.
//
// Numbers come from `arch/arm64/include/uapi/asm-generic/unistd.h`
// (sendfile = 71, fadvise64 = 223) and `arch/x86/entry/syscalls/syscall_64.tbl`
// (sendfile = 40, fadvise64 = 221). On x86_64 we forward to libc to
// guarantee the constant matches whatever the toolchain compiled with;
// on aarch64 we hardcode (the kernel ABI is stable). Any other arch
// will fail to compile here, which is the correct behaviour — adding
// support means making a deliberate choice.
#[cfg(target_arch = "aarch64")]
const SYS_SENDFILE: i64 = 71;
#[cfg(target_arch = "aarch64")]
const SYS_FADVISE64: i64 = 223;
#[cfg(target_arch = "x86_64")]
const SYS_SENDFILE: i64 = libc::SYS_sendfile;
#[cfg(target_arch = "x86_64")]
const SYS_FADVISE64: i64 = libc::SYS_fadvise64;

/// Base allow-list. Every syscall here exists on both `x86_64` and
/// `aarch64` (the two arches we currently target). Newer/legacy variants
/// that only exist on one arch live in [`BASE_ALLOW_X86_64_LEGACY`].
///
/// Grouped by purpose for readability. If you add a new entry, leave a
/// comment so the next reader knows *why*.
pub const BASE_ALLOW: &[i64] = &[
    // ---- Process exit & identity ----
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_getpid,
    libc::SYS_gettid,
    libc::SYS_getppid,
    libc::SYS_getpgid,
    libc::SYS_setpgid,
    libc::SYS_getsid,
    libc::SYS_setsid,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_get_robust_list,

    // ---- Fork / exec / wait ----
    // clone3 is the modern fork primitive used by glibc/musl; clone is
    // kept for compat. execve+execveat for shell-exec child spawn.
    libc::SYS_clone,
    libc::SYS_clone3,
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_wait4,
    libc::SYS_waitid,

    // ---- Memory management ----
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_mprotect,
    libc::SYS_madvise,

    // ---- File descriptors: open / close / dup / pipe ----
    // openat covers `open` (legacy); openat2 is its O_PATH-aware sibling.
    libc::SYS_openat,
    libc::SYS_openat2,
    libc::SYS_close,
    libc::SYS_close_range,
    libc::SYS_dup,
    libc::SYS_dup3,
    libc::SYS_pipe2,
    libc::SYS_fcntl,

    // ---- Read / write / seek ----
    libc::SYS_read,
    libc::SYS_readv,
    libc::SYS_pread64,
    libc::SYS_preadv,
    libc::SYS_preadv2,
    libc::SYS_write,
    libc::SYS_writev,
    libc::SYS_pwrite64,
    libc::SYS_pwritev,
    libc::SYS_pwritev2,
    libc::SYS_lseek,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    // Bulk file-copy primitives. GNU coreutils (`cp`, `cat`) call
    // `copy_file_range` first and fall back to `sendfile`; both also
    // pre-hint the kernel via `fadvise64`. Without these, `cp src dst`
    // dies with SIGSYS on its first read. They copy *between two
    // already-open file descriptors* and grant no capability beyond
    // what `openat` already does; `fadvise64` is a pure readahead hint
    // with no cross-process surface. (`SYS_sendfile` and `SYS_fadvise64`
    // are unexposed by libc 0.2 on aarch64, so the numeric ABI is
    // pulled in via [`SYS_SENDFILE`] / [`SYS_FADVISE64`] above.)
    libc::SYS_copy_file_range,
    SYS_SENDFILE,
    SYS_FADVISE64,

    // ---- Filesystem metadata ----
    // newfstatat is the universal stat on aarch64; statx is the modern
    // superset; faccessat/faccessat2 cover access checks.
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_statfs,
    libc::SYS_fstatfs,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_readlinkat,
    libc::SYS_getcwd,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_getdents64,
    libc::SYS_umask,

    // ---- Polling / event notification ----
    // epoll_pwait2 is the timespec-aware variant; ppoll/pselect6 are the
    // sigmask-aware modern poll/select.
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_pwait2,
    libc::SYS_eventfd2,
    libc::SYS_signalfd4,
    libc::SYS_ppoll,
    libc::SYS_pselect6,

    // ---- Timers & clocks ----
    // clock_settime / clock_adjtime are deliberately omitted (in the
    // catastrophic deny list).
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_gettimeofday,
    libc::SYS_nanosleep,
    libc::SYS_timer_create,
    libc::SYS_timer_settime,
    libc::SYS_timer_gettime,
    libc::SYS_timer_delete,
    libc::SYS_timer_getoverrun,
    libc::SYS_timerfd_create,
    libc::SYS_timerfd_settime,
    libc::SYS_timerfd_gettime,
    libc::SYS_getitimer,
    libc::SYS_setitimer,

    // ---- Signals ----
    // rt_sigreturn is implicit on every signal-handler return; missing it
    // would kill the worker as soon as any signal is delivered.
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_rt_sigtimedwait,
    libc::SYS_rt_sigsuspend,
    libc::SYS_rt_sigpending,
    libc::SYS_rt_sigqueueinfo,
    libc::SYS_sigaltstack,
    libc::SYS_kill,
    libc::SYS_tkill,
    libc::SYS_tgkill,

    // ---- Threading / scheduling primitives ----
    // futex powers std::sync, tokio mutex/oneshot, glibc pthread cond.
    // futex_waitv is the multi-futex variant added in 5.16; harmless to
    // allow even if unused.
    libc::SYS_futex,
    libc::SYS_futex_waitv,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_sched_setaffinity,
    libc::SYS_sched_getparam,
    libc::SYS_sched_getscheduler,
    libc::SYS_sched_get_priority_max,
    libc::SYS_sched_get_priority_min,
    libc::SYS_rseq,

    // ---- Random + identity ----
    libc::SYS_getrandom,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_getgroups,

    // ---- Resource limits / accounting ----
    libc::SYS_prlimit64,
    libc::SYS_getrusage,
    libc::SYS_getpriority,
    libc::SYS_setpriority,

    // ---- Misc runtime essentials ----
    // prctl is a multiplexer; PR_SET_NO_NEW_PRIVS is set *before* this
    // filter installs, so most prctls happen pre-filter, but we still
    // permit it for thread-name (PR_SET_NAME), seccomp-introspection,
    // etc.
    libc::SYS_prctl,
    libc::SYS_ioctl,
    libc::SYS_sysinfo,
    libc::SYS_uname,
    libc::SYS_membarrier,

    // ---- Landlock self-restriction (already used pre-filter; allowing
    // these means a future re-entry doesn't kill the worker) ----
    libc::SYS_landlock_create_ruleset,
    libc::SYS_landlock_add_rule,
    libc::SYS_landlock_restrict_self,
];

/// x86_64-only legacy syscalls. On `aarch64` these were never assigned a
/// number (the modern variants are the only entry point). Listing them
/// behind `cfg(target_arch = "x86_64")` keeps the same crate building on
/// both arches without `unused_imports` warnings.
#[cfg(target_arch = "x86_64")]
pub const BASE_ALLOW_X86_64_LEGACY: &[i64] = &[
    libc::SYS_open,
    libc::SYS_stat,
    libc::SYS_lstat,
    libc::SYS_access,
    libc::SYS_pipe,
    libc::SYS_dup2,
    libc::SYS_signalfd,
    libc::SYS_eventfd,
    libc::SYS_epoll_create,
    libc::SYS_epoll_wait,
    libc::SYS_poll,
    libc::SYS_select,
    libc::SYS_fork,
    libc::SYS_vfork,
    libc::SYS_arch_prctl,
    libc::SYS_getdents,
    libc::SYS_readlink,
    libc::SYS_alarm,
    libc::SYS_pause,
];

#[cfg(not(target_arch = "x86_64"))]
pub const BASE_ALLOW_X86_64_LEGACY: &[i64] = &[];

/// BSD-socket family. Permitted only under [`Profile::NetClient`].
///
/// These syscall numbers exist on both `x86_64` (where they were once
/// multiplexed via `socketcall` on i386 but are direct on x86_64) and
/// `aarch64`.
pub const NET_CLIENT_ADDITIONS: &[i64] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_setsockopt,
    libc::SYS_getsockopt,
    libc::SYS_getpeername,
    libc::SYS_getsockname,
    libc::SYS_recvfrom,
    libc::SYS_sendto,
    libc::SYS_recvmsg,
    libc::SYS_sendmsg,
    libc::SYS_recvmmsg,
    libc::SYS_sendmmsg,
    libc::SYS_shutdown,
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
    fn build_bpf_net_client_succeeds() {
        let bpf = build_bpf(Profile::NetClient).expect("net_client bpf must build");
        assert!(!bpf.is_empty(), "expected non-empty BPF program");
    }

    #[test]
    fn unshare_is_not_in_allow_list() {
        // The most important syscall in our threat model — escape into a
        // fresh user namespace — must NOT appear in any profile's
        // allow-list. If this regresses, the worker can re-enter
        // unshare(CLONE_NEWUSER) and bypass the namespace boundary.
        for profile in [Profile::Strict, Profile::NetClient] {
            let allow = allow_list_for(profile);
            assert!(
                !allow.contains(&libc::SYS_unshare),
                "unshare must never be allow-listed (profile {profile:?})"
            );
            assert!(
                !allow.contains(&libc::SYS_mount),
                "mount must never be allow-listed (profile {profile:?})"
            );
            assert!(
                !allow.contains(&libc::SYS_ptrace),
                "ptrace must never be allow-listed (profile {profile:?})"
            );
            assert!(
                !allow.contains(&libc::SYS_bpf),
                "bpf must never be allow-listed (profile {profile:?})"
            );
        }
    }

    #[test]
    fn socket_is_only_in_net_client_profile() {
        // The hard line between Strict and NetClient: socket() and the
        // BSD-socket family must be allowed under NetClient and killed
        // under Strict. This is the test that proves the two profiles
        // differ — if it ever regresses, NetClient and Strict have
        // collapsed back into the same set.
        let strict = allow_list_for(Profile::Strict);
        let net_client = allow_list_for(Profile::NetClient);

        assert!(
            !strict.contains(&libc::SYS_socket),
            "Strict must not allow socket()"
        );
        assert!(
            net_client.contains(&libc::SYS_socket),
            "NetClient must allow socket()"
        );

        // Sanity: the difference is exactly NET_CLIENT_ADDITIONS.
        for nr in NET_CLIENT_ADDITIONS {
            assert!(
                !strict.contains(nr),
                "syscall {nr} present in Strict but should be NetClient-only"
            );
            assert!(
                net_client.contains(nr),
                "syscall {nr} missing from NetClient"
            );
        }
    }

    #[test]
    fn essentials_are_in_base_allow_list() {
        // Smoke test: a handful of syscalls that *every* worker hits
        // during normal operation must be in the base list. If one of
        // these regresses, the worker dies in a confusing way (SIGSYS at
        // startup with no obvious cause) — surface the failure here
        // instead.
        for nr in [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_close,
            libc::SYS_openat,
            libc::SYS_mmap,
            libc::SYS_munmap,
            libc::SYS_mprotect,
            libc::SYS_brk,
            libc::SYS_futex,
            libc::SYS_clone3,
            libc::SYS_execve,
            libc::SYS_wait4,
            libc::SYS_exit_group,
            libc::SYS_rt_sigreturn,
        ] {
            assert!(
                BASE_ALLOW.contains(&nr),
                "essential syscall {nr} missing from BASE_ALLOW"
            );
        }
    }
}
