//! Run a child process under a wall-clock timeout.
//!
//! `std::process::Command` has no built-in timeout: `.output()` and
//! `.wait()` block until the child exits. A child that never exits —
//! for example `launchctl bootstrap` / `bootout` against a
//! churn-degraded macOS `gui/<uid>` domain (a documented launchd
//! pathology under heavy service install/uninstall load) — hangs the
//! calling thread *indefinitely at 0 % CPU*. In the supervisor that
//! wedges a daemon lifecycle call; in the test harness it wedges every
//! other thread waiting on the process-global serial lock the hung
//! bring-up still holds, which is exactly the "0-CPU deadlock under
//! heavy multi-cluster load" the `memory_layers_e2e` suite hit.
//!
//! [`run_capped`] bounds that wait. It spawns the child, drains stdout
//! and stderr on dedicated threads (so a child that fills a pipe buffer
//! still makes progress and we never deadlock on output), and polls for
//! exit until `timeout` elapses. On timeout it kills and reaps the
//! child and reports [`CappedOutcome::TimedOut`], turning an unbounded
//! hang into a fast, structured signal the caller can map to an error.

use std::io::{self, Read};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How often [`run_capped`] re-checks whether the child has exited.
///
/// Small enough that exit is detected promptly, large enough that the
/// poll loop costs no measurable CPU while waiting on a slow `launchctl`.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The result of a bounded child-process run.
#[derive(Debug)]
pub enum CappedOutcome {
    /// The child exited on its own before the timeout. Carries the
    /// captured status + stdout + stderr, exactly like [`Command::output`].
    Completed(Output),
    /// The timeout elapsed first. The child has been killed and reaped
    /// before this is returned, so no zombie lingers.
    TimedOut,
}

/// Spawn `cmd` and wait at most `timeout` for it to exit.
///
/// Overrides the command's stdio (stdin null, stdout/stderr piped) so
/// the captured `Output` is always populated and a child can never
/// block the parent on a terminal read. stdout/stderr are drained on
/// dedicated threads, so a child that writes more than one pipe
/// buffer's worth of output cannot deadlock against the wait.
///
/// Returns `Err` when the child cannot be spawned, `try_wait` itself
/// fails, or a pipe read fails mid-capture — a truncated stdout must not
/// pass as the child's whole answer (#892). A non-zero exit is a [`CappedOutcome::Completed`]
/// (the caller inspects [`Output::status`]); exceeding `timeout` is a
/// [`CappedOutcome::TimedOut`] — never an `Err`.
pub fn run_capped(cmd: &mut Command, timeout: Duration) -> io::Result<CappedOutcome> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Drain each pipe on its own thread. Reading after the child exits
    // would risk a deadlock for a child that fills a pipe buffer (it
    // blocks on write, never exits, we never read) — concurrent drains
    // keep both sides making progress regardless of output volume. Each
    // closure *returns* its capture (or the read error that cut it short),
    // so `join()` hands it back directly (no channel needed).
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let o_handle = thread::spawn(move || read_whole("stdout", stdout_pipe.as_mut()));
    let e_handle = thread::spawn(move || read_whole("stderr", stderr_pipe.as_mut()));

    // Poll for exit until the deadline. `try_wait` reaps the child the
    // moment it exits; the small sleep keeps the loop from busy-spinning.
    let deadline = Instant::now() + timeout;
    let exit_status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => thread::sleep(POLL_INTERVAL),
        }
    };

    let Some(status) = exit_status else {
        // Timed out: kill + reap so no zombie survives. Killing closes
        // the child's pipe ends, so the drain threads normally hit EOF
        // and exit at once — but we deliberately do NOT join them here.
        // If the child spawned a descendant that inherited the pipe, the
        // read would never see EOF and a join would re-introduce the very
        // unbounded wait this function exists to prevent. Returning
        // promptly honors the timeout; the detached threads exit on their
        // own once the pipe finally closes.
        let _ = child.kill();
        let _ = child.wait();
        return Ok(CappedOutcome::TimedOut);
    };

    // The child exited on its own, so its pipe write-ends are closed and
    // the drain threads have hit (or are about to hit) EOF — these joins
    // return promptly, handing back each capture. (As with
    // `Command::output`, a still-open inherited pipe could delay this; on
    // the non-timeout path that is the same contract the std API offers.)
    let stdout = joined("stdout", o_handle)?;
    let stderr = joined("stderr", e_handle)?;
    Ok(CappedOutcome::Completed(Output { status, stdout, stderr }))
}

/// A drain thread's capture. A panicked drain lost it, which is no answer
/// either — not an empty one (#892).
fn joined(name: &str, drain: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    drain
        .join()
        .map_err(|_| io::Error::other(format!("the child's {name} drain panicked; its output is lost")))?
}

/// Read `pipe` to EOF. A read error is `Err` naming the pipe and how much
/// arrived, never a short buffer passed off as the whole output (#892).
fn read_whole(name: &str, pipe: Option<&mut impl Read>) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(p) = pipe {
        p.read_to_end(&mut buf).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("reading the child's {name} failed after {} bytes: {e}", buf.len()),
            )
        })?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fast-exiting command returns `Completed` with its captured
    /// stdout and a success status — the helper must not perturb the
    /// normal (non-timeout) path.
    #[test]
    fn completed_for_fast_command() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello-capped");
        let outcome = run_capped(&mut cmd, Duration::from_secs(5)).expect("spawn echo");
        match outcome {
            CappedOutcome::Completed(out) => {
                assert!(out.status.success(), "echo must exit 0");
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.contains("hello-capped"),
                    "captured stdout must contain the echoed text; got {stdout:?}"
                );
            }
            CappedOutcome::TimedOut => panic!("echo must not time out"),
        }
    }

    /// A command that outlives the timeout returns `TimedOut` *quickly*
    /// (well before the command's own runtime) — proving the wait is
    /// actually bounded, not just eventually-returning.
    #[test]
    fn times_out_for_slow_command() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let outcome = run_capped(&mut cmd, Duration::from_millis(200)).expect("spawn sleep");
        let elapsed = start.elapsed();
        assert!(
            matches!(outcome, CappedOutcome::TimedOut),
            "a 30s sleep under a 200ms cap must time out"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout must return promptly (bounded), took {elapsed:?}"
        );
    }

    /// #892: a mid-capture read error is an `Err`, not a short `Completed`.
    #[test]
    fn read_whole_refuses_a_cut_short_capture() {
        struct HalfThenEio(bool);
        impl Read for HalfThenEio {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if std::mem::replace(&mut self.0, true) {
                    return Err(io::Error::other("EIO stand-in"));
                }
                buf[..4].copy_from_slice(b"half");
                Ok(4)
            }
        }
        let e = read_whole("stdout", Some(&mut HalfThenEio(false))).expect_err("truncated");
        assert!(e.to_string().contains("child's stdout failed after 4 bytes"), "{e}");
        assert_eq!(read_whole("stdout", Some(&mut &b"all"[..])).unwrap(), b"all");
    }

    /// #892: a panicked drain is an `Err`, not an empty capture.
    #[test]
    fn joined_refuses_a_panicked_drain() {
        let e = joined("stdout", thread::spawn(|| panic!("drain died"))).expect_err("lost");
        assert!(e.to_string().contains("stdout drain panicked"), "{e}");
        assert_eq!(joined("stdout", thread::spawn(|| Ok(b"all".to_vec()))).unwrap(), b"all");
    }

    /// A non-zero exit is a normal `Completed` outcome carrying the
    /// code — a timeout and a failure are distinct signals.
    #[test]
    fn completed_surfaces_nonzero_exit() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        let outcome = run_capped(&mut cmd, Duration::from_secs(5)).expect("spawn sh");
        match outcome {
            CappedOutcome::Completed(out) => {
                assert_eq!(out.status.code(), Some(3), "non-zero exit code must survive");
            }
            CappedOutcome::TimedOut => panic!("a fast `exit 3` must not time out"),
        }
    }
}
