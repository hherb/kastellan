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
//!
//! ## Layout
//!
//! Split 2026-07-06 to stay under the 500-LOC file cap; item bodies are
//! verbatim moves and every public `seccomp_lock::…` path is preserved via
//! the `pub use` re-exports below:
//!
//!   * **this file** — the mechanism: [`Profile`], the entry points
//!     ([`apply_from_env`], [`apply`]), the pure BPF builders
//!     ([`build_bpf`], [`build_io_uring_eperm_bpf`], [`allow_list_for`]),
//!     and the `prctl`/arch plumbing.
//!   * `allow_lists` — the per-profile syscall `const` tables
//!     ([`BASE_ALLOW`], [`NET_CLIENT_ADDITIONS`], the browser/ml/matrix
//!     additions) with their why-each-syscall commentary.
//!   * `tests` — the unit tests (filter shape + profile-boundary pins).

use std::collections::BTreeMap;

use seccompiler::{
    apply_filter_all_threads, BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch,
};

use crate::{LockdownError, SeccompReport};

mod allow_lists;
pub use allow_lists::{
    BASE_ALLOW, BASE_ALLOW_X86_64_LEGACY, BROWSER_CLIENT_ADDITIONS, BROWSER_IO_URING,
    MATRIX_CLIENT_ADDITIONS, ML_CLIENT_ADDITIONS, NET_CLIENT_ADDITIONS,
};

#[cfg(test)]
mod tests;

/// Profile selector exposed to workers via the `KASTELLAN_SECCOMP_PROFILE`
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
    /// `"browser_client"` — `net_client` **plus** the browser-specific
    /// syscalls a headless Chromium issues ([`BROWSER_CLIENT_ADDITIONS`],
    /// enumerated by the spike's `strace -f` — design spec §3.1), for the
    /// `browser-driver` worker.
    ///
    /// **`io_uring` carve-out:** Chromium probes `io_uring_setup`/
    /// `io_uring_enter`, but io_uring is a well-known sandbox-escape primitive,
    /// so it must NOT be plain-`Allow`ed. Killing it (the default for an
    /// un-listed syscall) would crash the browser instead of letting it fall
    /// back. So [`apply`] installs **a second seccomp filter** mapping just
    /// those two syscalls to `Errno(EPERM)`. The kernel runs all installed
    /// filters and takes the highest-precedence action; `ERRNO` outranks
    /// `ALLOW` (so io_uring → EPERM) while `KILL` still outranks everything
    /// (so a genuinely-unknown syscall is still killed). io_uring is therefore
    /// listed in [`allow_list_for`] (so the main filter returns `ALLOW`, not
    /// `KILL`, leaving the second filter free to downgrade it to `ERRNO`).
    BrowserClient,
    /// `"ml_client"` — `net_client` **plus** [`ML_CLIENT_ADDITIONS`]: the
    /// syscalls a torch/transformers inference worker (gliner-relex) issues
    /// beyond the net-client base, enumerated empirically on the DGX (aarch64)
    /// via the **kill-mode** loop — each run SIGSYS-dies on the first missing
    /// syscall, read back from `journalctl -k | grep type=1326` (`SECCOMP_RET_LOG`
    /// is printk-rate-limited on the DGX, so log-mode only surfaces the earliest
    /// denial per run; design spec 2026-06-16 §4). The worker is `Net::Deny`; the
    /// socket family is permitted at the syscall layer (torch opens sockets even
    /// fully offline) but the private netns gives it no route.
    MlClient,
    /// `"matrix_client"` — `net_client` **plus** [`MATRIX_CLIENT_ADDITIONS`]:
    /// the syscalls matrix-rust-sdk's SQLite crypto store needs beyond the
    /// net-client base (today just `ftruncate`), enumerated empirically on the
    /// DGX (aarch64) via the kill-mode loop (design spec 2026-06-24). For the
    /// long-lived live Matrix channel worker, which is `Net::Allowlist`
    /// (homeserver only); the socket family comes from the `net_client` base.
    /// NB: the worker builds its `tokio` runtime + sync task during pre-lockdown
    /// network init, so the filter must be TSYNC'd to cover those threads — see
    /// [`apply`].
    MatrixClient,
}

impl Profile {
    fn parse(s: &str) -> Result<Option<Self>, LockdownError> {
        match s {
            "strict" => Ok(Some(Profile::Strict)),
            "net_client" => Ok(Some(Profile::NetClient)),
            "browser_client" => Ok(Some(Profile::BrowserClient)),
            "ml_client" => Ok(Some(Profile::MlClient)),
            "matrix_client" => Ok(Some(Profile::MatrixClient)),
            "none" | "" => Ok(None),
            other => Err(LockdownError::Env(format!(
                "KASTELLAN_SECCOMP_PROFILE must be 'strict' | 'net_client' | \
                 'browser_client' | 'ml_client' | 'matrix_client' | 'none', got {other:?}"
            ))),
        }
    }
}

/// Read `KASTELLAN_SECCOMP_PROFILE` and apply the corresponding filter.
pub fn apply_from_env() -> Result<SeccompReport, LockdownError> {
    let raw = std::env::var("KASTELLAN_SECCOMP_PROFILE").unwrap_or_else(|_| "none".to_string());
    match Profile::parse(&raw)? {
        None => Ok(SeccompReport::Disabled),
        Some(p) => apply(p).map(|()| SeccompReport::Installed),
    }
}

/// Install the seccomp filter(s) for `profile`. Sets `PR_SET_NO_NEW_PRIVS`
/// first, which is required for unprivileged seccomp loading.
///
/// **Applied to every thread of the process via `SECCOMP_FILTER_FLAG_TSYNC`**
/// (`apply_filter_all_threads`), not just the calling thread. This matters when
/// a worker is *already* multi-threaded at lock-down time: the live Matrix
/// worker builds its `tokio` runtime + continuous sync task during the
/// pre-lockdown network init (login must happen before syscalls are
/// restricted), so those threads pre-exist `apply()`. The thread-local
/// `apply_filter` would bind the filter to the main thread only — which just
/// blocks in `block_on` — and leave all of matrix-sdk's network/SQLite/crypto
/// work on the unfiltered `tokio` pool (DGX-confirmed 2026-06-24: tokio threads
/// showed `/proc Seccomp:0`). TSYNC fails closed: if any sibling thread held an
/// incompatible filter the call errors and the worker exits; no kastellan
/// worker installs a filter before `apply()`, so it always succeeds. For a
/// worker that is single-threaded at lock-down (most: they lock down before
/// spawning threads, and filters auto-inherit to threads created afterwards)
/// TSYNC is equivalent to the thread-local apply.
///
/// Most profiles install exactly one filter. [`Profile::BrowserClient`] also
/// installs a separate filter mapping `io_uring_setup`/`io_uring_enter` to
/// `Errno(EPERM)` (see the variant docs): the main filter `Allow`s io_uring so
/// it isn't killed, then the io_uring filter downgrades it to EPERM, which the
/// kernel honours because `ERRNO` outranks `ALLOW` in seccomp action precedence.
///
/// **Install order matters** (only for installation, not for runtime
/// precedence). The io_uring filter is installed **first**, the restrictive
/// main filter **second**, because the second `seccomp(2)` install call must
/// itself be permitted by whatever filter is already active: the io_uring
/// filter has `mismatch_action = Allow`, so it permits the `SYS_seccomp` of the
/// second install. Were the main (`mismatch = KillProcess`) filter installed
/// first, installing the io_uring filter would be SIGSYS-killed (verified on the
/// DGX) unless we granted the worker `SYS_seccomp` — which we deliberately
/// don't. Runtime action precedence is install-order-independent, so the
/// EPERM/KILL semantics are identical either way.
pub fn apply(profile: Profile) -> Result<(), LockdownError> {
    set_no_new_privs()?;
    // For BrowserClient, install the permissive io_uring->EPERM filter FIRST so
    // its Allow-default permits the SYS_seccomp of the main filter's install.
    if matches!(profile, Profile::BrowserClient) {
        let io_uring = build_io_uring_eperm_bpf()?;
        apply_filter_all_threads(&io_uring).map_err(|e| {
            LockdownError::Seccomp(format!("apply_filter_all_threads (io_uring EPERM): {e}"))
        })?;
    }
    let main = build_bpf(profile)?;
    apply_filter_all_threads(&main)
        .map_err(|e| LockdownError::Seccomp(format!("apply_filter_all_threads: {e}")))?;
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

/// Pure builder: the **second** [`Profile::BrowserClient`] filter — a tiny
/// filter that matches only `io_uring_setup`/`io_uring_enter` and returns
/// `Errno(EPERM)`, allowing everything else.
///
/// Installed *in addition to* the main browser filter (see [`apply`]). Because
/// the kernel evaluates every installed filter and keeps the highest-precedence
/// action, and `ERRNO` outranks `ALLOW`, the net effect is io_uring → EPERM
/// even though the main filter `Allow`s it. (A genuinely-unknown syscall is
/// still `KILL`ed by the main filter, since `KILL` outranks `ERRNO`/`ALLOW`.)
///
/// EPERM (= `1`) makes Chromium fall back gracefully instead of dying on a
/// `SIGSYS` — io_uring is a known sandbox-escape primitive we deliberately
/// refuse rather than allow.
pub fn build_io_uring_eperm_bpf() -> Result<BpfProgram, LockdownError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for nr in BROWSER_IO_URING {
        rules.insert(*nr, Vec::new());
    }
    // match_action = Errno(EPERM) for io_uring; mismatch_action = Allow for
    // every other syscall (the main filter is what actually restricts those).
    const EPERM: u32 = 1;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow, // default: defer to the main filter
        SeccompAction::Errno(EPERM),
        target_arch()?,
    )
    .map_err(|e| LockdownError::Seccomp(format!("SeccompFilter::new (io_uring): {e}")))?;
    BpfProgram::try_from(filter)
        .map_err(|e| LockdownError::Seccomp(format!("BpfProgram::try_from (io_uring): {e}")))
}

/// Build the allow-list for `profile`. Returns a freshly-allocated `Vec`
/// because the contents are arch-dependent — see the `cfg` blocks in
/// `allow_lists`.
pub fn allow_list_for(profile: Profile) -> Vec<i64> {
    let mut out: Vec<i64> = BASE_ALLOW.to_vec();
    #[cfg(target_arch = "x86_64")]
    out.extend_from_slice(BASE_ALLOW_X86_64_LEGACY);
    // Both net-using profiles get the BSD-socket family.
    if matches!(
        profile,
        Profile::NetClient | Profile::BrowserClient | Profile::MlClient | Profile::MatrixClient
    ) {
        out.extend_from_slice(NET_CLIENT_ADDITIONS);
    }
    if matches!(profile, Profile::BrowserClient) {
        out.extend_from_slice(BROWSER_CLIENT_ADDITIONS);
        // io_uring is listed here so the MAIN filter returns Allow (not Kill);
        // the second filter from `build_io_uring_eperm_bpf` then downgrades it
        // to EPERM. Never reachable as a real allow — see the variant docs.
        out.extend_from_slice(BROWSER_IO_URING);
    }
    if matches!(profile, Profile::MlClient) {
        out.extend_from_slice(ML_CLIENT_ADDITIONS);
    }
    if matches!(profile, Profile::MatrixClient) {
        out.extend_from_slice(MATRIX_CLIENT_ADDITIONS);
    }
    out
}

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
