//! Run a subprocess under a wall-clock budget, so a wedged helper cannot hang
//! its caller forever with nothing to show for it.
//!
//! # Why this exists
//!
//! Every host *probe* in this workspace answers one question — "can this
//! backend give a run meaning?" — by shelling out to a small CLI and reading
//! its exit status. All of them used [`std::process::Command::output`], which
//! waits **forever**. That is fine for a CLI that always answers and wrong for
//! one that can stop answering:
//!
//! - Apple `container` is **daemon-backed** (the client talks XPC to
//!   `container-apiserver`). A wedged or half-started apiserver makes
//!   `container system status` block rather than fail.
//! - `systemd-run` is **daemon-backed** the same way (D-Bus to the user
//!   manager), so the Linux tier has the identical class.
//!
//! An unbounded probe turns that into the most opaque failure available:
//! `cargo test --workspace` stalls with **no `[SKIP]`, no `[WARN]`, no panic
//! and no message at all**, and nothing downstream can tell it apart from a
//! slow test. The module those probes serve exists to stop gates failing soft;
//! a gate that fails *silent and forever* is worse than one that fails soft.
//!
//! # What it guarantees
//!
//! [`output_within`] either returns the child's real [`Output`] or reports
//! [`Bounded::TimedOut`] — and a timeout **carries content**: the budget that
//! elapsed and whatever the child had already written to stderr. An error with
//! no content is a defect *multiplier* (#660/#669: three independent
//! production defects once hid behind one contentless `Protocol(EarlyExit)`),
//! so the point of this module is not the timeout, it is the message.
//!
//! # Why it is not `cfg`-gated
//!
//! Both micro-VM tiers need it and each runs on a different host, so a
//! `cfg`-gated copy would be exercised on one host only — the same reasoning
//! that keeps [`crate::guest_kernel_pin`] ungated. Everything here is plain
//! `std`: no new dependency, and the unit tests run on Linux and macOS alike.
//!
//! Issue #690.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Budget for a host *probe* — "is this backend usable at all?".
///
/// **Measured**, not guessed. On the dev Mac at Apple `container` 1.1.0, under
/// a load average of 27, the three probe calls answered in 10–240 ms:
/// `container --version` 10–60 ms, `container system status` 20 ms,
/// `container image inspect <tag>` 20–240 ms (the 240 ms is the cold first
/// call). Ten seconds is therefore 40–1000x headroom, which is the right shape
/// for this knob: it is not a latency budget and must never fire on a slow but
/// working host. It exists solely to convert *forever* into *a message*.
pub const PROBE_BUDGET: Duration = Duration::from_secs(10);

/// How often [`output_within`] looks to see whether the child has exited.
///
/// Polling rather than a blocking wait keeps the whole module inside `std`:
/// [`std::process::Child::wait`] cannot be interrupted, and every non-polling
/// alternative needs either a new dependency or a second thread that owns the
/// `Child` (which then cannot be killed from here). At 2 ms the fast path
/// costs at most one extra tick — against probes measured in tens of
/// milliseconds — and the slow path wakes 5000 times across [`PROBE_BUDGET`],
/// which is nothing next to the process spawn it is watching.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How long, after the child is gone, the pipe drains get to finish before
/// their output is taken as-is.
///
/// ⚠️ **This exists because joining them is not safe, and that was measured
/// the hard way.** A *grandchild* inherits the child's stdout and stderr, so a
/// grandchild that outlives its parent keeps the write end open and the drain
/// never sees EOF — `Command::output()` hangs forever in exactly that case, and
/// so did the first version of this module. It was caught by this branch's own
/// new `#686` test on the real host: bash was waiting on a `dirname` that had
/// wedged in `_dyld_start`, the budget killed bash, and `output_within` then
/// blocked **in `join_drain`** for 17 minutes. A bounded runner that is bounded
/// everywhere except its last step is not a bounded runner.
///
/// Half a second is generous for "the child is already dead; the kernel has
/// closed its fds" — the ordinary case, where the drains are finished before
/// this is even consulted.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// How a bounded subprocess run ended.
#[derive(Debug)]
pub enum Bounded {
    /// The child exited on its own inside the budget. Carries the real
    /// [`Output`], exactly as [`Command::output`] would have returned it —
    /// **including a non-zero exit status**, which is an answer, not a
    /// timeout.
    Exited(Output),
    /// The budget elapsed first. The child was killed and reaped, so no
    /// zombie is left behind.
    TimedOut(TimedOut),
}

/// What a run that overran its budget can still tell the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOut {
    /// The budget that elapsed.
    pub budget: Duration,
    /// Whatever the child had written to stderr before it was killed, lossily
    /// decoded and trimmed. Often empty — a wedged process is usually wedged
    /// *before* it says anything — which is why [`timed_out_reason`] has to
    /// read well without it.
    pub partial_stderr: String,
}

/// Run `cmd` to completion, or kill it once `budget` has elapsed.
///
/// Stdio is configured here exactly as [`Command::output`] configures it —
/// stdin null, stdout and stderr captured — so a caller's own `.stdout(…)` /
/// `.stderr(…)` settings are overridden, just as they are by `output()`.
///
/// `Err` means the child could not be **spawned** (a missing binary, a
/// permission error); it never means the child failed or overran. Those are
/// [`Bounded::Exited`] with a non-zero status and [`Bounded::TimedOut`]
/// respectively, and keeping the three apart is the whole point:
/// #684's folding defect was one CLI fault reported as another.
///
/// ⚠️ **Both pipes are drained on their own threads, and that is
/// load-bearing.** A child that writes more than one pipe buffer (64 KiB on
/// Linux, less on macOS) *blocks on the write* until somebody reads. A waiter
/// that only polls for exit would therefore report a timeout for a process
/// that is merely waiting for **us** — a self-inflicted false positive on
/// exactly the class this module is supposed to make legible.
pub fn output_within(cmd: &mut Command, budget: Duration) -> std::io::Result<Bounded> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    // `.take()` cannot fail: both pipes were just configured above.
    let out = drain(child.stdout.take().expect("stdout was piped"));
    let err = drain(child.stderr.take().expect("stderr was piped"));

    let deadline = Instant::now() + budget;
    let exited = loop {
        // Look for an exit BEFORE looking at the clock, so a child that
        // finished during the last sleep is reported as exited rather than
        // racing the deadline by a few microseconds.
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    match exited {
        Some(status) => {
            settle(&[&out, &err]);
            Ok(Bounded::Exited(Output { status, stdout: out.take(), stderr: err.take() }))
        }
        None => {
            // Kill first, THEN settle: killing closes the child's fds, which is
            // what lets the drains reach EOF in the ordinary case.
            let _ = child.kill();
            let _ = child.wait();
            settle(&[&out, &err]);
            let partial = String::from_utf8_lossy(&err.take()).trim().to_string();
            Ok(Bounded::TimedOut(TimedOut { budget, partial_stderr: partial }))
        }
    }
}

/// A pipe being read on its own thread, with what it has read so far.
///
/// ⚠️ **Never joined.** See [`DRAIN_GRACE`]: a grandchild holding the write end
/// makes the read block indefinitely, and a thread parked on a read that will
/// never return is a far smaller problem than a caller parked on that thread.
/// The thread ends on its own the moment the fd finally closes.
struct Pipe {
    buf: Arc<Mutex<Vec<u8>>>,
    done: Arc<AtomicBool>,
}

impl Pipe {
    /// Whatever has been read so far, which after [`settle`] is everything in
    /// every case but a grandchild still writing.
    fn take(&self) -> Vec<u8> {
        // A panicking drain thread would poison the lock; its buffer is still
        // the right answer, so recover it rather than propagate.
        self.buf.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Start draining `pipe` on its own thread.
///
/// ⚠️ **Draining is load-bearing, not tidiness.** A child that writes more than
/// one pipe buffer (64 KiB on Linux, less on macOS) *blocks on the write* until
/// somebody reads. A waiter that only polled for exit would therefore report a
/// timeout for a process that is merely waiting for **us** — a false positive
/// manufactured by the very thing meant to remove false verdicts.
fn drain(mut pipe: impl Read + Send + 'static) -> Pipe {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let (thread_buf, thread_done) = (Arc::clone(&buf), Arc::clone(&done));
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                // EOF, or a read error: either way there is nothing more to
                // get, and what arrived first is still worth keeping.
                Ok(0) | Err(_) => break,
                Ok(n) => thread_buf
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .extend_from_slice(&chunk[..n]),
            }
        }
        thread_done.store(true, Ordering::SeqCst);
    });
    Pipe { buf, done }
}

/// Wait up to [`DRAIN_GRACE`] for every pipe to reach EOF, then give up.
///
/// Giving up is the point: this returns whether the drains finished or not, so
/// no caller can be held by one.
fn settle(pipes: &[&Pipe]) {
    let deadline = Instant::now() + DRAIN_GRACE;
    while Instant::now() < deadline {
        if pipes.iter().all(|p| p.done.load(Ordering::SeqCst)) {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Why a probe never produced an exit status.
///
/// Two unrelated causes that need two unrelated remedies, kept apart by the
/// type rather than by prose. #684's defect was exactly this collapse — one
/// CLI fault reported as another, sending the operator to a ten-minute build
/// for a problem no build can fix — and #688's review split
/// `InspectFault::{Cli, Unreadable}` for the same reason.
#[derive(Debug)]
pub enum ProbeFailure {
    /// The command could not be spawned at all: it is not installed, not on
    /// `$PATH`, or not executable. The remedy is an install.
    Spawn(std::io::Error),
    /// The command ran and never answered inside its budget, and was killed.
    /// The remedy is whatever unwedges *that* helper.
    Wedged(TimedOut),
}

/// Run a host probe under `budget`, keeping the two no-answer causes apart.
///
/// A **non-zero exit is `Ok`** — the probe answered, and "no" is an answer.
/// Only the two ways of producing no answer at all become `Err`, and each
/// carries what its caller needs to name a remedy. Rendering is deliberately
/// left to the caller ([`timed_out_reason`]): a wedged `container-apiserver`
/// and a wedged user-systemd need different sentences, and one generic line
/// would be the folding defect again.
pub fn probe_output(cmd: &mut Command, budget: Duration) -> Result<Output, ProbeFailure> {
    match output_within(cmd, budget) {
        Ok(Bounded::Exited(out)) => Ok(out),
        Ok(Bounded::TimedOut(t)) => Err(ProbeFailure::Wedged(t)),
        Err(e) => Err(ProbeFailure::Spawn(e)),
    }
}

/// The operator-facing sentence for a run that overran its budget.
///
/// Pure, so every arm is reachable from a unit test on any host — the
/// [`output_within`] wiring above stays thin enough to have nothing of its own
/// to get wrong. (#688's review found the mirror of this: a fault arm proven
/// over an input the impure half could produce for only one of its two
/// causes.)
///
/// `command` is the command as the operator would type it, `hint` the remedy
/// **for this particular caller** — a wedged apiserver and an unreadable image
/// need different sentences, and a shared timeout that renders one generic
/// line would re-create the folding defect #684 removed.
pub fn timed_out_reason(command: &str, timed_out: &TimedOut, hint: &str) -> String {
    let mut reason =
        format!("`{command}` did not answer within {:?} and was killed", timed_out.budget);
    if !hint.is_empty() {
        reason.push_str(&format!("; {hint}"));
    }
    // Only append the child's own words when there are some: a trailing
    // `: ` with nothing after it reads as truncated output and sends the
    // reader looking for a message that was never there.
    if !timed_out.partial_stderr.is_empty() {
        reason.push_str(&format!(" (it had said: {})", timed_out.partial_stderr));
    }
    reason
}

#[cfg(test)]
mod tests;
