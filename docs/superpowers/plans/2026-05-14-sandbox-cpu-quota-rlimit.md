# Sandbox CPU Quota + RLIMIT_CPU — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `cpu_quota_pct` / `tasks_max` cgroup ceilings driven from `SandboxPolicy` (Linux), and enforce `policy.cpu_ms` cross-platform via `setrlimit(RLIMIT_CPU)` applied from the worker prelude before `lock_down()`.

**Architecture:** Two parallel tracks: (1) Linux cgroup wiring — pure plumbing of two `Option<T>` fields through `build_systemd_run_argv`; (2) cross-platform rlimit — new `workers/prelude/src/rlimit.rs` module reads `HHAGENT_CPU_MS` env var (set by `core::tool_host::derive_lockdown_env`) and applies `RLIMIT_CPU` before lock-down. The two tracks compose at the `serve_stdio` entry point.

**Tech Stack:** Rust 2021. `libc 0.2` for `setrlimit` FFI. `seccompiler` + `landlock` Linux-only (unchanged). `systemd-run --user --scope` for cgroup invocation (unchanged).

---

## File structure

| File | Status | Responsibility |
| --- | --- | --- |
| [`sandbox/src/lib.rs`](../../../sandbox/src/lib.rs) | MODIFY | + 2 fields on `SandboxPolicy`; extend `Default`; 1 new unit test |
| [`sandbox/src/linux_cgroup.rs`](../../../sandbox/src/linux_cgroup.rs) | MODIFY | Read `policy.cpu_quota_pct.unwrap_or(DEFAULT)` and `policy.tasks_max.unwrap_or(DEFAULT)`; 4 new unit tests |
| [`workers/prelude/Cargo.toml`](../../../workers/prelude/Cargo.toml) | MODIFY | Promote `libc = "0.2"` from Linux-cfg to top-level deps |
| `workers/prelude/src/rlimit.rs` | CREATE | New module — `cpu_ms_to_seconds` pure helper, `RlimitReport`, `RlimitError`, `apply_from_env` |
| [`workers/prelude/src/lib.rs`](../../../workers/prelude/src/lib.rs) | MODIFY | `mod rlimit;`; `LockdownReport` restructure (`SkippedNonLinux` → `NonLinux { rlimit }`, `Linux { …, rlimit }`); `serve_stdio` composes rlimit + lock-down |
| [`workers/prelude/src/bin/lockdown_probe.rs`](../../../workers/prelude/src/bin/lockdown_probe.rs) | MODIFY | Call `rlimit::apply_from_env` at top alongside `lock_down`; new `cpu-burner` subcommand |
| `workers/prelude/tests/rlimit_smoke.rs` | CREATE | Integration test — spawn `lockdown-probe cpu-burner` with `HHAGENT_CPU_MS=200`, assert SIGXCPU/SIGKILL within wall-clock tolerance |
| [`core/src/tool_host.rs`](../../../core/src/tool_host.rs) | MODIFY | + `pub const ENV_CPU_MS`; extend `derive_lockdown_env`; 2 new unit tests |

Each task below produces a self-contained commit. TDD red→green→commit on every task.

---

### Task 1: Add `cpu_quota_pct` and `tasks_max` fields to `SandboxPolicy`

**Files:**
- Modify: `sandbox/src/lib.rs` (struct definition, `Default` impl, test module)

The previous session shipped `Default for SandboxPolicy` precisely so this field addition is zero-churn. Every fixture site already uses `..SandboxPolicy::default()`.

- [ ] **Step 1: Extend the test for `Default` to assert both new fields are `None`**

Add this test to the existing `tests` module in `sandbox/src/lib.rs` (next to `sandbox_policy_default_is_strict_deny_with_one_second_budget`):

```rust
/// Both new tunables default to `None`, which falls back to the
/// hardcoded defense-in-depth ceilings in `linux_cgroup`. Production
/// policies override explicitly when they need tighter caps.
#[test]
fn sandbox_policy_default_leaves_cpu_quota_and_tasks_max_unset() {
    let p = SandboxPolicy::default();
    assert_eq!(p.cpu_quota_pct, None);
    assert_eq!(p.tasks_max, None);
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-sandbox --lib sandbox_policy_default_leaves_cpu_quota_and_tasks_max_unset 2>&1 | tail -10
```

Expected: compile error `no field cpu_quota_pct on type SandboxPolicy` (or `tasks_max`).

- [ ] **Step 3: Add the fields + extend `Default`**

In `sandbox/src/lib.rs`, modify the `SandboxPolicy` struct:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// Read-only mounts/paths.
    pub fs_read: Vec<PathBuf>,
    /// Writable paths (typically a per-worker scratch dir).
    pub fs_write: Vec<PathBuf>,
    /// Network policy.
    pub net: Net,
    /// Hard CPU-time limit (milliseconds). Enforced via
    /// `setrlimit(RLIMIT_CPU)` from the worker prelude (POSIX, so applies
    /// on Linux and macOS). `0` means "unset, no rlimit applied".
    pub cpu_ms: u64,
    /// Hard memory limit (megabytes). Linux only — enforced via cgroup
    /// `MemoryMax`. macOS memory enforcement is deferred to the future
    /// micro-VM backend (RLIMIT_AS has high false-positive risk).
    pub mem_mb: u64,
    /// Profile preset.
    pub profile: Profile,
    /// Per-worker CPU bandwidth ceiling (percent of one CPU). `None`
    /// falls back to the defense-in-depth default (200%) hardcoded in
    /// [`crate::linux_cgroup`]. Linux cgroup only — has no effect on
    /// macOS (no equivalent primitive in Seatbelt).
    #[serde(default)]
    pub cpu_quota_pct: Option<u32>,
    /// Per-worker max task count (cgroup `pids.max`). `None` falls
    /// back to the defense-in-depth default (64). Linux cgroup only.
    #[serde(default)]
    pub tasks_max: Option<u64>,
    /// Environment variables to set inside the jail. Empty by default
    /// — the host environment is **always** cleared before this is
    /// applied, so the jail sees only what's listed here.
    #[serde(default)]
    pub env: Vec<(String, String)>,
}
```

And the `Default` impl:

```rust
impl Default for SandboxPolicy {
    /// Conservative defaults: no FS access, no network, strict profile,
    /// 1-second CPU budget, 64 MiB memory, no cgroup overrides. Production
    /// callers (e.g. `shell_exec_entry`) override the limits explicitly;
    /// the `Default` impl exists so tests and future field additions can
    /// use `..Default::default()` without churning every fixture.
    fn default() -> Self {
        Self {
            fs_read: Vec::new(),
            fs_write: Vec::new(),
            net: Net::default(),
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::default(),
            cpu_quota_pct: None,
            tasks_max: None,
            env: Vec::new(),
        }
    }
}
```

- [ ] **Step 4: Run the new test + all sandbox tests to verify green**

```bash
source "$HOME/.cargo/env" && cargo test -p hhagent-sandbox --lib 2>&1 | tail -20
```

Expected: all sandbox-lib tests pass (existing 30+ + 1 new = 31+). Zero warnings.

- [ ] **Step 5: Build the full workspace to confirm no fixture site broke**

```bash
source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -5
```

Expected: clean build. The previous session migrated every fixture to `..SandboxPolicy::default()`, so this should succeed.

- [ ] **Step 6: Commit**

```bash
git add sandbox/src/lib.rs && git commit -m "$(cat <<'EOF'
sandbox(policy): add cpu_quota_pct + tasks_max policy fields

Both default to None, which falls back to the existing hardcoded
defense-in-depth ceilings in linux_cgroup (200% CPU, 64 tasks). The
previous session shipped Default for SandboxPolicy precisely so this
field addition is zero-churn for fixture sites that already use
..SandboxPolicy::default().

cpu_quota_pct and tasks_max are documented as Linux cgroup only. macOS
Seatbelt has no equivalent primitive; CPU bandwidth on macOS waits for
the future micro-VM backend (issue #55).

Closes part of issue #6.
EOF
)"
```

---

### Task 2: Wire `cpu_quota_pct` + `tasks_max` through `build_systemd_run_argv`

**Files:**
- Modify: `sandbox/src/linux_cgroup.rs` (function body, doc, test module)

- [ ] **Step 1: Write the two new failing tests**

The existing tests `argv_sets_default_cpu_quota_percent` and `argv_sets_default_tasks_max` already pin the "None falls back to the named-const default" code path (they construct policies via `policy_with_mem` which uses `SandboxPolicy::default()`, leaving `cpu_quota_pct` and `tasks_max` as `None`). The only new code path is the override case, so we add two tests for that.

Add to the `tests` module in `sandbox/src/linux_cgroup.rs`, after the existing `argv_sets_default_tasks_max` test:

```rust
/// Helper: a policy that sets cpu_quota_pct.
fn policy_with_cpu_quota(pct: u32) -> SandboxPolicy {
    SandboxPolicy {
        mem_mb: 64,
        cpu_quota_pct: Some(pct),
        ..SandboxPolicy::default()
    }
}

/// Helper: a policy that sets tasks_max.
fn policy_with_tasks_max(n: u64) -> SandboxPolicy {
    SandboxPolicy {
        mem_mb: 64,
        tasks_max: Some(n),
        ..SandboxPolicy::default()
    }
}

#[test]
fn argv_uses_policy_cpu_quota_when_set() {
    let argv = build_systemd_run_argv(&policy_with_cpu_quota(50));
    let joined = argv.join(" ");
    assert!(
        joined.contains("-p CPUQuota=50%"),
        "expected CPUQuota=50% from policy override in: {joined}"
    );
    // Make sure the default 200% isn't *also* present.
    assert!(
        !joined.contains("CPUQuota=200%"),
        "default 200% should not leak through when policy overrides it: {joined}"
    );
}

#[test]
fn argv_uses_policy_tasks_max_when_set() {
    let argv = build_systemd_run_argv(&policy_with_tasks_max(8));
    let joined = argv.join(" ");
    assert!(
        joined.contains("-p TasksMax=8"),
        "expected TasksMax=8 from policy override in: {joined}"
    );
    assert!(
        !joined.contains("TasksMax=64"),
        "default TasksMax=64 should not leak when policy overrides it: {joined}"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-sandbox --lib argv_uses_policy_ 2>&1 | tail -15
```

Expected: 2 failures — both new tests fail because the current code is hardcoded to `DEFAULT_CPU_QUOTA_PCT=200` and `DEFAULT_TASKS_MAX=64`, ignoring the policy override.

- [ ] **Step 3: Replace the hardcoded formats with policy reads**

In `sandbox/src/linux_cgroup.rs::build_systemd_run_argv`, replace the two lines that emit `CPUQuota` and `TasksMax` with policy-driven reads. Find:

```rust
    // CPU bandwidth cap (defense-in-depth default; not policy-driven yet).
    argv.push("-p".into());
    argv.push(format!("CPUQuota={}%", DEFAULT_CPU_QUOTA_PCT));

    // Task count cap (defense-in-depth default; not policy-driven yet).
    argv.push("-p".into());
    argv.push(format!("TasksMax={}", DEFAULT_TASKS_MAX));
```

Replace with:

```rust
    // CPU bandwidth cap. Policy-driven via `cpu_quota_pct`; the named
    // const is the defense-in-depth fallback when the policy doesn't
    // tighten it further.
    let cpu_quota_pct = policy.cpu_quota_pct.unwrap_or(DEFAULT_CPU_QUOTA_PCT);
    argv.push("-p".into());
    argv.push(format!("CPUQuota={cpu_quota_pct}%"));

    // Task count cap. Policy-driven via `tasks_max`; the named const is
    // the defense-in-depth fallback. A worker that legitimately uses a
    // few helper threads (Rust runtime, Python interpreter) stays well
    // under 64; tighten via policy.tasks_max for stricter cases.
    let tasks_max = policy.tasks_max.unwrap_or(DEFAULT_TASKS_MAX);
    argv.push("-p".into());
    argv.push(format!("TasksMax={tasks_max}"));
```

Also update the module-level doc comment that says "Hardcoded defense-in-depth default; not yet driven from `SandboxPolicy`." — change both `CPUQuota` and `TasksMax` doc bullets to describe the new "policy-driven via `cpu_quota_pct`/`tasks_max`, fallback is the named const" shape. The "What this module does **not** yet enforce" section's two bullets about tunable `cpu_quota_pct`/`tasks_max` need to go away (they're now done) — keep only the `policy.cpu_ms` bullet (which moves to the prelude in Task 5).

- [ ] **Step 4: Re-run the two new tests to confirm green**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-sandbox --lib argv_uses_policy_ 2>&1 | tail -10
```

Expected: 2 pass. Also re-run the existing default-path tests to confirm they still pin the fallback:

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-sandbox --lib argv_sets_default_ 2>&1 | tail -10
```

Expected: 2 pass (`argv_sets_default_cpu_quota_percent`, `argv_sets_default_tasks_max`).

- [ ] **Step 5: Run the full sandbox unit suite to confirm no regression**

```bash
source "$HOME/.cargo/env" && cargo test -p hhagent-sandbox --lib 2>&1 | tail -5
```

Expected: all sandbox-lib tests pass; zero warnings.

- [ ] **Step 6: Commit**

```bash
git add sandbox/src/linux_cgroup.rs && git commit -m "$(cat <<'EOF'
sandbox(cgroup): drive CPUQuota and TasksMax from policy fields

build_systemd_run_argv now reads policy.cpu_quota_pct and policy.tasks_max
when set, falling back to the existing named consts DEFAULT_CPU_QUOTA_PCT
(200) and DEFAULT_TASKS_MAX (64). The two named consts stay so the
defense-in-depth ceiling is auditable and the "default behaviour"
contract is pinned by a regression test.

Closes part of issue #6 (Linux cgroup tunable side).
EOF
)"
```

---

### Task 3: Promote `libc` to top-level dependency in `workers/prelude`

**Files:**
- Modify: `workers/prelude/Cargo.toml`

`setrlimit` is POSIX. The new `rlimit.rs` module is cross-platform, so `libc` can't stay gated to `cfg(target_os = "linux")`.

- [ ] **Step 1: Edit `Cargo.toml`**

In `workers/prelude/Cargo.toml`, move `libc = "0.2"` from the Linux-cfg target table to the top-level `[dependencies]`. Find:

```toml
[dependencies]
hhagent-protocol = { path = "../../protocol" }
serde            = { workspace = true }
serde_json       = { workspace = true }
thiserror        = { workspace = true }

# Linux-only kernel containment helpers. On macOS these targets are skipped;
# `lock_down()` is a no-op there because Seatbelt enforces equivalent policy
# from the parent side (see sandbox::macos_seatbelt — Phase 0b).
[target.'cfg(target_os = "linux")'.dependencies]
landlock    = { workspace = true }
seccompiler = { workspace = true }
libc        = "0.2"
```

Replace with:

```toml
[dependencies]
hhagent-protocol = { path = "../../protocol" }
serde            = { workspace = true }
serde_json       = { workspace = true }
thiserror        = { workspace = true }
# libc gives us POSIX setrlimit on both Linux and macOS. The Landlock +
# seccomp deps below remain Linux-only (the macOS prelude does no
# kernel containment of its own — Seatbelt does it from the parent).
libc             = "0.2"

# Linux-only kernel containment helpers. On macOS these targets are skipped;
# `lock_down()` is a no-op there because Seatbelt enforces equivalent policy
# from the parent side (see sandbox::macos_seatbelt — Phase 0b).
[target.'cfg(target_os = "linux")'.dependencies]
landlock    = { workspace = true }
seccompiler = { workspace = true }
```

- [ ] **Step 2: Build to verify nothing breaks**

```bash
source "$HOME/.cargo/env" && cargo build -p hhagent-worker-prelude 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 3: Commit**

```bash
git add workers/prelude/Cargo.toml && git commit -m "$(cat <<'EOF'
workers/prelude(deps): promote libc to top-level for cross-platform setrlimit

Moves libc = "0.2" out of the Linux-cfg target table so the upcoming
rlimit module can call libc::setrlimit on both Linux and macOS. The
Linux-only landlock + seccompiler deps stay gated where they are.

Prep for the workers/prelude/src/rlimit.rs module landing next.
EOF
)"
```

---

### Task 4: Pure helper `cpu_ms_to_seconds`

**Files:**
- Create: `workers/prelude/src/rlimit.rs`
- Modify: `workers/prelude/src/lib.rs` (add `pub mod rlimit;` declaration)

Start with the pure helper because it can be tested in total isolation from FFI and env reads.

- [ ] **Step 1: Create the new module file with the pure helper + tests**

Create `workers/prelude/src/rlimit.rs`:

```rust
//! Worker-side `setrlimit` enforcement for `policy.cpu_ms`.
//!
//! Cross-platform — `setrlimit` is POSIX, so the same code runs on
//! Linux and macOS. This module is the cross-platform companion to
//! [`crate::lock_down`] (which is Linux-only).
//!
//! ## How it composes with seccomp
//!
//! `apply_from_env` is called by [`crate::serve_stdio`] **before**
//! `lock_down`. Some future seccomp profiles may ban `prlimit64`; setting
//! the cap earlier guarantees the cap is in place before any syscall
//! restrictions land.
//!
//! ## Why `RLIMIT_CPU` and not cgroup CPU-seconds
//!
//! cgroup v2 has no direct "total CPU-seconds budget" primitive — its
//! CPU primitive is bandwidth (`CPUQuota=N%`). `RLIMIT_CPU` is the
//! natural enforcement for `policy.cpu_ms`. Resolution is integer
//! seconds (with `SIGXCPU` on soft, `SIGKILL` on hard); the worker has
//! no `SIGXCPU` handler installed so the soft hit terminates the
//! process immediately — equivalent to a clean kill.

use std::env;

/// Env var read by [`apply_from_env`]. Set by
/// `hhagent_core::tool_host::derive_lockdown_env` from
/// `policy.cpu_ms`.
pub const ENV_CPU_MS: &str = "HHAGENT_CPU_MS";

/// Status of the rlimit layer after [`apply_from_env`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlimitReport {
    /// `RLIMIT_CPU` set successfully at `cpu_seconds` (soft = hard).
    Applied { cpu_seconds: u64 },
    /// Env var was unset or `"0"`. No rlimit applied. The worker still
    /// runs but has no CPU-time ceiling beyond cgroup bandwidth (Linux)
    /// or whatever the parent supervisor enforces.
    Disabled,
}

/// Errors from [`apply_from_env`]. Both variants are fail-closed:
/// `serve_stdio` propagates them as `io::Error` and the worker exits
/// before serving any request.
#[derive(Debug, thiserror::Error)]
pub enum RlimitError {
    /// `HHAGENT_CPU_MS` was set but couldn't be parsed as `u64`.
    #[error("env {ENV_CPU_MS}: {0}")]
    Env(String),
    /// `setrlimit(RLIMIT_CPU, …)` returned a non-zero error code.
    #[error("setrlimit RLIMIT_CPU: {0}")]
    SetRlimit(String),
}

/// Convert a millisecond CPU budget to integer seconds for
/// `RLIMIT_CPU`. Ceiling division with a 1-second floor when `ms > 0`;
/// `ms == 0` → `0` (the "no rlimit" sentinel).
///
/// `RLIMIT_CPU`'s resolution is integer seconds, so any fractional
/// millisecond budget needs to be rounded *up* — rounding down would
/// effectively halve a 500 ms budget to 0. The 1-second floor ensures
/// even a 1 ms budget produces a meaningful kill (after at least one
/// second of CPU time, which is the kernel's resolution).
///
/// Saturates on overflow rather than panicking: a caller passing
/// `u64::MAX` gets back `u64::MAX`, not a runtime panic.
///
/// ```text
/// 0       → 0
/// 1       → 1
/// 999     → 1
/// 1000    → 1
/// 1001    → 2
/// 1999    → 2
/// 2000    → 2
/// u64::MAX → u64::MAX  (saturating)
/// ```
pub fn cpu_ms_to_seconds(ms: u64) -> u64 {
    if ms == 0 {
        return 0;
    }
    // (ms + 999) / 1000 with saturating add to defend against
    // u64::MAX + 999 overflow.
    ms.saturating_add(999) / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_ms_to_seconds_zero_yields_zero() {
        assert_eq!(cpu_ms_to_seconds(0), 0);
    }

    #[test]
    fn cpu_ms_to_seconds_one_yields_one() {
        // A 1 ms budget rounds up to the 1-second floor.
        assert_eq!(cpu_ms_to_seconds(1), 1);
    }

    #[test]
    fn cpu_ms_to_seconds_just_under_one_second_yields_one() {
        assert_eq!(cpu_ms_to_seconds(999), 1);
    }

    #[test]
    fn cpu_ms_to_seconds_exactly_one_second_yields_one() {
        assert_eq!(cpu_ms_to_seconds(1_000), 1);
    }

    #[test]
    fn cpu_ms_to_seconds_just_over_one_second_yields_two() {
        // 1001 ms rounds up to 2 s.
        assert_eq!(cpu_ms_to_seconds(1_001), 2);
    }

    #[test]
    fn cpu_ms_to_seconds_saturates_on_overflow() {
        // u64::MAX must not panic.
        assert_eq!(cpu_ms_to_seconds(u64::MAX), u64::MAX / 1_000);
    }
}
```

In `workers/prelude/src/lib.rs`, add a module declaration. Find the existing module declarations near the top:

```rust
#[cfg(target_os = "linux")]
pub mod landlock_lock;
#[cfg(target_os = "linux")]
pub mod seccomp_lock;
```

Add immediately below (cross-platform — NO `cfg` gate):

```rust
pub mod rlimit;
```

- [ ] **Step 2: Run the six new tests to confirm they pass (compile + green)**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-worker-prelude --lib rlimit:: 2>&1 | tail -10
```

Expected: 6 pass.

- [ ] **Step 3: Commit**

```bash
git add workers/prelude/src/rlimit.rs workers/prelude/src/lib.rs && \
git commit -m "$(cat <<'EOF'
workers/prelude(rlimit): pure helper cpu_ms_to_seconds

Adds the workers/prelude/src/rlimit.rs module with the pure helper
cpu_ms_to_seconds (ceiling-div with 1-second floor, saturating on
overflow). RlimitReport + RlimitError types added; apply_from_env will
land in Task 5.

The module is cross-platform (no cfg gate) since setrlimit is POSIX —
the macOS prelude path needs this too.

Prep for cpu_ms enforcement (issue #6).
EOF
)"
```

---

### Task 5: `apply_from_env` — env parse + `libc::setrlimit` FFI

**Files:**
- Modify: `workers/prelude/src/rlimit.rs`

- [ ] **Step 1: Write the three new tests at the end of the existing `tests` module in `workers/prelude/src/rlimit.rs`**

```rust
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Tests in this module mutate the process-wide env block, which
    /// cargo's per-binary test harness runs in parallel by default.
    /// Take this mutex while inside any `apply_from_env` test so two
    /// tests don't trample each other's `HHAGENT_CPU_MS` setting.
    ///
    /// Pattern lifted from `hhagent_tests_common::serial::serial_lock`.
    fn env_lock() -> MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        // unwrap_or_else handles the rare poisoned-mutex case: a test
        // that panics while holding the lock would otherwise abort
        // every subsequent test with a useless error.
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Helper: temporarily set HHAGENT_CPU_MS, run a closure, then
    /// restore the prior value. Returns the closure's value.
    ///
    /// Workspace is on Rust 2021 edition where `set_var` /
    /// `remove_var` are safe; the Mutex returned by `env_lock` is
    /// what makes them race-free within this binary.
    fn with_env_var<F, R>(value: Option<&str>, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = env_lock();
        let prior = std::env::var(ENV_CPU_MS).ok();
        match value {
            Some(v) => std::env::set_var(ENV_CPU_MS, v),
            None => std::env::remove_var(ENV_CPU_MS),
        }
        let out = f();
        match prior {
            Some(v) => std::env::set_var(ENV_CPU_MS, v),
            None => std::env::remove_var(ENV_CPU_MS),
        }
        out
    }

    #[test]
    fn apply_from_env_unset_returns_disabled() {
        let report = with_env_var(None, apply_from_env)
            .expect("apply_from_env must succeed when env is unset");
        assert_eq!(report, RlimitReport::Disabled);
    }

    #[test]
    fn apply_from_env_zero_returns_disabled() {
        let report = with_env_var(Some("0"), apply_from_env)
            .expect("apply_from_env must succeed when env is 0");
        assert_eq!(report, RlimitReport::Disabled);
    }

    #[test]
    fn apply_from_env_garbage_returns_env_error() {
        let err = with_env_var(Some("not-a-number"), apply_from_env)
            .expect_err("apply_from_env must reject garbage");
        match err {
            RlimitError::Env(_) => {}
            other => panic!("expected RlimitError::Env, got {other:?}"),
        }
    }

    /// Happy path: a generous CPU budget gets applied without error.
    /// The kernel returns success regardless of whether the worker
    /// ever uses any CPU, so this only proves the FFI path is wired.
    /// Effective enforcement is covered by `rlimit_smoke.rs`.
    #[test]
    fn apply_from_env_with_generous_budget_applies() {
        // 30 seconds; well above anything this test itself would use.
        let report = with_env_var(Some("30000"), apply_from_env)
            .expect("apply_from_env must succeed with a generous budget");
        match report {
            RlimitReport::Applied { cpu_seconds } => assert_eq!(cpu_seconds, 30),
            RlimitReport::Disabled => panic!("expected Applied, got Disabled"),
        }
    }
```

- [ ] **Step 2: Run them to verify they fail**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-worker-prelude --lib rlimit::tests::apply_from_env 2>&1 | tail -15
```

Expected: compile error — `apply_from_env` is not defined.

- [ ] **Step 3: Implement `apply_from_env`**

Add to `workers/prelude/src/rlimit.rs`, immediately after `cpu_ms_to_seconds`:

```rust
/// Read `HHAGENT_CPU_MS` and apply `RLIMIT_CPU` if set and non-zero.
///
/// Returns `Disabled` if the env var is unset, empty, or `"0"`. Returns
/// an error if the value is set but not parseable as `u64`, or if
/// `setrlimit` itself fails (rare — `EPERM` only when the soft limit
/// would exceed the hard limit, which can't happen here since we set
/// them equal).
///
/// Called by [`crate::serve_stdio`] before [`crate::lock_down`].
pub fn apply_from_env() -> Result<RlimitReport, RlimitError> {
    let raw = match env::var(ENV_CPU_MS) {
        Ok(s) if s.is_empty() => return Ok(RlimitReport::Disabled),
        Ok(s) => s,
        Err(env::VarError::NotPresent) => return Ok(RlimitReport::Disabled),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(RlimitError::Env("value is not valid UTF-8".into()));
        }
    };

    let ms: u64 = raw
        .parse()
        .map_err(|e| RlimitError::Env(format!("parse {raw:?} as u64: {e}")))?;
    let cpu_seconds = cpu_ms_to_seconds(ms);

    if cpu_seconds == 0 {
        return Ok(RlimitReport::Disabled);
    }

    apply_cpu_seconds(cpu_seconds).map(|()| RlimitReport::Applied { cpu_seconds })
}

/// Call `setrlimit(RLIMIT_CPU, { rlim_cur, rlim_max } = (cpu_seconds, cpu_seconds))`.
///
/// Setting soft == hard means the kernel sends `SIGXCPU` and (since the
/// worker has no handler) the process terminates immediately at the
/// soft limit. This is the cleanest kill semantics RLIMIT_CPU offers.
fn apply_cpu_seconds(cpu_seconds: u64) -> Result<(), RlimitError> {
    // libc's rlim_t is u64 on glibc/musl Linux and u64 on macOS — both
    // accept our u64 input directly. The cast is explicit so a future
    // platform with a narrower rlim_t fails loudly at the type layer.
    let lim = libc::rlimit {
        rlim_cur: cpu_seconds as libc::rlim_t,
        rlim_max: cpu_seconds as libc::rlim_t,
    };
    // SAFETY: setrlimit takes a resource id (immediate) and a pointer
    // to a stack-local rlimit struct; the struct lives for the entire
    // duration of the call. Failure mode is a -1 return + errno set.
    let rc = unsafe { libc::setrlimit(libc::RLIMIT_CPU, &lim) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(RlimitError::SetRlimit(err.to_string()));
    }
    Ok(())
}
```

- [ ] **Step 4: Run the four new tests to confirm green**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-worker-prelude --lib rlimit::tests::apply_from_env 2>&1 | tail -10
```

Expected: 4 pass. Note `apply_from_env_with_generous_budget_applies` actually calls `setrlimit` on the test process — that's fine; 30 s is more than the test suite uses, and `setrlimit(RLIMIT_CPU)` doesn't reset on test-process exit (process-scoped).

- [ ] **Step 5: Run the full prelude unit suite to confirm no regression**

```bash
source "$HOME/.cargo/env" && cargo test -p hhagent-worker-prelude --lib 2>&1 | tail -10
```

Expected: ~21 tests pass (11 existing + 10 new for rlimit), zero warnings.

- [ ] **Step 6: Commit**

```bash
git add workers/prelude/src/rlimit.rs && git commit -m "$(cat <<'EOF'
workers/prelude(rlimit): apply_from_env reads HHAGENT_CPU_MS + setrlimit

apply_from_env reads HHAGENT_CPU_MS, parses to u64 milliseconds, converts
via cpu_ms_to_seconds (ceiling-div with 1-second floor), and calls
libc::setrlimit(RLIMIT_CPU, …) with soft = hard. Failure modes:
- unset / "0" / empty → Disabled (no rlimit applied)
- non-u64 garbage → Err(Env)
- setrlimit FFI returns -1 → Err(SetRlimit) carrying errno

Setting soft = hard means SIGXCPU at the soft limit terminates the worker
immediately (no handler installed); the cleanest kill semantics RLIMIT_CPU
offers.

Cross-platform: setrlimit is POSIX, so this path runs on both Linux and
macOS. The Linux-only lock_down (Landlock + seccomp) composes on top in
Task 6.

Closes part of issue #6 (cross-platform cpu_ms enforcement).
EOF
)"
```

---

### Task 6: Restructure `LockdownReport` + compose `rlimit` into `serve_stdio`

**Files:**
- Modify: `workers/prelude/src/lib.rs` (`LockdownReport` enum, `lock_down`, `serve_stdio`)

The `SkippedNonLinux` variant becomes `NonLinux { rlimit }`; the `Linux` variant gains an `rlimit` field. `lock_down` itself stays single-purpose (Landlock + seccomp only) — `serve_stdio` composes the two layers.

- [ ] **Step 1: Read the current `lock_down` + `serve_stdio` for context**

```bash
sed -n '46,156p' /home/hherb/src/hhagent/workers/prelude/src/lib.rs
```

Expected: shows `LockdownReport`, `lock_down`, `serve_stdio` definitions.

- [ ] **Step 2: Restructure `LockdownReport`, extend `lock_down`, and rewire `serve_stdio`**

In `workers/prelude/src/lib.rs`, replace:

```rust
/// What `lock_down` actually managed to install. Returned so the worker can
/// log it (and tests can assert on it).
#[derive(Debug)]
pub enum LockdownReport {
    /// Both layers installed and enforcing.
    Linux {
        landlock: LandlockReport,
        seccomp: SeccompReport,
    },
    /// Non-Linux target — both layers are no-ops here. Containment is the
    /// parent's job (Seatbelt).
    SkippedNonLinux,
}
```

With:

```rust
/// What `serve_stdio` actually managed to install. Returned so the
/// worker can log it (and tests can assert on it).
///
/// Two-layer composition: `rlimit::apply_from_env` (cross-platform,
/// POSIX `setrlimit`) plus `lock_down` (Linux Landlock + seccomp;
/// no-op on macOS, where Seatbelt enforces containment from the parent
/// side). `rlimit` runs *before* `lock_down` so the CPU ceiling is
/// armed before any seccomp restrictions on `prlimit`-family syscalls.
#[derive(Debug)]
pub enum LockdownReport {
    /// Linux: Landlock + seccomp + rlimit.
    Linux {
        landlock: LandlockReport,
        seccomp: SeccompReport,
        rlimit: rlimit::RlimitReport,
    },
    /// macOS or other non-Linux: kernel containment is the parent's
    /// job (Seatbelt), but rlimit still applies (POSIX).
    NonLinux {
        rlimit: rlimit::RlimitReport,
    },
}
```

Then replace the existing `lock_down` body (which currently returns the old `LockdownReport`):

```rust
/// Apply both kernel layers, in order: Landlock first (it's a one-way FS
/// restriction), then seccomp (one-way syscall restriction).
///
/// Reads its policy from environment variables set by the parent process
/// (`core::tool_host`):
///
///   * `HHAGENT_LANDLOCK_RW`  — JSON array of absolute paths the worker may
///     write to (its scratch dir). Read-only access to `/usr`, `/lib*`,
///     `/etc/ld.so.cache` is implicit so dynamic-linker + libc still work.
///   * `HHAGENT_SECCOMP_PROFILE` — `"strict"`, `"net_client"`, or `"none"`.
///     `"none"` disables seccomp entirely (used in tests).
///
/// The function only fails on programmer error (malformed env, kernel ABI
/// returns an error). A kernel that lacks Landlock support is reported via
/// [`LandlockReport::KernelTooOld`], not via an error — callers should still
/// proceed, since bwrap is the primary containment layer.
///
/// **Does not apply `setrlimit`.** That's [`rlimit::apply_from_env`]'s job;
/// [`serve_stdio`] composes the two. Callers using `lock_down` directly
/// (e.g. the `lockdown-probe` binary) are responsible for invoking
/// `rlimit::apply_from_env` themselves if they want CPU-time enforcement.
/// The returned `LockdownReport` carries `rlimit: RlimitReport::Disabled`
/// from this entry point.
pub fn lock_down() -> Result<LockdownReport, LockdownError> {
    #[cfg(target_os = "linux")]
    {
        let landlock = landlock_lock::apply_from_env()?;
        let seccomp = seccomp_lock::apply_from_env()?;
        Ok(LockdownReport::Linux {
            landlock,
            seccomp,
            rlimit: rlimit::RlimitReport::Disabled,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(LockdownReport::NonLinux {
            rlimit: rlimit::RlimitReport::Disabled,
        })
    }
}
```

And replace `serve_stdio`:

```rust
/// Drop-in replacement for `hhagent_protocol::server::serve_stdio` that
/// applies `rlimit::apply_from_env` and [`lock_down`] before entering
/// the request loop. This is the recommended entry point for tool
/// workers.
///
/// Order matters:
///
/// 1. **`rlimit::apply_from_env` first.** Sets `RLIMIT_CPU` before any
///    syscall restrictions land — defends against future seccomp profiles
///    that ban `prlimit64`. Cross-platform (POSIX).
/// 2. **`lock_down` second.** Linux Landlock + seccomp; no-op on macOS.
///
/// Both layers fail closed: any error returns `io::Error` and the worker
/// exits before serving any request.
pub fn serve_stdio<H: Handler>(handler: &mut H) -> io::Result<()> {
    let rlimit = rlimit::apply_from_env()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let report = match lock_down() {
        Ok(LockdownReport::Linux {
            landlock, seccomp, ..
        }) => LockdownReport::Linux {
            landlock,
            seccomp,
            rlimit,
        },
        Ok(LockdownReport::NonLinux { .. }) => LockdownReport::NonLinux { rlimit },
        Err(e) => {
            return Err(io::Error::new(io::ErrorKind::Other, e.to_string()));
        }
    };

    // Single, structured line on stderr so the parent can capture it
    // for the audit log without parsing JSON. Workers that want
    // richer logging can call `rlimit::apply_from_env` + `lock_down`
    // themselves and skip this.
    eprintln!("hhagent-worker-prelude: lockdown {report:?}");

    hhagent_protocol::server::serve_stdio(handler)
}
```

- [ ] **Step 3: Build the workspace to surface any callers that match on `SkippedNonLinux`**

```bash
source "$HOME/.cargo/env" && cargo build --workspace 2>&1 | tail -20
```

Expected: clean build. Grep confirms no other caller pattern-matches on the old variant — `serve_stdio`'s `eprintln!` uses the auto-derived `Debug` impl.

- [ ] **Step 4: Run the prelude unit suite + sandbox suite to verify no regression**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-worker-prelude --lib 2>&1 | tail -5 && \
  cargo test -p hhagent-sandbox --lib 2>&1 | tail -5
```

Expected: all pass; zero warnings.

- [ ] **Step 5: Commit**

```bash
git add workers/prelude/src/lib.rs && git commit -m "$(cat <<'EOF'
workers/prelude: compose rlimit + lock_down in serve_stdio

Restructures LockdownReport so both Linux and NonLinux variants carry
an rlimit field (RlimitReport). serve_stdio now calls
rlimit::apply_from_env first (cross-platform POSIX setrlimit), then
lock_down (Linux Landlock + seccomp; no-op on macOS), then assembles
the final report for the audit-line eprintln.

lock_down itself stays single-purpose: Landlock + seccomp only.
External callers (e.g. lockdown-probe) get LockdownReport::*::{rlimit:
Disabled} as the lock_down return, since rlimit is serve_stdio's
composition layer.

Closes part of issue #6 (worker-side cpu_ms enforcement composed
into the standard prelude entry point).
EOF
)"
```

---

### Task 7: Add `HHAGENT_CPU_MS` to `derive_lockdown_env`

**Files:**
- Modify: `core/src/tool_host.rs` (const, function, tests)

- [ ] **Step 1: Write the two new failing tests**

In `core/src/tool_host.rs`'s `#[cfg(test)] mod tests`, add after `derive_does_not_overwrite_caller_supplied_env`:

```rust
    #[test]
    fn derive_adds_cpu_ms_env_when_policy_sets_it() {
        let mut p = base_policy();
        p.cpu_ms = 2_500;
        let derived = derive_lockdown_env(&p);
        let cpu_ms_entry = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_CPU_MS)
            .expect("cpu_ms env must be derived when policy.cpu_ms > 0");
        assert_eq!(cpu_ms_entry.1, "2500");
    }

    #[test]
    fn derive_omits_cpu_ms_env_when_policy_is_zero() {
        // policy.cpu_ms == 0 is the "no rlimit" sentinel (matches how
        // policy.mem_mb == 0 means "omit MemoryMax" in linux_cgroup).
        // The worker prelude reads "unset" as Disabled, so omitting the
        // env is the right wire signal.
        let mut p = base_policy();
        p.cpu_ms = 0;
        let derived = derive_lockdown_env(&p);
        assert!(
            !derived.env.iter().any(|(k, _)| k == ENV_CPU_MS),
            "ENV_CPU_MS must be omitted when policy.cpu_ms == 0; env was {:?}",
            derived.env
        );
    }
```

- [ ] **Step 2: Run them to verify they fail**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-core --lib derive_ 2>&1 | tail -10
```

Expected: compile error — `ENV_CPU_MS` is not defined.

- [ ] **Step 3: Add the const and extend the function**

In `core/src/tool_host.rs`, find the existing `ENV_LANDLOCK_RW` / `ENV_SECCOMP_PROFILE` const block (around line 222–228) and add immediately after:

```rust
/// Env var name read by `hhagent-worker-prelude::rlimit` for the
/// `policy.cpu_ms` budget. Plumbed cross-platform — applied via
/// `setrlimit(RLIMIT_CPU)` from the worker prelude before lock-down.
/// Omitted (not set to `"0"`) when `policy.cpu_ms == 0` so the prelude
/// can treat "unset" as the canonical `Disabled` signal.
pub const ENV_CPU_MS: &str = "HHAGENT_CPU_MS";
```

Then find `derive_lockdown_env` and extend it. Replace:

```rust
pub fn derive_lockdown_env(policy: &SandboxPolicy) -> SandboxPolicy {
    let mut out = policy.clone();
    let has_landlock = out.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RW);
    let has_seccomp = out.env.iter().any(|(k, _)| k == ENV_SECCOMP_PROFILE);

    if !has_landlock {
        let rw_paths: Vec<String> = out
            .fs_write
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // serde_json on a Vec<String> is infallible — `unwrap` is safe here.
        let json = serde_json::to_string(&rw_paths).unwrap();
        out.env.push((ENV_LANDLOCK_RW.into(), json));
    }
    if !has_seccomp {
        let value = match out.profile {
            Profile::WorkerStrict => "strict",
            Profile::WorkerNetClient => "net_client",
        };
        out.env.push((ENV_SECCOMP_PROFILE.into(), value.into()));
    }
    out
}
```

With:

```rust
pub fn derive_lockdown_env(policy: &SandboxPolicy) -> SandboxPolicy {
    let mut out = policy.clone();
    let has_landlock = out.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RW);
    let has_seccomp = out.env.iter().any(|(k, _)| k == ENV_SECCOMP_PROFILE);
    let has_cpu_ms = out.env.iter().any(|(k, _)| k == ENV_CPU_MS);

    if !has_landlock {
        let rw_paths: Vec<String> = out
            .fs_write
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // serde_json on a Vec<String> is infallible — `unwrap` is safe here.
        let json = serde_json::to_string(&rw_paths).unwrap();
        out.env.push((ENV_LANDLOCK_RW.into(), json));
    }
    if !has_seccomp {
        let value = match out.profile {
            Profile::WorkerStrict => "strict",
            Profile::WorkerNetClient => "net_client",
        };
        out.env.push((ENV_SECCOMP_PROFILE.into(), value.into()));
    }
    // cpu_ms == 0 means "policy didn't set it"; omit the env so the
    // prelude's apply_from_env sees no var and returns Disabled.
    if !has_cpu_ms && policy.cpu_ms > 0 {
        out.env.push((ENV_CPU_MS.into(), policy.cpu_ms.to_string()));
    }
    out
}
```

- [ ] **Step 4: Run the new tests + extend an existing fixture-shape test**

```bash
source "$HOME/.cargo/env" && \
  cargo test -p hhagent-core --lib derive_ 2>&1 | tail -10
```

Expected: 6 pass (4 existing + 2 new). All `derive_*` tests green.

- [ ] **Step 5: Commit**

```bash
git add core/src/tool_host.rs && git commit -m "$(cat <<'EOF'
core(tool_host): plumb HHAGENT_CPU_MS via derive_lockdown_env

derive_lockdown_env now appends HHAGENT_CPU_MS = policy.cpu_ms when
policy.cpu_ms > 0 (omitted when 0 so the prelude's apply_from_env sees
"unset" and returns Disabled — the canonical "no rlimit" signal).

Symmetric with the existing HHAGENT_LANDLOCK_RW + HHAGENT_SECCOMP_PROFILE
plumbing: same caller-supplied-wins pattern, same chokepoint property
(spawn_worker always calls derive_lockdown_env first).

The worker prelude's rlimit::apply_from_env shipped in Task 5 reads
this env at worker start-up.

Closes the env-plumbing side of issue #6.
EOF
)"
```

---

### Task 8: Add `cpu-burner` subcommand to `lockdown-probe`

**Files:**
- Modify: `workers/prelude/src/bin/lockdown_probe.rs`

The probe binary applies `rlimit::apply_from_env` at the same top-of-main location as `lock_down`, so the new `cpu-burner` subcommand can do its busy loop after both layers are armed.

- [ ] **Step 1: Edit `lockdown_probe.rs` to apply rlimit + add the subcommand**

In `workers/prelude/src/bin/lockdown_probe.rs`:

a) Update the doc comment in the file header to describe the new subcommand and the rlimit step. After the existing `exec-after-lockdown` block, append:

```rust
//! lockdown-probe cpu-burner
//!     Call rlimit::apply_from_env() and lock_down(), then enter a
//!     CPU-bound busy loop. If HHAGENT_CPU_MS was set, the kernel kills
//!     the process via SIGXCPU/SIGKILL within `cpu_seconds`. Used by
//!     `rlimit_smoke.rs` to verify worker-side cpu_ms enforcement.
//!     Exits 0 if the loop runs for > 10 wall-clock seconds (the test
//!     interprets that as "rlimit failed to apply").
```

b) In `main`, after the existing `lock_down()` call (around line 67 with `eprintln!("LOCKDOWN_REPORT: {report:?}");`), insert the rlimit application. Replace this section:

```rust
    let report = match hhagent_worker_prelude::lock_down() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("LOCKDOWN_ERROR: {e}");
            return ExitCode::from(70);
        }
    };
    eprintln!("LOCKDOWN_REPORT: {report:?}");
```

With:

```rust
    // Apply rlimit first, matching serve_stdio's order. Cross-platform.
    let rlimit_report = match hhagent_worker_prelude::rlimit::apply_from_env() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("RLIMIT_ERROR: {e}");
            return ExitCode::from(72);
        }
    };
    eprintln!("RLIMIT_REPORT: {rlimit_report:?}");

    let report = match hhagent_worker_prelude::lock_down() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("LOCKDOWN_ERROR: {e}");
            return ExitCode::from(70);
        }
    };
    eprintln!("LOCKDOWN_REPORT: {report:?}");
```

c) Add the `cpu-burner` subcommand to the dispatch match. Insert into the `match args[0].as_str()` block — make it cross-platform (no `cfg` gate):

```rust
        "cpu-burner" => probe_cpu_burner(),
```

d) Add the helper at the end of the file:

```rust
/// Busy-loop on CPU until either:
///   * the kernel kills us via SIGXCPU/SIGKILL (the rlimit fired), or
///   * 10 wall-clock seconds elapse (rlimit didn't fire — test failure).
///
/// Used by `rlimit_smoke.rs` to verify the worker-side rlimit layer
/// actually enforces the CPU budget the parent encoded in
/// HHAGENT_CPU_MS. Volatile reads + writes defend against the loop
/// being optimised away under release builds.
fn probe_cpu_burner() -> ExitCode {
    use std::time::Instant;
    let start = Instant::now();
    let mut counter: u64 = 0;
    // Wall-clock cap is generous — 10s gives a 200 ms cpu_ms budget at
    // least ~50x headroom to fire SIGXCPU even on a deeply contended
    // host. If we reach the cap we exit 0, which the test treats as
    // failure (the test expects to be killed by signal).
    while start.elapsed().as_secs() < 10 {
        // `read_volatile` + `write_volatile` keep the loop alive under
        // release optimisations.
        let prev = unsafe { std::ptr::read_volatile(&counter) };
        unsafe { std::ptr::write_volatile(&mut counter, prev.wrapping_add(1)) };
    }
    eprintln!("cpu-burner: hit 10s wall-clock cap, counter={counter}");
    ExitCode::from(0)
}
```

- [ ] **Step 2: Build the probe binary to confirm it compiles**

```bash
source "$HOME/.cargo/env" && cargo build --bin hhagent-lockdown-probe 2>&1 | tail -5
```

Expected: clean build.

- [ ] **Step 3: Smoke-test the new subcommand by hand**

```bash
HHAGENT_CPU_MS=200 ./target/debug/hhagent-lockdown-probe cpu-burner ; echo "exit=$?"
```

Expected: process exits via signal (exit code 137 = SIGKILL after SIGXCPU, or 152 = SIGXCPU directly, depending on libc + handler), in well under 10 seconds. Look for the `RLIMIT_REPORT: Applied { cpu_seconds: 1 }` line on stderr.

Note: the exit code shape can vary by shell — `bash` reports `128+signum` for signal deaths. Either of `128+9=137` (SIGKILL) or `128+24=152` (SIGXCPU) is acceptable.

- [ ] **Step 4: Verify the no-env baseline runs unbounded**

```bash
( ./target/debug/hhagent-lockdown-probe cpu-burner ) & sleep 2 ; kill $! 2>/dev/null ; wait $! 2>/dev/null ; echo done
```

Expected: process is still alive after 2 seconds when we kill it (because `HHAGENT_CPU_MS` was unset → `RLIMIT_REPORT: Disabled` → loop runs unmolested until our `kill`). The `done` message prints.

- [ ] **Step 5: Run the full prelude suite to confirm no regression**

```bash
source "$HOME/.cargo/env" && cargo test -p hhagent-worker-prelude 2>&1 | tail -10
```

Expected: all green; zero warnings.

- [ ] **Step 6: Commit**

```bash
git add workers/prelude/src/bin/lockdown_probe.rs && git commit -m "$(cat <<'EOF'
workers/prelude(probe): add cpu-burner subcommand + apply rlimit at top

The probe binary now applies rlimit::apply_from_env at the top alongside
lock_down (same order as serve_stdio). New cpu-burner subcommand enters
a CPU-bound busy loop with a 10 s wall-clock safety cap; if rlimit::
apply_from_env actually applied an RLIMIT_CPU budget via HHAGENT_CPU_MS,
the kernel kills the process via SIGXCPU/SIGKILL before the cap hits.

Loop uses ptr::{read_volatile, write_volatile} so the compiler can't
optimise the work away under release builds.

Prep for rlimit_smoke.rs integration test landing in Task 9.
EOF
)"
```

---

### Task 9: `rlimit_smoke.rs` cross-platform integration test

**Files:**
- Create: `workers/prelude/tests/rlimit_smoke.rs`

Run the `lockdown-probe cpu-burner` binary as a subprocess with `HHAGENT_CPU_MS=200`, verify it's killed by signal in well under 10 seconds. Mirrors the pattern of `seccomp_smoke.rs` and `landlock_smoke.rs`.

- [ ] **Step 1: Create the new test file**

Create `workers/prelude/tests/rlimit_smoke.rs`:

```rust
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
```

- [ ] **Step 2: Build the workspace (so the probe binary exists) and run the smoke test**

```bash
source "$HOME/.cargo/env" && \
  cargo build -p hhagent-worker-prelude --bin hhagent-lockdown-probe 2>&1 | tail -3 && \
  cargo test -p hhagent-worker-prelude --test rlimit_smoke 2>&1 | tail -15
```

Expected: 2 tests pass. First test should complete in ~1–3 seconds; second in ~2 seconds.

- [ ] **Step 3: Run the full workspace test suite to confirm zero regressions**

```bash
source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -20
```

Expected: 429 → ~447 tests pass, 0 fail, 0 SKIP (on Linux with the AppArmor profile installed), 0 warnings.

If the count is lower or higher than expected, investigate before committing — a test-count anomaly could mean a test silently skipped or an unintended new test got added.

- [ ] **Step 4: Commit**

```bash
git add workers/prelude/tests/rlimit_smoke.rs && git commit -m "$(cat <<'EOF'
workers/prelude(test): rlimit_smoke cross-platform integration test

Two tests pin the worker-side cpu_ms enforcement end-to-end:

1. cpu_burner_under_short_budget_is_killed_promptly — spawns the
   probe binary with HHAGENT_CPU_MS=200 and asserts the process is
   killed by signal within 8 wall-clock seconds (status.code() is None).
   Defends against "rlimit was not applied at all" regression where
   the burner would run to its own 10s safety cap.

2. cpu_burner_with_no_env_runs_past_one_second — positive control;
   spawns without HHAGENT_CPU_MS and asserts the process is still
   alive after 2 seconds. Defends against a regression where
   apply_from_env silently applies a default cap.

Cross-platform: setrlimit(RLIMIT_CPU) is POSIX. The test runs
unchanged on Linux and macOS.

Closes the integration-test side of issue #6.
EOF
)"
```

---

### Task 10: HANDOVER + ROADMAP update + final workspace verification

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

End-of-session bookkeeping per CLAUDE.md rule #8 and the HANDOVER convention.

- [ ] **Step 1: Get final test count + commit hash for the HANDOVER**

```bash
source "$HOME/.cargo/env" && \
  cargo test --workspace 2>&1 | grep -E "test result|^running" | tail -20 && \
  echo "---" && git log --oneline main..HEAD
```

Expected: full passing count (~447), zero failures, zero `[SKIP]` lines, zero warnings; 10 new commits on the branch (one per task).

- [ ] **Step 2: Update HANDOVER.md**

Edit `docs/devel/handovers/HANDOVER.md`:

a) Bump the header `Last updated`, `Last commit (main)` (stays at `25c312c` since main hasn't moved), `This session's working branch` to `feat/sandbox-cpu-rlimit-quota`.

b) Insert a new "Recently completed (this session, 2026-05-14 — Option G: cpu_quota_pct + tasks_max + setrlimit cpu_ms, branch `feat/sandbox-cpu-rlimit-quota`)" section right after the existing top header block. Include:

- Why this slice now: previous session shipped the `Default for SandboxPolicy` prereq specifically so this could land zero-churn.
- The shipped shape: 2 new policy fields, `linux_cgroup` wiring, cross-platform `rlimit` module, `serve_stdio` composition, `derive_lockdown_env` env plumbing, `lockdown-probe cpu-burner` subcommand, `rlimit_smoke.rs` integration test.
- What this slice deliberately does NOT do: `RLIMIT_AS` for memory (false-positive risk; deferred to micro-VM via issue #55), macOS Seatbelt CPU bandwidth (no usable primitive), per-profile defaults.
- Test count delta: 429 → ~447 (+18: 1 sandbox lib + 4 cgroup unit + 6 prelude cpu_ms_to_seconds + 4 prelude apply_from_env + 2 core derive_lockdown_env + 2 rlimit_smoke integration; verify with the actual cargo output and update the number if it's different).
- Files affected: 7 (the table from the spec).
- Issue #6 fully closed; issue #55 (macOS micro-VM spike) filed as the next macOS-direction follow-up.

c) Update the "Working state" `cargo test --workspace` line to show the new count.

d) Move the "Option G" item from "Next TODO" → reference the new "Recently completed" entry.

e) Add a new "Next TODO (pick one)" line about issue #55 (macOS micro-VM spike) as the natural macOS-direction follow-up; preserve the other items.

f) Update the "Open follow-up issues" table: mark issue #6 as closed by this session, link issue #55 as the new spike follow-up.

- [ ] **Step 3: Update ROADMAP.md**

In `docs/devel/ROADMAP.md`, find the "Phase 0 hardening" section's cgroup line (around line 38) — append:

```
- [x] **Issue #6 main body — policy-driven `cpu_quota_pct` / `tasks_max` + `setrlimit(RLIMIT_CPU)`-based `cpu_ms` enforcement** — landed 2026-05-14 on branch `feat/sandbox-cpu-rlimit-quota`. Two new `SandboxPolicy` fields (both `Option`, defaulted None) drive `build_systemd_run_argv`'s `CPUQuota` and `TasksMax` properties when set; otherwise fall back to the existing 200% / 64 defense-in-depth defaults. New cross-platform `workers/prelude/src/rlimit.rs` module reads `HHAGENT_CPU_MS` (set by `tool_host::derive_lockdown_env` from `policy.cpu_ms`) and applies `RLIMIT_CPU` (soft = hard for clean kill) before lock-down. Cross-platform via POSIX; the same code path runs on Linux and macOS. Test count: 429 → ~447 (+18). Closes issue #6.
```

If the section structure differs, find the equivalent right place — the cgroup CPU/memory caps line is the parent context.

- [ ] **Step 4: Commit HANDOVER + ROADMAP**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md && \
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): Option G shipped — sandbox CPU rlimit + cgroup quota

Closes issue #6 main body. Two new SandboxPolicy fields (cpu_quota_pct,
tasks_max) drive the cgroup ceilings on Linux when set, falling back to
the existing defense-in-depth defaults (200%, 64) when None. Cross-
platform setrlimit(RLIMIT_CPU) enforcement for policy.cpu_ms shipped via
new workers/prelude/src/rlimit.rs module, composed into serve_stdio
alongside lock_down.

Test count: 429 → ~447 (+18: see HANDOVER entry for the breakdown).

Next macOS direction: issue #55 — Apple `container` micro-VM spike.
EOF
)"
```

- [ ] **Step 5: Final verification — sanity-check the branch is ready**

```bash
git log --oneline main..HEAD && echo "---" && git status
```

Expected: 10–11 commits on the branch, clean working tree.

---

## Self-review

Spec coverage:
- `SandboxPolicy` field additions → Task 1.
- Linux cgroup wiring → Task 2.
- `libc` top-level dep → Task 3.
- `cpu_ms_to_seconds` pure helper → Task 4.
- `apply_from_env` + `setrlimit` FFI → Task 5.
- `LockdownReport` restructure + `serve_stdio` composition → Task 6.
- `ENV_CPU_MS` const + `derive_lockdown_env` extension → Task 7.
- `lockdown-probe cpu-burner` subcommand → Task 8.
- `rlimit_smoke.rs` integration → Task 9.
- HANDOVER / ROADMAP update → Task 10.

Placeholder scan: no TBD / TODO / "implement later" / "etc.". Every step has the full code or command.

Type consistency: `RlimitReport::Applied { cpu_seconds }` shape consistent across Tasks 4, 5, 6, 8. `RlimitError::{Env, SetRlimit}` consistent. `LockdownReport::{Linux { …, rlimit }, NonLinux { rlimit }}` shape used identically in Tasks 6 and 8. `cpu_ms_to_seconds(0) → 0` sentinel consistent across Tasks 4 and 5. `apply_from_env` returns `Result<RlimitReport, RlimitError>` consistently.

Scope check: single PR-sized, focused on one issue (#6 main body) plus the `Default` field-shape prereq from the previous session. No scope creep into macOS micro-VM (filed as #55), no Seatbelt CPU-quota fold-in, no RLIMIT_AS memory enforcement.

---

## TDD discipline reminders

- Every task's first step is the **failing test** with the exact red-confirmation command.
- Implementation comes only after the red is confirmed.
- The commit is the **last step** of each task — no batching across tasks.
- File-size soft cap (500 LOC) — `workers/prelude/src/rlimit.rs` lands at ~150 LOC including tests; `workers/prelude/tests/rlimit_smoke.rs` at ~100 LOC. Both well under.
