//! Integration test for `kastellan-worker-lockdown-exec`: the shim applies the
//! prelude seccomp filter, then execve's a target which inherits it.
//!
//! KASTELLAN_LANDLOCK_PROFILE=none is required: with Landlock on, the shim's
//! ruleset (read+exec under /usr etc.) would deny exec of the probe binary,
//! which lives in the cargo target dir — exactly the seccomp-only posture
//! browser-driver uses.

#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};

const SHIM: &str = env!("CARGO_BIN_EXE_kastellan-worker-lockdown-exec");
const PROBE: &str = env!("CARGO_BIN_EXE_kastellan-lockdown-probe");
const SIGSYS: i32 = libc::SIGSYS;

/// Run `SHIM PROBE <target_args>` with the given env (cleared otherwise).
fn run_shim(env: &[(&str, &str)], target_args: &[&str]) -> Output {
    Command::new(SHIM)
        .arg(PROBE)
        .args(target_args)
        .env_clear()
        .envs(env.iter().copied())
        .output()
        .expect("failed to spawn lockdown-exec shim")
}

/// Skip guard: confirm this host can install a seccomp filter at all. Reuses
/// the probe's self-lockdown path (it prints "Installed" on stderr).
fn seccomp_enforced() -> bool {
    let out = Command::new(PROBE)
        .args(["seccomp-getpid"])
        .env_clear()
        .envs([("KASTELLAN_SECCOMP_PROFILE", "strict")])
        .output()
        .expect("failed to spawn probe");
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.contains("Installed") {
        eprintln!("\n[SKIP] seccomp not installable on this host: {stderr}");
        return false;
    }
    true
}

#[test]
fn baseline_raw_unshare_without_shim_is_not_killed() {
    // Run the probe directly (no shim, no seccomp). Proves raw-unshare does not
    // self-lockdown, so the SIGSYS in the next test is genuinely inherited.
    // No env is set: the `raw-*` fast path dispatches before the probe reads
    // KASTELLAN_SECCOMP_PROFILE, so there is nothing to configure here.
    let out = Command::new(PROBE)
        .args(["raw-unshare"])
        .env_clear()
        .output()
        .expect("failed to spawn probe");
    assert!(
        out.status.signal().is_none(),
        "raw-unshare must not be SIGSYS-killed without a filter; got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn shim_seccomp_is_inherited_and_kills_unshare() {
    if !seccomp_enforced() {
        return;
    }
    let out = run_shim(
        &[
            ("KASTELLAN_SECCOMP_PROFILE", "strict"),
            ("KASTELLAN_LANDLOCK_PROFILE", "none"),
        ],
        &["raw-unshare"],
    );
    assert_eq!(
        out.status.signal(),
        Some(SIGSYS),
        "expected the shim's seccomp filter to kill unshare across execve; got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    // A signal kill has no exit code — confirms it was SIGSYS, not a normal
    // exit (mirrors seccomp_smoke.rs::unshare_is_killed_by_sigsys).
    assert!(
        out.status.code().is_none(),
        "unshare under the inherited filter must be signal-killed, not exit with a code: {:?}",
        out.status.code()
    );
}

#[test]
fn shim_target_runs_and_innocent_syscall_survives() {
    if !seccomp_enforced() {
        return;
    }
    let out = run_shim(
        &[
            ("KASTELLAN_SECCOMP_PROFILE", "strict"),
            ("KASTELLAN_LANDLOCK_PROFILE", "none"),
        ],
        &["raw-getpid"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "shim must execve the target and getpid must survive; got {:?}\nstderr: {stderr}",
        out.status,
    );
    // Confirm the probe actually ran past the execve (not a shim-side exit):
    // probe_getpid prints "getpid() = <pid>" on stderr.
    assert!(
        stderr.contains("getpid()"),
        "expected the probe to run and print getpid output across the execve; stderr: {stderr}"
    );
}
