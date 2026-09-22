//! #749: the neutralising panic hook must not turn a panic into a
//! message-less `SIGABRT` when stderr is broken.
//!
//! #742's hook renders a panic itself rather than delegating to the default
//! one, and it writes with `eprintln!` for the same libtest-capture reason
//! `worker_stderr` does. But `eprintln!` **panics** when the write fails, and
//! a panic raised *inside a panic hook* is a panic while panicking, which
//! aborts the process immediately.
//!
//! That is strictly worse than the #733 case whose probe this borrows. There,
//! a worker report was lost. Here, the panic message that would have explained
//! the failure is lost too — and so is libtest's own `test result: FAILED`
//! line, because the process never gets to print it.
//!
//! ## Measured, not argued
//!
//! A child whose fd 2 is the write end of a pipe with no reader, panicking
//! under each hook (macOS 27, arm64):
//!
//! | hook | outcome |
//! | --- | --- |
//! | unguarded `eprintln!` | **signal 6 (SIGABRT)**, nothing on any stream |
//! | guarded by `stderr_is_writable()` | exit 101, message intact |
//!
//! ⚠️ **Only `EPIPE` does this.** A merely CLOSED fd 2 (`2>&-`) is harmless:
//! `std` swallows `EBADF` on stdio via `handle_ebadf`, so `eprintln!` returns
//! `Ok` and never panics. Measured during #745's review and re-measured here.
//! So the fixture below needs a real broken **pipe**; closing the descriptor
//! would prove nothing.
//!
//! ## Why a child process, and why `--nocapture`
//!
//! Mirrors `core/tests/worker_report_broken_stderr_e2e.rs`, for the same two
//! reasons:
//!
//! * **A child**, because the fixture destroys its own fd 2 and then panics.
//!   There is no way back from either.
//! * **`--nocapture`**, because libtest captures `eprintln!` through
//!   `std::io::set_output_capture` — into a buffer, not onto fd 2. Under a
//!   capturing run the hook would write happily into the buffer, the write
//!   would not fail, and this suite would pass against a deleted guard. The
//!   capture is precisely what has to be out of the way for the bug to exist.
//!
//! ⚠️ **The parent asserts the child FAILED its test but was not SIGNALLED.**
//! A fixture that panics is supposed to fail; what must not happen is the
//! abort. So "the child exited non-zero" is not the property — "the child
//! exited non-zero *and libtest reported the failure itself*" is, and that
//! second half is what a SIGABRT destroys.

use std::process::Command;

/// Set by the parent on the child it launches. The fixture does nothing
/// without it.
///
/// ⚠️ **Load-bearing, not belt-and-braces with `#[ignore]`.** This fixture
/// deliberately destroys its own stderr and panics, and this tree documents
/// `cargo test … -- --ignored` as the way to run the Firecracker tier — which
/// would otherwise run it in a process that still had tests to report.
const FIXTURE_ENV: &str = "KASTELLAN_PANIC_HOOK_BROKEN_STDERR_FIXTURE";

/// Printed to **stdout** by the fixture immediately before it panics.
///
/// The verdict cannot travel on stderr — that is the thing the fixture broke —
/// and it must be printed *before* the panic, because after it the process is
/// either unwinding or already gone.
const ABOUT_TO_PANIC: &str = "kastellan-test: hook installed, stderr broken, panicking now";

/// The panic payload. Distinctive enough that finding it on any stream would
/// mean the hook wrote after all.
const PAYLOAD: &str = "kastellan-test: panic-hook broken-stderr probe";

/// `true` when this process was launched by [`run_inner_fixture`].
///
/// Honours the tree's knob dialect (`1|true|yes|on`) rather than inventing a
/// second one.
fn is_the_child() -> bool {
    std::env::var(FIXTURE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Inner fixture: install the hook, make fd 2 a pipe whose reader has gone,
/// then panic.
///
/// Deliberately contains no `assert!` after the breakage: a failing assertion
/// would panic, which is what we are measuring, and it could not render its
/// own message anyway.
#[test]
#[ignore = "inner fixture: destroys its own stderr and panics; run by its parent in a child process"]
fn inner_fixture_panics_with_a_broken_stderr_pipe() {
    if !is_the_child() {
        return;
    }
    kastellan_tests_common::panic_hook::install_once();

    let mut fds = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "make a pipe");
    let (read, write) = (fds[0], fds[1]);
    // Put the pipe's write end where stderr was, then hang up the reader —
    // this is `… | head` finishing.
    assert!(unsafe { libc::dup2(write, libc::STDERR_FILENO) } >= 0, "dup2 onto fd 2");
    unsafe {
        libc::close(write);
        libc::close(read);
    }
    // From here on nothing in this process may touch stderr and survive.
    println!("{ABOUT_TO_PANIC}");
    panic!("{PAYLOAD}");
}

/// The property: a panic with a broken stderr is still a *reported* test
/// failure, not an abort.
#[test]
fn a_panic_with_a_broken_stderr_does_not_abort_the_process() {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args([
            "--exact",
            "inner_fixture_panics_with_a_broken_stderr_pipe",
            "--ignored",
            "--test-threads",
            "1",
            "--nocapture",
        ])
        .env(FIXTURE_ENV, "1")
        .output()
        .expect("re-exec this test binary to run the inner fixture");
    let signalled: Option<i32> = std::os::unix::process::ExitStatusExt::signal(&out.status);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let both = format!("--- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}");

    // POSITIVE CONTROL. `--exact` on a misspelled name exits 0 having run
    // NOTHING, and "the child was not signalled" is trivially true of a child
    // that did nothing. This is the trap that has reported a whole mutation
    // batch as surviving against zero tests.
    assert!(
        stdout.contains(ABOUT_TO_PANIC),
        "the fixture never reached its panic, so this run proves nothing. Either the name \
         filter did not select it or `{FIXTURE_ENV}` did not reach it.\n{both}"
    );

    // THE PROPERTY, half one: no abort.
    assert_eq!(
        signalled, None,
        "a panic with a broken stderr must not kill the process with a signal. Signal 6 here \
         is #749: the hook's own `eprintln!` panicked while panicking, which aborts with no \
         output at all. Check that the hook still returns early on \
         `kastellan_core::worker_stderr::stderr_is_writable()`.\n{both}"
    );

    // THE PROPERTY, half two — and the half a SIGABRT destroys. libtest
    // accounts for the failure through `catch_unwind`, not through the hook,
    // so a hook that declines to print must still leave a reported failure.
    assert!(
        stdout.contains("test result: FAILED. 0 passed; 1 failed"),
        "libtest must still report the panic as a test failure. Losing this line is what \
         makes #749 worse than the #733 case it borrows its probe from: there a report went \
         missing, here the whole account of the failure does.\n{both}"
    );

    // And the payload must not have reached a stream by some other route,
    // which would mean the fixture never actually broke stderr.
    assert!(
        !stderr.contains(PAYLOAD),
        "the panic payload appeared on the child's stderr, so the fixture did not actually \
         break it and this run proves nothing.\n{both}"
    );
}
