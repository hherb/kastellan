//! Reusable draining of a sandboxed worker's piped stderr.
//!
//! The sandbox backends spawn workers with `stderr(Stdio::piped())`, but the
//! JSON-RPC [`Client`](kastellan_protocol::client::Client) only reads stdout. A
//! worker that writes more than the ~64 KiB pipe buffer to stderr would then
//! **block on write and deadlock** (and the diagnostics are silently discarded).
//! Draining the pipe to EOF on a detached thread prevents both: the worker can't
//! stall, and each chunk surfaces at `debug` for troubleshooting.
//!
//! Two consumers share this:
//! - `tool_host::spawn_worker` drains tool-worker stderr ([`spawn_drain`]).
//! - the Matrix channel worker additionally retains a bounded **tail** of recent
//!   lines ([`spawn_drain_with_tail`]) so the driver can log the worker's death
//!   cause + exit status when it dies (#348).

use std::collections::VecDeque;
use std::io::Read;
use std::process::ExitStatus;
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
    fn mark_drained(&self) {
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
/// [`format_early_exit_report`], both of which log at warn — visible in an
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

/// Human-readable one-line summary of a worker's death for the daemon log: the
/// exit status (which distinguishes a clean `exit status: 1` — a deliberate
/// fail-loud exit — from a `signal: 6 (SIGABRT)` — a crash) plus the recent
/// stderr lines, joined for a single log record.
pub fn format_death_report(status: Option<ExitStatus>, stderr_tail: &[String]) -> String {
    let status_str = match status {
        Some(s) => s.to_string(),
        None => "exit status unknown (not yet reaped)".to_string(),
    };
    if stderr_tail.is_empty() {
        format!("worker exited ({status_str}); no stderr captured")
    } else {
        format!("worker exited ({status_str}); recent stderr: {}", stderr_tail.join(" | "))
    }
}

/// Explain an `EarlyExit` — the protocol client's "worker exited before
/// responding" — using the worker's own retained stderr.
///
/// `EarlyExit` is the single most content-free failure this system produces: it
/// says the pipe closed and nothing else. Three separate micro-VM production
/// defects all surfaced as exactly this string, and telling them apart took a
/// session (#666). The worker almost always *did* say why, on the stream nobody
/// was reading — this turns that stream into the message.
///
/// The empty case is deliberately not silent, and does not read like the
/// populated one: "said nothing" is itself a diagnosis (a hard kill, a jail
/// refused before the worker ran, a guest that never booted), and it points at
/// a different place to look than a worker that logged a reason.
pub fn format_early_exit_report(program: &str, method: &str, stderr_tail: &[String]) -> String {
    if stderr_tail.is_empty() {
        format!(
            "worker {program} exited before responding to {method}, and wrote NOTHING to \
             stderr — suspect a kill (wall-clock/OOM/seccomp) or a sandbox that refused the \
             spawn before the worker ran, rather than the worker's own logic"
        )
    } else {
        format!(
            "worker {program} exited before responding to {method}; its last words: {}",
            stderr_tail.join(" | ")
        )
    }
}

/// The [`format_early_exit_report`] counterpart for a worker whose stderr was
/// never piped, so there is no tail to quote.
///
/// A separate function rather than a third arm of the one above, because the
/// *input* is different: that one is given a tail and reports on its contents,
/// including when it is empty (a worker that ran and said nothing — a kill, a
/// refused spawn). This one is reached when no tail exists at all, which is a
/// statement about our own spawn path and not about the worker.
///
/// Still names the program and the method, which is the half that survives:
/// on the micro-VM path the process that exits is `kastellan-microvm-run` and
/// the tool is whatever ran inside the guest, so "which worker" is not
/// recoverable from the caller's tool name.
///
/// ⚠️ **Defensive, and currently unreachable.** Its caller's `None` arm fires
/// only when `SupervisedWorker` has no stderr tail, and all four sandbox
/// backends spawn with `stderr(Stdio::piped())` while `spawn_worker` is the
/// only constructor. So the test below pins this wording, not a reachable
/// path — worth stating plainly rather than letting a future reader mistake
/// coverage here for coverage of the arm.
pub fn format_unpiped_early_exit_report(program: &str, method: &str) -> String {
    format!(
        "worker {program} exited before responding to {method}; its stderr was not piped, \
         so there is nothing to report"
    )
}

/// Marker prefixing every line [`emit_early_exit_report`] writes to the
/// process's own stderr, so the fallback is greppable and distinguishable from
/// the `tracing` rendering of the same text.
///
/// Deliberately **not** one of this tree's three evidence markers (`[SKIP]`,
/// `[WARN]`, `[E2E]`). `scripts/run-e2e-gate.sh` asserts **zero** `[WARN]`
/// lines in a profile run, and every profile passes `--nocapture`, so a line
/// this function emits during a gate run reaches the gate log even from a
/// passing test. Any suite that provoked an early exit deliberately would then
/// turn every profile red. Nothing in a profile does so today — the separation
/// is cheap insurance, not a description of current suites.
///
/// ⚠️ The gate's greps are **anchored at line start**, so the test beside this
/// asserts the marker does not *begin with* one of the three — equality would
/// let `[WARN] …` through.
pub const EARLY_EXIT_STDERR_MARKER: &str = "[worker-early-exit]";

/// The exact bytes the stderr fallback writes. Pure, so the rendering can be
/// pinned without a process that has no subscriber.
///
/// ⚠️ **Neutralises here, not only in the caller.** The one-line property the
/// marker doc above argues for — that this can never produce a second, column-0
/// line a gate grep reads as `[SKIP]`/`[WARN]`/`[E2E]` — has to belong to the
/// function that *renders the line*, not to the call order inside
/// [`emit_early_exit_report`]. This is `pub`, and the obvious second producer
/// already exists: the persistent-worker death report
/// ([#730](https://github.com/hherb/kastellan/issues/730)) has the same defect
/// one layer over. A caller that reached for this formatter and neutralised
/// nothing would hand a model-authored `\n` straight to a gate log — the
/// drift-between-copies shape CLAUDE.md's bwrap-argv note names, where the
/// incomplete copy is the one that breaks.
///
/// `neutralise_controls` is idempotent and length-preserving, so
/// `emit_early_exit_report` neutralising first costs nothing and both orders
/// give identical bytes.
pub fn format_early_exit_stderr_fallback(report: &str) -> String {
    format!(
        "{EARLY_EXIT_STDERR_MARKER} {}",
        crate::untrusted_text::neutralise_controls(report)
    )
}

/// Report a worker's early exit through `tracing`, **and** through the
/// process's own stderr when no `tracing` subscriber is installed.
///
/// The one producer, so every caller gets both channels (#725).
///
/// **Who gets the fallback.** The daemon installs a subscriber as the first
/// statement of `main` (`core/src/main.rs`), before any dispatch, so it never
/// fires there and the daemon's behaviour is unchanged. It is *not* limited to
/// tests, though: `kastellan-cli` installs no subscriber anywhere, and
/// `kastellan-cli guard capture` dispatches the real web-fetch worker
/// (`core/src/bin/kastellan-cli/guard_capture.rs`). That command gains the
/// line, which is the intent — it is operator-facing and has no log for the
/// report to reach.
///
/// **Test binaries install none**, and that is the case this exists for: 29 of
/// the 30 `core/tests/*.rs` suites that dispatch to a real worker never call
/// `tracing_subscriber` (count: suites calling `dispatch`/`dispatch_with_sink`,
/// the only path here), so #666's report went nowhere in precisely the place a
/// human was reading. #719 is the worked example — a session spent on it with
/// the cause still unknown, then one `tracing_subscriber` line in the failing
/// test and the next run printed the Python traceback that named it.
///
/// ⚠️ **`eprintln!`, not `writeln!(std::io::stderr(), …)`.** libtest captures a
/// test's output through `std::io::set_output_capture`, which the
/// `print!`/`eprint!` **macros** consult and the `Stdout`/`Stderr` handles do
/// not. (The default panic hook consults it too, which is why a panic message
/// also lands in the captured block.) Writing to the handle directly goes to
/// the real file descriptor, bypasses the capture, and so never appears under
/// the failing test that needs it. Pinned end-to-end — by reading the child's
/// two streams *separately* — in
/// `core/tests/worker_early_exit_stderr_fallback_e2e.rs`.
///
/// Log-only in both channels, and deliberately so: the tail is raw worker
/// output, and the dispatch chokepoint scrubs redeemed secrets out of
/// everything that reaches the planner and the audit row (audit H1). These
/// bytes go to the process's own stderr and nowhere else — never into a
/// returned error, the planner, or an audit row.
///
/// ⚠️ **The whole report is neutralised here, not just the tail.** Tail lines
/// are already control-stripped as they enter the ring (`push_trimmed`), but
/// `program` and `method` are interpolated raw by the formatters above, and
/// `method` can be **model-authored**: `qualified_method` returns `None` for a
/// method outside the tool's advertised set and the caller then passes the
/// planner's string verbatim. Without this, a `\n` in it could forge a
/// column-0 line in a gate log and an ESC could drive the terminal reading it.
/// `neutralise_controls` is idempotent, so the already-clean tail is
/// unaffected, and it keeps the report to exactly one line.
///
/// This call covers the **`tracing`** channel.
/// [`format_early_exit_stderr_fallback`] neutralises again for its own line —
/// deliberately not relying on this one, because it is `pub` and a second
/// producer must not be able to bypass the property by calling it directly.
/// Both orders give identical bytes.
///
/// ⚠️ **One subscriber anywhere in a binary silences the fallback for all of
/// it.** `tracing::dispatcher::has_been_set` is a process-global `AtomicBool`
/// that `with_default` sets as well as `set_global_default`, and which is
/// **never cleared** — dropping a scoped guard does not re-enable the
/// fallback. So it cannot distinguish "this thread has a subscriber" from
/// "some other test installed one earlier". That is the conservative
/// direction — a binary with a subscriber loses nothing, it just reads the
/// report through `tracing` — but a suite that installs one in one test and
/// expects the fallback in another will not get it.
///
/// ⚠️ **It also asks the wrong question.** "Is a subscriber installed" is not
/// "will this WARN be recorded": an installed subscriber that *filters the
/// event out* drops it on both channels, silently. That is reachable in the
/// deployed daemon through a target-scoped `RUST_LOG` in the operator overlay.
/// [#734](https://github.com/hherb/kastellan/issues/734) tracks moving to a
/// delivery check (`event_enabled!`).
///
/// ⚠️ `has_been_set` is `#[doc(hidden)]` in `tracing` and carries no stability
/// guarantee. It is the only way to ask the question, and its removal would be
/// a compile error rather than a silent behaviour change; a change in its
/// meaning would be caught by the e2e above.
///
/// ⚠️ `eprintln!` panics if the write fails (a closed or broken stderr), and
/// the guard is `!has_been_set()` — *not* "am I the daemon". So the exposed set
/// is every binary without a subscriber: the test binaries, and
/// `kastellan-cli`. `kastellan-cli guard capture … 2>&1 | head` gives the
/// reader an `EPIPE` (Rust sets `SIGPIPE` to `SIG_IGN`), and under
/// `panic = "abort"` that is a silent `SIGABRT` rather than a message. It is
/// not a regression — `guard_capture` already prints with these macros
/// throughout, so the same pipeline already aborts on its very first
/// diagnostic — but this line is on an ERROR path, where losing the run costs
/// more. Tracked in
/// [#733](https://github.com/hherb/kastellan/issues/733).
pub fn emit_early_exit_report(report: &str) {
    let report = crate::untrusted_text::neutralise_controls(report);
    tracing::warn!("{report}");
    if !tracing::dispatcher::has_been_set() {
        eprintln!("{}", format_early_exit_stderr_fallback(&report));
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
    fn death_report_no_status_no_stderr() {
        let report = format_death_report(None, &[]);
        assert!(report.contains("exit status unknown"), "{report}");
        assert!(report.contains("no stderr captured"), "{report}");
    }

    #[test]
    fn death_report_includes_status_and_stderr_tail() {
        // A real non-zero ExitStatus so the rendering (and signal-vs-exit
        // distinction the daemon log relies on) is exercised, not mocked.
        let status = std::process::Command::new("false")
            .status()
            .expect("spawn /usr/bin/false");
        let report = format_death_report(Some(status), &["boom".into(), "trace".into()]);
        assert!(report.contains("exit status"), "{report}");
        assert!(report.contains("boom | trace"), "{report}");
    }

    #[test]
    fn early_exit_report_carries_the_workers_last_words() {
        let r = format_early_exit_report(
            "kastellan-microvm-run",
            "python.exec",
            &["microvm-init: relay UDS bind failed".to_string()],
        );
        assert!(r.contains("kastellan-microvm-run"), "{r}");
        assert!(r.contains("python.exec"), "{r}");
        assert!(r.contains("relay UDS bind failed"), "{r}");
    }

    #[test]
    fn early_exit_report_says_so_when_the_worker_was_silent() {
        // Silence is a different diagnosis, not a missing one — it must point
        // somewhere (a kill, a refused spawn) rather than read as an absence.
        let r = format_early_exit_report("w", "m", &[]);
        assert!(r.contains("NOTHING"), "{r}");
        assert!(r.contains("kill"), "silence must name where to look: {r}");
    }

    #[test]
    fn unpiped_early_exit_report_still_names_which_worker_and_which_method() {
        // The one thing this arm can still say. It is reached when the backend
        // handed us no stderr pipe, so there is no tail to quote — but on the
        // micro-VM path the process that exits is the launcher and the tool is
        // whatever ran in the guest, so "which worker" is not recoverable from
        // the caller's tool name and dropping it would leave nothing at all.
        let r = format_unpiped_early_exit_report("kastellan-microvm-run", "python.exec");
        assert!(r.contains("kastellan-microvm-run"), "{r}");
        assert!(r.contains("python.exec"), "{r}");
        assert!(r.contains("not piped"), "it must say WHY there is no tail: {r}");
    }

    #[test]
    fn the_stderr_fallback_line_is_marked_and_keeps_the_report_whole() {
        // The marker makes the fallback greppable and tells a reader which
        // channel they are looking at; the report must survive it intact,
        // because the report is the entire point of the line.
        let report = format_early_exit_report("w", "m", &["last words".to_string()]);
        let line = format_early_exit_stderr_fallback(&report);
        assert!(
            line.starts_with(EARLY_EXIT_STDERR_MARKER),
            "the marker must lead, so a grep anchored at line start finds it: {line}"
        );
        assert!(line.contains(&report), "the report must not be altered: {line}");
    }

    #[test]
    fn the_stderr_fallback_marker_is_distinctive_and_not_a_gate_evidence_marker() {
        // `scripts/run-e2e-gate.sh` asserts ZERO `[WARN]` lines in a profile
        // run, and every profile passes `--nocapture`, so a line this module
        // emits reaches the gate log even from a PASSING test. Borrowing one of
        // the three evidence markers would therefore turn a profile red for a
        // suite that was working.
        //
        // ⚠️ No profile selects an early-exit suite today — see the const's own
        // doc, which is the accurate statement. This is insurance against the
        // profile that adds one, not a description of current suites; an
        // earlier version of this comment claimed the opposite and contradicted
        // the const two hundred lines up.
        //
        // `starts_with`, not equality: the gate's greps are anchored at line
        // start (`grep -c '^\[WARN\]'`), so a marker of `"[WARN] early-exit"`
        // would pass an inequality check and still trip the assertion.
        for marker in ["[SKIP]", "[WARN]", "[E2E]"] {
            assert!(
                !EARLY_EXIT_STDERR_MARKER.starts_with(marker),
                "the fallback marker must not BEGIN with the gate evidence marker {marker}: \
                 the gate greps them anchored at line start"
            );
        }
        // Without this the check above is vacuous: `""` — and any marker that
        // is a strict prefix of all three, like `"["` — satisfies every
        // `starts_with` in this file, and `stdout.contains("")` in the e2e is
        // true of any output at all. Every other assertion on the marker reads
        // the const, so this is the only place its VALUE is pinned.
        assert!(
            EARLY_EXIT_STDERR_MARKER.starts_with("[worker-")
                && EARLY_EXIT_STDERR_MARKER.ends_with(']')
                && EARLY_EXIT_STDERR_MARKER.len() > "[worker-]".len(),
            "the marker must be a non-trivial bracketed `[worker-…]` token; an empty or \
             single-character marker passes every other check in this file vacuously, \
             including `contains` in the e2e. Got: {EARLY_EXIT_STDERR_MARKER:?}"
        );
    }

    #[test]
    fn the_stderr_fallback_neutralises_a_model_authored_control_character() {
        // `program` and `method` are interpolated RAW by the formatters above
        // (tail lines are already stripped by `push_trimmed`), and `method` is
        // model-authored: `qualified_method` returns `None` outside the tool's
        // advertised set and the planner's string then goes through verbatim.
        //
        // This pins the property on the PUB formatter rather than on
        // `emit_early_exit_report`'s call order, so a second producer — #730's
        // persistent-worker death report is the one already filed — cannot
        // render an un-neutralised line by reaching for this function.
        let report = format_early_exit_report(
            "w",
            "m\u{1b}[31m\n[WARN] forged-gate-line",
            &["last words".to_string()],
        );
        let line = format_early_exit_stderr_fallback(&report);
        assert!(
            !line.contains('\u{1b}'),
            "an ESC would be an ANSI sequence executing in the reader's terminal: {line:?}"
        );
        assert_eq!(
            line.lines().count(),
            1,
            "the fallback must be exactly ONE line — a `\\n` here forges a column-0 line that \
             `run-e2e-gate.sh`'s `^\\[WARN\\]` grep counts, failing an unrelated profile: \
             {line:?}"
        );
        // Neutralisation maps the class to a space, so the text must SURVIVE —
        // mid-line. A check that merely asserted its absence would also pass if
        // `method` stopped reaching the report at all.
        assert!(
            line.contains("forged-gate-line"),
            "the text must survive as text; only its line-forging effect is removed: {line:?}"
        );
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
        // daemon log at WARN via `format_early_exit_report` — i.e. visible in
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
}
