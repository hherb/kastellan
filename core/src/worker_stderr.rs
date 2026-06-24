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
use std::sync::{Arc, Mutex};

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
}

impl StderrTail {
    /// A tail retaining at most `cap` lines (oldest evicted first). `cap == 0`
    /// retains nothing.
    pub fn new(cap: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::new())),
            cap,
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
                tracing::debug!(worker_pid = pid, "worker stderr: {}", chunk.trim_end());
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
fn push_trimmed(tail: &StderrTail, line: &str) {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if !trimmed.is_empty() {
        tail.push(trimmed.to_string());
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
    std::thread::spawn(move || drain_reader(pid, stderr, Some(&thread_tail)));
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
}
