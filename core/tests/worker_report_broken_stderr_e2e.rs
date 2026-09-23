//! #733: writing a worker report to a broken stderr must not kill the process.
//!
//! `eprintln!` has no fallible form — on a write error it **panics**, and the
//! workspace release profile is `panic = "abort"`, so that is a `SIGABRT` with
//! no message. `worker_stderr::report::delivery::is_writable` closes that in
//! front of the macro with a `poll(2)` probe.
//!
//! ## Why the probe's own unit tests are not enough
//!
//! `delivery.rs` has six tests for `is_writable`, and they are good ones — but
//! every one of them tests the **pure probe** against a descriptor it made for
//! the purpose. None reaches `write_fallback_line`, the probe's only
//! production consumer, and none reproduces #733's actual shape. Measured
//! during #745's review: **deleting the guard from `write_fallback_line`
//! entirely left the whole unit suite green**, caught only by a dead-code
//! warning. A subtler mutant — probing `STDOUT_FILENO` instead of
//! `STDERR_FILENO` — survives even that, because in every test binary stdout
//! is writable too.
//!
//! So this suite drives the real emitter with a real broken fd 2, from
//! outside, in the two shapes #733 names.
//!
//! ## ⚠️ Only ONE of the two shapes is a crash, and that is measured
//!
//! `std` deliberately swallows `EBADF` on stdio (`handle_ebadf` in
//! `library/std/src/io/stdio.rs`), so a **closed** fd 2 is not fatal at all.
//! Measured on macOS 27 (arm64), after `close(2)` with `fcntl(2, F_GETFD)`
//! confirming the descriptor really is gone:
//!
//! | stderr shape | raw `write(2, …)` | `std::io::stderr().write` | `eprintln!` |
//! | --- | --- | --- | --- |
//! | closed (`2>&-`) | `-1` `EBADF` | **`Ok(1)`** — swallowed | **does not panic** |
//! | pipe, reader gone (`… \| head`) | `-1` `EPIPE` | `Err(BrokenPipe)` | **panics** |
//!
//! So #733's abort is reachable through the **broken-pipe** shape only — which
//! is exactly the shape the issue names. The closed-fd shape is still worth a
//! fixture, but for a different property: the guard must **suppress** a write
//! that would go nowhere, and getting that wrong is how the macOS `POLLNVAL`
//! bug (see `is_writable`'s doc) suppressed reports that would have been read.
//! Each test below says which of the two it is asserting.
//!
//! ## What these tests kill
//!
//! Both mutants were run and both died, on both tests:
//!
//! | mutant | outcome |
//! | --- | --- |
//! | delete the `is_writable` guard from `write_fallback_line` | **KILLED** |
//! | probe `STDOUT_FILENO` instead of `STDERR_FILENO` | **KILLED** |
//!
//! The second is the one that matters: it survives the unit suite *and* the
//! dead-code warning, because in every test binary stdout is writable too.
//!
//! ## Why a child process, and why `--nocapture`
//!
//! Both are load-bearing and neither is incidental:
//!
//! * **A child**, because the fixture *closes the test binary's own fd 2*.
//!   There is no way back from that, and a parent process that did it would
//!   take every later test in the binary with it.
//! * **`--nocapture`**, because libtest captures `eprintln!` through
//!   `std::io::set_output_capture` — into a buffer, not onto fd 2. Under a
//!   capturing run the emitter would write happily to a closed descriptor's
//!   stand-in and this suite would pass against a deleted guard. The capture
//!   is precisely what has to be out of the way for the bug to be reachable.
//!
//! ⚠️ **The verdict travels on STDOUT.** The fixture cannot report anything on
//! stderr — that is the thing it broke — so it prints to stdout and the parent
//! reads it there. An `assert!` inside the fixture would try to render its
//! message to the broken fd and double-panic.
//!
//! ⚠️ **The parent asserts the child SUCCEEDED**, the opposite of this tree's
//! other re-exec suites, because here the bug is a crash rather than a missing
//! line. The positive control therefore cannot be "the child failed"; it is
//! the `1 passed` line plus the verdict string, both of which a child that ran
//! nothing would lack.

use std::process::Command;

/// Set by the parent on the child it launches. The fixtures do nothing without
/// it.
///
/// ⚠️ **Load-bearing, not belt-and-braces with `#[ignore]`.** These fixtures
/// deliberately destroy their own stderr, and this tree documents
/// `cargo test … -- --ignored` as the way to run the Firecracker tier — which
/// would otherwise run them in a process that still had tests to report.
const FIXTURE_ENV: &str = "KASTELLAN_BROKEN_STDERR_FIXTURE";

/// What a fixture prints to **stdout** once it has survived the emit.
///
/// `suppressed=true` is the whole claim: the emitter noticed the descriptor
/// was unusable and declined, rather than writing and dying.
const VERDICT_PREFIX: &str = "kastellan-test: VERDICT suppressed=";

/// A report body distinctive enough that finding it anywhere would mean the
/// emitter wrote after all.
const REPORT: &str = "kastellan-test: broken-stderr probe";

/// `true` when this process was launched by [`run_inner_fixture`].
///
/// Honours the tree's knob dialect (`1|true|yes|on`) rather than inventing a
/// second one.
fn is_the_child() -> bool {
    std::env::var(FIXTURE_ENV).is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Emit a worker failure report and print the verdict to stdout.
///
/// Deliberately **not** an `assert!`: stderr is broken by the time this runs,
/// so a failing assertion could not render its own message. The parent does
/// the judging.
fn emit_and_report_on_stdout() {
    let suppressed = !kastellan_core::worker_stderr::emit_worker_failure_report(REPORT);
    println!("{VERDICT_PREFIX}{suppressed}");
}

/// Inner fixture: fd 2 **closed**, which is what `2>&-` leaves behind.
#[test]
#[ignore = "inner fixture: destroys its own stderr; run by its parent in a child process"]
fn inner_fixture_stderr_is_closed() {
    if !is_the_child() {
        return;
    }
    // From here on nothing in this process may touch stderr.
    assert_eq!(unsafe { libc::close(libc::STDERR_FILENO) }, 0, "close fd 2");
    emit_and_report_on_stdout();
}

/// Inner fixture: fd 2 is a **pipe whose reader has gone** — #733's named
/// shape, `kastellan-cli guard capture … | head`.
#[test]
#[ignore = "inner fixture: destroys its own stderr; run by its parent in a child process"]
fn inner_fixture_stderr_is_a_pipe_with_no_reader() {
    if !is_the_child() {
        return;
    }
    let mut fds = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "make a pipe");
    let (read, write) = (fds[0], fds[1]);
    // Put the pipe's write end where stderr was, then hang up the reader.
    assert!(unsafe { libc::dup2(write, libc::STDERR_FILENO) } >= 0, "dup2 onto fd 2");
    unsafe {
        libc::close(write);
        libc::close(read); // the reader exits — this is `| head` finishing
    }
    emit_and_report_on_stdout();
}

/// What the parent learns from one child run.
struct ChildRun {
    succeeded: bool,
    signalled: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Re-run this binary with `--exact <name> --nocapture`.
fn run_inner_fixture(name: &str) -> ChildRun {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args(["--exact", name, "--ignored", "--test-threads", "1", "--nocapture"])
        .env(FIXTURE_ENV, "1")
        .output()
        .unwrap_or_else(|e| panic!("re-exec this test binary to run `{name}`: {e}"));
    ChildRun {
        succeeded: out.status.success(),
        signalled: std::os::unix::process::ExitStatusExt::signal(&out.status),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// The shared body: one fixture, one broken-stderr shape, one verdict.
fn assert_the_emitter_survives(name: &str, shape: &str) {
    let run = run_inner_fixture(name);
    let both =
        format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", run.stdout, run.stderr);

    // POSITIVE CONTROL #1. `--exact` on a misspelled name exits 0 having run
    // NOTHING, and "the child did not crash" is trivially true of a child that
    // did nothing. This is the trap that has reported a whole mutation batch
    // as surviving against zero tests.
    assert!(
        run.stdout.contains("test result: ok. 1 passed"),
        "exactly one test must have run and passed in the child. Anything else means the name \
         filter did not select `{name}`, or the fixture no-op'd because `{FIXTURE_ENV}` did not \
         reach it.\n{both}"
    );

    // POSITIVE CONTROL #2: the fixture reached the emitter at all.
    assert!(
        run.stdout.contains(VERDICT_PREFIX),
        "the fixture never printed its verdict, so it did not reach \
         `emit_worker_failure_report` and the assertions below are about nothing.\n{both}"
    );

    // THE PROPERTY. A child that failed its test is the debug-profile form of
    // #733's crash: `eprintln!`'s panic unwinds here and aborts under the
    // release profile's `panic = "abort"`. A child killed by a signal is the
    // abort itself.
    assert!(
        run.succeeded,
        "the emitter must SURVIVE a {shape} stderr, and this child did not (signal: {:?}). \
         `eprintln!` panics when the write fails and the release profile is `panic = \"abort\"`, \
         so in a shipped binary this is a silent SIGABRT (#733). Check that \
         `write_fallback_line` still guards on `is_writable(libc::STDERR_FILENO)` — and that it \
         probes STDERR and not some other descriptor.\n{both}",
        run.signalled
    );

    assert!(
        run.stdout.contains(&format!("{VERDICT_PREFIX}true")),
        "the emitter must report the line as SUPPRESSED on a {shape} stderr. `suppressed=false` \
         means it believed the write went somewhere — which on this descriptor it cannot \
         have.\n{both}"
    );

    // And the report must not have reached a stream by some other route,
    // which would mean the fixture never actually broke stderr.
    assert!(
        !run.stderr.contains(REPORT),
        "the report text appeared on the child's stderr, so the fixture did not actually break \
         it and this run proves nothing.\n{both}"
    );
}

/// The **suppression** half: `2>&-`, which `std` survives on its own.
///
/// Measured (see the module doc): `std` swallows `EBADF` on stdio, so this
/// shape never aborted. What it pins is that the guard still recognises the
/// descriptor as unusable and declines — the arm the macOS `POLLNVAL` bug got
/// wrong in the *other* direction, suppressing live character devices.
#[test]
fn the_emitter_suppresses_the_line_on_a_closed_stderr() {
    // #748: this parent reads no REQUIRE knob, so nothing else installs the
    // neutralising panic hook in this process — and the `worker-report` gate
    // profile refuses any binary that never announces it. First statement, so
    // a panic anywhere below is rendered by it.
    kastellan_tests_common::panic_hook::install_once();
    assert_the_emitter_survives("inner_fixture_stderr_is_closed", "CLOSED");
}

/// The **crash** half: #733 as filed, `kastellan-cli guard capture … | head`.
///
/// This is the shape where `eprintln!` really does panic, and under
/// `panic = "abort"` really is a silent `SIGABRT`.
#[test]
fn the_emitter_survives_a_pipe_whose_reader_is_gone() {
    kastellan_tests_common::panic_hook::install_once();
    assert_the_emitter_survives(
        "inner_fixture_stderr_is_a_pipe_with_no_reader",
        "BROKEN-PIPE",
    );
}
