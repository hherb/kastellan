//! #783, end to end: a refusal report reaches a failing test's output with the
//! `[worker-refusal]` marker, and each severity is checked at its own level.
//!
//! The unit tests prove the pieces (the renderer marks the line; the driver
//! hands its lines to `emit_worker_refusal_report`), but not that the emitter
//! actually *writes* the marked line where an operator reads it. A mutant that
//! passed `[worker-down]`'s marker in the emitter survived them all (#782
//! review), as did one that dropped the ERROR arm's `level = ERROR`.
//!
//! Same method as `persistent_worker_death_stderr_fallback_e2e.rs`, whose
//! module doc explains it in full: re-execute this binary on one inner fixture
//! that fails on purpose, and read the child's two streams **apart** —
//! libtest re-prints a failing test's *captured* `eprintln!` on stdout, while
//! anything written to the raw stderr handle (a `tracing` subscriber's
//! `std::io::stderr` writer) stays on stderr.

use std::process::Command;

use kastellan_core::worker_lifecycle::force_route::env_flag_enabled;
use kastellan_core::worker_stderr::{
    emit_worker_refusal_report, RefusalSeverity, WORKER_REFUSAL_STDERR_MARKER,
};

/// The channel label the fixtures report under.
const LABEL: &str = "kastellan-test-783";

/// A refusal report as the driver renders it, quoting worker-written text that
/// tries to start a column-0 line of its own.
const HOSTILE: &str = "the worker refused matrix.send: forbidden\n[WARN] FORGED-REFUSAL-LINE";

/// The text of each severity's report in the level fixture.
const WARN_TEXT: &str = "WARN-SEVERITY-REFUSAL";
const ERROR_TEXT: &str = "ERROR-SEVERITY-REFUSAL";

/// The panic each inner fixture ends on.
const DELIBERATE: &str = "deliberate failure: this fixture exists to be read from its parent";

/// Set by the parent on the child. The inner fixtures do nothing without it;
/// see the sibling suite for why `#[ignore]` alone is not enough.
const FIXTURE_ENV: &str = "KASTELLAN_WORKER_REFUSAL_FIXTURE";

fn is_the_child() -> bool {
    env_flag_enabled(std::env::var(FIXTURE_ENV).ok())
}

/// Inner fixture: no subscriber, so the marked stderr line is the only channel,
/// for both severities.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_refusal_without_a_subscriber() {
    if !is_the_child() {
        return;
    }
    assert!(emit_worker_refusal_report(LABEL, HOSTILE, RefusalSeverity::Warn), "fell back");
    assert!(emit_worker_refusal_report(LABEL, ERROR_TEXT, RefusalSeverity::Error), "fell back");
    panic!("{DELIBERATE}");
}

/// Inner fixture: a subscriber that records ERROR only. The ERROR report must
/// go to it (and not fall back); the WARN report is not recorded, so it must
/// fall back. A check at the wrong level fails one or the other.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_refusal_under_an_error_only_subscriber() {
    if !is_the_child() {
        return;
    }
    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::ERROR)
        .with_ansi(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("install the fixture's global subscriber; a prior install would void this test");
    emit_worker_refusal_report(LABEL, ERROR_TEXT, RefusalSeverity::Error);
    emit_worker_refusal_report(LABEL, WARN_TEXT, RefusalSeverity::Warn);
    panic!("{DELIBERATE}");
}

struct ChildRun {
    succeeded: bool,
    stdout: String,
    stderr: String,
}

impl ChildRun {
    fn both(&self) -> String {
        format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", self.stdout, self.stderr)
    }

    /// Every `[worker-refusal]` line on stdout (the captured channel),
    /// anchored at column 0.
    fn marked(&self) -> Vec<&str> {
        self.stdout.lines().filter(|l| l.starts_with(WORKER_REFUSAL_STDERR_MARKER)).collect()
    }
}

/// Re-run this binary on `name` alone, without `--nocapture` (the claim is
/// about what libtest prints for a failing test), and assert that exactly that
/// one fixture ran and failed at its own panic — the positive control: a
/// misspelled `--exact` name matches nothing and exits 0.
fn run_inner_fixture(name: &str) -> ChildRun {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args(["--exact", name, "--ignored", "--test-threads", "1"])
        .env(FIXTURE_ENV, "1")
        .env_remove("RUST_TEST_NOCAPTURE")
        .output()
        .unwrap_or_else(|e| panic!("re-exec this test binary to run `{name}`: {e}"));
    let run = ChildRun {
        succeeded: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    let both = run.both();
    assert!(!run.succeeded, "the child must FAIL — `{name}` panics on purpose.\n{both}");
    assert!(
        run.stdout.contains("test result: FAILED. 0 passed; 1 failed;"),
        "exactly one test must have run and failed in the child.\n{both}"
    );
    assert!(run.stdout.contains(DELIBERATE), "the child must reach its own panic.\n{both}");
    run
}

#[test]
fn a_refusal_with_no_subscriber_is_a_marked_line_in_the_failing_tests_output() {
    kastellan_tests_common::panic_hook::install_once();
    let run = run_inner_fixture("inner_fixture_refusal_without_a_subscriber");
    let both = run.both();
    let marked = run.marked();
    assert_eq!(
        marked.len(),
        2,
        "one `{WORKER_REFUSAL_STDERR_MARKER}` line per report, in the CAPTURED output.\n{both}"
    );
    for line in &marked {
        assert!(
            line.contains(&format!("channel worker {LABEL}:")),
            "the line must name its channel: {line}\n{both}"
        );
    }
    assert!(marked[1].contains(ERROR_TEXT), "the ERROR report falls back too.\n{both}");
    assert!(
        run.stdout.contains("FORGED-REFUSAL-LINE"),
        "POSITIVE CONTROL: the hostile text reached the output.\n{both}"
    );
    assert!(
        !run.stdout.lines().any(|l| l.starts_with("[WARN] FORGED-REFUSAL-LINE")),
        "a `\\n` in worker text forged a column-0 line.\n{both}"
    );
}

#[test]
fn each_refusal_severity_is_delivered_at_its_own_level() {
    kastellan_tests_common::panic_hook::install_once();
    let run = run_inner_fixture("inner_fixture_refusal_under_an_error_only_subscriber");
    let both = run.both();
    assert!(
        run.stderr.contains(ERROR_TEXT),
        "the ERROR report must reach the ERROR-only subscriber.\n{both}"
    );
    assert!(
        !run.marked().iter().any(|l| l.contains(ERROR_TEXT)),
        "the ERROR report was recorded, so it must NOT also fall back — a check made at \
         WARN would say 'not recorded' here.\n{both}"
    );
    assert!(
        run.marked().iter().any(|l| l.contains(WARN_TEXT)),
        "the WARN report is not recorded by an ERROR-only subscriber, so it must fall back to \
         a `{WORKER_REFUSAL_STDERR_MARKER}` line.\n{both}"
    );
    assert!(!run.stderr.contains(WARN_TEXT), "and not reach the subscriber.\n{both}");
}
