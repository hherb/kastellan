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
//! drain threads that fill it. What is then *said* about a dead worker — the
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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A bounded, shared ring of a worker's most-recent stderr lines. Cloneable
/// (it's `Arc`-backed): the drain thread pushes, the owning caller snapshots when
/// the worker dies.
#[derive(Clone)]
pub struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
    cap: usize,
    /// Set once the drain thread has read the pipe to EOF.
    ///
    /// Without this a caller that reacts to a worker's death races the drainer
    /// and usually wins: it snapshots an EMPTY tail and reports "no stderr
    /// captured" for a worker that explained itself perfectly well a
    /// millisecond later. That failure mode is indistinguishable from the
    /// worker having said nothing, which is precisely the ambiguity #666 exists
    /// to remove — so the flag is load-bearing, not a convenience.
    drained: Arc<AtomicBool>,
}

impl StderrTail {
    /// A tail retaining at most `cap` lines (oldest evicted first). `cap == 0`
    /// retains nothing.
    pub fn new(cap: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::new())),
            cap,
            drained: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Mark the pipe as read to EOF. Called by the drain thread when it returns.
    ///
    /// `pub(crate)`, not `pub`: outside this crate the flag is the drain
    /// thread's to set, and a caller that set it early would make
    /// [`Self::wait_for_drain`] lie. In-crate it is reachable so a test can
    /// stand in for the drain thread — `worker_lifecycle::persistent`'s
    /// death-tail test needs to land a line *after* the collector is already
    /// waiting, which is the production race and cannot be staged without it.
    pub(crate) fn mark_drained(&self) {
        self.drained.store(true, Ordering::Release);
    }

    /// Block until the drain thread reaches EOF, or `timeout` elapses.
    ///
    /// Returns `true` if the drain completed. Callers use this on the ERROR
    /// path only: a worker that died owes an explanation, and the few
    /// milliseconds the pipe needs to flush are worth spending to get one. A
    /// `false` return still leaves whatever arrived so far readable — the tail
    /// is a bounded ring, not a transaction.
    pub fn wait_for_drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.drained.load(Ordering::Acquire) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
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
pub fn drain_reader<R: Read>(pid: u32, mut reader: R, tail: Option<&StderrTail>) {
    let mut buf = [0u8; 8192];
    let mut carry = String::new();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF — pipe closed (worker exited)
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
            Err(_) => break, // genuine read error — pipe gone, nothing left to drain
        }
    }
    // Flush a trailing partial line (output with no terminating newline).
    if let Some(tail) = tail {
        push_trimmed(tail, &carry);
    }
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
    std::thread::spawn(move || drain_reader(pid, stderr, None));
}

/// Like [`spawn_drain`] but also retains a bounded tail of recent lines, returned
/// for the caller to [`snapshot`](StderrTail::snapshot) when the worker dies.
pub fn spawn_drain_with_tail(pid: u32, stderr: std::process::ChildStderr) -> StderrTail {
    let tail = StderrTail::new(DEFAULT_TAIL_LINES);
    let thread_tail = tail.clone();
    std::thread::spawn(move || {
        drain_reader(pid, stderr, Some(&thread_tail));
        // Always mark, even if the read ended in an error: a caller waiting for
        // an explanation must not block for the full timeout because the pipe
        // broke.
        thread_tail.mark_drained();
    });
    tail
}

/// How long a caller reporting a worker failure waits for the stderr drainer to
/// reach EOF before snapshotting the tail.
///
/// ⚠️ **A cap, not a cost.** [`StderrTail::wait_for_drain`] returns as soon as
/// `mark_drained` lands, so a worker whose stderr has already closed costs the
/// ~2 ms poll interval rather than this.
pub const TAIL_DRAIN_WAIT: Duration = Duration::from_millis(250);

/// Snapshot a worker's stderr tail, **waiting for the drainer first**.
///
/// The one copy for both reporting paths (#737). It was two: `tool_host`'s
/// `EARLY_EXIT_DRAIN_WAIT` + inline wait for a tool worker, and
/// `worker_lifecycle::persistent`'s `DEATH_DRAIN_WAIT` + `collect_death_tail`
/// for a persistent one — identical values doing an identical job, with the
/// latter's own doc saying "if a third appears, hoist them". #737 was that
/// third caller.
///
/// ⚠️ **The wait is the whole function, and it is not an optimisation.** A
/// worker that has just closed its pipes has usually NOT been drained yet, so
/// snapshotting immediately yields an empty ring and the report renders
/// "wrote NOTHING" / "no stderr captured" for a worker that explained itself a
/// millisecond later. That contentless line is the exact defect #730 exists to
/// remove, and #735's review found this path had silently lost the wait.
///
/// ⚠️ **Extracted so a test can reach the decision at all.** In its two
/// callers the wait sits behind a real `Client` over a spawned child, which no
/// unit test can construct — so a mutant deleting it survived every suite in
/// the tree, measured. That is why this is a free function over a
/// [`StderrTail`] rather than a line inside each caller
/// [[unreachable-success-path-proves-nothing]].
pub fn collect_tail_after_drain(tail: &StderrTail) -> CapturedTail {
    let complete = tail.wait_for_drain(TAIL_DRAIN_WAIT);
    CapturedTail { lines: tail.snapshot(), complete }
}

/// A worker's retained stderr **and whether it is all of it**
/// ([#732](https://github.com/hherb/kastellan/issues/732)).
///
/// ## Why the flag travels with the lines
///
/// [`collect_tail_after_drain`] used to discard [`StderrTail::wait_for_drain`]'s
/// return value, and the renderers then stated the opposite of what the system
/// knew. `StderrTail::drained`'s own doc calls that ambiguity "precisely the
/// ambiguity #666 exists to remove — so the flag is load-bearing, not a
/// convenience", and the one call site dropped it on the floor.
///
/// Two separate lies came out of that, and they are why this is a struct rather
/// than a second parameter someone can forget to pass:
///
/// * **Empty and incomplete read as "the worker said nothing."** The report
///   then printed a *diagnosis* — suspect a kill, a jail that refused the
///   spawn, seccomp — for a worker that explained itself at 260 ms and was
///   simply not waited for. The operator audits cgroup limits and seccomp
///   profiles for a fault that was in the guest's Python. That is #719
///   reproduced, except that this time the report actively points the wrong
///   way.
/// * **Non-empty and incomplete read as "its last words."** The ring evicts
///   **oldest** first, so under a *complete* drain the tail really is the last
///   thing the worker said. Under an incomplete one it is the **first** thing —
///   the boot lines — labelled as the last. An operator correlating "last
///   words" against the moment of death is reading startup noise.
///
/// ⚠️ **`complete: false` is not "no data".** Whatever arrived before the cap
/// is still here and still worth printing; the tail is a bounded ring, not a
/// transaction. What changes is what may be *claimed* about it.
/// ⚠️ **The fields are PRIVATE, and that is the difference between this type
/// and a tuple with a good doc comment.** With `pub complete`, a caller could
/// write `t.complete = true` and forge the one claim the type exists to gate —
/// which is exactly the door [`StderrTail::mark_drained`] is `pub(crate)` to
/// keep shut one layer down. Read access goes through [`CapturedTail::lines`],
/// [`CapturedTail::is_complete`] and [`CapturedTail::is_known_silent`];
/// construction goes through [`CapturedTail::complete`] /
/// [`CapturedTail::partial`] or [`collect_tail_after_drain`], all of which
/// state the claim being made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTail {
    /// The lines that had arrived when the snapshot was taken.
    lines: Vec<String>,
    /// `true` when the drain thread reached EOF within [`TAIL_DRAIN_WAIT`], so
    /// these lines are the worker's complete retained stderr.
    complete: bool,
}

impl CapturedTail {
    /// A tail known to be the whole of what the worker wrote.
    ///
    /// For tests and for callers that did not have to wait. Named rather than
    /// constructed field-by-field so a test reads as the *claim* it is making.
    pub fn complete(lines: Vec<String>) -> Self {
        Self { lines, complete: true }
    }

    /// A tail whose drain timed out: possibly incomplete, possibly empty only
    /// because nothing had arrived yet.
    pub fn partial(lines: Vec<String>) -> Self {
        Self { lines, complete: false }
    }

    /// The lines that had arrived, whether or not that is all of them.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// `true` when the drain reached EOF, so these lines are the worker's
    /// complete retained stderr and really are its most recent ones.
    ///
    /// ⚠️ **Asking this is not the same as asking [`Self::is_known_silent`].**
    /// It answers "may I call these the *last* words", not "may I say the
    /// worker was silent" — a renderer needs both, and needs them in that
    /// order. See [`crate::worker_stderr::format_worker_failure_report`] for
    /// the four-arm shape that gets it right.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// `true` when the worker is **known** to have written nothing.
    ///
    /// ⚠️ **Not the same as `lines().is_empty()`, and that is the whole
    /// point.** An empty *partial* tail is "we did not wait long enough to
    /// find out", which must never be rendered as the worker having stayed
    /// silent.
    ///
    /// ⚠️ **This is the only predicate that licenses a *diagnosis*** — the
    /// "suspect a kill (wall-clock/OOM/seccomp)" sentence and "no stderr
    /// captured". Both renderers match it **first**, so their later
    /// `lines().is_empty()` arm can only be a partial. That ordering is a
    /// convention the compiler does not check: a new renderer that tests the
    /// vector on its own will silently lose the distinction, which is what
    /// #732 was. Copy the existing arm order.
    pub fn is_known_silent(&self) -> bool {
        self.lines.is_empty() && self.complete
    }
}

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
        drain_reader(0, Cursor::new(data), Some(&tail));
        assert_eq!(
            tail.snapshot(),
            vec!["first line".to_string(), "second line".to_string(), "partial".to_string()]
        );
    }

    #[test]
    fn drain_reader_skips_blank_lines_and_survives_non_utf8() {
        let tail = StderrTail::new(10);
        // A blank line between two real ones, plus a stray non-UTF-8 byte (0xff)
        // that must not halt the drain.
        let mut data = b"alpha\n\nbeta".to_vec();
        data.push(0xff);
        drain_reader(0, Cursor::new(data), Some(&tail));
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
        drain_reader(0, Cursor::new(data), Some(&tail));
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
        drain_reader(0, Cursor::new(b"noisy\nworker\n".to_vec()), None);
    }

    #[test]
    fn wait_for_drain_returns_once_the_drainer_finishes() {
        let tail = StderrTail::new(4);
        assert!(
            !tail.wait_for_drain(Duration::from_millis(10)),
            "an un-drained tail must time out rather than claim completion"
        );
        tail.mark_drained();
        assert!(
            tail.wait_for_drain(Duration::from_millis(10)),
            "a drained tail must be observed as drained"
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
            drain_reader(0, Cursor::new(b"boom\n".to_vec()), Some(&t));
            t.mark_drained();
        });
        assert!(
            tail.wait_for_drain(Duration::from_secs(5)),
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
        drain_reader(
            0,
            Cursor::new("red\u{1b}[31m and 8-bit \u{9b}31m\nsecond\n".as_bytes().to_vec()),
            Some(&tail),
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
            drain_reader(
                0,
                std::io::Cursor::new(b"matrix sync failed, refusing to continue\n".to_vec()),
                Some(&writer),
            );
            writer.mark_drained();
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

        assert!(
            !collected.is_complete(),
            "a drain that never reached EOF must be reported INCOMPLETE; saying otherwise lets \
             `format_death_report` call a boot line the worker's most recent output"
        );
        assert_eq!(
            collected.lines(),
            ["worker booting".to_string()],
            "an incomplete drain still yields whatever arrived — the tail is a bounded ring, \
             not a transaction. Dropping the lines would trade one wrong report for another"
        );
        assert!(
            !collected.is_known_silent(),
            "a non-empty tail is never `known silent`, whatever the flag says"
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

    #[test]
    fn an_empty_tail_is_only_known_silent_when_the_drain_completed() {
        // The distinction the whole of #732 rests on, at its smallest. These
        // two values have identical `lines`, and exactly one of them licenses
        // the "suspect a kill (wall-clock/OOM/seccomp)" sentence.
        assert!(
            CapturedTail::complete(vec![]).is_known_silent(),
            "an EMPTY tail whose drain reached EOF is the worker genuinely saying nothing — \
             the one case a report may diagnose"
        );
        assert!(
            !CapturedTail::partial(vec![]).is_known_silent(),
            "an empty tail whose drain TIMED OUT is a fact about us, not about the worker. \
             Treating it as silence is what sends an operator to audit seccomp and cgroups \
             for a fault that was in the guest's Python (#732)"
        );
    }

}
