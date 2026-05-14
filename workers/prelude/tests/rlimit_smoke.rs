//! Cross-platform integration test for `workers/prelude/src/rlimit.rs`.
//!
//! Spawns the `lockdown-probe cpu-burner` binary with `HHAGENT_CPU_MS=200`
//! and verifies the kernel kills it via signal (SIGXCPU → SIGKILL)
//! within a generous wall-clock budget. The regression we're guarding
//! against is "rlimit was not applied at all" — which would let the
//! burner run for > 10 seconds before its own safety cap fires.
//!
//! Why cross-platform: `setrlimit(RLIMIT_CPU)` is POSIX and works on
//! Linux + macOS unchanged. This test runs on both.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Cargo provides this env var at compile time for tests in the same
/// crate as the binary target. Resolves to the absolute path of the
/// built `hhagent-lockdown-probe` binary in the workspace target dir.
/// Same pattern `seccomp_smoke.rs` uses.
const PROBE: &str = env!("CARGO_BIN_EXE_hhagent-lockdown-probe");

#[test]
fn cpu_burner_under_short_budget_is_killed_promptly() {
    // 200 ms cpu_ms → ceiling-div to 1 second RLIMIT_CPU. The kernel's
    // resolution is integer seconds, so we expect the kill within
    // 1–3 seconds wall-clock on a non-contended host. Give a generous
    // 8 seconds before declaring the rlimit didn't fire.
    let start = Instant::now();
    let status = Command::new(PROBE)
        .arg("cpu-burner")
        .env_clear()
        .env("HHAGENT_CPU_MS", "200")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("spawn lockdown-probe cpu-burner");
    let elapsed = start.elapsed();

    // The test interprets "killed by any signal" as the rlimit firing.
    // Exit codes are ambiguous across platforms (SIGXCPU vs SIGKILL
    // mapping varies), but `ExitStatus::code()` is `None` whenever the
    // process died via signal — which is the load-bearing fact.
    assert!(
        status.code().is_none(),
        "expected cpu-burner to be killed by signal under HHAGENT_CPU_MS=200, \
         got exit code {:?} after {:?}",
        status.code(),
        elapsed
    );

    // Defense-in-depth assertion: even if some future platform mapped
    // SIGXCPU to a normal exit code, we still expect the kill to land
    // well before the burner's own 10-second wall-clock cap.
    assert!(
        elapsed < Duration::from_secs(8),
        "cpu-burner was supposed to be killed by RLIMIT_CPU within 1–3s, \
         but actually ran for {elapsed:?} — rlimit may not have applied"
    );
}

#[test]
fn cpu_burner_with_no_env_runs_past_one_second() {
    // Positive control: without HHAGENT_CPU_MS the burner runs
    // unmolested. A future regression that silently disables
    // apply_from_env (e.g. always returns Disabled regardless of env)
    // would still pass the first test alone — this test catches that.
    //
    // We don't let it run to its own 10 s cap (slow test); instead we
    // SIGKILL it ourselves after 2 seconds and confirm it's still
    // alive at that point.
    let mut child = Command::new(PROBE)
        .arg("cpu-burner")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn lockdown-probe cpu-burner");

    std::thread::sleep(Duration::from_secs(2));

    // try_wait returns Ok(Some(_)) if the child has already exited,
    // Ok(None) if still running. With no rlimit, it must still be running.
    let still_running = matches!(child.try_wait(), Ok(None));
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        still_running,
        "expected cpu-burner with no HHAGENT_CPU_MS to still be running after 2s; \
         it exited early, which suggests apply_from_env is incorrectly applying a default cap"
    );
}
