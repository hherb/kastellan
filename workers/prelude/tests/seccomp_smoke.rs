//! Integration tests for the seccomp-bpf layer of `kastellan-worker-prelude`.
//!
//! Verifies the deny-list does what it says — denied syscalls trigger
//! SIGSYS-kill, allowed syscalls survive. Like `landlock_smoke`, each test
//! runs the `kastellan-lockdown-probe` binary as a subprocess so the
//! one-way filter doesn't poison sibling tests.
//!
//! ## Skip pattern
//!
//! seccomp-bpf has been in mainline Linux since 3.5 (2012), so on any
//! reasonable contemporary host these tests run. We still detect a
//! `Disabled` report on stderr — surfaces the case where the test
//! environment forgot to set `KASTELLAN_SECCOMP_PROFILE`.

#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};

const PROBE: &str = env!("CARGO_BIN_EXE_kastellan-lockdown-probe");

/// SIGSYS = 31 on Linux. The kernel sends this when seccomp's
/// `KillProcess` action fires. We use the libc constant so a future arch
/// with a different number still works.
const SIGSYS: i32 = libc::SIGSYS;

fn run_probe(env: &[(&str, &str)], args: &[&str]) -> Output {
    Command::new(PROBE)
        .args(args)
        .env_clear()
        .envs(env.iter().copied())
        .output()
        .expect("failed to spawn lockdown-probe")
}

fn seccomp_enforced() -> bool {
    // No-op invocation just to read the LOCKDOWN_REPORT line on stderr.
    let out = run_probe(
        &[("KASTELLAN_SECCOMP_PROFILE", "strict")],
        &["seccomp-getpid"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.contains("Installed") {
        eprintln!("\n[SKIP] seccomp filter not installed: {stderr}");
        return false;
    }
    true
}

#[test]
fn unshare_is_killed_by_sigsys() {
    if !seccomp_enforced() {
        return;
    }
    let out = run_probe(
        &[("KASTELLAN_SECCOMP_PROFILE", "strict")],
        &["seccomp-unshare"],
    );

    // KillProcess sends SIGSYS to the offending thread, which terminates
    // the process. We should see signal == SIGSYS and no exit code.
    assert_eq!(
        out.status.signal(),
        Some(SIGSYS),
        "expected SIGSYS kill, got status {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.code().is_none(),
        "process exited normally with code {:?} — seccomp did not block unshare!",
        out.status.code()
    );
}

#[test]
fn mount_is_killed_by_sigsys() {
    if !seccomp_enforced() {
        return;
    }
    let out = run_probe(
        &[("KASTELLAN_SECCOMP_PROFILE", "strict")],
        &["seccomp-mount"],
    );
    assert_eq!(
        out.status.signal(),
        Some(SIGSYS),
        "expected SIGSYS kill, got status {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn getpid_survives_lockdown() {
    if !seccomp_enforced() {
        return;
    }
    // Innocent syscall — must be in the allow-list. If this test starts
    // failing, the allow-list dropped a runtime-essential syscall.
    let out = run_probe(
        &[("KASTELLAN_SECCOMP_PROFILE", "strict")],
        &["seccomp-getpid"],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "getpid should survive lockdown, got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Stage-2 invariant: under `Profile::Strict` the BSD-socket family is
/// **not** in the allow-list, so `socket(AF_INET, SOCK_STREAM, 0)` must
/// trigger `SECCOMP_RET_KILL_PROCESS` (SIGSYS). Without this, a future
/// regression that re-merged Strict and NetClient would silently allow
/// outbound networking from a worker that's supposed to be air-gapped.
#[test]
fn socket_is_killed_under_strict() {
    if !seccomp_enforced() {
        return;
    }
    let out = run_probe(
        &[("KASTELLAN_SECCOMP_PROFILE", "strict")],
        &["seccomp-socket"],
    );
    assert_eq!(
        out.status.signal(),
        Some(SIGSYS),
        "expected SIGSYS kill under Strict, got status {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Stage-2 invariant: under `Profile::NetClient` the BSD-socket family
/// **is** in the allow-list. The probe must survive — either with exit 0
/// (socket() succeeded) or exit 3 (socket() returned an errno but
/// seccomp didn't kill us). What we must *not* see is SIGSYS.
#[test]
fn socket_survives_under_net_client() {
    if !seccomp_enforced() {
        return;
    }
    let out = run_probe(
        &[("KASTELLAN_SECCOMP_PROFILE", "net_client")],
        &["seccomp-socket"],
    );
    assert!(
        out.status.signal().is_none(),
        "socket() must not be killed under NetClient, got signal {:?}\nstderr: {}",
        out.status.signal(),
        String::from_utf8_lossy(&out.stderr)
    );
    let code = out.status.code();
    assert!(
        code == Some(0) || code == Some(3),
        "socket probe under NetClient should exit 0 or 3, got {code:?}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Symmetric coverage: the catastrophic syscall set must remain killed
/// under `Profile::NetClient` too. `unshare(CLONE_NEWUSER)` is the
/// canary — it's the one syscall that, if reachable, would let a
/// compromised worker escape into a fresh user namespace.
#[test]
fn unshare_is_killed_under_net_client() {
    if !seccomp_enforced() {
        return;
    }
    let out = run_probe(
        &[("KASTELLAN_SECCOMP_PROFILE", "net_client")],
        &["seccomp-unshare"],
    );
    assert_eq!(
        out.status.signal(),
        Some(SIGSYS),
        "expected SIGSYS kill under NetClient, got status {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
