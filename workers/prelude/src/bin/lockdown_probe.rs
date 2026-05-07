//! `hhagent-lockdown-probe`: a tiny CLI that integration tests spawn as a
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
//! ```
//!
//! All subcommands print `LOCKDOWN_REPORT: {report}` to stderr first, so
//! the parent test can confirm which layers were active.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: lockdown-probe <subcommand> [args]");
        return ExitCode::from(64);
    }

    // Lock down first, then dispatch. If lock_down itself fails, that's a
    // distinct exit code so tests can tell "the filter machinery is
    // broken" apart from "the filter blocked the test action".
    let report = match hhagent_worker_prelude::lock_down() {
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
            b"none\0".as_ptr() as *const _,
            b"/tmp/__hhagent_probe_mount\0".as_ptr() as *const _,
            b"tmpfs\0".as_ptr() as *const _,
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

#[cfg(target_os = "linux")]
fn errno() -> i32 {
    // Avoid pulling in libc::__errno_location signature differences across
    // glibc/musl by going through std.
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
