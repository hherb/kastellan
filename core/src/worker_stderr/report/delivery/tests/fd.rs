//! Would a write to a descriptor fail: [`is_writable`] against real pipes,
//! closed descriptors, character devices and files, on both hosts.

use std::io::Write;

use super::super::*;

/// A pipe, as a pair of raw descriptors we own and close by hand.
///
/// Deliberately not `std::process::Command`'s plumbing or a `File`: the
/// point is to control exactly when the read end closes, which is the
/// state [`is_writable`] has to recognise.
struct Pipe {
    read: RawFd,
    write: RawFd,
}

impl Pipe {
    fn new() -> Self {
        let mut fds = [0 as RawFd; 2];
        assert_eq!(
            unsafe { libc::pipe(fds.as_mut_ptr()) },
            0,
            "the test needs a pipe; this is a host problem, not a code one"
        );
        Self {
            read: fds[0],
            write: fds[1],
        }
    }

    /// Close the read end, which is what makes the write end broken.
    fn close_read_end(&mut self) {
        if self.read >= 0 {
            unsafe { libc::close(self.read) };
            self.read = -1;
        }
    }

    /// Ground truth: does a real write to the write end actually fail?
    ///
    /// Without this the suite would be asserting that `is_writable` agrees
    /// with our *belief* about the pipe's state rather than with the
    /// kernel's behaviour — and a probe that returned `false` for every
    /// descriptor would pass the broken-pipe test while silently disabling
    /// the fallback everywhere.
    fn a_real_write_fails(&self) -> bool {
        let mut f =
            unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(self.write) };
        let failed = f.write_all(b"x").is_err();
        // We do not own this descriptor through the `File`; `Drop` would
        // close it out from under `self`.
        std::mem::forget(f);
        failed
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.close_read_end();
        if self.write >= 0 {
            unsafe { libc::close(self.write) };
        }
    }
}


#[test]
fn a_live_pipe_is_writable() {
    // POSITIVE CONTROL for the whole file. Without it, an `is_writable`
    // that returned `false` unconditionally would pass every other test
    // here — while suppressing every worker report in the tree, which is
    // the exact silence this module exists to remove.
    let p = Pipe::new();
    assert!(
        is_writable(p.write),
        "a pipe with its read end still open must be reported writable, or the fallback \
         is suppressed for every healthy process"
    );
    assert!(
        !p.a_real_write_fails(),
        "ground truth: a real write to a live pipe must succeed, or this fixture is not \
         testing what it claims"
    );
}

#[test]
fn a_pipe_whose_reader_is_gone_is_not_writable() {
    let mut p = Pipe::new();
    p.close_read_end();
    // Ground truth FIRST, so a fixture that failed to break the pipe is a
    // red here rather than a silently vacuous assertion below.
    assert!(
        p.a_real_write_fails(),
        "ground truth: with the read end closed a real write must fail (EPIPE). If it did \
         not, this fixture never created the state `is_writable` is being asked about"
    );
    assert!(
        !is_writable(p.write),
        "the write end of a pipe with no reader must be reported UNwritable — this is the \
         `kastellan-cli guard capture … | head` case, where `eprintln!` panics and \
         `panic = \"abort\"` turns it into a silent SIGABRT (#733)"
    );
}

/// A descriptor number that was open, is now closed, and that no
/// concurrently running test can reclaim.
///
/// ⚠️ **The NUMBER is the whole fixture, and the obvious version of this
/// test is flaky.** Its first draft closed a `pipe(2)` and probed fds 3
/// and 4. `open(2)` hands out the **lowest free** descriptor and libtest
/// runs this module's other fixtures on parallel threads —
/// [`a_live_pipe_is_writable`] opens a pipe,
/// [`a_regular_file_is_writable`] a tempfile — so fd 3 was routinely
/// reopened between the close and the probe, and `poll` then correctly
/// reported a live descriptor. Measured on macOS: **10 failures in 10**
/// runs of `cargo test -p kastellan-core --lib worker_stderr`, the narrow
/// form CLAUDE.md documents, and **0 in 10** full-workspace sweeps, where
/// fd 3 is long since taken. A sweep-only green is exactly how this would
/// have shipped. The lowest-free rule cannot hand back a number this high
/// while the binary holds a few dozen descriptors.
const UNRECLAIMABLE_FD: RawFd = 900;

/// Open [`UNRECLAIMABLE_FD`], prove it is open, then close it.
fn a_closed_descriptor() -> RawFd {
    let p = Pipe::new();
    assert!(
        unsafe { libc::dup2(p.write, UNRECLAIMABLE_FD) } >= 0,
        "the test needs to place a descriptor at fd {UNRECLAIMABLE_FD}; this is a host \
         problem, not a code one"
    );
    // Ground truth in the other direction: it really is open right now, so
    // the assertion below is about the CLOSE and not about a number that
    // was never valid.
    assert!(
        is_open(UNRECLAIMABLE_FD),
        "fd {UNRECLAIMABLE_FD} must be open before we close it, or this fixture never \
         created the transition it is testing"
    );
    unsafe { libc::close(UNRECLAIMABLE_FD) };
    assert!(
        !is_open(UNRECLAIMABLE_FD),
        "fd {UNRECLAIMABLE_FD} must be closed after `close`; if it is not, another thread \
         reclaimed it and this fixture is racing"
    );
    UNRECLAIMABLE_FD
}

#[test]
fn a_descriptor_that_was_never_open_is_not_writable() {
    // `2>&-` leaves fd 2 closed. `poll` reports POLLNVAL rather than an
    // error, so this is a distinct arm from the broken-pipe one — and
    // since #745's review, POLLNVAL is believed only when `fcntl` agrees,
    // so this also pins the corroboration step.
    let fd = a_closed_descriptor();
    assert!(
        !is_writable(fd),
        "a closed descriptor ({fd}) must be reported UNwritable; `2>&-` is the shape"
    );
}

#[test]
fn a_character_device_is_writable() {
    // ⚠️ REGRESSION GUARD, macOS-specific and measured. Darwin's `poll`
    // sets POLLNVAL on a perfectly usable character device, so the
    // original bare `POLLNVAL` arm reported `/dev/null` UNwritable while a
    // real write to it succeeded — fail-CLOSED, which suppressed every
    // worker report in any process run `2>/dev/null`. That is the one
    // direction this module's doc forbids.
    //
    // Linux does not share the quirk, so on Linux this test passes with or
    // without the corroboration and cannot prove it. It is still the right
    // place for the assertion: the mask is one piece of code and it has to
    // be correct on both hosts. Same argument as the POLLERR/POLLHUP
    // split, which `is_writable`'s doc tabulates.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .expect("/dev/null must be openable");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    // Ground truth FIRST: the kernel really will accept a write here, so a
    // `false` below is the probe being wrong rather than the fixture.
    assert!(
        (&file).write(b"x").is_ok(),
        "ground truth: a real write to /dev/null must succeed, or this fixture is not \
         testing what it claims"
    );
    assert!(
        is_writable(fd),
        "/dev/null must be reported writable. On macOS `poll` answers POLLNVAL for a live \
         character device, so a bare POLLNVAL arm suppresses every report from a process \
         run `2>/dev/null` — the silence this module exists to remove. POLLNVAL must stay \
         corroborated by `fcntl(F_GETFD)`"
    );
}

#[test]
fn a_regular_file_is_writable() {
    // The deployed daemon's stderr is a file or a journal socket, not a
    // tty. If `is_writable` were to answer `false` for a file, #734's fix
    // would land the daemon back in silence — the opposite of its point.
    let dir = tempfile::tempdir().expect("a temp dir");
    let file = std::fs::File::create(dir.path().join("stderr")).expect("create the file");
    assert!(
        is_writable(std::os::fd::AsRawFd::as_raw_fd(&file)),
        "a regular file must be reported writable — this is how the daemon's stderr is \
         usually wired"
    );
}

#[test]
fn this_test_binarys_own_stderr_is_writable() {
    // The case every other test in the tree depends on. A probe that said
    // `false` here would silently strip the marked fallback line out of
    // both e2e suites, which assert on its presence — but this test names
    // the reason, so the failure points at the probe rather than at them.
    assert!(
        is_writable(libc::STDERR_FILENO),
        "the test binary's own stderr must be reported writable"
    );
}
