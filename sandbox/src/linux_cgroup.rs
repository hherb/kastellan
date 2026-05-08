//! Linux cgroup v2 CPU/memory caps via `systemd-run --user --scope`.
//!
//! Wrapping `bwrap` in `systemd-run --user --scope ...` places the
//! worker inside a transient cgroup that enforces hard memory and
//! defense-in-depth CPU/tasks ceilings. The cgroup is set up *before*
//! `bwrap` creates the worker namespace — so `systemd-run` is the
//! **outer** process and `bwrap` is the **inner** process. (If we did
//! it the other way around the worker could already be alive and
//! allocating before the limits were applied.)
//!
//! What this module enforces today:
//!   - `MemoryMax = policy.mem_mb` MiB — kernel OOM-kills on overrun.
//!     Verified by `worker_with_low_mem_max_is_oom_killed` in
//!     `tests/linux_smoke.rs`. **Paired with `MemorySwapMax=0`** so
//!     overflow can't silently page to swap (which would let a runaway
//!     worker burn host I/O and degrade the system without ever
//!     hitting the cap). On hosts without swap, `MemorySwapMax=0` is a
//!     no-op — emitting it unconditionally is harmless and keeps the
//!     contract honest.
//!   - `CPUQuota = 200%` — at most two CPUs of bandwidth. Hardcoded
//!     defense-in-depth default; not yet driven from `SandboxPolicy`.
//!     Resists CPU starvation of the host when policy doesn't tighten
//!     it further.
//!   - `TasksMax = 64` — fork-bomb resistance. Hardcoded default.
//!     Workers that legitimately use a few helper threads (Rust runtime,
//!     Python interpreter) stay well under this; a runaway loop
//!     spawning processes hits `EAGAIN` quickly.
//!
//! What this module does **not** yet enforce:
//!   - `policy.cpu_ms` is documented as a CPU-time budget. cgroup v2
//!     has no direct primitive for that (its CPU primitive is
//!     bandwidth, not budget). Eventual enforcement will be via
//!     `setrlimit(RLIMIT_CPU)` from the worker prelude before
//!     `exec(2)`. Tracked as a follow-up GitHub issue.
//!   - Tunable `cpu_quota_pct` / `tasks_max` policy fields. The two
//!     defaults above are hardcoded for now to avoid a `SandboxPolicy`
//!     schema change in the same session that introduces the cgroup
//!     wiring. Tracked as a follow-up GitHub issue.
//!
//! Why `--scope` and not `--service`:
//!   - `--scope` runs the wrapped command in the **foreground** of the
//!     calling shell with stdio inherited. We need that, because every
//!     worker speaks JSON-RPC over stdio.
//!   - `--service` would detach into a transient service unit and
//!     redirect stdio to the journal. That breaks JSON-RPC.

use std::process::{Command, Stdio};

use crate::{SandboxError, SandboxPolicy};

/// Defense-in-depth CPU bandwidth ceiling: at most 2 CPUs.
///
/// Resists CPU starvation of the host even when `policy.cpu_ms` is
/// unset or large. Will become tunable once `SandboxPolicy` grows a
/// `cpu_quota_pct` field.
const DEFAULT_CPU_QUOTA_PCT: u32 = 200;

/// Defense-in-depth task ceiling: 64 tasks per worker.
///
/// Defends against fork-bombs without breaking workers that use a
/// small number of helper threads. Will become tunable once
/// `SandboxPolicy` grows a `tasks_max` field.
const DEFAULT_TASKS_MAX: u64 = 64;

/// Build the `systemd-run` prefix argv for wrapping a sandboxed worker.
///
/// Returns the argv up to *and including* the `--` separator that
/// precedes the inner program. The caller appends the inner argv
/// (typically the output of [`crate::linux_bwrap::build_argv`]) directly
/// after — the result is a complete `systemd-run ... -- bwrap ...`
/// invocation.
///
/// Pure function, no I/O. Unit-testable in isolation from spawning.
///
/// # Example
///
/// ```ignore
/// let mut argv = build_systemd_run_argv(&policy);
/// argv.extend(build_argv(&policy, program, args));
/// // argv is now: systemd-run ... -- bwrap ...
/// ```
pub fn build_systemd_run_argv(policy: &SandboxPolicy) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(16);

    argv.push("systemd-run".into());
    // `--user` runs against the per-user systemd manager (no privilege
    // escalation, no system-wide effect).
    argv.push("--user".into());
    // `--scope` runs in the foreground, inheriting stdio. Required for
    // JSON-RPC over stdio.
    argv.push("--scope".into());
    // `--quiet` suppresses systemd-run's "Running as unit ..." banner
    // so it doesn't pollute stderr (and confuse JSON-RPC line readers
    // that watch stderr for diagnostics).
    argv.push("--quiet".into());
    // `--collect` auto-removes the transient unit on exit, even on
    // failure. Without it, failed scopes accumulate in
    // `systemctl --user --failed`.
    argv.push("--collect".into());

    // Memory cap (the primary policy-driven enforcement this layer adds).
    // `MemoryMax=0` is interpreted by systemd as "no limit", which is
    // not what a `mem_mb == 0` policy means — historically that field
    // is unset/uninitialised, not "unlimited". To stay fail-safe we
    // emit the property only when the policy explicitly asked for one.
    //
    // Pair with `MemorySwapMax=0`: without it, a worker that overruns
    // its RAM allotment is paged to swap instead of being OOM-killed.
    // On a 15 GiB-swap host that lets a runaway burn host I/O for many
    // seconds before any cap fires. With `MemorySwapMax=0` the kernel
    // counts swap against the cap (i.e. swap is unavailable to the
    // cgroup), so the OOM killer fires the moment RSS hits MemoryMax.
    if policy.mem_mb > 0 {
        argv.push("-p".into());
        argv.push(format!("MemoryMax={}M", policy.mem_mb));
        argv.push("-p".into());
        argv.push("MemorySwapMax=0".into());
    }

    // CPU bandwidth cap (defense-in-depth default; not policy-driven yet).
    argv.push("-p".into());
    argv.push(format!("CPUQuota={}%", DEFAULT_CPU_QUOTA_PCT));

    // Task count cap (defense-in-depth default; not policy-driven yet).
    argv.push("-p".into());
    argv.push(format!("TasksMax={}", DEFAULT_TASKS_MAX));

    // The `--` separator tells systemd-run that everything after is the
    // command to execute, not more `systemd-run` flags.
    argv.push("--".into());

    argv
}

/// Probe whether `systemd-run --user --scope` is usable on this host.
///
/// Mirrors the [`crate::linux_bwrap::LinuxBwrap::probe`] pattern: run
/// `/usr/bin/true` inside a minimal transient scope and report success
/// or a structured error. A failed probe means the user has no live
/// systemd manager (e.g. the session bus is down, or this is a
/// non-systemd distro), and the sandbox layer must fail closed —
/// containment defense-in-depth requires the cgroup ceiling to be
/// available.
pub fn cgroup_probe() -> Result<(), SandboxError> {
    let output = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SandboxError::Backend(format!("could not spawn systemd-run: {e}")))?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let hint = if stderr.contains("Failed to connect to bus")
        || stderr.contains("user@")
        || stderr.contains("No medium found")
    {
        "\n\nThe per-user systemd manager does not appear to be running. \
         On a normal desktop session it starts automatically; on headless \
         hosts you may need `loginctl enable-linger $USER` or to start a \
         user session manually."
    } else {
        ""
    };
    Err(SandboxError::Backend(format!(
        "systemd-run --user probe failed: {}{hint}",
        stderr.trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Net, Profile};

    /// A minimal policy carrying only the fields the cgroup layer reads,
    /// so unit tests don't have to track the full `SandboxPolicy` shape.
    fn policy_with_mem(mb: u64) -> SandboxPolicy {
        SandboxPolicy {
            fs_read: vec![],
            fs_write: vec![],
            net: Net::Deny,
            cpu_ms: 1_000,
            mem_mb: mb,
            profile: Profile::WorkerStrict,
            env: vec![],
        }
    }

    #[test]
    fn argv_starts_with_systemd_run() {
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        assert_eq!(argv[0], "systemd-run");
    }

    #[test]
    fn argv_uses_user_scope_quiet_collect() {
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        assert!(argv.contains(&"--user".into()), "argv: {argv:?}");
        assert!(argv.contains(&"--scope".into()), "argv: {argv:?}");
        assert!(argv.contains(&"--quiet".into()), "argv: {argv:?}");
        assert!(argv.contains(&"--collect".into()), "argv: {argv:?}");
    }

    #[test]
    fn argv_sets_memory_max_in_megabytes_from_policy() {
        let argv = build_systemd_run_argv(&policy_with_mem(128));
        let joined = argv.join(" ");
        assert!(
            joined.contains("-p MemoryMax=128M"),
            "expected MemoryMax=128M in: {joined}"
        );
    }

    #[test]
    fn argv_omits_memory_max_when_policy_is_zero() {
        // mem_mb=0 means "policy didn't set this" — not "unlimited".
        // systemd-run's interpretation of MemoryMax=0 is "unlimited",
        // which would silently downgrade the contract. Better to omit.
        let argv = build_systemd_run_argv(&policy_with_mem(0));
        let joined = argv.join(" ");
        assert!(
            !joined.contains("MemoryMax"),
            "expected no MemoryMax property when mem_mb=0, got: {joined}"
        );
        assert!(
            !joined.contains("MemorySwapMax"),
            "MemorySwapMax should also be omitted when MemoryMax is, got: {joined}"
        );
    }

    #[test]
    fn argv_pairs_memory_max_with_memory_swap_max_zero() {
        // Without MemorySwapMax=0 the kernel pages overruns to swap on
        // hosts that have any. The cap is only honest when both are set.
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        let joined = argv.join(" ");
        assert!(
            joined.contains("-p MemoryMax=64M"),
            "expected MemoryMax=64M in: {joined}"
        );
        assert!(
            joined.contains("-p MemorySwapMax=0"),
            "expected MemorySwapMax=0 paired with MemoryMax in: {joined}"
        );
    }

    #[test]
    fn argv_sets_default_cpu_quota_percent() {
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        let joined = argv.join(" ");
        assert!(
            joined.contains("-p CPUQuota=200%"),
            "expected default CPUQuota=200% in: {joined}"
        );
    }

    #[test]
    fn argv_sets_default_tasks_max() {
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        let joined = argv.join(" ");
        assert!(
            joined.contains("-p TasksMax=64"),
            "expected default TasksMax=64 in: {joined}"
        );
    }

    #[test]
    fn argv_ends_with_double_dash_separator() {
        // The trailing `--` is part of the prefix's contract: the caller
        // appends inner argv right after. Without it, the inner program
        // could be misinterpreted as more `systemd-run` flags.
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        assert_eq!(argv.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn argv_does_not_include_inner_program() {
        // linux_cgroup is composed with linux_bwrap by the spawn site;
        // it must not bake in any knowledge of bwrap or the worker.
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        assert!(!argv.contains(&"bwrap".into()));
        assert!(!argv.iter().any(|s| s.starts_with("/")));
    }

    #[test]
    fn property_args_use_the_p_flag_form() {
        // systemd-run accepts both `-p Key=Val` (two argv tokens) and
        // `--property=Key=Val` (one token). We use the former because
        // it's harder to mis-parse and is what the upstream docs lead
        // with.
        let argv = build_systemd_run_argv(&policy_with_mem(64));
        let dash_p_count = argv.iter().filter(|s| *s == "-p").count();
        // mem_mb=64 → MemoryMax + MemorySwapMax + CPUQuota + TasksMax
        // = 4 properties.
        assert_eq!(dash_p_count, 4, "argv: {argv:?}");
    }
}
