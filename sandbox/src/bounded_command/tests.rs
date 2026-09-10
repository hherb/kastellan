//! Unit tests for [`super`] — the bounded subprocess runner (#690).
//!
//! Every test here runs on **both** hosts: the only external programs used are
//! `bash`, `sleep` and `head`, which the workspace already requires everywhere
//! (`images.rs` shells out to `bash -n`, and the rootfs builds need coreutils).
//! ⚠️ Nothing here parses a tool's *output format* — only byte counts, exit
//! codes and our own strings — because GNU and BSD coreutils disagree about
//! formatting and a test that reads their prose breaks on one host only.

use super::*;

/// A budget far below any real one, so the timeout tests finish quickly. Long
/// enough that a loaded CI host cannot fail to *start* `bash` inside it.
const SHORT: Duration = Duration::from_millis(500);

/// A budget far above what the fast tests need, so they can never flake into
/// the timeout arm on a contended host.
const GENEROUS: Duration = Duration::from_secs(60);

fn bash(script: &str) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(script);
    cmd
}

/// The ordinary case must be indistinguishable from `Command::output()`.
#[test]
fn a_fast_command_returns_its_output() {
    let out = match output_within(&mut bash("printf hi"), GENEROUS).expect("bash spawns") {
        Bounded::Exited(out) => out,
        Bounded::TimedOut(t) => panic!("`printf hi` timed out at {:?}", t.budget),
    };
    assert!(out.status.success(), "printf should succeed: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
}

/// **A non-zero exit is an answer, not a timeout.** Folding the two together
/// would be #684's defect in a new place: a CLI that said "no" reported as a
/// CLI that said nothing.
#[test]
fn a_failing_command_is_exited_not_timed_out() {
    let out = match output_within(&mut bash("echo nope >&2; exit 3"), GENEROUS).expect("spawns") {
        Bounded::Exited(out) => out,
        Bounded::TimedOut(t) => panic!("`exit 3` timed out at {:?}", t.budget),
    };
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nope"),
        "stderr should survive a non-zero exit"
    );
}

/// The point of the module: a child that would never answer is killed, and the
/// caller gets control back long before the child would have finished.
///
/// The `sleep 30` is the stand-in for a wedged `container-apiserver`. The
/// elapsed assertion is what proves the budget was honoured — without it, an
/// implementation that simply waited 30 s and then *reported* a timeout would
/// pass.
#[test]
fn a_command_that_will_not_answer_is_killed_at_the_budget() {
    let started = Instant::now();
    let timed_out = match output_within(&mut bash("sleep 30"), SHORT).expect("bash spawns") {
        Bounded::TimedOut(t) => t,
        Bounded::Exited(out) => panic!("`sleep 30` exited inside 500ms: {:?}", out.status),
    };
    let elapsed = started.elapsed();
    assert_eq!(timed_out.budget, SHORT, "the verdict must carry the budget that elapsed");
    assert!(
        elapsed < Duration::from_secs(10),
        "returned after {elapsed:?}: the budget was not honoured (the child sleeps 30s)"
    );
}

/// A timeout must carry the child's last words — an error with no content is a
/// defect multiplier, which is the reason this module exists at all.
#[test]
fn a_timeout_keeps_what_the_child_had_already_said() {
    let script = "echo apiserver-not-responding >&2; sleep 30";
    let timed_out = match output_within(&mut bash(script), SHORT).expect("bash spawns") {
        Bounded::TimedOut(t) => t,
        Bounded::Exited(out) => panic!("expected a timeout, got {:?}", out.status),
    };
    assert!(
        timed_out.partial_stderr.contains("apiserver-not-responding"),
        "partial stderr lost the child's own words: {:?}",
        timed_out.partial_stderr
    );
}

/// ⚠️ **The regression test for the naive implementation.**
///
/// A child writing more than one pipe buffer *blocks on the write* until
/// somebody reads. A runner that polls for exit without draining the pipes
/// would report a timeout for a process that is only waiting for **us** — a
/// false positive manufactured by the very thing meant to remove false
/// verdicts. 512 KiB is comfortably past both platforms' buffers (64 KiB on
/// Linux, less on macOS).
#[test]
fn a_child_that_outfills_the_pipe_buffer_does_not_deadlock() {
    const BYTES: usize = 512 * 1024;
    let script = format!("head -c {BYTES} /dev/zero");
    let out = match output_within(&mut bash(&script), GENEROUS).expect("bash spawns") {
        Bounded::Exited(out) => out,
        Bounded::TimedOut(t) => {
            panic!("a {BYTES}-byte writer timed out at {:?} — the pipes are not drained", t.budget)
        }
    };
    assert_eq!(out.stdout.len(), BYTES, "the whole stream must survive");
}

/// Both pipes, not just the one the runner happens to look at on timeout.
#[test]
fn a_child_that_outfills_the_stderr_buffer_does_not_deadlock() {
    const BYTES: usize = 512 * 1024;
    let script = format!("head -c {BYTES} /dev/zero >&2");
    let out = match output_within(&mut bash(&script), GENEROUS).expect("bash spawns") {
        Bounded::Exited(out) => out,
        Bounded::TimedOut(t) => {
            panic!("a {BYTES}-byte stderr writer timed out at {:?}", t.budget)
        }
    };
    assert_eq!(out.stderr.len(), BYTES);
}

/// ⚠️ **The regression test for the hang this module SHIPPED WITH, found by
/// another test on this same branch.**
///
/// A grandchild inherits the child's stdout and stderr. The first version
/// joined the drain threads, so a grandchild outliving its parent kept the
/// write end open, the read never saw EOF, and `output_within` blocked
/// **after** it had already killed the child and decided the verdict —
/// measured at 17 minutes on the real host, where bash was waiting on a
/// `dirname` wedged in `_dyld_start`.
///
/// `sleep 30 &` then `exit 0` is that shape in one line: bash is gone in
/// milliseconds and the `sleep` holds the pipe for half a minute. The budget
/// here is deliberately *generous*, because the hang was never about the
/// budget — the child exited well inside it.
#[test]
fn a_grandchild_that_outlives_the_child_does_not_hold_the_runner() {
    let started = Instant::now();
    let out = match output_within(&mut bash("sleep 30 & exit 0"), GENEROUS).expect("spawns") {
        Bounded::Exited(out) => out,
        Bounded::TimedOut(t) => panic!("bash exits at once; it cannot time out at {:?}", t.budget),
    };
    let elapsed = started.elapsed();
    assert!(out.status.success());
    assert!(
        elapsed < Duration::from_secs(10),
        "returned after {elapsed:?}: the runner waited on a grandchild's pipe, which is the \
         unbounded wait this module exists to remove"
    );
}

/// The same shape on the timeout path: kill the child, and still do not wait
/// on a grandchild that the kill did not reach.
#[test]
fn a_grandchild_does_not_hold_the_runner_on_the_timeout_path_either() {
    let started = Instant::now();
    match output_within(&mut bash("sleep 30 & sleep 30"), SHORT).expect("spawns") {
        Bounded::TimedOut(t) => assert_eq!(t.budget, SHORT),
        Bounded::Exited(out) => panic!("expected a timeout, got {:?}", out.status),
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "returned after {elapsed:?}: the timeout path still waited on the grandchild"
    );
}

/// A binary that is not there is a **spawn** failure, and must stay
/// distinguishable from both other outcomes.
#[test]
fn a_missing_binary_is_an_error_not_a_timeout() {
    let mut cmd = Command::new("kastellan-no-such-binary-690");
    let err = output_within(&mut cmd, GENEROUS).expect_err("a missing binary cannot spawn");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// Stdin is closed, exactly as [`Command::output`] closes it — otherwise a
/// probe that reads stdin would inherit the test harness's and hang for a
/// reason that has nothing to do with the budget.
#[test]
fn stdin_is_closed_so_a_reader_sees_eof() {
    let out = match output_within(&mut bash("cat; echo done"), SHORT).expect("bash spawns") {
        Bounded::Exited(out) => out,
        Bounded::TimedOut(t) => panic!("`cat` did not see EOF within {:?}", t.budget),
    };
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "done");
}

// ---------------------------------------------------------------------------
// `timed_out_reason` — pure, so every arm is reachable on any host.
// ---------------------------------------------------------------------------

fn verdict(partial: &str) -> TimedOut {
    TimedOut { budget: Duration::from_secs(10), partial_stderr: partial.to_string() }
}

/// The three things an operator needs: what ran, how long it was given, and
/// what to do about it.
#[test]
fn the_reason_names_the_command_the_budget_and_the_remedy() {
    let reason = timed_out_reason(
        "container system status",
        &verdict(""),
        "the apiserver may be wedged — try `container system stop && container system start`",
    );
    assert!(reason.contains("container system status"), "{reason}");
    assert!(reason.contains("10s"), "the budget must be legible: {reason}");
    assert!(reason.contains("container system start"), "the remedy must survive: {reason}");
}

/// The child's own words are worth more than anything we can infer, so they
/// are carried through verbatim.
#[test]
fn the_reason_carries_the_childs_last_words() {
    let reason = timed_out_reason("container system status", &verdict("XPC connection invalid"), "");
    assert!(reason.contains("XPC connection invalid"), "{reason}");
}

/// ⚠️ An empty stderr is the *common* case for a wedge — a process that never
/// answered usually never spoke either — so the sentence has to read correctly
/// without one. A dangling `: ` reads as truncated output and sends the reader
/// hunting for a message that was never there.
#[test]
fn the_reason_reads_correctly_when_the_child_said_nothing() {
    let reason = timed_out_reason("container --version", &verdict(""), "install it with `brew`");
    assert!(!reason.contains("it had said"), "no empty last-words clause: {reason}");
    assert!(!reason.ends_with(':'), "no dangling separator: {reason}");
    assert!(reason.ends_with("install it with `brew`"), "{reason}");
}

/// A caller with no useful remedy still gets a sentence, not a fragment.
#[test]
fn the_reason_reads_correctly_with_no_hint() {
    let reason = timed_out_reason("debugfs -R cat", &verdict(""), "");
    assert_eq!(reason, "`debugfs -R cat` did not answer within 10s and was killed");
}

/// Sub-second budgets are used by these very tests, so their rendering has to
/// be legible too — `500ms`, not `0.5s` or `PT0.5S`.
#[test]
fn a_sub_second_budget_renders_legibly() {
    let short = TimedOut { budget: SHORT, partial_stderr: String::new() };
    let reason = timed_out_reason("x", &short, "");
    assert!(reason.contains("500ms"), "{reason}");
}

// ---------------------------------------------------------------------------
// `probe_output` — the two no-answer causes must stay apart.
// ---------------------------------------------------------------------------

/// A probe that answered "no" is not a probe that failed to answer. Folding
/// the two is #684's defect, and it is the reason this wrapper exists at all.
#[test]
fn a_probe_that_answers_no_is_ok_not_a_failure() {
    let out = probe_output(&mut bash("exit 7"), GENEROUS).expect("a non-zero exit is an answer");
    assert_eq!(out.status.code(), Some(7));
}

/// An absent binary is a `Spawn` failure: the remedy is an install.
#[test]
fn a_missing_probe_binary_is_a_spawn_failure() {
    let mut cmd = Command::new("kastellan-no-such-binary-690");
    match probe_output(&mut cmd, GENEROUS) {
        Err(ProbeFailure::Spawn(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        other => panic!("expected a spawn failure, got {other:?}"),
    }
}

/// A helper that never answers is `Wedged`: the remedy is to unwedge it, and
/// no install will help.
#[test]
fn a_probe_that_never_answers_is_wedged() {
    match probe_output(&mut bash("sleep 30"), SHORT) {
        Err(ProbeFailure::Wedged(t)) => assert_eq!(t.budget, SHORT),
        other => panic!("expected a wedge, got {other:?}"),
    }
}
