//! Reusable draining of a sandboxed worker's piped stderr.
//!
//! The sandbox backends spawn workers with `stderr(Stdio::piped())`, but the
//! JSON-RPC [`Client`](kastellan_protocol::client::Client) only reads stdout. A
//! worker that writes more than the ~64 KiB pipe buffer to stderr would then
//! **block on write and deadlock** (and the diagnostics are silently discarded).
//! Draining the pipe to EOF on a detached thread prevents both: the worker can't
//! stall, and each chunk surfaces at `debug` for troubleshooting.
//!
//! **Four** call sites share this — counted from the rows, not from memory
//! (this list said "two consumers" and named the wrong one until the #735
//! review):
//!
//! | call site | what it takes |
//! | --- | --- |
//! | `tool_host::spawn_worker` | [`spawn_drain_with_tail`] — tool workers |
//! | `egress::spawn` | [`spawn_drain_with_tail`] — the egress proxy sidecar |
//! | `worker_lifecycle::persistent::ClientTransport` | [`spawn_drain_with_tail`] — persistent workers |
//! | `broker::spawn` | [`spawn_drain`] — the trusted brokers, no tail retained |
//!
//! The **tail** ([`spawn_drain_with_tail`]) is what makes a death report
//! possible: it retains a bounded window of recent lines so the worker's own
//! explanation survives the process (#348).
//!
//! ⚠️ **`ClientTransport` — not "the Matrix channel worker" — is the persistent
//! consumer**, and it serves BOTH production long-lived workers (`matrix` and
//! `email`). The Matrix channel had its own supervisor historically and no
//! longer does.
//!
//! ⚠️ **The driver no longer logs the death cause itself** (#730). It hands the
//! rendered report to [`emit_persistent_death_report`], which owns the marker,
//! the neutralisation and the choice of output channel.
//!
//! # Layout
//!
//! This module is the **capture** half: the bounded [`StderrTail`] ring and the
//! drain threads that fill it. The small immutable value those threads hand to
//! a reporting caller, [`CapturedTail`], lives beside it in `captured.rs`.
//! What is then *said* about a dead worker — the
//! report formatters, the stderr-fallback markers and the emitters that choose
//! a channel — lives in the `report/` directory
//! (`{mod,shared,delivery,tool_worker,persistent}.rs`), whose items are
//! re-exported here so `worker_stderr::` remains the one public path for both
//! halves. (Named as files, not linked: `mod report` is private, and a
//! `[`report`]` link makes rustdoc warn that public documentation points at a
//! private item.)

mod report;
pub use report::*;

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default number of recent stderr lines retained for a death report.
pub const DEFAULT_TAIL_LINES: usize = 50;

/// Cap on the in-progress (newline-free) carry buffer in [`drain_reader`]. A
/// worker that streams to stderr without ever emitting a newline would otherwise
/// grow `carry` unbounded — and in this project a **compromised** sandboxed
/// worker is in scope (see `docs/threat-model.md`), so an unbounded buffer fed
/// from worker stderr is a DoS vector on the core daemon. When `carry` reaches
/// this many bytes we flush it as a synthetic line (bounded by the tail ring)
/// and start fresh, so memory stays bounded regardless of worker output.
const MAX_CARRY_BYTES: usize = 64 * 1024;

/// Encoding of [`StderrTail::drain_end`]'s three states in one `AtomicU8`.
///
/// An atomic rather than a `Mutex<Option<DrainEnd>>` because
/// [`StderrTail::wait_for_drain`] polls it every 2 ms while the drain thread
/// holds the *lines* mutex, and a waiter that can never block is one fewer
/// thing to reason about on a failure path.
const DRAIN_IN_PROGRESS: u8 = 0;
const DRAIN_EOF: u8 = 1;
const DRAIN_READ_ERROR: u8 = 2;

/// Pure: encode a finished drain's outcome for the atomic.
fn encode_drain_end(end: DrainEnd) -> u8 {
    match end {
        DrainEnd::Eof => DRAIN_EOF,
        DrainEnd::ReadError => DRAIN_READ_ERROR,
    }
}

/// Pure: is `raw` a value [`encode_drain_end`] can actually produce?
///
/// ⚠️ **Exists so the warning can live OUTSIDE [`decode_drain_end`].** Both an
/// in-progress drain and a corrupt byte decode to `None` — correctly, since
/// both mean "we may not claim completeness" — but only the second is worth
/// telling anyone about, and [`StderrTail::wait_for_drain`] calls the decoder
/// **every 2 ms for the full [`TAIL_DRAIN_WAIT`]**. A `tracing::error!` inside
/// the decoder is therefore ~125 identical lines per worker failure, on the one
/// path that has to stay readable. Splitting the question keeps the decoder
/// pure and lets the waiter warn exactly once.
fn is_known_drain_byte(raw: u8) -> bool {
    matches!(raw, DRAIN_IN_PROGRESS | DRAIN_EOF | DRAIN_READ_ERROR)
}

/// Pure: decode the atomic back into "how the drain ended, if it has".
///
/// A free function rather than an inline `match` so the encoding has exactly
/// one reader and a unit test can pin every input — including the impossible
/// one, whose fail direction matters: an unrecognised byte must read as *still
/// draining*, so a caller waits rather than claiming a completeness it has not
/// established.
///
/// ⚠️ **An unrecognised byte must fail SAFE, and it must not be silent — but
/// the noise belongs to the caller, not here.** [`encode_drain_end`] matches
/// exhaustively, so adding a [`DrainEnd`] variant breaks it and forces the
/// author to pick a byte; *this* arm would go on compiling and map the new byte
/// to "still draining", which costs a caller the full [`TAIL_DRAIN_WAIT`] and
/// then renders as `NothingCapturedYet` — a fact about *waiting* standing in
/// for a fact about the *encoding*, which is the collapse #732/#746/#747 were.
/// [`StderrTail::wait_for_drain`] says so once, using [`is_known_drain_byte`].
///
/// ⚠️ **Stays pure, and the doc line above is load-bearing.** Logging here
/// looks equivalent and is not: the waiter calls this every 2 ms for the whole
/// cap, so a `tracing::error!` in this function is ~125 identical lines per
/// worker failure. Deliberately not a `debug_assert!` either — that would make
/// the fail-safe `None` untestable and diverge debug from release on a failure
/// path.
fn decode_drain_end(raw: u8) -> Option<DrainEnd> {
    match raw {
        DRAIN_EOF => Some(DrainEnd::Eof),
        DRAIN_READ_ERROR => Some(DrainEnd::ReadError),
        _ => None,
    }
}

/// A bounded, shared ring of a worker's most-recent stderr lines. Cloneable
/// (it's `Arc`-backed): the drain thread pushes, the owning caller snapshots when
/// the worker dies.
#[derive(Clone)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
    cap: usize,
    /// How the drain thread finished, encoded by [`encode_drain_end`].
    ///
    /// Without this a caller that reacts to a worker's death races the drainer
    /// and usually wins: it snapshots an EMPTY tail and reports "no stderr
    /// captured" for a worker that explained itself perfectly well a
    /// millisecond later. That failure mode is indistinguishable from the
    /// worker having said nothing, which is precisely the ambiguity #666 exists
    /// to remove — so this is load-bearing, not a convenience.
    ///
    /// ⚠️ **Three states, not two** ([#747]). "Finished" is not the same
    /// question as "reached EOF": the drain thread marks itself finished even
    /// when `read(2)` failed, so that a waiter does not burn the full timeout
    /// on a pipe that broke. Recording that as EOF is what let an unreadable
    /// pipe earn the "the worker wrote NOTHING" diagnosis.
    ///
    /// [#747]: https://github.com/hherb/kastellan/issues/747
    drain_end: Arc<AtomicU8>,
}

impl StderrTail {
    /// A tail retaining at most `cap` lines (oldest evicted first). `cap == 0`
    /// retains nothing.
    pub fn new(cap: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::new())),
            cap,
            drain_end: Arc::new(AtomicU8::new(DRAIN_IN_PROGRESS)),
        }
    }

    /// Record how the drain finished. Called by the drain thread as it
    /// returns.
    ///
    /// `pub(crate)`, not `pub`: outside this crate this is the drain thread's
    /// to set, and a caller that set it early would make
    /// [`Self::wait_for_drain`] lie. In-crate it is reachable so a test can
    /// stand in for the drain thread — the death-tail tests need to land a
    /// line *after* the collector is already waiting, which is the production
    /// race and cannot be staged without it.
    pub(crate) fn mark_drained(&self, end: DrainEnd) {
        self.drain_end.store(encode_drain_end(end), Ordering::Release);
    }

    /// Block until the drain thread finishes, or `timeout` elapses.
    ///
    /// Returns **how** it finished, or `None` if it had not finished in time.
    /// Callers use this on the ERROR path only: a worker that died owes an
    /// explanation, and the few milliseconds the pipe needs to flush are worth
    /// spending to get one. A `None` return still leaves whatever arrived so
    /// far readable — the tail is a bounded ring, not a transaction.
    ///
    /// ⚠️ **`Some(DrainEnd::ReadError)` is not success** ([#747]). It means the
    /// wait is over, not that we read everything; see [`DrainEnd`].
    ///
    /// [#747]: https://github.com/hherb/kastellan/issues/747
    pub fn wait_for_drain(&self, timeout: Duration) -> Option<DrainEnd> {
        let deadline = Instant::now() + timeout;
        // ⚠️ Once per WAIT, not once per poll: this loop runs every 2 ms for the
        // whole cap, so an unconditional log here is ~125 identical lines on a
        // worker-failure path. The flag is the whole reason the warning is not
        // inside `decode_drain_end`.
        let mut warned = false;
        loop {
            let raw = self.drain_end.load(Ordering::Acquire);
            if let Some(end) = decode_drain_end(raw) {
                return Some(end);
            }
            // Unrecognised is NOT the same as in-progress, though both wait.
            // Only the first means something is wrong with us.
            if !warned && !is_known_drain_byte(raw) {
                warned = true;
                tracing::error!(
                    raw,
                    "unrecognised drain_end encoding; treating as still-draining, so this \
                     caller will burn the full stderr drain wait and then report a PARTIAL \
                     capture for a worker whose drain may well have finished"
                );
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Append one line, evicting the oldest if at capacity.
    fn push(&self, line: String) {
        if self.cap == 0 {
            return;
        }
        let mut guard = self.lines.lock().expect("stderr tail not poisoned");
        while guard.len() >= self.cap {
            guard.pop_front();
        }
        guard.push_back(line);
    }

    /// Snapshot the retained lines, oldest first.
    pub fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .expect("stderr tail not poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// Read `reader` (a worker's stderr) to EOF, logging each chunk at `debug` and —
/// when `tail` is given — splitting complete lines into it.
///
/// Reads **raw bytes**, not `BufRead::lines`: a lines iterator yields an `Err` on
/// the first invalid-UTF-8 byte and would stop draining, re-opening the very
/// deadlock this guards against. Each chunk is logged lossily so non-UTF-8 bytes
/// surface as `�` rather than halting the drain. Blank lines are not retained in
/// the tail (diagnostic noise).
///
/// Returns **how the read ended**, which the caller stores on the tail so a
/// renderer can tell "the worker wrote nothing" from "we could not read the
/// worker's pipe" ([#747](https://github.com/hherb/kastellan/issues/747)).
#[must_use = "the drain outcome is what distinguishes a silent worker from an unreadable pipe (#747)"]
pub fn drain_reader<R: Read>(pid: u32, mut reader: R, tail: Option<&StderrTail>) -> DrainEnd {
    let mut buf = [0u8; 8192];
    let mut carry = String::new();
    let end = loop {
        match reader.read(&mut buf) {
            Ok(0) => break DrainEnd::Eof, // pipe closed (worker exited)
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                // Neutralise terminal-control characters before the bytes reach
                // `tracing`. A compromised worker is in scope, the daemon log is
                // read in a terminal, and an ESC (or the 8-bit CSI that does the
                // same job) in this stream is an ANSI sequence executing in
                // whoever is tailing it. Same class the prompt escaper uses —
                // one definition, in `untrusted_text`.
                //
                // ⚠️ On the LOG COPY only. `\n` is in that class (it is what
                // would forge a row in a prompt), so neutralising `chunk` itself
                // would replace every newline with a space and the line split
                // below would never fire again — one line, forever, silently.
                // The split runs on the raw text; each line is neutralised as it
                // enters the tail, in `push_trimmed`.
                tracing::debug!(
                    worker_pid = pid,
                    "worker stderr: {}",
                    crate::untrusted_text::neutralise_controls(chunk.trim_end())
                );
                if let Some(tail) = tail {
                    carry.push_str(&chunk);
                    while let Some(nl) = carry.find('\n') {
                        let line: String = carry.drain(..=nl).collect();
                        push_trimmed(tail, &line);
                    }
                    // Bound the newline-free remainder: a worker that never emits
                    // a `\n` can't grow `carry` without limit (#350 review).
                    if carry.len() >= MAX_CARRY_BYTES {
                        push_trimmed(tail, &carry);
                        carry.clear();
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // A genuine read error. The pipe is gone and there is nothing left
            // to drain — but this is emphatically NOT the same as EOF, because
            // we do not know what the worker wrote after it (#747).
            //
            // ⚠️ **Logged here because this is the only place the errno
            // exists.** `DrainEnd` is `Copy` and lives in an `AtomicU8`, so it
            // cannot carry the error, and the renderers' "suspect the
            // transport (a torn-down guest's vsock/pty fd)" sentence is a
            // hard-coded guess. It is right for `EIO`; for `EBADF` the fault
            // is a double-close in OUR fd handling and that sentence sends the
            // operator to audit a guest that is fine. Dropping the kind here
            // would leave nothing anywhere to tell the two apart.
            Err(e) => {
                tracing::warn!(
                    worker_pid = pid,
                    error = %e,
                    kind = ?e.kind(),
                    "worker stderr drain ended on a READ ERROR; the remainder is unrecoverable"
                );
                break DrainEnd::ReadError;
            }
        }
    };
    // Flush a trailing partial line (output with no terminating newline).
    if let Some(tail) = tail {
        push_trimmed(tail, &carry);
    }
    end
}

/// Push `line` into `tail` after stripping line endings, skipping blanks.
///
/// **This is where the tail's text is neutralised**, and the placement is
/// load-bearing: the tail feeds `format_death_report` and
/// [`format_worker_failure_report`], both of which log at warn — visible in an
/// operator's terminal by default — and doing it any earlier would eat the `\n`
/// the caller splits on. Line endings are stripped first, so the neutralisation
/// only ever sees the interior of a line.
fn push_trimmed(tail: &StderrTail, line: &str) {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if !trimmed.is_empty() {
        tail.push(crate::untrusted_text::neutralise_controls(trimmed));
    }
}

/// Spawn a detached thread draining `stderr` at `debug` (no retained tail). Keeps
/// the pipe empty so the worker can't deadlock writing to a full stderr buffer;
/// the thread ends when the worker's stderr closes (process exit).
pub fn spawn_drain(pid: u32, stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        // With no tail there is no *report* to qualify — but a `ReadError` is
        // still operator-actionable here, and in a way the tailed paths are
        // not. This thread is the only thing keeping the pipe empty (see the
        // module header: a worker past the ~64 KiB buffer "would then block on
        // write and deadlock"). After a read error that protection is gone and
        // the thread exits, so the sidecar can hang on its next large write
        // with nothing in the log pointing at the pipe.
        if drain_reader(pid, stderr, None) == DrainEnd::ReadError {
            tracing::warn!(
                worker_pid = pid,
                "stderr drain ended on a READ ERROR; this stderr is no longer drained and \
                 the process may block writing to a full pipe"
            );
        }
    });
}

/// Drain `reader` into `tail` and record **how the drain ended** on it.
///
/// Always marks, even when the read failed: a caller waiting for an
/// explanation must not block for the full [`TAIL_DRAIN_WAIT`] because the pipe
/// broke. ⚠️ What it marks *with* is the
/// [#747](https://github.com/hherb/kastellan/issues/747) fix — before it, a
/// failed read was recorded as EOF and an empty tail then earned the "the
/// worker wrote NOTHING — suspect a kill" diagnosis.
///
/// ⚠️ **Extracted from [`spawn_drain_with_tail`]'s closure so a test can reach
/// it at all.** In production the reader is a [`std::process::ChildStderr`],
/// which no unit test can construct without a real child — so a mutant
/// replacing `end` with a hard-coded `DrainEnd::Eof` (i.e. reintroducing #747
/// exactly) would survive every suite in the tree. Generic over `Read` for the
/// same reason [`collect_tail_after_drain`] is a free function
/// [[unreachable-success-path-proves-nothing]].
fn drain_and_mark<R: Read>(pid: u32, reader: R, tail: &StderrTail) {
    let end = drain_reader(pid, reader, Some(tail));
    tail.mark_drained(end);
}

/// Like [`spawn_drain`] but also retains a bounded tail of recent lines, returned
/// for the caller to [`snapshot`](StderrTail::snapshot) when the worker dies.
pub fn spawn_drain_with_tail(pid: u32, stderr: std::process::ChildStderr) -> StderrTail {
    let tail = StderrTail::new(DEFAULT_TAIL_LINES);
    let thread_tail = tail.clone();
    std::thread::spawn(move || drain_and_mark(pid, stderr, &thread_tail));
    tail
}

/// How long a caller reporting a worker failure waits for the stderr drainer to
/// reach EOF before snapshotting the tail.
///
/// ⚠️ **A cap, not a cost.** [`StderrTail::wait_for_drain`] checks the flag
/// *before* its first sleep, so a worker whose stderr has already been drained
/// costs ~0 rather than this — not even the 2 ms poll interval.
pub const TAIL_DRAIN_WAIT: Duration = Duration::from_millis(250);

/// Snapshot a worker's stderr tail, **waiting for the drainer first**.
///
/// The one copy for every reporting path (#737). It was two: `tool_host`'s
/// `EARLY_EXIT_DRAIN_WAIT` + inline wait for a tool worker, and
/// `worker_lifecycle::persistent`'s `DEATH_DRAIN_WAIT` + `collect_death_tail`
/// for a persistent one — identical values doing an identical job, with the
/// latter's own doc saying "if a third appears, hoist them". #737 was that
/// third caller, and hoisting is what made #746 a three-line change rather
/// than a fourth copy.
///
/// There are now **three** production callers: `tool_host::spawn_worker`,
/// `worker_lifecycle::persistent`, and the egress sidecar bring-up
/// (`egress::spawn::stderr_note`, added by #746).
///
/// ⚠️ **The wait is the whole function, and it is not an optimisation.** A
/// worker that has just closed its pipes has usually NOT been drained yet, so
/// snapshotting immediately yields an empty ring and the report renders
/// "wrote NOTHING" / "no stderr captured" for a worker that explained itself a
/// millisecond later. That contentless line is the exact defect #730 exists to
/// remove, and #735's review found this path had silently lost the wait.
///
/// ⚠️ **Extracted so a test can reach the decision at all.** In every caller
/// the wait sits behind a spawned child no unit test can construct — a real
/// `Client` in the two worker paths, a sidecar `Child` observed via `try_wait`
/// in the egress one — so a mutant deleting it survived every suite in the
/// tree, measured. That is why this is a free function over a [`StderrTail`]
/// rather than a line inside each caller
/// [[unreachable-success-path-proves-nothing]].
pub fn collect_tail_after_drain(tail: &StderrTail) -> CapturedTail {
    // ⚠️ **Wait FIRST, snapshot SECOND.** Hoisting the snapshot above the wait
    // reads as a harmless reorder and is the whole defect: it captures the ring
    // as it was before the drainer had flushed, so a worker that explained
    // itself is reported as having said nothing. Two tests pin this order, and
    // both stage the real race with a drainer that writes 30 ms late:
    // `collecting_a_worker_tail_waits_for_the_drainer_instead_of_racing_it`
    // (below, #735) and `egress::spawn::tests::
    // stderr_note_waits_for_the_drainer_instead_of_racing_it` (#746). Under a
    // hoisted snapshot the first sees `KnownSilent` where it asserts the
    // worker's line, and the second renders "no stderr captured".
    let end = tail.wait_for_drain(TAIL_DRAIN_WAIT);
    let lines = tail.snapshot();
    // `from_drain` rather than a struct literal: `CapturedTail`'s fields are
    // private (#745's review — a `t.complete = true` forged the one claim the
    // type exists to gate). It takes the wait's own `Option<DrainEnd>`, so
    // there is no place here to get the mapping wrong.
    CapturedTail::from_drain(lines, end)
}

mod captured;
pub use captured::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn tail_retains_bounded_lines_oldest_evicted() {
        let tail = StderrTail::new(2);
        tail.push("one".into());
        tail.push("two".into());
        tail.push("three".into());
        assert_eq!(tail.snapshot(), vec!["two".to_string(), "three".to_string()]);
    }

    #[test]
    fn tail_zero_cap_retains_nothing() {
        let tail = StderrTail::new(0);
        tail.push("x".into());
        assert!(tail.snapshot().is_empty());
    }

    #[test]
    fn drain_reader_populates_tail_with_complete_and_partial_lines() {
        let tail = StderrTail::new(10);
        // Two newline-terminated lines + a trailing partial line (no `\n`).
        let data = b"first line\nsecond line\npartial".to_vec();
        assert_eq!(drain_reader(0, Cursor::new(data), Some(&tail)), DrainEnd::Eof);
        assert_eq!(
            tail.snapshot(),
            vec!["first line".to_string(), "second line".to_string(), "partial".to_string()]
        );
    }

    /// A reader that yields its bytes and then fails, standing in for the
    /// guest fd that returns `EIO` when a micro-VM or container is torn down —
    /// [#747](https://github.com/hherb/kastellan/issues/747)'s motivating case.
    struct FailsAfterData {
        data: Cursor<Vec<u8>>,
        /// Errors yielded before any real read, to exercise the `EINTR` arm.
        interrupts: usize,
    }

    impl Read for FailsAfterData {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.interrupts > 0 {
                self.interrupts -= 1;
                return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "EINTR"));
            }
            match self.data.read(buf)? {
                // Where a real pipe would report EOF, report a hard failure
                // instead: the drain stops, but NOT because the worker is done.
                0 => Err(std::io::Error::other("EIO: transport went away")),
                n => Ok(n),
            }
        }
    }

    #[test]
    fn a_read_error_ends_the_drain_as_failed_not_as_eof() {
        // #747's core. Before this, `drain_reader` returned nothing and the
        // spawner marked the tail drained unconditionally, so a pipe that blew
        // up read as "the worker closed its stderr" — and an empty tail then
        // earned the "wrote NOTHING — suspect a kill" diagnosis for a worker
        // that may have explained itself perfectly into a pipe we lost.
        let tail = StderrTail::new(8);
        let reader = FailsAfterData {
            data: Cursor::new(b"worker booting\n".to_vec()),
            interrupts: 0,
        };
        assert_eq!(
            drain_reader(0, reader, Some(&tail)),
            DrainEnd::ReadError,
            "a drain stopped by a read error must say so; reporting EOF is what lets a \
             renderer diagnose the worker for a fault in our own transport"
        );
        assert_eq!(
            tail.snapshot(),
            vec!["worker booting".to_string()],
            "POSITIVE CONTROL: whatever DID arrive before the error is still retained — a \
             failed drain is not a reason to discard the lines we got"
        );
    }

    #[test]
    fn an_interrupted_read_is_retried_rather_than_ending_the_drain() {
        // The `EINTR` arm is `continue`, not `break`, and the distinction is
        // easy to lose while editing the loop that #747 changed. A signal
        // arriving mid-read must not truncate a worker's explanation.
        let tail = StderrTail::new(8);
        let reader = FailsAfterData {
            data: Cursor::new(b"survived the signal\n".to_vec()),
            interrupts: 3,
        };
        assert_eq!(drain_reader(0, reader, Some(&tail)), DrainEnd::ReadError);
        assert_eq!(
            tail.snapshot(),
            vec!["survived the signal".to_string()],
            "three EINTRs before the first real read must be retried, not treated as the end \
             of the stream"
        );
    }

    #[test]
    fn the_drain_thread_records_a_read_error_on_the_tail() {
        // The seam between `drain_reader`'s verdict and the tail every
        // renderer reads. In production this runs on a detached thread over a
        // `ChildStderr`, which is exactly why it is a separate generic
        // function: hard-coding `DrainEnd::Eof` here reintroduces #747 in full
        // and no other test in the tree can see it.
        let tail = StderrTail::new(8);
        drain_and_mark(
            0,
            FailsAfterData { data: Cursor::new(b"last thing\n".to_vec()), interrupts: 0 },
            &tail,
        );
        assert_eq!(
            tail.wait_for_drain(Duration::from_millis(0)),
            Some(DrainEnd::ReadError),
            "the drain thread must record WHY it stopped, not merely that it stopped"
        );

        // POSITIVE CONTROL: the same helper over a clean reader records EOF,
        // so the assertion above cannot pass by the helper always reporting a
        // failure.
        let clean = StderrTail::new(8);
        drain_and_mark(0, Cursor::new(b"all done\n".to_vec()), &clean);
        assert_eq!(clean.wait_for_drain(Duration::from_millis(0)), Some(DrainEnd::Eof));
    }

    #[test]
    fn a_failed_drain_is_collected_as_drain_failed_not_as_complete() {
        // The end-to-end of #747 at the seam a unit test can reach: the
        // spawner stores whatever `drain_reader` returned, and
        // `collect_tail_after_drain` must carry it into the rendered state
        // rather than flattening it to "complete".
        let tail = StderrTail::new(8);
        tail.mark_drained(DrainEnd::ReadError);
        let collected = collect_tail_after_drain(&tail);
        assert_eq!(
            collected.state(),
            TailState::DrainFailedSilent,
            "an empty tail whose drain FAILED must never reach a renderer as KnownSilent — \
             that is the one state licensed to print a diagnosis"
        );
    }

    #[test]
    fn an_unrecognised_drain_byte_is_distinguishable_from_a_drain_still_in_progress() {
        // Both decode to `None` — that is the fail-safe, and it is right — but a
        // caller must be able to tell "not finished yet" from "this encoding is
        // broken", because the second deserves a log line and the first must
        // never produce one. Without this split the warning would have to live
        // inside `decode_drain_end`, which `wait_for_drain` calls every 2 ms for
        // the full 250 ms cap: ~125 identical error lines per worker failure,
        // on the one path that must stay readable.
        for known in [DRAIN_IN_PROGRESS, DRAIN_EOF, DRAIN_READ_ERROR] {
            assert!(
                is_known_drain_byte(known),
                "{known} is part of the encoding and must never be reported as corrupt"
            );
        }
        for unknown in [3u8, 99, 255] {
            assert!(
                !is_known_drain_byte(unknown),
                "{unknown} is not a value `encode_drain_end` can produce, so a tail holding it \
                 means the encoding has drifted and someone must be told"
            );
        }
    }

    #[test]
    fn the_drain_end_encoding_round_trips_and_fails_safe() {
        // The atomic's three states are the only thing between a drain outcome
        // and the renderer. Pinned including the impossible input, whose fail
        // direction matters: an unrecognised byte must read as STILL DRAINING,
        // so a caller waits rather than claiming a completeness it has not
        // established.
        for end in [DrainEnd::Eof, DrainEnd::ReadError] {
            assert_eq!(decode_drain_end(encode_drain_end(end)), Some(end), "{end:?}");
        }
        assert_eq!(decode_drain_end(DRAIN_IN_PROGRESS), None);
        assert_eq!(
            decode_drain_end(99),
            None,
            "an unknown byte must not decode as a finished drain"
        );
        // The two encodings must differ, or the distinction the whole issue is
        // about cannot survive the atomic.
        assert_ne!(encode_drain_end(DrainEnd::Eof), encode_drain_end(DrainEnd::ReadError));
        assert_ne!(encode_drain_end(DrainEnd::Eof), DRAIN_IN_PROGRESS);
        assert_ne!(encode_drain_end(DrainEnd::ReadError), DRAIN_IN_PROGRESS);
    }

    #[test]
    fn drain_reader_skips_blank_lines_and_survives_non_utf8() {
        let tail = StderrTail::new(10);
        // A blank line between two real ones, plus a stray non-UTF-8 byte (0xff)
        // that must not halt the drain.
        let mut data = b"alpha\n\nbeta".to_vec();
        data.push(0xff);
        assert_eq!(drain_reader(0, Cursor::new(data), Some(&tail)), DrainEnd::Eof);
        let snap = tail.snapshot();
        assert_eq!(snap[0], "alpha");
        // The blank line is skipped; the trailing chunk (beta + replacement char)
        // is retained as one line.
        assert_eq!(snap.len(), 2, "blank line not retained: {snap:?}");
        assert!(snap[1].starts_with("beta"), "got {:?}", snap[1]);
    }

    #[test]
    fn drain_reader_bounds_newline_free_carry() {
        // A worker streaming to stderr without ever emitting a `\n` must not grow
        // the carry buffer without limit; the drain flushes it as synthetic lines
        // once it crosses MAX_CARRY_BYTES, so the tail captures the output and
        // memory stays bounded (#350 review).
        let tail = StderrTail::new(DEFAULT_TAIL_LINES);
        let data = vec![b'x'; MAX_CARRY_BYTES * 3 + 7]; // no newline anywhere
        assert_eq!(drain_reader(0, Cursor::new(data), Some(&tail)), DrainEnd::Eof);
        let snap = tail.snapshot();
        assert!(!snap.is_empty(), "newline-free output should still be captured");
        // No retained line exceeds the cap by more than a single read chunk's worth.
        for line in &snap {
            assert!(
                line.len() <= MAX_CARRY_BYTES + 8192,
                "carry line not bounded: {} bytes",
                line.len()
            );
        }
    }

    #[test]
    fn drain_reader_without_tail_does_not_panic() {
        // The no-tail path (tool_host's use) just drains; it must run cleanly.
        assert_eq!(drain_reader(0, Cursor::new(b"noisy\nworker\n".to_vec()), None), DrainEnd::Eof);
    }

    #[test]
    fn wait_for_drain_returns_once_the_drainer_finishes() {
        let tail = StderrTail::new(4);
        assert!(
            tail.wait_for_drain(Duration::from_millis(10)).is_none(),
            "an un-drained tail must time out rather than claim completion"
        );
        tail.mark_drained(DrainEnd::Eof);
        assert!(
            tail.wait_for_drain(Duration::from_millis(10)) == Some(DrainEnd::Eof),
            "a tail drained to EOF must be observed as drained, and as having reached EOF"
        );
    }

    #[test]
    fn drain_then_mark_makes_the_tail_observably_complete() {
        // The whole point of the flag: a caller reacting to a worker death must
        // be able to WAIT for the explanation instead of racing it. This is the
        // exact sequence `spawn_drain_with_tail`'s thread runs, driven from a
        // Cursor so it needs no real child process.
        let tail = StderrTail::new(4);
        let t = tail.clone();
        let h = std::thread::spawn(move || {
            assert_eq!(drain_reader(0, Cursor::new(b"boom\n".to_vec()), Some(&t)), DrainEnd::Eof);
            t.mark_drained(DrainEnd::Eof);
        });
        assert!(
            tail.wait_for_drain(Duration::from_secs(5)) == Some(DrainEnd::Eof),
            "the drain must be observed as complete"
        );
        h.join().unwrap();
        assert_eq!(tail.snapshot(), vec!["boom".to_string()]);
    }

    #[test]
    fn drain_reader_neutralises_terminal_controls_before_they_reach_the_log() {
        // A compromised worker is in scope, and this stream now reaches the
        // daemon log at WARN via `format_worker_failure_report` — i.e. visible in
        // an operator's terminal by default rather than only under `debug`.
        // An ESC here would be an ANSI sequence executing in that terminal.
        let tail = StderrTail::new(10);
        assert_eq!(
            drain_reader(
                0,
                Cursor::new("red\u{1b}[31m and 8-bit \u{9b}31m\nsecond\n".as_bytes().to_vec()),
                Some(&tail),
            ),
            DrainEnd::Eof
        );
        let got = tail.snapshot();
        // TWO lines, not one: `\n` is itself in the neutralised class, so
        // neutralising before the split would collapse the whole stream into a
        // single line — which is how the first version of this broke.
        assert_eq!(got.len(), 2, "the line split must survive neutralisation: {got:?}");
        assert_eq!(got[1], "second", "{got:?}");
        assert!(
            !got[0].contains('\u{1b}') && !got[0].contains('\u{9b}'),
            "both the 7-bit ESC and the 8-bit CSI must be neutralised: {:?}",
            got[0]
        );
        assert!(
            got[0].contains("red") && got[0].contains("[31m"),
            "the surrounding text must survive — a log line is evidence: {:?}",
            got[0]
        );
    }
    #[test]
    fn collecting_a_worker_tail_waits_for_the_drainer_instead_of_racing_it() {
        // The property #735 restored: a worker that has just closed its pipes
        // has usually NOT been drained yet, so snapshotting immediately yields
        // an empty ring and `format_death_report` renders "no stderr captured"
        // for a worker that explained itself a millisecond later. That is the
        // contentless line #730's whole point was to avoid.
        //
        // The drainer here lands its line AFTER this thread has already called
        // `collect_tail_after_drain`, which is exactly the race in production. Deleting
        // the `wait_for_drain` makes this return empty and the test fails.
        let tail = StderrTail::new(8);
        let writer = tail.clone();
        let drain = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let end = drain_reader(
                0,
                std::io::Cursor::new(b"matrix sync failed, refusing to continue\n".to_vec()),
                Some(&writer),
            );
            writer.mark_drained(end);
        });

        let collected = collect_tail_after_drain(&tail);
        drain.join().expect("the fixture's drain thread");

        // Asserts the WHOLE `CapturedTail`, flag included: the drainer finished
        // well inside the 250 ms cap, so the correct verdict is `complete`.
        // Before #732 this function returned only the lines, and a mutant that
        // reported every drain as timed-out was invisible here.
        assert_eq!(
            collected,
            CapturedTail::complete(vec!["matrix sync failed, refusing to continue".to_string()]),
            "the dying worker's explanation must be collected, not raced — an empty tail here \
             is the `no stderr captured` line that made #730's fix contentless — and the \
             capture must report itself COMPLETE, or the renderers hedge a tail that is in \
             fact the worker's last word"
        );
    }

    #[test]
    fn a_drain_that_never_finishes_is_reported_as_incomplete() {
        // #732: `collect_tail_after_drain` used to discard `wait_for_drain`'s
        // verdict, so this state was indistinguishable from a completed drain
        // and the renderers then asserted a diagnosis they had not earned.
        //
        // No `mark_drained` is ever called, so the wait burns its whole cap.
        let tail = StderrTail::new(8);
        tail.push("worker booting".to_string());

        let started = Instant::now();
        let collected = collect_tail_after_drain(&tail);
        let waited = started.elapsed();

        assert_eq!(
            collected.state(),
            TailState::FirstWords(&["worker booting".to_string()]),
            "a drain that never reached EOF must be reported as FIRST words; saying otherwise \
             lets `format_death_report` call a boot line the worker's most recent output. The \
             lines must still be there — an incomplete drain still yields whatever arrived, \
             since the tail is a bounded ring and not a transaction"
        );
        // POSITIVE CONTROL on the fixture itself: if the cap were not actually
        // being waited out, this test would pass for the wrong reason — a
        // `wait_for_drain` that returned `false` immediately is a different
        // (and also broken) implementation.
        assert!(
            waited >= TAIL_DRAIN_WAIT,
            "the collector must actually wait out its cap before giving up; waited {waited:?}, \
             cap is {TAIL_DRAIN_WAIT:?}"
        );
    }

}
