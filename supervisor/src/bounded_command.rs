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
use std::sync::mpsc;
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
/// Returns `Err` only when the child cannot be spawned (or `try_wait`
/// itself fails). A non-zero exit is a [`CappedOutcome::Completed`]
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
    // keep both sides making progress regardless of output volume.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let (otx, orx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    let o_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = otx.send(buf);
    });
    let e_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = etx.send(buf);
    });

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
        // the pipes, which lets the reader threads finish; join them so
        // their fds are released before we return.
        let _ = child.kill();
        let _ = child.wait();
        let _ = o_handle.join();
        let _ = e_handle.join();
        return Ok(CappedOutcome::TimedOut);
    };

    let _ = o_handle.join();
    let _ = e_handle.join();
    let stdout = orx.recv().unwrap_or_default();
    let stderr = erx.recv().unwrap_or_default();
    Ok(CappedOutcome::Completed(Output { status, stdout, stderr }))
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
