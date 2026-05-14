# Design: sandbox CPU/tasks quota policy fields + `setrlimit(RLIMIT_CPU)` enforcement

**Status:** approved 2026-05-14 — supersedes the "Option G" sketch in `HANDOVER.md`.
**Closes (in part):** [issue #6](https://github.com/hherb/hhagent/issues/6) main body.
**Prereq landed:** `Default for SandboxPolicy` (previous session, PR #54).

## Background

`sandbox/src/linux_cgroup.rs` wraps every Linux worker in `systemd-run --user --scope`
with three cgroup ceilings:

- `MemoryMax=policy.mem_mb M` — **policy-driven**, paired with `MemorySwapMax=0`.
- `CPUQuota=200%` — **hardcoded** defense-in-depth default (at most 2 CPUs).
- `TasksMax=64` — **hardcoded** defense-in-depth default (fork-bomb resistance).

`SandboxPolicy.cpu_ms` is documented as a CPU-time budget but is currently
unenforced by any backend. cgroup v2 has no direct primitive for total
CPU-seconds budget (its CPU primitive is bandwidth, not budget); the natural
enforcement is `setrlimit(RLIMIT_CPU)` from the worker prelude before `exec(2)`.

The previous session shipped `impl Default for SandboxPolicy` precisely so this
slice can add fields without churning every test fixture.

## Goals

1. Make the two cgroup defense-in-depth ceilings policy-driven (with the
   current hardcoded values as fallbacks).
2. Enforce `policy.cpu_ms` cross-platform via `setrlimit(RLIMIT_CPU)` from the
   worker prelude, before lock-down.
3. Keep the public surface backwards-compatible: every existing fixture using
   `..SandboxPolicy::default()` continues to compile and behave identically.

## Non-goals

- macOS memory enforcement via `RLIMIT_AS`. Counts virtual address space, not
  RSS; high false-positive risk for malloc-heavy workers (e.g. Python).
  Deferred to the future micro-VM backend.
- macOS Seatbelt CPU-bandwidth equivalent. No usable primitive.
- Per-profile-class default ceilings (different defaults per
  `Profile::WorkerStrict` vs `WorkerNetClient`). Separate concern.
- `RLIMIT_AS` (or any rlimit) for memory. Memory stays cgroup-only on Linux;
  macOS memory is out of scope this slice.

## Design

### `SandboxPolicy` shape

```rust
pub struct SandboxPolicy {
    pub fs_read: Vec<PathBuf>,
    pub fs_write: Vec<PathBuf>,
    pub net: Net,
    pub cpu_ms: u64,       // existing; now actually enforced
    pub mem_mb: u64,
    pub profile: Profile,
    /// NEW. Per-worker CPU bandwidth ceiling (percent of one CPU).
    /// `None` falls back to defense-in-depth default (200%). Linux cgroup only.
    #[serde(default)]
    pub cpu_quota_pct: Option<u32>,
    /// NEW. Per-worker max task count (cgroup pids.max).
    /// `None` falls back to defense-in-depth default (64). Linux cgroup only.
    #[serde(default)]
    pub tasks_max: Option<u64>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}
```

`Default::default()` leaves both new fields `None`. Every existing fixture
already uses `..SandboxPolicy::default()` after the previous session's hoist,
so this is a zero-churn field addition.

### Linux cgroup wiring

`sandbox/src/linux_cgroup.rs::build_systemd_run_argv` switches the two
hardcoded `format!()` literals to:

```rust
let cpu_quota_pct = policy.cpu_quota_pct.unwrap_or(DEFAULT_CPU_QUOTA_PCT);
let tasks_max    = policy.tasks_max.unwrap_or(DEFAULT_TASKS_MAX);
```

The named consts `DEFAULT_CPU_QUOTA_PCT = 200` and `DEFAULT_TASKS_MAX = 64`
stay so the audit trail (and the test pin "the default is 200%") survive.

The unit-test module gets four new pins:

- `cpu_quota_pct = None` → `CPUQuota=200%` in argv (default).
- `cpu_quota_pct = Some(50)` → `CPUQuota=50%` in argv (override).
- `tasks_max = None` → `TasksMax=64` in argv (default).
- `tasks_max = Some(8)` → `TasksMax=8` in argv (override).

### Worker-prelude rlimit module

New file `workers/prelude/src/rlimit.rs`. **Cross-platform** (no
`#[cfg(target_os = "linux")]` gate — `setrlimit` is POSIX and works on macOS
too). Public surface:

```rust
/// Status of the rlimit layer after `apply_from_env`.
#[derive(Debug, Clone, Copy)]
pub enum RlimitReport {
    /// `RLIMIT_CPU` applied at `cpu_seconds` (soft = hard).
    Applied { cpu_seconds: u64 },
    /// `HHAGENT_CPU_MS` was unset, or `"0"`. No rlimit applied.
    Disabled,
}

#[derive(Debug, thiserror::Error)]
pub enum RlimitError {
    #[error("env: {0}")]
    Env(String),
    #[error("setrlimit RLIMIT_CPU: {0}")]
    SetRlimit(String),
}

/// Pure: convert a millisecond budget to integer seconds for `RLIMIT_CPU`.
/// Ceiling-div with a 1-second floor when `ms > 0`; `ms == 0` → 0.
/// Saturates on overflow (no panic for `u64::MAX`).
pub fn cpu_ms_to_seconds(ms: u64) -> u64;

/// Read `HHAGENT_CPU_MS` and apply `RLIMIT_CPU` if set and non-zero.
pub fn apply_from_env() -> Result<RlimitReport, RlimitError>;
```

`RLIMIT_CPU` semantics: the kernel sends `SIGXCPU` when soft limit is reached,
then `SIGKILL` shortly after when hard limit is reached. We set `soft = hard`
so the kill is clean — `SIGXCPU` is not catchable on workers because the
worker has no signal handler installed for it, so the default action (process
termination) fires immediately.

Setting `setrlimit` failure (`EPERM`) is fail-closed: `apply_from_env` returns
`Err(RlimitError::SetRlimit)`, and `serve_stdio` propagates that as an
`io::Error` exactly as it already does for `lock_down` failures. **No** silent
degradation — `cpu_ms` is a security ceiling, not a hint.

### Cross-platform `libc` dep

`workers/prelude/Cargo.toml` promotes `libc = "0.2"` from
`[target.'cfg(target_os = "linux")'.dependencies]` to top-level
`[dependencies]`. The Linux-only `landlock` + `seccompiler` stay gated where
they are.

### `LockdownReport` restructure

```rust
pub enum LockdownReport {
    /// Linux: Landlock + seccomp + rlimit.
    Linux {
        landlock: LandlockReport,
        seccomp: SeccompReport,
        rlimit: RlimitReport,
    },
    /// macOS or other non-Linux: kernel containment is the parent's job
    /// (Seatbelt), but rlimit still applies cross-platform.
    NonLinux {
        rlimit: RlimitReport,
    },
}
```

The rename from `SkippedNonLinux` to `NonLinux { rlimit }` is a deliberate
backward-incompat. The only in-tree match site is `serve_stdio`'s
`eprintln!("hhagent-worker-prelude: lockdown {report:?}")` which uses the
auto-derived `Debug` impl, so the rename is mechanical.

### `serve_stdio` order

`lock_down()` keeps Landlock + seccomp as its responsibility; rlimit is a
separate concern composed in `serve_stdio`. This avoids having to thread a
placeholder `RlimitReport` through `lock_down`'s return type and keeps each
function single-purpose.

```rust
// In lib.rs:
pub fn lock_down() -> Result<LockdownReport, LockdownError> {
    // Linux: Landlock + seccomp; non-Linux: no-op (Seatbelt handles it).
    // rlimit is NOT applied here — serve_stdio composes it separately.
    // ...
}

pub fn serve_stdio<H: Handler>(handler: &mut H) -> io::Result<()> {
    // 1. setrlimit FIRST: arm the CPU budget before seccomp (some
    //    profiles ban prlimit; setting it earlier is safer).
    let rlimit = rlimit::apply_from_env().map_err(io_err)?;
    // 2. Then lock_down (Landlock + seccomp; no-op on macOS).
    let report = lock_down_with_rlimit(rlimit).map_err(io_err)?;
    eprintln!("hhagent-worker-prelude: lockdown {report:?}");
    hhagent_protocol::server::serve_stdio(handler)
}

// Helper: call lock_down() and inject the already-applied rlimit value.
fn lock_down_with_rlimit(rlimit: RlimitReport) -> Result<LockdownReport, LockdownError> {
    match lock_down()? {
        LockdownReport::Linux { landlock, seccomp, .. } =>
            Ok(LockdownReport::Linux { landlock, seccomp, rlimit }),
        LockdownReport::NonLinux { .. } =>
            Ok(LockdownReport::NonLinux { rlimit }),
    }
}
```

`lock_down()` itself returns `LockdownReport` with `rlimit:
RlimitReport::Disabled` (the placeholder); `serve_stdio` rebuilds it with the
real rlimit value. External callers of `lock_down()` (the `lockdown-probe`
binary's pre-existing subcommands) keep their working signature; the new
`cpu-burner` subcommand calls `rlimit::apply_from_env()` itself before
entering its busy loop.

### Env-var plumbing — `core::tool_host`

`core/src/tool_host.rs::derive_lockdown_env` gains a third entry:

```rust
pub const ENV_CPU_MS: &str = "HHAGENT_CPU_MS";

fn derive_lockdown_env(policy: &SandboxPolicy) -> SandboxPolicy {
    // ... existing landlock + seccomp env derivation ...

    let has_cpu_ms = out.env.iter().any(|(k, _)| k == ENV_CPU_MS);
    if !has_cpu_ms && policy.cpu_ms > 0 {
        out.env.push((ENV_CPU_MS.into(), policy.cpu_ms.to_string()));
    }
    out
}
```

When `cpu_ms == 0` (the "policy didn't set this" sentinel, matching how
`mem_mb == 0` is handled in the cgroup layer), the env var is omitted and
`rlimit::apply_from_env` returns `Disabled`. The "caller-supplied wins"
contract is preserved for tests that need to inject a different value.

### `lockdown-probe` `cpu-burner` subcommand

New subcommand mirrors the existing `mem-burner`:

```
lockdown-probe cpu-burner
    Call lock_down() via serve_stdio path's prerequisites, then busy-loop
    forever. If RLIMIT_CPU was applied, the kernel kills the process via
    SIGXCPU/SIGKILL within `cpu_seconds`. Used by rlimit_smoke.rs.
```

Implementation: after `lock_down()` (which sets seccomp + landlock but *not*
rlimit — the probe binary doesn't go through `serve_stdio`), enter a
busy loop. The probe binary calls `rlimit::apply_from_env()` itself before the
loop, mirroring `serve_stdio`'s order.

### Integration test — `rlimit_smoke.rs`

```rust
//! cross-platform: setrlimit is POSIX, works on Linux + macOS.

#[test]
fn cpu_burner_under_short_budget_is_killed_promptly() {
    // Spawn lockdown-probe cpu-burner with HHAGENT_CPU_MS=200.
    // Expect the subprocess to die within ~2 s wall-clock, killed by
    // SIGXCPU or SIGKILL (both indicate the rlimit fired). Loose
    // wall-clock tolerance because CPU-second != wall-clock second on
    // a busy host; the regression we're guarding against is "rlimit
    // was not applied at all", which would let the burner run for
    // > 30 s.
}

#[test]
fn cpu_burner_with_no_env_runs_unbounded_baseline() {
    // Without HHAGENT_CPU_MS, the cpu-burner runs > 1 s wall-clock
    // unmolested. Positive control so a future regression in
    // apply_from_env (always-disabled) is caught.
    // Uses a timeout via Child::kill so the test itself doesn't hang.
}
```

## Test plan

| Suite | Δ | What it pins |
| --- | --- | --- |
| `sandbox` unit (linux) | +4 | `build_systemd_run_argv` override paths for `cpu_quota_pct` and `tasks_max` |
| `sandbox` unit (cross) | +2 | `SandboxPolicy::default()` has both new fields = `None`; existing Default test extended |
| `prelude` unit | +8 | `cpu_ms_to_seconds` boundaries (0, 1, 999, 1000, 1001, u64::MAX); `apply_from_env` parse-error + happy + disabled |
| `prelude` integration | +2 | `rlimit_smoke.rs` — cpu-burner SIGXCPU happy + no-env baseline |
| `core` unit | +2 | `derive_lockdown_env` adds `HHAGENT_CPU_MS`; omits when `cpu_ms == 0` |
| **Total** | **+18** | Workspace count 429 → ~447 |

## Files affected (7)

| File | Change shape |
| --- | --- |
| [`sandbox/src/lib.rs`](sandbox/src/lib.rs) | 2 new fields; extend `Default`; extend default-test |
| [`sandbox/src/linux_cgroup.rs`](sandbox/src/linux_cgroup.rs) | 2 reads `policy.x.unwrap_or(DEFAULT_X)`; 4 new unit tests |
| [`workers/prelude/Cargo.toml`](workers/prelude/Cargo.toml) | promote `libc` to top-level dep |
| [`workers/prelude/src/lib.rs`](workers/prelude/src/lib.rs) | `mod rlimit;`; `LockdownReport` restructured; `serve_stdio` composes |
| `workers/prelude/src/rlimit.rs` | NEW (~150 LOC incl. tests) |
| [`workers/prelude/src/bin/lockdown_probe.rs`](workers/prelude/src/bin/lockdown_probe.rs) | new `cpu-burner` subcommand |
| `workers/prelude/tests/rlimit_smoke.rs` | NEW (~100 LOC) |
| [`core/src/tool_host.rs`](core/src/tool_host.rs) | `ENV_CPU_MS` const; extend `derive_lockdown_env`; 2 new unit tests |

## TDD order

1. **Sandbox shape change first** — add fields + Default + 1 unit test (compile-error red on every fixture that uses literal init; should already be migrated). Plumb new fields through `build_systemd_run_argv`; 4 cgroup unit tests red → green.
2. **`cpu_ms_to_seconds` pure helper** — 6 unit tests red → green, ~10 LOC body.
3. **`rlimit::apply_from_env`** — 3 unit tests red → green (env parse + setrlimit FFI). FFI body uses `libc::setrlimit(libc::RLIMIT_CPU, ...)`.
4. **`LockdownReport` restructure** — touches `lock_down`, `serve_stdio`. Confirm the existing 11 prelude unit tests stay green.
5. **`lockdown-probe cpu-burner` subcommand** — single match arm, body is `apply_from_env() + lock_down() + loop {}`.
6. **`rlimit_smoke.rs` integration** — write the two tests red, then run against the compiled probe binary green.
7. **`derive_lockdown_env`** — 2 unit tests red → green; 1-line change to the function.
8. **Full workspace** — confirm 429 → ~447, 0 fail, 0 SKIP, 0 warnings on Linux.

## What this slice deliberately does NOT do

- **`RLIMIT_AS` for memory.** Documented as a "future work" line in
  `LockdownReport` doc comment. Not on the field surface today.
- **macOS Seatbelt `cpu_quota_pct` / `tasks_max`.** No usable primitive. The
  two new fields are documented as "Linux cgroup only" on the policy struct.
- **Per-profile-class defaults.** `Profile::WorkerStrict` and
  `WorkerNetClient` continue to share the same defense-in-depth ceiling.
- **Runtime-pool re-derivation.** Policies are derived once per worker spawn;
  no need to recompute on a re-claim.

## Open questions

- **macOS rlimit_smoke coverage.** The new test is cross-platform but
  CI hasn't been exercised on macOS in this session. Manually verify on
  macOS-side at the next macOS session (HANDOVER's "macOS (main)" line).
  Filed as a low-priority follow-up if it diverges.
