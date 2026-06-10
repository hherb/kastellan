//! `kastellan-lockdown-probe`: a tiny CLI that integration tests spawn as a
//! subprocess to verify the Landlock + seccomp filters actually do what
//! they claim. It is not a tool worker; it is a test fixture.
//!
//! Subcommands (one per invocation):
//!
//! ```text
//! lockdown-probe landlock-write <abs-path>
//!     Call lock_down(), then try to create+write a file at <abs-path>.
//!     Exit 0 on success, 1 on Landlock-denied, 2 on other I/O error.
//!
//! lockdown-probe landlock-read <abs-path>
//!     Call lock_down(), then try to open <abs-path> read-only.
//!     Exit 0 on success, 1 on denied, 2 on other I/O error.
//!
//! lockdown-probe seccomp-unshare
//!     Call lock_down(), then attempt unshare(CLONE_NEWUSER). Should be
//!     killed by SIGSYS — if we ever return, exit 0 (filter failed).
//!
//! lockdown-probe seccomp-mount
//!     Call lock_down(), then attempt mount(...). Should be SIGSYS-killed.
//!
//! lockdown-probe seccomp-getpid
//!     Call lock_down(), then call getpid(). Exit 0 on success — verifies
//!     the filter doesn't kill innocent syscalls.
//!
//! lockdown-probe seccomp-socket
//!     Call lock_down(), then attempt socket(AF_INET, SOCK_STREAM, 0).
//!     Under Profile::Strict the BSD-socket family is not allow-listed —
//!     expect SIGSYS. Under Profile::NetClient socket() is allow-listed —
//!     expect the call to succeed (or fail gracefully with EAFNOSUPPORT
//!     etc., which still proves seccomp didn't kill us). Exit 0 on socket
//!     success, 3 on socket failure with errno (still alive — useful when
//!     the test host has no IPv4 stack).
//!
//! lockdown-probe exec-after-lockdown <binary> [<binary-args>...]
//!     Call lock_down() — the seccomp filter installs and Landlock
//!     restricts FS access to `KASTELLAN_LANDLOCK_RW` — then `execve()`
//!     into `<binary>` with the remaining args. The new process
//!     inherits the seccomp filter (filters survive execve under
//!     PR_SET_NO_NEW_PRIVS, which lock_down already set). Used by
//!     `coreutils_smoke.rs` to audit BASE_ALLOW against common worker
//!     binaries (`cp`, `cat`, `mkdir`, …). If `execve` fails, exit 71.
//!
//! lockdown-probe cpu-burner
//!     Call rlimit::apply_from_env() and lock_down(), then enter a
//!     CPU-bound busy loop. If KASTELLAN_CPU_MS was set, the kernel kills
//!     the process via SIGXCPU/SIGKILL within `cpu_seconds`. Used by
//!     `rlimit_smoke.rs` to verify worker-side cpu_ms enforcement.
//!     Exits 0 if the loop runs for > 10 wall-clock seconds (the test
//!     interprets that as "rlimit failed to apply").
//!
//! lockdown-probe rlimit-report
//!     No-op subcommand: the prelude path at the top of `main` runs
//!     (apply_from_env → lock_down → both reports printed to stderr)
//!     and then we exit 0 without doing anything else. Used by
//!     `rlimit_apply_smoke.rs` to assert the happy-path FFI shape of
//!     `apply_from_env` from a fresh subprocess — keeps the
//!     `setrlimit` side-effect out of the prelude unit-test binary
//!     (where it would permanently lower the test binary's CPU budget).
//! ```
//!
//! All subcommands print `RLIMIT_REPORT: {report}` and `LOCKDOWN_REPORT: {report}`
//! to stderr first, so the parent test can confirm which layers were active.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: lockdown-probe <subcommand> [args]");
        return ExitCode::from(64);
    }

    // Apply rlimit first, matching serve_stdio's order. Cross-platform.
    let rlimit_report = match kastellan_worker_prelude::rlimit::apply_from_env() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("RLIMIT_ERROR: {e}");
            return ExitCode::from(72);
        }
    };
    eprintln!("RLIMIT_REPORT: {rlimit_report:?}");

    // Lock down next, then dispatch. If lock_down itself fails, that's a
    // distinct exit code so tests can tell "the filter machinery is
    // broken" apart from "the filter blocked the test action".
    let report = match kastellan_worker_prelude::lock_down() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("LOCKDOWN_ERROR: {e}");
            return ExitCode::from(70);
        }
    };
    eprintln!("LOCKDOWN_REPORT: {report:?}");

    match args[0].as_str() {
        "landlock-write" => probe_write(args.get(1).map(String::as_str).unwrap_or("")),
        "landlock-read" => probe_read(args.get(1).map(String::as_str).unwrap_or("")),
        #[cfg(target_os = "linux")]
        "seccomp-unshare" => probe_unshare(),
        #[cfg(target_os = "linux")]
        "seccomp-mount" => probe_mount(),
        #[cfg(target_os = "linux")]
        "seccomp-socket" => probe_socket(),
        "seccomp-getpid" => probe_getpid(),
        #[cfg(target_os = "linux")]
        "exec-after-lockdown" => probe_exec_after_lockdown(&args[1..]),
        "cpu-burner" => probe_cpu_burner(),
        "rlimit-report" => probe_rlimit_report(),
        other => {
            eprintln!("unknown subcommand: {other}");
            ExitCode::from(64)
        }
    }
}

/// Try to create + write to the given path. Maps:
///   * success → exit 0
///   * `PermissionDenied` (Landlock or DAC) → exit 1
///   * any other I/O error → exit 2
fn probe_write(path: &str) -> ExitCode {
    use std::io::Write;
    if path.is_empty() {
        eprintln!("landlock-write requires a path");
        return ExitCode::from(64);
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(mut f) => match f.write_all(b"probe").and_then(|_| f.flush()) {
            Ok(()) => ExitCode::from(0),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => ExitCode::from(1),
            Err(e) => {
                eprintln!("landlock-write {path:?} write/flush error: {e}");
                ExitCode::from(2)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => ExitCode::from(1),
        Err(e) => {
            eprintln!("landlock-write {path:?} open error: {e}");
            ExitCode::from(2)
        }
    }
}

fn probe_read(path: &str) -> ExitCode {
    if path.is_empty() {
        eprintln!("landlock-read requires a path");
        return ExitCode::from(64);
    }
    match std::fs::File::open(path) {
        Ok(_) => ExitCode::from(0),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => ExitCode::from(1),
        Err(e) => {
            eprintln!("landlock-read {path:?} open error: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(target_os = "linux")]
fn probe_unshare() -> ExitCode {
    // SAFETY: unshare() takes a single immediate flag and modifies only
    // this process's namespace state. We expect SIGSYS to terminate us
    // before this returns; if we get to read the return code, seccomp
    // failed to install or failed to match.
    let rc = unsafe { libc::unshare(libc::CLONE_NEWUSER) };
    eprintln!("UNEXPECTED: unshare returned rc={rc} errno={}", errno());
    ExitCode::from(0) // intentionally 0 — test interprets "any exit at all" as failure
}

#[cfg(target_os = "linux")]
fn probe_mount() -> ExitCode {
    // SAFETY: mount() with all-NULL pointers and an empty source/target
    // just returns EFAULT on a kernel without seccomp; with seccomp-kill
    // we expect SIGSYS before the syscall completes. Pointers are
    // c-string literals so they remain valid for the duration of the call.
    let rc = unsafe {
        libc::mount(
            c"none".as_ptr(),
            c"/tmp/__kastellan_probe_mount".as_ptr(),
            c"tmpfs".as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    eprintln!("UNEXPECTED: mount returned rc={rc} errno={}", errno());
    ExitCode::from(0)
}

/// Try to create an IPv4 TCP socket. Outcomes the test layer cares about:
///
///   * Process killed by SIGSYS → seccomp blocked socket() (expected
///     under `Profile::Strict`).
///   * Process exits 0 → socket() returned a valid fd (expected under
///     `Profile::NetClient` on a host with an IPv4 stack).
///   * Process exits 3 → socket() returned -1 with some errno but we
///     survived seccomp (also acceptable under `NetClient` — the
///     point of the test is that the *syscall entry* wasn't blocked).
#[cfg(target_os = "linux")]
fn probe_socket() -> ExitCode {
    // SAFETY: socket() with these constants takes no pointer args; on a
    // kernel without an IPv4 stack it simply returns -1/EAFNOSUPPORT
    // rather than misbehaving.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd >= 0 {
        // Close the fd to be polite; close() is in BASE_ALLOW so we
        // expect this to succeed regardless of profile.
        unsafe { libc::close(fd) };
        return ExitCode::from(0);
    }
    eprintln!("socket() returned {fd}, errno={}", errno());
    ExitCode::from(3)
}

fn probe_getpid() -> ExitCode {
    // getpid() is allowed by the deny-list; just call it to confirm the
    // filter doesn't accidentally kill innocent syscalls.
    let pid = std::process::id();
    eprintln!("getpid() = {pid}");
    ExitCode::from(0)
}

/// Lock down already happened above; now `execve` into the requested
/// binary with the remaining args. The new process inherits the
/// seccomp filter and Landlock ruleset. Used by `coreutils_smoke.rs`
/// to audit `BASE_ALLOW` against common worker binaries (issue #5).
///
/// If `exec` fails, surface the OS error and exit 71. (Successful exec
/// never returns to this stack frame.)
#[cfg(target_os = "linux")]
fn probe_exec_after_lockdown(args: &[String]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    if args.is_empty() {
        eprintln!("exec-after-lockdown requires a binary path");
        return ExitCode::from(64);
    }
    let bin = &args[0];
    let rest = &args[1..];
    let err = std::process::Command::new(bin).args(rest).exec();
    eprintln!("exec({bin:?}) failed: {err}");
    ExitCode::from(71)
}

#[cfg(target_os = "linux")]
fn errno() -> i32 {
    // Avoid pulling in libc::__errno_location signature differences across
    // glibc/musl by going through std.
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Busy-loop on CPU until either:
///   * the kernel kills us via SIGXCPU/SIGKILL (the rlimit fired), or
///   * 10 wall-clock seconds elapse (rlimit didn't fire — test failure).
///
/// Used by `rlimit_smoke.rs` to verify the worker-side rlimit layer
/// actually enforces the CPU budget the parent encoded in
/// KASTELLAN_CPU_MS. Volatile reads + writes defend against the loop
/// being optimised away under release builds.
fn probe_cpu_burner() -> ExitCode {
    use std::time::Instant;
    let start = Instant::now();
    let mut counter: u64 = 0;
    // Wall-clock cap is generous — 10s gives a 200 ms cpu_ms budget at
    // least ~50x headroom to fire SIGXCPU even on a deeply contended
    // host. If we reach the cap we exit 0, which the test treats as
    // failure (the test expects to be killed by signal).
    while start.elapsed().as_secs() < 10 {
        // `read_volatile` + `write_volatile` keep the loop alive under
        // release optimisations.
        let prev = unsafe { std::ptr::read_volatile(&counter) };
        unsafe { std::ptr::write_volatile(&mut counter, prev.wrapping_add(1)) };
    }
    eprintln!("cpu-burner: hit 10s wall-clock cap, counter={counter}");
    ExitCode::from(0)
}

/// No-op subcommand used by `rlimit_apply_smoke.rs`.
///
/// The prelude path at the top of `main` already ran (apply_from_env →
/// lock_down → both reports stderr'd). Exiting cleanly with 0 lets the
/// parent test read the printed `RLIMIT_REPORT:` line and assert the
/// happy-path FFI shape without driving any further behaviour from
/// inside the lockdown.
fn probe_rlimit_report() -> ExitCode {
    ExitCode::from(0)
}
