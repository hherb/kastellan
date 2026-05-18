//! Subprocess-isolated coverage of `rlimit::apply_from_env`'s happy
//! path (issue #57).
//!
//! ## Why this lives in `tests/` and not next to `apply_from_env`
//!
//! `setrlimit(RLIMIT_CPU, …)` is process-scoped and the hard limit can
//! only be tightened thereafter, not raised. An in-process test that
//! calls `apply_from_env` with a real budget would permanently lower
//! the prelude test binary's CPU budget for every subsequent test in
//! the same run — easy to live with today (the suite is sub-second
//! CPU), but a latent foot-gun the moment a CPU-heavy test gets added.
//!
//! The fix is to assert the FFI shape from a *fresh* subprocess: spawn
//! `hhagent-lockdown-probe rlimit-report` with `HHAGENT_CPU_MS=30000`,
//! let the probe's `main` call `apply_from_env` at startup, read the
//! probe's stderr (which already prints `RLIMIT_REPORT: {report:?}`),
//! and confirm the report says `Applied { cpu_seconds: 30 }`.
//!
//! Cross-platform: `RLIMIT_CPU` is POSIX; this test runs on Linux and
//! macOS unchanged. The companion `rlimit_smoke.rs` covers the
//! enforcement side (kernel actually kills the burner under a short
//! budget); this file covers the FFI/report side (a successful
//! `setrlimit` call surfaces as `Applied`).

use std::process::{Command, Stdio};

const PROBE: &str = env!("CARGO_BIN_EXE_hhagent-lockdown-probe");

#[test]
fn apply_from_env_with_generous_budget_reports_applied() {
    // 30000 ms → ceiling-div to 30 RLIMIT_CPU seconds. Generous enough
    // that even on a slow CI host the probe exits cleanly without ever
    // bumping into the cap; the test cares only about the report
    // surface, not enforcement.
    let output = Command::new(PROBE)
        .arg("rlimit-report")
        .env_clear()
        .env("HHAGENT_CPU_MS", "30000")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lockdown-probe rlimit-report");

    assert!(
        output.status.success(),
        "probe must exit 0 in the rlimit-report happy path; \
         got status {:?}; stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Match the exact debug shape printed by `eprintln!("RLIMIT_REPORT:
    // {report:?}")` in `lockdown_probe.rs`. The substring assertion is
    // resilient to extra LOCKDOWN_REPORT lines etc. on the same stderr.
    assert!(
        stderr.contains("RLIMIT_REPORT: Applied { cpu_seconds: 30 }"),
        "expected RLIMIT_REPORT: Applied {{ cpu_seconds: 30 }} on stderr; \
         got:\n{stderr}",
    );
}

#[test]
fn apply_from_env_with_no_budget_env_reports_disabled() {
    // Negative control: omit HHAGENT_CPU_MS entirely and assert the
    // probe prints `Disabled`. A future regression that silently
    // applies a default cap regardless of env would fail this — the
    // companion enforcement test (`cpu_burner_with_no_env_runs_past_one_second`)
    // catches the same thing from the other end, but this is the
    // faster + more direct surface check.
    let output = Command::new(PROBE)
        .arg("rlimit-report")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lockdown-probe rlimit-report");

    assert!(
        output.status.success(),
        "probe must exit 0 with no HHAGENT_CPU_MS set; got status {:?}; \
         stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("RLIMIT_REPORT: Disabled"),
        "expected RLIMIT_REPORT: Disabled on stderr when HHAGENT_CPU_MS is unset; \
         got:\n{stderr}",
    );
}
