//! Sandbox helpers: `[SKIP]` probe, backend factory, canonical
//! shell-exec policy.
//!
//! Both `skip_if_sandbox_unavailable` and `backend()` are cfg-gated
//! per-OS so a single call site reads cleanly on Linux + macOS without
//! per-test `#[cfg]` ladders.

use std::path::Path;

use hhagent_sandbox::{SandboxBackend, SandboxPolicy};

/// Returns `true` if the per-OS sandbox backend's probe fails. Caller
/// should `return` immediately to short-circuit the test.
///
/// Linux: requires bwrap + unprivileged user-namespace permission
/// (AppArmor profile installed via
/// `scripts/linux/install-bwrap-apparmor-profile.sh`).
/// macOS: requires `/usr/bin/sandbox-exec` (present on all stock
/// installs from 10.5+).
#[cfg(target_os = "linux")]
pub fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::linux_bwrap::LinuxBwrap;
    if let Err(e) = LinuxBwrap::probe() {
        eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
        return true;
    }
    false
}

#[cfg(target_os = "macos")]
pub fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::macos_seatbelt::MacosSeatbelt;
    if let Err(e) = MacosSeatbelt::probe() {
        eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
        return true;
    }
    false
}

/// Boxed per-OS [`SandboxBackend`] for use in tests that spawn a
/// real sandboxed worker. The cfg-gating mirrors `default_backend()`
/// in `hhagent_sandbox` but stays here so tests don't import a
/// production helper that may grow per-feature gates.
#[cfg(target_os = "linux")]
pub fn backend() -> Box<dyn SandboxBackend> {
    Box::new(hhagent_sandbox::linux_bwrap::LinuxBwrap::new())
}

#[cfg(target_os = "macos")]
pub fn backend() -> Box<dyn SandboxBackend> {
    Box::new(hhagent_sandbox::macos_seatbelt::MacosSeatbelt::new())
}

/// Canonical sandbox policy for the shell-exec worker.
///
/// * `fs_read` = the worker binary itself (so it can be mapped at
///   spawn).
/// * `net = Deny` — shell-exec is never a network tool.
/// * `cpu_ms = 5_000`, `mem_mb = 256` — generous defaults for the
///   `echo` happy path; the tests that hit OOM or budget paths
///   override these.
/// * `profile = WorkerStrict` — Landlock + seccomp lockdown applied
///   from inside the worker before serve_stdio.
/// * `env` carries `HHAGENT_SHELL_ALLOWLIST` as a JSON array of
///   strings (the worker's allowlist contract).
///
/// Scope: this helper is for *direct* worker-spawn tests (e.g.
/// `shell_exec_e2e`, `audit_dispatch_e2e`) that bypass the daemon and
/// drive the worker themselves. Daemon-backed tests (e.g.
/// `cli_ask_e2e`, `observation_capture`) do not use this helper —
/// they seed the `tool_allowlists` table via
/// [`crate::allowlist::seed_tool_allowlist`] and let the daemon's
/// `build_tool_registry` pack `HHAGENT_SHELL_ALLOWLIST` from the DB
/// at spawn time.
pub fn policy_for_shell_exec(worker: &Path, allowlist: &[&str]) -> SandboxPolicy {
    let allow_json = serde_json::to_string(allowlist).expect("serialize allowlist");
    SandboxPolicy {
        fs_read: vec![worker.to_path_buf()],
        cpu_ms: 5_000,
        mem_mb: 256,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
        ..SandboxPolicy::default()
    }
}
