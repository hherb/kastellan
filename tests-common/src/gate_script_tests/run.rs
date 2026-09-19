//! `scripts/run-e2e-gate.sh`, actually RUN, against a fake `cargo`.
//!
//! The parent module reads the script's profile table; until this file, no test
//! ran the script. That let a defect ship that no table scan could see. The
//! verdict block read `${PIPESTATUS[1]}` on the line after an assignment, and an
//! assignment is itself a command, so it had already reset `PIPESTATUS` to a
//! single element. Under `set -u` the script then died with "unbound variable"
//! straight after every test run: exit 1, on every profile, whatever the tests
//! did. A gate that could not pass. It was found running the `gliner` profile
//! for issue #719.
//!
//! How these tests work: a fake `cargo` goes first on `PATH`. It ignores its
//! arguments, prints a canned test log, and exits with a chosen status. The
//! real script then runs as normal. The fake replaces only the part the gate
//! does not own, the test run itself; the log file, the counting, the floors
//! and the exit status are all the real script's.
//!
//! Each test checks the verdict's MESSAGE as well as its exit status. Exit 1
//! alone cannot tell "the gate refused this run" from "the gate crashed", and
//! the crash is exactly what these tests exist to catch.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{profiles, script_path};

/// The profile these tests drive. It must run on any host (`os` = `any`), so
/// the script does not refuse it before reaching the verdict; checked below.
const PROFILE: &str = "gliner";

/// A throwaway directory used as `$HOME` (so the gate's log lands here, not in
/// the real home) and holding the fake `cargo`. Removed on drop.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let root = std::env::temp_dir().join(format!(
            "kastellan-gate-run-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("bin"))
            .unwrap_or_else(|e| panic!("create {}: {e}", root.display()));
        Self { root }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// One `[E2E] <tier>: …` line for every unit of every floor in [`PROFILE`] —
/// the evidence a healthy run of that profile produces.
///
/// Derived from the profile table rather than written out, so renaming a tier
/// or adding a floor does not turn these tests into tests of a stale copy.
fn e2e_evidence_for_profile() -> String {
    let profile = profiles()
        .into_iter()
        .find(|p| p.name == PROFILE)
        .unwrap_or_else(|| panic!("no `{PROFILE}` profile in the gate script"));
    assert_eq!(
        profile.os, "any",
        "the `{PROFILE}` profile must run on any host, or the script refuses it before \
         the verdict these tests check"
    );
    let mut lines = String::new();
    for pair in profile.e2e_floors.split(',') {
        let (tier, floor) = pair
            .split_once('=')
            .unwrap_or_else(|| panic!("E2E floor {pair:?} is not tier=N"));
        let floor: usize = floor
            .parse()
            .unwrap_or_else(|e| panic!("E2E floor {pair:?}: {e}"));
        for _ in 0..floor {
            lines.push_str(&format!("[E2E] {tier}: precondition met (fake cargo)\n"));
        }
    }
    assert!(!lines.is_empty(), "the `{PROFILE}` profile names no E2E floor");
    lines
}

/// Run the real gate script for [`PROFILE`], with a fake `cargo` that prints
/// `log` and exits with `exit_code`.
fn run_gate(tag: &str, log: &str, exit_code: i32) -> Output {
    let scratch = Scratch::new(tag);
    let fake_cargo = scratch.root.join("bin/cargo");
    // A quoted heredoc prints the log byte for byte (no expansion of `$`).
    let body = format!(
        "#!/bin/sh\ncat <<'KASTELLAN_FAKE_CARGO_EOF'\n{log}\nKASTELLAN_FAKE_CARGO_EOF\nexit {exit_code}\n"
    );
    std::fs::write(&fake_cargo, body).expect("write the fake cargo");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_cargo, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake cargo executable");
    }
    let path = format!(
        "{}:{}",
        scratch.root.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("bash")
        .arg(script_path())
        .arg(PROFILE)
        .env("HOME", &scratch.root)
        .env("PATH", path)
        .output()
        .expect("run the gate script under bash")
}

/// Render an [`Output`] for an assertion message.
fn describe(out: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const PASSED_FIVE: &str =
    "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s";

#[test]
fn a_run_that_meets_every_floor_passes_the_gate() {
    let log = format!("{}{PASSED_FIVE}", e2e_evidence_for_profile());
    let out = run_gate("healthy", &log, 0);
    assert!(
        out.status.success(),
        "a run meeting every floor must pass: a gate that cannot pass gets switched off\n{}",
        describe(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("✅ gate passed as evidence"),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_run_that_selected_zero_tests_fails_the_gate() {
    // Issue #664's shape: a name filter that matches nothing exits 0.
    let log = "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 5 filtered out; finished in 0.00s";
    let out = run_gate("zero", log, 0);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("POSITIVE CONTROL FAILED: 0 tests passed"),
        "the gate must refuse for the stated reason, not by crashing\n{}",
        describe(&out)
    );
}

#[test]
fn a_failed_test_run_fails_the_gate_even_with_the_evidence_present() {
    // Every floor met, but cargo itself exited 101. This pins that the gate
    // reads cargo's status (the pipeline's FIRST exit code), not tee's.
    let log = format!("{}{PASSED_FIVE}", e2e_evidence_for_profile());
    let out = run_gate("cargo-failed", &log, 101);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("the test run itself failed (exit 101)"),
        "{}",
        describe(&out)
    );
}
