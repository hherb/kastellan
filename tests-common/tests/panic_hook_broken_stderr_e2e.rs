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
//! | guarded by `stderr_is_writable()` | exit 101, libtest still reports the failure |
//!
//! ⚠️ **The panic TEXT does not survive either way, and this suite asserts as
//! much** (`!stderr.contains(PAYLOAD)`). On that fd nothing can be written;
//! the guard's job is to keep the *process* alive so libtest's own
//! `test result: FAILED` line still gets printed. Read the row above as "the
//! failure is still accounted for", never as "the message is intact".
//!
//! ⚠️ A merely CLOSED fd 2 (`2>&-`) is harmless: `std` swallows `EBADF` on
//! stdio via `handle_ebadf`, so `eprintln!` returns `Ok` and never panics.
//! Measured during #745's review and re-measured here. So the fixture below
//! needs a real broken **pipe**; closing the descriptor would prove nothing.
//! (`EPIPE` is the motivating errno, not the only one — a pty slave whose
//! master closed gives `EIO`, and `eprintln!` panics on any write error.)
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

/// Printed to **stdout** by either fixture immediately before it panics.
///
/// The verdict cannot travel on stderr — in the broken-pipe fixture that is the
/// very thing destroyed, and in the healthy one stderr is what we are *testing*,
/// so a control that rode on it would be circular. It must be printed *before*
/// the panic, because after it the process is either unwinding or already gone.
///
/// Worded neutrally on purpose: both fixtures print it, so it must not assert
/// anything about the state of fd 2.
const ABOUT_TO_PANIC: &str = "kastellan-test: hook installed, fixture ready, panicking now";

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
    // ⚠️ Both closes are CHECKED. A failed `close(read)` leaves a live reader,
    // so the pipe would still be writable, `stderr_is_writable()` would return
    // true, and the whole property would go vacuous while still passing — the
    // `ABOUT_TO_PANIC` control cannot tell that apart. Asserting here is safe:
    // it is before the breakage takes effect for our purposes, and a panic at
    // this point can still be rendered.
    assert_eq!(unsafe { libc::close(write) }, 0, "close the spare write end");
    assert_eq!(unsafe { libc::close(read) }, 0, "hang up the reader");
    // From here on nothing in this process may touch stderr and survive.
    println!("{ABOUT_TO_PANIC}");
    panic!("{PAYLOAD}");
}

/// Inner fixture: install the hook and panic with a **healthy** stderr.
///
/// The positive control for [`a_panic_with_a_broken_stderr_does_not_abort_the_process`],
/// and the reason it lives here rather than only in `core`'s
/// `panic_hook_gate_safety_e2e`: without it, a mutant
/// `stderr_is_writable() -> false` passes this entire file — no signal ✓,
/// `test result: FAILED` ✓, payload absent from stderr ✓ — and the crate that
/// OWNS the hook cannot prove its hook ever speaks (#750's review).
#[test]
#[ignore = "inner fixture: panics on purpose; run by its parent in a child process"]
fn inner_fixture_panics_with_a_healthy_stderr() {
    if !is_the_child() {
        return;
    }
    kastellan_tests_common::panic_hook::install_once();
    // Twice on purpose: the chokepoint installs on EVERY knob read, so the
    // announcement must be once per process, not once per call — or a gate log
    // fills with it. The parent counts exactly one.
    kastellan_tests_common::panic_hook::install_once();
    println!("{ABOUT_TO_PANIC}");
    panic!("{PAYLOAD}");
}

/// Re-exec this test binary to run one `#[ignore]`d inner fixture in a child.
///
/// `--nocapture` is hard-coded rather than inherited: libtest captures
/// `eprintln!` through `std::io::set_output_capture`, into a buffer rather than
/// onto fd 2. Under a capturing child the hook would write happily into that
/// buffer, the write would not fail, and the broken-stderr suite would pass
/// against a deleted guard. The capture is precisely what has to be out of the
/// way for the bug to exist.
fn run_inner_fixture(test_name: &str) -> std::process::Output {
    let exe = std::env::current_exe().expect("this test binary's own path");
    Command::new(exe)
        .args([test_name, "--exact", "--ignored", "--test-threads", "1", "--nocapture"])
        .env(FIXTURE_ENV, "1")
        .output()
        .expect("re-exec this test binary to run the inner fixture")
}

/// POSITIVE CONTROL: with a working stderr the hook actually SPEAKS.
///
/// Kills the mutant the guard invites — `stderr_is_writable()` hard-coded to
/// `false`, or the early `return` made unconditional — which the broken-stderr
/// test cannot see, because there the silence is the expected outcome.
#[test]
fn a_panic_with_a_healthy_stderr_still_renders_the_message() {
    // #748: this parent reads no REQUIRE knob, so nothing else installs the
    // neutralising panic hook in this process — and the `worker-report` gate
    // profile refuses any binary that never announces it. First statement, so
    // a panic anywhere below is rendered by it.
    kastellan_tests_common::panic_hook::install_once();
    let out = run_inner_fixture("inner_fixture_panics_with_a_healthy_stderr");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let both = format!("--- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}");

    // Same fixture-reached control as the sibling test: `--exact` on a name
    // that matches nothing exits 0 having run NOTHING.
    assert!(
        stdout.contains(ABOUT_TO_PANIC),
        "the fixture never reached its panic, so this run proves nothing.\n{both}"
    );
    assert!(
        stderr.contains(kastellan_tests_common::panic_hook::PANIC_MARKER),
        "the hook's marker is absent, so the neutralising hook did not render this panic — \
         either the guard is returning early on a HEALTHY stderr, or the hook was never \
         installed.\n{both}"
    );
    assert!(
        stderr.contains(PAYLOAD),
        "the panic payload never reached stderr, so the hook is silent on a stream it can \
         perfectly well write to.\n{both}"
    );
    // #748: installing the hook ANNOUNCES it, once, at column 0. The gate
    // script's "every test binary reached the hook" check counts exactly this
    // line, so a hook that installs silently would make every profile red —
    // and one that announced without installing would make that check a lie.
    // Here both halves are visible in one child: the announcement, and the
    // `[panic]` rendering above that only an installed hook produces.
    let marker = kastellan_tests_common::panic_hook::HOOK_INSTALLED_MARKER;
    let announcements = stderr.lines().filter(|l| l.starts_with(marker)).count();
    assert_eq!(
        announcements, 1,
        "installing the hook must print exactly one column-0 `{marker}` line (the fixture \
         calls `install_once` twice; `Once` must keep the second call silent).\n{both}"
    );
}

/// The property: a panic with a broken stderr is still a *reported* test
/// failure, not an abort.
#[test]
fn a_panic_with_a_broken_stderr_does_not_abort_the_process() {
    kastellan_tests_common::panic_hook::install_once();
    let out = run_inner_fixture("inner_fixture_panics_with_a_broken_stderr_pipe");
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

    // Half three, and it is a STATEMENT of the cost, not a fixture control.
    //
    // ⚠️ **This assertion cannot fail, and saying so is the point** (#750's
    // review found it claiming to be a control). `Command::output` hands the
    // child a pipe on fd 2; the fixture's `dup2` overwrites that descriptor
    // before anything is written to it, closing the child's only copy of the
    // parent's write end. From then on `out.stderr` is EOF-empty whatever the
    // hook does — a hook that wrote the payload successfully would write it
    // into the fixture's OWN unread pipe, never into what we read here. What
    // actually establishes that the fixture broke stderr is the
    // `ABOUT_TO_PANIC` control above, which proves it got past the `dup2`.
    //
    // It is kept because it pins the thing the doc comments kept getting
    // wrong: the guard does NOT deliver the panic text. If someone ever makes
    // the hook fall back to another stream, this is the line that has to be
    // revisited deliberately rather than discovered.
    assert!(
        !stderr.contains(PAYLOAD),
        "the panic payload reached the parent's view of the child's stderr, which this \
         fixture's own `dup2` should have made impossible — the test's model of the \
         plumbing is wrong.\n{both}"
    );
}
