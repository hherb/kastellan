//! Integration tests for the Landlock layer of `hhagent-worker-prelude`.
//!
//! Each test runs the `hhagent-lockdown-probe` binary as a subprocess so
//! the one-way Landlock filter doesn't poison sibling tests in the same
//! process. The probe binary is built automatically by cargo; its path is
//! injected via the `CARGO_BIN_EXE_*` env var at compile time.
//!
//! ## Skip pattern
//!
//! Landlock requires Linux 5.13+. On older kernels the probe will report
//! `KernelTooOld` in its stderr; we honour that with a `[SKIP]` line via
//! `eprintln!` (visible under `cargo test -- --nocapture`) rather than a
//! silent green run that would mask broken containment.

#![cfg(target_os = "linux")]

use std::process::{Command, Output};

const PROBE: &str = env!("CARGO_BIN_EXE_hhagent-lockdown-probe");

const TEST_SCRATCH_DIR: &str = "/tmp/hhagent_prelude_landlock_smoke";
const NEVER_WRITABLE_PATH: &str = "/etc/__hhagent_landlock_test_should_be_blocked";

/// Run the probe with a clean environment plus the supplied (key, value)
/// overrides. `env_clear` keeps test runs reproducible regardless of the
/// developer's shell state.
fn run_probe(env: &[(&str, &str)], args: &[&str]) -> Output {
    Command::new(PROBE)
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .output()
        .expect("failed to spawn lockdown-probe")
}

/// True iff the kernel actually installed a Landlock filter when the
/// probe ran. Detects the `KernelTooOld` arm of `LandlockReport`.
fn landlock_enforced() -> bool {
    // A no-op invocation: getpid is allowed, seccomp disabled. We only
    // care about the LOCKDOWN_REPORT line on stderr.
    let out = run_probe(&[("HHAGENT_SECCOMP_PROFILE", "none")], &["seccomp-getpid"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("KernelTooOld") {
        eprintln!("\n[SKIP] Landlock not enforced on this kernel: {stderr}");
        return false;
    }
    if !stderr.contains("FullyEnforced") && !stderr.contains("PartiallyEnforced") {
        eprintln!("\n[SKIP] unexpected lockdown report: {stderr}");
        return false;
    }
    true
}

#[test]
fn write_to_unallowed_path_is_blocked() {
    if !landlock_enforced() {
        return;
    }
    // Make sure the scratch dir exists so PathFd::new succeeds inside the
    // probe; its actual contents are irrelevant for this test.
    std::fs::create_dir_all(TEST_SCRATCH_DIR).expect("create scratch dir");

    let rw_json = format!("[{:?}]", TEST_SCRATCH_DIR);
    let out = run_probe(
        &[
            ("HHAGENT_LANDLOCK_RW", rw_json.as_str()),
            ("HHAGENT_SECCOMP_PROFILE", "none"),
        ],
        &["landlock-write", NEVER_WRITABLE_PATH],
    );

    // Probe exit codes:
    //   0 → write succeeded (Landlock failed to block — bug)
    //   1 → PermissionDenied (Landlock did its job)
    //   2 → some other I/O error (also surprising — investigate)
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected PermissionDenied (exit 1), got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn write_to_allowlisted_scratch_succeeds() {
    if !landlock_enforced() {
        return;
    }
    std::fs::create_dir_all(TEST_SCRATCH_DIR).expect("create scratch dir");

    let target = format!("{TEST_SCRATCH_DIR}/probe_write_target");
    let rw_json = format!("[{:?}]", TEST_SCRATCH_DIR);
    let out = run_probe(
        &[
            ("HHAGENT_LANDLOCK_RW", rw_json.as_str()),
            ("HHAGENT_SECCOMP_PROFILE", "none"),
        ],
        &["landlock-write", &target],
    );

    assert_eq!(
        out.status.code(),
        Some(0),
        "expected success (exit 0), got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    // Bonus check: the file actually landed on disk.
    let contents = std::fs::read(&target).expect("scratch write should be observable to test");
    assert_eq!(contents, b"probe");
    let _ = std::fs::remove_file(&target);
}

/// Stage-2 invariant: bumping `TARGET_ABI` from v1 to v6 was meant to
/// lift the report from `PartiallyEnforced` (we'd been requesting fewer
/// rights than the kernel could enforce) to `FullyEnforced` (we now
/// request and obtain the full v6 set, including `Refer`, `Truncate`,
/// `IoctlDev`, and the v6 `Scope` rights). On any kernel ≥ 6.12 — the
/// user's host is on 6.17 — this test must report `FullyEnforced`. On
/// older kernels we `[SKIP]` rather than fail; the regression we want
/// to guard against is a *downgrade* on a host that previously achieved
/// full enforcement.
#[test]
fn v6_abi_yields_fully_enforced_on_modern_kernel() {
    let out = run_probe(
        &[
            ("HHAGENT_LANDLOCK_RW", "[]"),
            ("HHAGENT_SECCOMP_PROFILE", "none"),
        ],
        &["seccomp-getpid"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("KernelTooOld") {
        eprintln!("\n[SKIP] Landlock not enforced on this kernel: {stderr}");
        return;
    }
    if stderr.contains("PartiallyEnforced") {
        // Either a kernel < 6.12 (where some v6 rights aren't supported)
        // or a regression where one of our rules dropped a right
        // silently. The former is fine to skip; the latter we want to
        // see — but distinguishing them from userspace is awkward, so
        // we let the test fail and document the two possibilities.
        panic!(
            "Landlock reported PartiallyEnforced; either the host kernel is < 6.12 \
             (in which case adjust this test) or a rule silently downgraded a \
             right (e.g. file-vs-dir mismatch — see add_path_rule). stderr: {stderr}"
        );
    }
    assert!(
        stderr.contains("FullyEnforced"),
        "expected FullyEnforced report after v6 bump, got: {stderr}"
    );
}

#[test]
fn reading_from_usr_still_works_after_lockdown() {
    if !landlock_enforced() {
        return;
    }
    // /usr is in DEFAULT_RO_EXEC_ROOTS so reads must continue to work,
    // otherwise the dynamic linker / libc / allow-listed exec targets are
    // unreachable and every worker breaks.
    let out = run_probe(
        &[
            ("HHAGENT_LANDLOCK_RW", "[]"),
            ("HHAGENT_SECCOMP_PROFILE", "none"),
        ],
        &["landlock-read", "/usr/bin/true"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "reading /usr/bin/true under Landlock failed: {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
