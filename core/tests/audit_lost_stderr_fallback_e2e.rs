//! #799, end to end: an audit-lost report reaches a failing test's output with
//! the `[audit-lost]` marker, cannot be made to forge a second line by the id
//! it quotes, and is delivered at its own level (ERROR).
//!
//! The unit tests prove the pieces — the formatter neutralises control
//! characters, the sinks hand their lines to `emit_audit_lost_report` — but not
//! that the emitter actually *writes* the marked line where an operator reads
//! it, nor what it returns. Same method as
//! `worker_refusal_stderr_fallback_e2e.rs` (and, in full,
//! `persistent_worker_death_stderr_fallback_e2e.rs`): re-execute this binary
//! on one inner fixture that fails on purpose, and read the child's two
//! streams **apart** — libtest re-prints a failing test's *captured*
//! `eprintln!` on stdout, while a `tracing` subscriber's `std::io::stderr`
//! writer stays on stderr.

use std::process::Command;

use kastellan_core::worker_lifecycle::force_route::env_flag_enabled;
use kastellan_core::worker_stderr::{
    emit_audit_lost_report, AuditLostWriter, AUDIT_LOST_STDERR_MARKER,
};

/// A sink's report quoting a worker-supplied message id that tries to start a
/// column-0 line of its own.
const HOSTILE: &str =
    "channel.skipped_ack_only row for message \"<a@h>\n[SKIP] FORGED-AUDIT-LOST-LINE\" not written";

/// The shutdown's report in the fixtures.
const SHUTDOWN_TEXT: &str = "SHUTDOWN-AUDIT-LOST-REPORT";

/// The panic each inner fixture ends on.
const DELIBERATE: &str = "deliberate failure: this fixture exists to be read from its parent";

/// Set by the parent on the child. The inner fixtures do nothing without it;
/// see `persistent_worker_death_stderr_fallback_e2e.rs` for why `#[ignore]`
/// alone is not enough.
const FIXTURE_ENV: &str = "KASTELLAN_AUDIT_LOST_FIXTURE";

fn is_the_child() -> bool {
    env_flag_enabled(std::env::var(FIXTURE_ENV).ok())
}

/// Inner fixture: no subscriber, so the marked stderr line is the only channel.
/// The emitter says so by returning `true`.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_audit_lost_without_a_subscriber() {
    if !is_the_child() {
        return;
    }
    assert!(emit_audit_lost_report(AuditLostWriter::Email, HOSTILE), "fell back");
    assert!(emit_audit_lost_report(AuditLostWriter::Shutdown, SHUTDOWN_TEXT), "fell back");
    panic!("{DELIBERATE}");
}

/// Inner fixture: a subscriber that records ERROR only. The report is ERROR,
/// so it must go to the subscriber and NOT fall back — and the emitter says
/// so by returning `false`.
#[test]
#[ignore = "inner fixture: fails on purpose; run by its parent test in a child process"]
fn inner_fixture_audit_lost_under_an_error_only_subscriber() {
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
    assert!(!emit_audit_lost_report(AuditLostWriter::Shutdown, SHUTDOWN_TEXT), "recorded");
    panic!("{DELIBERATE}");
}

struct ChildRun {
    stdout: String,
    stderr: String,
}

impl ChildRun {
    fn both(&self) -> String {
        format!("--- child stdout ---\n{}\n--- child stderr ---\n{}", self.stdout, self.stderr)
    }

    /// Every `[audit-lost]` line on stdout (the captured channel), anchored
    /// at column 0.
    fn marked(&self) -> Vec<&str> {
        self.stdout.lines().filter(|l| l.starts_with(AUDIT_LOST_STDERR_MARKER)).collect()
    }
}

/// Re-run this binary on `name` alone, without `--nocapture`, and assert that
/// exactly that one fixture ran and failed at its own panic — the positive
/// control: a misspelled `--exact` name matches nothing and exits 0.
fn run_inner_fixture(name: &str) -> ChildRun {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = Command::new(exe)
        .args(["--exact", name, "--ignored", "--test-threads", "1"])
        .env(FIXTURE_ENV, "1")
        .env_remove("RUST_TEST_NOCAPTURE")
        .output()
        .unwrap_or_else(|e| panic!("re-exec this test binary to run `{name}`: {e}"));
    let run = ChildRun {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    let both = run.both();
    assert!(!out.status.success(), "the child must FAIL — `{name}` panics on purpose.\n{both}");
    assert!(
        run.stdout.contains("test result: FAILED. 0 passed; 1 failed;"),
        "exactly one test must have run and failed in the child.\n{both}"
    );
    assert!(run.stdout.contains(DELIBERATE), "the child must reach its own panic.\n{both}");
    run
}

#[test]
fn an_audit_lost_report_with_no_subscriber_is_one_marked_line_per_report() {
    kastellan_tests_common::panic_hook::install_once();
    let run = run_inner_fixture("inner_fixture_audit_lost_without_a_subscriber");
    let both = run.both();
    let marked = run.marked();
    assert_eq!(
        marked.len(),
        2,
        "one `{AUDIT_LOST_STDERR_MARKER}` line per report, in the CAPTURED output.\n{both}"
    );
    assert!(marked[0].contains("audit rows from email:"), "it names its writer.\n{both}");
    assert!(marked[1].contains("audit rows from shutdown:") && marked[1].contains(SHUTDOWN_TEXT));
    assert!(
        run.stdout.contains("FORGED-AUDIT-LOST-LINE"),
        "POSITIVE CONTROL: the hostile id reached the output.\n{both}"
    );
    assert!(
        !run.stdout.lines().any(|l| l.starts_with("[SKIP] FORGED-AUDIT-LOST-LINE")),
        "a `\\n` in a quoted id forged a column-0 line.\n{both}"
    );
}

#[test]
fn an_audit_lost_report_is_delivered_at_error() {
    kastellan_tests_common::panic_hook::install_once();
    let run = run_inner_fixture("inner_fixture_audit_lost_under_an_error_only_subscriber");
    let both = run.both();
    assert!(
        run.stderr.contains(SHUTDOWN_TEXT),
        "the report must reach the ERROR-only subscriber.\n{both}"
    );
    assert!(
        run.marked().is_empty(),
        "the report was recorded, so it must NOT also fall back — a check made below ERROR \
         would say 'not recorded' here.\n{both}"
    );
}
