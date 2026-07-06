//! The seccomp syscall allow-list tables, one `const` slice per grant
//! family.
//!
//! Split out of the parent `seccomp_lock.rs` 2026-07-06 (500-LOC cap);
//! the parent re-exports every table here via `pub use` — see its
//! module doc for the split/re-export rationale. Const bodies and doc
//! comments are verbatim moves, except that six intra-doc links to
//! parent items ([`super::Profile`] variants, [`super::allow_list_for`],
//! [`super::build_io_uring_eperm_bpf`]) are re-anchored via `super::`
//! now that they cross a module boundary.
//!
//! The tables compose (see [`super::allow_list_for`]): every profile
//! starts from [`BASE_ALLOW`] (+ [`BASE_ALLOW_X86_64_LEGACY`] on
//! x86_64); net-using profiles add [`NET_CLIENT_ADDITIONS`]; the
//! browser/ml/matrix profiles add their empirically-enumerated extras
//! on top of that.

// libc 0.2 doesn't expose `SYS_sendfile` and `SYS_fadvise64` on aarch64
// even though the kernel implements both at stable ABI numbers. Define
// them locally so [`BASE_ALLOW`] compiles unchanged on both arches.
//
// Last checked: libc 0.2.186 (latest 0.2.x release as of 2026-05-14) —
// still missing on `linux/gnu/b64/aarch64`. Re-check on every libc bump;
// drop the aarch64 arms below the moment `libc::SYS_sendfile` /
// `libc::SYS_fadvise64` resolve on that target. Tracked in issue #3.
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

    // ---- Filesystem mutation ----
    // Every common coreutil that mutates the filesystem hits one of
    // these. They grant no capability beyond what `openat` already
    // does (each operates on paths the caller can already see + write
    // to via openat with O_CREAT/O_TRUNC); Landlock + bwrap bound the
    // reachable path set independently. Added in the BASE_ALLOW audit
    // for issue #5 — `mkdir`, `touch`, `mv`, `rm`, `gzip` would all
    // SIGSYS at startup without these.
    //
    //   * `mkdirat`   — `mkdir` (create directory)
    //   * `unlinkat`  — `rm`, `mv` (cross-device), `gzip` (remove src)
    //   * `renameat`  — legacy rename family member
    //   * `renameat2` — `mv` (atomic rename), modern variant with flags
    //   * `utimensat` — `touch`, `cp -p`, `tar -x` (set mtime/atime)
    //   * `linkat`    — `ln`, `cp --link` (hardlink); same capability
    //     bounds as openat — creates one extra directory entry for an
    //     already-reachable inode.
    //   * `symlinkat` — `ln -s`, `cp --symbolic-link`
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_utimensat,
    libc::SYS_linkat,
    libc::SYS_symlinkat,

    // ---- Filesystem permission mutation ----
    // `fchmodat` + `fchmod` let callers change a file's mode bits.
    // The reachable set is bounded by what the caller can already
    // open() + write() to — Landlock + DAC still gate this — and a
    // worker writing 0644 vs 0755 to its own scratch file isn't a
    // capability uplift. Required by `tar -x` (preserve modes) and
    // by any worker that creates an executable script in its scratch
    // dir.
    libc::SYS_fchmodat,
    libc::SYS_fchmod,

    // `fchown` + `fchownat`: change file ownership. The kernel
    // already restricts these for non-root processes — a worker
    // running as uid `kastellan` cannot `chown(uid=root)`; the most a
    // worker can do is preserve or set ownership to its own uid/gid.
    // Required by `gzip` (preserves group on the compressed
    // replacement) and `cp -p` (preserves ownership when permissions
    // permit). Not a capability uplift over what the worker's uid
    // already has via `openat`. Issue #5 audit pin.
    libc::SYS_fchown,
    libc::SYS_fchownat,

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
    // Legacy filesystem-mutation variants — the modern `at`-suffix
    // syscalls are in [`BASE_ALLOW`]; on x86_64 the original names
    // also exist and some statically-linked or older binaries still
    // call them. Same capability bounds as the `at`-suffix variants
    // (issue #5 BASE_ALLOW audit).
    libc::SYS_unlink,
    libc::SYS_rename,
    libc::SYS_mkdir,
    libc::SYS_rmdir,
    libc::SYS_utime,
    libc::SYS_utimes,
    libc::SYS_futimesat,
    libc::SYS_chmod,
    libc::SYS_link,
    libc::SYS_symlink,
    libc::SYS_creat,
    // Legacy chown family; kernel-enforced capability bounds apply
    // identically to the `fchown`/`fchownat` entries in BASE_ALLOW.
    libc::SYS_chown,
    libc::SYS_lchown,
];

#[cfg(not(target_arch = "x86_64"))]
pub const BASE_ALLOW_X86_64_LEGACY: &[i64] = &[];

/// BSD-socket family. Permitted only under [`Profile::NetClient`](super::Profile::NetClient).
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

/// Browser-specific syscalls a headless Chromium issues on top of
/// [`NET_CLIENT_ADDITIONS`]. Permitted only under
/// [`Profile::BrowserClient`](super::Profile::BrowserClient).
///
/// Enumerated by the spike via `strace -f -c` of the full bwrapped Chromium
/// process tree, then diffed against the `net_client` set (design spec §3.1).
/// (`pivot_root`/`umount2` also appeared in the trace but are **bwrap's own**
/// container setup, run before the worker self-applies the filter, so they are
/// deliberately NOT here.) Every entry exists on both `x86_64` and `aarch64`.
pub const BROWSER_CLIENT_ADDITIONS: &[i64] = &[
    libc::SYS_fallocate,
    libc::SYS_ftruncate,
    libc::SYS_getresgid,
    libc::SYS_getresuid,
    libc::SYS_inotify_add_watch,
    libc::SYS_inotify_init1,
    libc::SYS_memfd_create,
    libc::SYS_pidfd_open,
    libc::SYS_restart_syscall,
    // capget + capset: both are required post-filter and both are confirmed by
    // the DGX acceptance gate (issue #281, 2026-06-15) — removing either breaks
    // the render. Without capget, Playwright's bundled Node.js driver is
    // SIGSYS-killed at startup (surfacing as `'PlaywrightContextManager' has no
    // attr '_playwright'`). Without capset, Chromium crashes while spawning a
    // page/renderer (`Browser.new_page: ... browser has been closed`) — its
    // zygote/process setup adjusts the same-process capability set. Both operate
    // ONLY on this process's own capability bitmask and grant no privilege
    // uplift: the worker runs inside bwrap's unprivileged user namespace
    // (`--unshare-all`), where any capability is namespaced — it confers nothing
    // against the host and cannot be mapped out of the userns. PR_SET_NO_NEW_PRIVS
    // additionally blocks gaining privileges across the inevitable execve. So
    // capset can at most shuffle caps the kernel already confined to this jail;
    // it cannot raise host privilege. (If bwrap were ever run setuid or with
    // --cap-add, this reasoning would need revisiting — neither is done here.)
    libc::SYS_capget,
    libc::SYS_capset,
];

/// The `io_uring` syscalls Chromium probes. Listed in
/// [`allow_list_for`](super::allow_list_for) for
/// [`Profile::BrowserClient`](super::Profile::BrowserClient) so the **main**
/// filter returns `Allow` (not `Kill`) — but a **second** filter
/// ([`build_io_uring_eperm_bpf`](super::build_io_uring_eperm_bpf)) downgrades
/// them to `Errno(EPERM)`. io_uring is a known sandbox-escape primitive; we
/// refuse it gracefully rather than allow it or crash the browser. See the
/// [`Profile::BrowserClient`](super::Profile::BrowserClient) docs for the
/// precedence reasoning.
pub const BROWSER_IO_URING: &[i64] = &[libc::SYS_io_uring_setup, libc::SYS_io_uring_enter];

/// torch/transformers-specific syscalls beyond [`NET_CLIENT_ADDITIONS`].
/// Permitted only under [`Profile::MlClient`](super::Profile::MlClient)
/// (gliner-relex).
///
/// **Enumerated empirically** on the DGX (aarch64) by tracing a real
/// `knowledgator/gliner-relex-multi-v1.0` worker — both the `device="auto"`
/// CUDA-availability probe AND the CPU model-load + `extract` it falls back to
/// inside the jail (no `/dev/nvidia*` bound) — and diffing the observed syscalls
/// against the bare `net_client` allow-list (design spec 2026-06-16 §4). Every
/// observed syscall was already covered by [`BASE_ALLOW`] +
/// [`NET_CLIENT_ADDITIONS`] **except** the five below. (The trace also confirmed
/// torch issues `socket`/`bind`/`connect` even fully offline — hence the
/// `net_client` base — and probed **no** escape primitives or io_uring.)
///
/// Escape primitives (namespace/mount/ptrace/bpf/io_uring/keyring) are NEVER
/// added here — they stay killed by the default action.
pub const ML_CLIENT_ADDITIONS: &[i64] = &[
    // NUMA memory-policy syscalls PyTorch's threadpool / OpenMP arena
    // allocator issues while placing tensor memory across NUMA nodes
    // (observed: `mbind` ×20, `get_mempolicy` ×1) during the CPU inference path.
    // `set_mempolicy` / `migrate_pages` were NOT observed and are deliberately
    // left out (add iff a future trace shows them).
    libc::SYS_mbind,
    libc::SYS_get_mempolicy,
    // Memory-locking syscalls libcuda issues while pinning host pages during the
    // `device="auto"` CUDA-availability probe the worker runs at startup. Not hit
    // in the current CPU-only jail (no `/dev/nvidia*` bound, so the probe fails
    // before pinning), but a GPU-bound gliner deployment reaches them — included
    // forward-looking. `mlock2` (the modern flag-taking variant) was NOT observed.
    // Both lock only this process's OWN pages (bounded by `RLIMIT_MEMLOCK`); no
    // namespace/privilege/escape surface — same benign class as `madvise`.
    //
    // CAVEAT: unlike the other four (all DGX-observed and therefore exercised by
    // the real-model e2e gate), these two are the ONLY grants in this set NOT
    // hit on the current CPU-only DGX — so the e2e suite does not cover them. They
    // rest on the benign-class argument above, not an empirical kill-mode trace.
    // If the GPU path is never deployed, removing them is safe (a future CPU-only
    // trace would not regress); they're kept so a `device=auto` GPU host does not
    // SIGSYS on first model load.
    libc::SYS_mlock,
    libc::SYS_munlock,
    // `mknodat` — the worker creates a special file (FIFO/regular) in its writable
    // scratch during startup; confirmed load-bearing by the DGX kill-mode gate
    // (`syscall=33`). Same filesystem-mutation family as `mkdirat`/`unlinkat` in
    // [`BASE_ALLOW`] and bounded identically: it can only create nodes where the
    // worker can already write (bwrap `/tmp` tmpfs + Landlock). Device-node
    // creation (`S_IFCHR`/`S_IFBLK`) needs `CAP_MKNOD` the unprivileged user-ns
    // worker lacks against the host, and a device node minted inside an
    // unprivileged user-ns cannot be opened to reach real hardware — so no
    // device-access surface. Scoped to `ml_client` (not widened into BASE_ALLOW).
    libc::SYS_mknodat,
];

/// matrix-rust-sdk-specific syscalls beyond [`NET_CLIENT_ADDITIONS`].
/// Permitted only under [`Profile::MatrixClient`](super::Profile::MatrixClient)
/// (the live Matrix channel worker).
///
/// **Enumerated empirically** on the DGX (aarch64) by running the real
/// `live-matrix` worker (login + E2E sync + send/recv) against a throwaway
/// loopback homeserver under the kill-mode filter, and diffing the observed
/// syscalls against the bare `net_client` allow-list (design spec 2026-06-24
/// §A). Three converging lines of evidence: (1) under bare `net_client` a
/// `tokio-rt-worker` thread `SIGSYS`-died on `syscall=46` (`ftruncate`) during
/// the SQLite crypto-store's WAL maintenance after ~tens of seconds of sync;
/// (2) a `SECCOMP_RET_LOG` run logged **only** `syscall=46` beyond `net_client`
/// across init + 45s sync + a send/recv round-trip; (3) with `ftruncate` added,
/// a 50s kill-mode session (all 21 threads `Seccomp:2` via TSYNC) survived with
/// **zero** denials. Everything else matrix-rust-sdk needs was already covered
/// by [`BASE_ALLOW`] + [`NET_CLIENT_ADDITIONS`].
///
/// `ftruncate` truncates an already-open fd the worker owns (the SQLite DB /
/// WAL inside its writable store dir) — same benign file-mutation class as the
/// `write`/`fallocate`-adjacent calls in [`BASE_ALLOW`], bounded by Landlock
/// (RW = the store dir only). No namespace/privilege/escape surface. Escape
/// primitives (namespace/mount/ptrace/bpf/io_uring/keyring) are NEVER added.
pub const MATRIX_CLIENT_ADDITIONS: &[i64] = &[
    // SQLite (matrix-sdk-sqlite crypto + state store) truncates its WAL/journal
    // during checkpointing on a long-lived connection. DGX kill-mode confirmed
    // load-bearing (`syscall=46`); not hit by a sub-2s round-trip, only by a
    // long-running worker — i.e. exactly production. (Also present in
    // [`BROWSER_CLIENT_ADDITIONS`] for Chromium's on-disk caches.)
    libc::SYS_ftruncate,
];
