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
fn e2e_evidence_for_profile() -> String {
    e2e_evidence_for(PROFILE)
}

/// [`e2e_evidence_for_profile`] for any `os = any` profile.
///
/// Derived from the profile table rather than written out, so renaming a tier
/// or adding a floor does not turn these tests into tests of a stale copy.
fn e2e_evidence_for(name: &str) -> String {
    let profile = profiles()
        .into_iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("no `{name}` profile in the gate script"));
    assert_eq!(
        profile.os, "any",
        "the `{name}` profile must run on any host, or the script refuses it before \
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
    assert!(!lines.is_empty(), "the `{name}` profile names no E2E floor");
    lines
}

/// Cargo's header for one test binary, followed by the hook's announcement —
/// the section a binary that reached the panic hook produces (#748).
fn hooked_binary(name: &str) -> String {
    format!("{}{}\n", unhooked_binary(name), crate::panic_hook::install_line())
}

/// Cargo's header for one test binary, with NO announcement after it.
fn unhooked_binary(name: &str) -> String {
    format!("     Running tests/{name}.rs (target/debug/deps/{name}-0123456789abcdef)\n")
}

/// A healthy run of `profile`: one hooked binary, every floor met, 5 passed.
fn healthy_log_for(profile: &str) -> String {
    format!("{}{}{PASSED_FIVE}", hooked_binary("fake_suite_e2e"), e2e_evidence_for(profile))
}

/// Run the real gate script for [`PROFILE`], with a fake `cargo` that prints
/// `log` and exits with `exit_code`.
fn run_gate(tag: &str, log: &str, exit_code: i32) -> Output {
    run_gate_profile(PROFILE, tag, log, exit_code)
}

/// [`run_gate`] for a named profile.
fn run_gate_profile(profile: &str, tag: &str, log: &str, exit_code: i32) -> Output {
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
    // The throwaway `HOME` does two jobs. The gate's log lands in it, and the
    // script's `source "$HOME/.cargo/env"` finds nothing there. The real env
    // file would put `~/.cargo/bin` ahead of the fake on `PATH`, and this
    // "fake" run would quietly become a real `cargo test`.
    Command::new("bash")
        .arg(script_path())
        .arg(profile)
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
    let log = healthy_log_for(PROFILE);
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
    let log = healthy_log_for(PROFILE);
    let out = run_gate("cargo-failed", &log, 101);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("the test run itself failed (exit 101)"),
        "{}",
        describe(&out)
    );
}

/// The refusal the script prints when a tier's `[E2E]` floor is not met.
fn tier_floor_refusal() -> String {
    format!("'[E2E] {PROFILE_TIER}' lines, floor is")
}

/// The tier [`PROFILE`] demands evidence from. Pinned against the table so a
/// rename fails here loudly rather than making the floor tests vacuous.
const PROFILE_TIER: &str = "gliner-relex";

#[test]
fn the_profile_tier_these_tests_name_is_the_one_the_table_demands() {
    assert!(
        e2e_evidence_for_profile().contains(&format!("[E2E] {PROFILE_TIER}: ")),
        "the `{PROFILE}` profile no longer demands `{PROFILE_TIER}` evidence; update PROFILE_TIER"
    );
}

#[test]
fn a_run_with_no_e2e_evidence_fails_the_gate() {
    // Tests passed and cargo exited 0, but the demanded tier never announced a
    // met precondition: the knob reached nothing.
    let log = format!("{}{PASSED_FIVE}", hooked_binary("fake_suite_e2e"));
    let out = run_gate("no-e2e", &log, 0);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&tier_floor_refusal()),
        "{}",
        describe(&out)
    );
}

#[test]
fn evidence_from_another_tier_does_not_satisfy_the_floor() {
    // The `gliner` profile also sets the Postgres knob. A floor that counted
    // every `[E2E]` line cleared on one Postgres line, the defect the per-tier
    // floors exist to stop.
    let log = format!(
        "{}[E2E] Postgres-backed: precondition met (fake cargo)\n{PASSED_FIVE}",
        hooked_binary("fake_suite_e2e")
    );
    let out = run_gate("wrong-tier", &log, 0);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&tier_floor_refusal()),
        "a Postgres line must not count as gliner-relex evidence\n{}",
        describe(&out)
    );
}

#[test]
fn a_warn_line_fails_an_otherwise_healthy_run() {
    // `[WARN]` means a knob was set but not honoured (an out-of-dialect value),
    // so the run was not the demanded one, whatever else it shows.
    let log = format!(
        "{}{}[WARN] KASTELLAN_PG_REQUIRE_E2E=y is not a recognised value; treating as unset\n{PASSED_FIVE}",
        hooked_binary("fake_suite_e2e"),
        e2e_evidence_for_profile()
    );
    let out = run_gate("warn", &log, 0);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("[WARN] line(s)"),
        "{}",
        describe(&out)
    );
}

// ---------------------------------------------------------------------------
// #748: every binary reached the panic hook, and `[panic]` is counted.
// ---------------------------------------------------------------------------

/// The profile the `[panic]`-cap tests drive: the first host-agnostic profile
/// whose MAX_PANIC is a number. Derived, so the tests follow the table.
fn capped_profile() -> (String, u32) {
    profiles()
        .into_iter()
        .filter(|p| p.os == "any")
        .find_map(|p| p.max_panic.parse::<u32>().ok().map(|cap| (p.name, cap)))
        .expect("some `os = any` profile caps MAX_PANIC — see at_least_one_profile_caps_panic_lines")
}

#[test]
fn a_binary_that_never_reached_the_panic_hook_fails_the_gate() {
    // Two binaries; the second announced no hook. Its panics would have gone
    // through the DEFAULT hook — unneutralised, and invisible to the `[panic]`
    // cap — and it had no REQUIRE knob to turn its skips into failures.
    let log = format!(
        "{}{}{}{PASSED_FIVE}",
        hooked_binary("reached_e2e"),
        unhooked_binary("never_reached_e2e"),
        e2e_evidence_for_profile()
    );
    let out = run_gate("unhooked", &log, 0);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    // Only the refusal block, not the whole stdout: the script tees the log to
    // stdout, so every binary's `Running` line appears above the verdict.
    let refusal = stdout
        .split_once("never reached the panic hook")
        .map(|(_, rest)| rest.split("full log:").next().unwrap_or(rest))
        .unwrap_or_else(|| panic!("no hook refusal\n{}", describe(&out)));
    assert!(
        refusal.contains("never_reached_e2e") && !refusal.contains("tests/reached_e2e.rs"),
        "the refusal must name the unhooked binary and only that one\n{}",
        describe(&out)
    );
}

#[test]
fn the_hook_check_is_per_binary_not_per_run() {
    // One announcement from the FIRST binary must not cover the second: the
    // total would be 1 either way, so a run-wide count would pass this.
    let log = format!(
        "{}{}{}{PASSED_FIVE}",
        hooked_binary("first_e2e"),
        unhooked_binary("second_e2e"),
        e2e_evidence_for_profile()
    );
    let out = run_gate("per-binary", &log, 0);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let refusal = stdout.split_once("never reached the panic hook").map(|(_, r)| r);
    assert!(
        refusal.is_some_and(|r| r.contains("second_e2e") && !r.contains("tests/first_e2e.rs")),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_log_with_passed_tests_but_no_running_lines_is_refused() {
    // Without cargo's `Running` headers there is nothing to attribute an
    // announcement to, and "no binary lacked the hook" would be vacuously
    // true. That is a refusal to judge, not a pass.
    let log = format!("{}{PASSED_FIVE}", e2e_evidence_for_profile());
    let out = run_gate("no-running", &log, 0);
    assert_ne!(out.status.code(), Some(0), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no `Running` lines")
            || String::from_utf8_lossy(&out.stderr).contains("no `Running` lines"),
        "{}",
        describe(&out)
    );
}

#[test]
fn panic_lines_over_the_cap_fail_the_gate() {
    let (profile, cap) = capped_profile();
    let panics: String = (0..=cap)
        .map(|i| format!("[panic] thread 'caught' panicked at f.rs:{i}:1: swallowed\n"))
        .collect();
    let log = format!("{panics}{}", healthy_log_for(&profile));
    let out = run_gate_profile(&profile, "panic-over", &log, 0);
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("[panic] line(s)"),
        "{}",
        describe(&out)
    );
}

#[test]
fn a_capped_profile_passes_at_exactly_its_cap() {
    // The boundary: a cap of N allows N. Without this, a `-ge` for `-gt` in the
    // script would refuse every healthy run of a profile capped above zero.
    let (profile, cap) = capped_profile();
    let panics: String = (0..cap)
        .map(|i| format!("[panic] thread 'caught' panicked at f.rs:{i}:1: expected\n"))
        .collect();
    let log = format!("{panics}{}", healthy_log_for(&profile));
    let out = run_gate_profile(&profile, "panic-at-cap", &log, 0);
    assert!(out.status.success(), "{}", describe(&out));
}

#[test]
fn the_install_announcement_is_not_counted_as_a_panic() {
    // `[panic-hook]` begins with `[panic`, so an unanchored or bracket-less
    // grep would count every announcement as a panic and fail every healthy
    // run of a capped profile.
    let (profile, cap) = capped_profile();
    let extra: String = (0..=cap).map(|_| format!("{}\n", crate::panic_hook::install_line())).collect();
    let log = format!("{}{extra}", healthy_log_for(&profile));
    let out = run_gate_profile(&profile, "announce-not-panic", &log, 0);
    assert!(out.status.success(), "{}", describe(&out));
}
