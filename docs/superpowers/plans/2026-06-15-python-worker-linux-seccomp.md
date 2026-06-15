# Linux seccomp for pure-Python workers (#281) — browser-driver first — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the pure-Python `browser-driver` worker under the worker-side `browser_client` seccomp filter on Linux by spawning it through a new Rust exec-shim that applies the prelude lockdown and `execve`s the venv script (the child inherits the filter under `NO_NEW_PRIVS`).

**Architecture:** A new `kastellan-worker-lockdown-exec` binary in the prelude crate runs `rlimit::apply_from_env()` → `lock_down()` → `execve(target)`. Landlock is left **off** for browser-driver via a new additive `KASTELLAN_LANDLOCK_PROFILE=none` signal (bwrap mounts remain the FS boundary). `ToolEntry` gains an `Option<PathBuf>` shim field; a pure helper wraps `(program, args)` at the two spawn sites; the browser-driver manifest discovers the shim (Linux-only, fail-closed).

**Tech Stack:** Rust (workspace, rustc 1.96.0), `seccompiler` + `landlock` (Linux-gated), bwrap. Spec: `docs/superpowers/specs/2026-06-15-python-worker-linux-seccomp-design.md`.

**Verification reality:** seccomp is Linux-only and cannot be exercised on the dev Mac. Mac CI = compile + clippy + unit tests + skip-as-pass suite. The **real acceptance gate is the DGX** (Task 9): `browser_driver_e2e --ignored` rendering a page with the `browser_client` filter actually active. Drive it as `ssh dgx '<cmd>'` (the allow-rule is a prefix match — no flags before the hostname).

**Standing rule:** `source "$HOME/.cargo/env"` before any cargo command (cargo isn't on the non-interactive PATH). Never `git add -A` — stage named files only (an untracked `assets/agent_with_the_keys.png` and `.claude/scheduled_tasks.lock` must stay out).

---

## Task 1: Landlock disable signal (`KASTELLAN_LANDLOCK_PROFILE=none`)

Adds an additive way to skip the Landlock layer so the shim can be "seccomp-only" for browser-driver. Existing workers never set the var → byte-identical.

**Files:**
- Modify: `workers/prelude/src/lib.rs` (add `LandlockReport::Disabled`)
- Modify: `workers/prelude/src/landlock_lock.rs` (pure predicate + `apply_from_env` gate + tests)

- [ ] **Step 1: Write the failing test** — append to the `mod tests` in `workers/prelude/src/landlock_lock.rs`:

```rust
    // ── KASTELLAN_LANDLOCK_PROFILE disable signal ────────────────────────

    #[test]
    fn landlock_disabled_only_for_explicit_none() {
        assert!(landlock_disabled_by_profile(Some("none")));
        assert!(!landlock_disabled_by_profile(Some("")));
        assert!(!landlock_disabled_by_profile(Some("strict")));
        assert!(!landlock_disabled_by_profile(None));
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-prelude landlock_disabled_only_for_explicit_none`
Expected: FAIL — `cannot find function landlock_disabled_by_profile`.

- [ ] **Step 3: Add the pure predicate + wire `apply_from_env`** in `workers/prelude/src/landlock_lock.rs`.

Add this pure helper just above `apply_from_env`:

```rust
/// Name of the env var that can disable the Landlock layer. Value `"none"`
/// skips the ruleset entirely; unset / any other value keeps the default
/// behavior. Used by workers whose filesystem surface is not yet validated
/// against a Landlock ruleset (e.g. browser-driver/Chromium), where bwrap's
/// mount namespace remains the filesystem-containment layer.
pub const LANDLOCK_PROFILE_ENV: &str = "KASTELLAN_LANDLOCK_PROFILE";

/// Pure predicate: should the Landlock layer be skipped for this profile value?
/// Only the exact string `"none"` disables it (mirrors the seccomp `"none"`
/// convention). Split out so it is unit-testable without touching process env.
pub fn landlock_disabled_by_profile(profile: Option<&str>) -> bool {
    profile == Some("none")
}
```

Then change `apply_from_env` to gate on it (add the early return as the first lines of the function body):

```rust
pub fn apply_from_env() -> Result<LandlockReport, LockdownError> {
    // Explicit opt-out: a worker that sets KASTELLAN_LANDLOCK_PROFILE=none gets
    // no Landlock ruleset. bwrap's mount namespace still contains it.
    let profile = std::env::var(LANDLOCK_PROFILE_ENV).ok();
    if landlock_disabled_by_profile(profile.as_deref()) {
        return Ok(LandlockReport::Disabled);
    }
    let rw_paths = parse_rw_env_var()?;
    let ro_paths = parse_ro_env_var()?;
    apply(&rw_paths, &ro_paths)
}
```

- [ ] **Step 4: Add the `Disabled` report variant** in `workers/prelude/src/lib.rs`. In `enum LandlockReport`, add after `KernelTooOld`:

```rust
    /// Deliberately not installed — the worker set
    /// `KASTELLAN_LANDLOCK_PROFILE=none`. bwrap's mount namespace remains the
    /// filesystem-containment layer. Not an error; logged via the report.
    Disabled,
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-prelude landlock_disabled_only_for_explicit_none`
Expected: PASS. (If any `match` on `LandlockReport` is now non-exhaustive, rustc will point at it — there are none in production today; add a `Disabled` arm if the compiler flags one.)

- [ ] **Step 6: Build the crate to confirm no exhaustiveness breaks**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-prelude`
Expected: clean build.

- [ ] **Step 7: Commit**

```bash
git add workers/prelude/src/lib.rs workers/prelude/src/landlock_lock.rs
git commit -m "feat(prelude): KASTELLAN_LANDLOCK_PROFILE=none disables Landlock (#281)

Additive opt-out so a worker can run seccomp-only. Existing workers never
set the var, so behavior is byte-identical. Adds LandlockReport::Disabled.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: The `kastellan-worker-lockdown-exec` shim binary

The production form of the proven `exec-after-lockdown` fixture: lock down, then `execve` the target. Compiles cross-platform (no-op lockdown off-Linux); it is only *inserted* on Linux (Task 8).

**Files:**
- Create: `workers/prelude/src/bin/lockdown_exec.rs`
- Modify: `workers/prelude/Cargo.toml` (second `[[bin]]`)

- [ ] **Step 1: Create the shim** `workers/prelude/src/bin/lockdown_exec.rs`:

```rust
//! `kastellan-worker-lockdown-exec`: production exec-shim that applies the
//! worker prelude lockdown (rlimit + Landlock + seccomp, all read from env)
//! and then `execve`s into a target binary. The target inherits the seccomp
//! filter (and Landlock ruleset, when enabled) because seccomp filters survive
//! `execve` under `PR_SET_NO_NEW_PRIVS`, which `lock_down` sets.
//!
//! Why it exists: pure-Python venv workers (browser-driver, gliner-relex) are
//! console scripts that `linux_bwrap` spawns directly — they never run the Rust
//! prelude, so without this shim they get no worker-side seccomp on Linux
//! (issue #281). Wrapping their spawn in this shim closes that gap.
//!
//! Reads the exact env `core::tool_host::derive_lockdown_env` already injects
//! for every worker (`KASTELLAN_SECCOMP_PROFILE`, `KASTELLAN_CPU_MS`,
//! `KASTELLAN_LANDLOCK_RW` / `_RO` / `_PROFILE`). No new host-side plumbing.
//!
//! Cross-platform: on non-Linux, `lock_down` is a no-op (Seatbelt contains from
//! the parent) and the manifest does not insert this shim — but the binary
//! still builds so the workspace compiles everywhere.
//!
//! Exit codes (a successful `execve` never returns):
//!   64 usage error (no target)   70 lock_down failed
//!   71 execve failed             72 rlimit failed   73 unsupported platform

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let target: OsString = match args.next() {
        Some(t) => t,
        None => {
            eprintln!("usage: kastellan-worker-lockdown-exec <target-binary> [args...]");
            return ExitCode::from(64);
        }
    };
    let rest: Vec<OsString> = args.collect();

    // rlimit first (matches serve_stdio: arm the CPU ceiling before any seccomp
    // restriction on the prlimit family). No-op when KASTELLAN_CPU_MS is unset.
    if let Err(e) = kastellan_worker_prelude::rlimit::apply_from_env() {
        eprintln!("kastellan-worker-lockdown-exec: rlimit error: {e}");
        return ExitCode::from(72);
    }
    // Landlock (env-gated; KASTELLAN_LANDLOCK_PROFILE=none skips it) + seccomp.
    match kastellan_worker_prelude::lock_down() {
        Ok(report) => eprintln!("kastellan-worker-lockdown-exec: lockdown {report:?}"),
        Err(e) => {
            eprintln!("kastellan-worker-lockdown-exec: lockdown error: {e}");
            return ExitCode::from(70);
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` replaces this process image; the seccomp filter persists.
        let err = std::process::Command::new(&target).args(&rest).exec();
        eprintln!("kastellan-worker-lockdown-exec: exec({target:?}) failed: {err}");
        ExitCode::from(71)
    }
    #[cfg(not(unix))]
    {
        eprintln!("kastellan-worker-lockdown-exec: unsupported non-unix platform");
        ExitCode::from(73)
    }
}
```

- [ ] **Step 2: Register the binary** in `workers/prelude/Cargo.toml` — add below the existing `[[bin]]`:

```toml
[[bin]]
name = "kastellan-worker-lockdown-exec"
path = "src/bin/lockdown_exec.rs"
```

- [ ] **Step 3: Build to verify it compiles**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-prelude --bin kastellan-worker-lockdown-exec`
Expected: clean build; `target/debug/kastellan-worker-lockdown-exec` exists.

- [ ] **Step 4: Commit**

```bash
git add workers/prelude/src/bin/lockdown_exec.rs workers/prelude/Cargo.toml
git commit -m "feat(prelude): add kastellan-worker-lockdown-exec shim (#281)

Applies rlimit + lock_down then execve's a target binary, which inherits the
seccomp filter under NO_NEW_PRIVS. Production form of the exec-after-lockdown
fixture; used to bring pure-Python venv workers under worker-side seccomp.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `raw-*` probe subcommands for the inheritance test

The shim's smoke test needs a target that runs the test syscall **without** locking down itself — otherwise an inherited filter can't be told from a self-applied one. Add `raw-getpid` / `raw-unshare` to the probe, dispatched **before** its top-of-main lockdown.

**Files:**
- Modify: `workers/prelude/src/bin/lockdown_probe.rs`

- [ ] **Step 1: Add the pre-lockdown fast path.** In `workers/prelude/src/bin/lockdown_probe.rs`, immediately after the `if args.is_empty()` guard and **before** the `rlimit::apply_from_env()` call, insert:

```rust
    // Pre-lockdown fast path: `raw-*` subcommands deliberately run WITHOUT
    // applying any rlimit/lockdown of their own. They verify that a *parent*
    // (the kastellan-worker-lockdown-exec shim) which locked down and then
    // execve'd us actually carried its seccomp filter across the exec. If we
    // self-locked-down here, the test couldn't distinguish an inherited filter
    // from a freshly-applied one.
    #[cfg(target_os = "linux")]
    match args[0].as_str() {
        "raw-getpid" => return probe_getpid(),
        "raw-unshare" => return probe_unshare(),
        _ => {}
    }
```

(`probe_getpid` and `probe_unshare` already exist and do not call `lock_down`. Document the two new subcommands in the module-level `//!` subcommand list to keep it accurate.)

- [ ] **Step 2: Build to verify it compiles**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-prelude --bin kastellan-lockdown-probe`
Expected: clean build.

- [ ] **Step 3: Commit**

```bash
git add workers/prelude/src/bin/lockdown_probe.rs
git commit -m "test(prelude): raw-getpid/raw-unshare probe subcommands (no self-lockdown) (#281)

Pre-lockdown fast path so the lockdown-exec smoke test can prove seccomp is
inherited across the shim's execve rather than self-applied.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: prelude integration smoke test for the shim

Proves: (a) `raw-unshare` is NOT killed without the shim (target doesn't self-lockdown), (b) the shim's seccomp filter kills `raw-unshare` across the exec, (c) the target still runs (`raw-getpid` survives).

**Files:**
- Create: `workers/prelude/tests/lockdown_exec_smoke.rs`

- [ ] **Step 1: Write the test** `workers/prelude/tests/lockdown_exec_smoke.rs`:

```rust
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
    let out = Command::new(PROBE)
        .args(["raw-unshare"])
        .env_clear()
        .envs([("KASTELLAN_SECCOMP_PROFILE", "none")])
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
    assert_eq!(
        out.status.code(),
        Some(0),
        "shim must execve the target and getpid must survive; got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}
```

- [ ] **Step 2: Run the test (Linux only)**

Run (on the DGX, or any Linux host): `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-prelude --test lockdown_exec_smoke -- --nocapture`
Expected: 3 passed (or `[SKIP]` lines if the host lacks seccomp). On the **Mac** the file is `#![cfg(target_os = "linux")]` so it compiles to an empty test binary — confirm with `cargo test -p kastellan-worker-prelude --test lockdown_exec_smoke` → `0 passed`.

- [ ] **Step 3: Commit**

```bash
git add workers/prelude/tests/lockdown_exec_smoke.rs
git commit -m "test(prelude): lockdown-exec inheritance smoke test (#281)

Proves the shim's seccomp filter crosses execve (raw-unshare -> SIGSYS) and
the target still runs (raw-getpid -> 0); baseline confirms raw-* don't
self-lockdown.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `ToolEntry.lockdown_shim` field + `build_program_and_args` helper

Adds the field (default `None` = unchanged) and a pure spawn-invocation helper. Updates every `ToolEntry` literal in the same commit so the workspace stays green.

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs` (new field)
- Create: `core/src/tool_host/spawn_invocation.rs` (pure helper + unit tests)
- Modify: `core/src/tool_host.rs` (`mod` + `pub use`)
- Modify: `core/src/tool_host/lockdown_env.rs` (add `ENV_LANDLOCK_PROFILE` const + re-export)
- Modify: every `ToolEntry { … }` literal (see list in Step 4)

- [ ] **Step 1: Write the failing helper test** — create `core/src/tool_host/spawn_invocation.rs`:

```rust
//! Pure helper that builds the `(program, args)` pair to spawn for a worker,
//! honoring an optional lockdown shim.
//!
//! When `shim` is `Some`, the worker binary runs *through* the shim
//! (`kastellan-worker-lockdown-exec`): the shim applies the prelude lockdown
//! then `execve`s the real binary, which inherits the seccomp filter. This is
//! how pure-Python venv workers (browser-driver) get worker-side seccomp on
//! Linux, where bwrap spawns them directly and never runs the Rust prelude.
//! When `None`, the binary is spawned directly — every Rust worker, which
//! locks itself down via `serve_stdio`.

use std::path::Path;

/// Build `(program, args)` for the sandbox spawn. Owned returns so callers can
/// borrow `&str` into a `WorkerSpec`. `base_args` is the worker's own argv
/// (empty for every current worker).
pub fn build_program_and_args(
    binary: &Path,
    shim: Option<&Path>,
    base_args: &[&str],
) -> (String, Vec<String>) {
    match shim {
        Some(shim) => {
            let program = shim.to_string_lossy().into_owned();
            let mut args = Vec::with_capacity(base_args.len() + 1);
            args.push(binary.to_string_lossy().into_owned());
            args.extend(base_args.iter().map(|a| a.to_string()));
            (program, args)
        }
        None => (
            binary.to_string_lossy().into_owned(),
            base_args.iter().map(|a| a.to_string()).collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn no_shim_spawns_binary_directly() {
        let (program, args) = build_program_and_args(Path::new("/venv/bin/worker"), None, &[]);
        assert_eq!(program, "/venv/bin/worker");
        assert!(args.is_empty());
    }

    #[test]
    fn no_shim_preserves_base_args() {
        let (program, args) =
            build_program_and_args(Path::new("/venv/bin/worker"), None, &["--x", "y"]);
        assert_eq!(program, "/venv/bin/worker");
        assert_eq!(args, vec!["--x".to_string(), "y".to_string()]);
    }

    #[test]
    fn shim_wraps_binary_as_first_arg() {
        let (program, args) = build_program_and_args(
            Path::new("/venv/bin/worker"),
            Some(Path::new("/usr/bin/kastellan-worker-lockdown-exec")),
            &["--flag"],
        );
        assert_eq!(program, "/usr/bin/kastellan-worker-lockdown-exec");
        assert_eq!(
            args,
            vec!["/venv/bin/worker".to_string(), "--flag".to_string()]
        );
    }
}
```

- [ ] **Step 2: Wire the module** in `core/src/tool_host.rs`. Near the other submodule declarations / re-exports (where `lockdown_env` is declared), add:

```rust
mod spawn_invocation;
pub use spawn_invocation::build_program_and_args;
```

- [ ] **Step 3: Run the helper test to verify it fails then passes.**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core build_program_and_args 2>&1 | tail -20`
Expected first run (before Step 2 wiring is saved): FAIL/compile error. After Steps 1–2 saved: the 3 helper tests PASS once the crate compiles — but it won't compile yet because `ToolEntry` lacks the field (Steps 4–5). Proceed to Step 4; this test passes at Step 6.

- [ ] **Step 4: Add the field** to `core/src/scheduler/tool_dispatch.rs` — inside `pub struct ToolEntry`, after `container_image`:

```rust
    /// Optional lockdown shim the worker is spawned *through*
    /// (`kastellan-worker-lockdown-exec`). `None` (every Rust worker) spawns
    /// the binary directly — the worker locks itself down via the prelude's
    /// `serve_stdio`. `Some(path)` is set by manifests for pure-Python venv
    /// workers (browser-driver) on Linux: bwrap spawns them directly and never
    /// runs the Rust prelude, so the shim applies the seccomp filter and
    /// `execve`s the real binary, which inherits it. See issue #281.
    pub lockdown_shim: Option<std::path::PathBuf>,
```

- [ ] **Step 5: Add `lockdown_shim: None,` to every `ToolEntry { … }` literal EXCEPT browser-driver** (browser-driver is wired in Task 8 — set it to `None` here for now so it compiles). The literals:

```
core/src/sandbox_health.rs:209
core/src/workers/shell_exec.rs:38
core/src/workers/python_exec.rs:148
core/src/workers/web_fetch.rs:68
core/src/workers/web_search.rs:79
core/src/workers/browser_driver.rs:253        # None for now; Task 8 changes it
core/src/workers/gliner_relex/entry.rs:166
core/src/workers/gliner_relex/entry.rs:226
core/src/worker_lifecycle/composite.rs:138
core/src/worker_lifecycle/composite.rs:157
core/src/scheduler/tool_dispatch/tests.rs:131
core/src/worker_lifecycle/manager/tests.rs:29,120,140,212,237
core/tests/worker_lifecycle_idle_timeout_e2e.rs:115
core/tests/scheduler_step_dispatch_e2e.rs:173
core/tests/lifecycle_container_routing_e2e.rs:94
```

Add the field next to `container_image: …,` in each. Then run `cargo build -p kastellan-core --all-targets` and add the field to **any** literal the compiler still flags (the line numbers above will drift as you edit).

- [ ] **Step 6: Add the `ENV_LANDLOCK_PROFILE` const** in `core/src/tool_host/lockdown_env.rs`, next to `ENV_LANDLOCK_RO` etc.:

```rust
/// Env var read by `kastellan-worker-prelude::landlock_lock` to disable the
/// Landlock layer (`"none"`). Source of truth for the string is the prelude;
/// mirrored here for manifests that set it (browser-driver). Not set by
/// `derive_lockdown_env` — only explicitly by a manifest that opts out.
pub const ENV_LANDLOCK_PROFILE: &str = "KASTELLAN_LANDLOCK_PROFILE";
```

And add `ENV_LANDLOCK_PROFILE` to the `pub use lockdown_env::{…}` re-export list in `core/src/tool_host.rs`.

- [ ] **Step 7: Build + run the helper test**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --all-targets && cargo test -p kastellan-core build_program_and_args`
Expected: clean build; 3 helper tests PASS.

- [ ] **Step 8: Commit**

```bash
git add core/src/scheduler/tool_dispatch.rs core/src/tool_host.rs \
        core/src/tool_host/spawn_invocation.rs core/src/tool_host/lockdown_env.rs \
        core/src/sandbox_health.rs core/src/workers core/src/worker_lifecycle \
        core/tests/worker_lifecycle_idle_timeout_e2e.rs \
        core/tests/scheduler_step_dispatch_e2e.rs \
        core/tests/lifecycle_container_routing_e2e.rs
git commit -m "feat(core): ToolEntry.lockdown_shim + build_program_and_args helper (#281)

Additive Option<PathBuf> (default None = unchanged) + a pure spawn-invocation
helper. All existing entries pass None, so behavior is byte-identical.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Wire the helper into both cold-spawn sites

No behavior change yet (every entry still `None`), but routes both spawn sites through the helper so Task 8 only flips a manifest.

**Files:**
- Modify: `core/src/worker_lifecycle/manager.rs:233-239` (SingleUse)
- Modify: `core/src/worker_lifecycle/idle_timeout.rs:462-468` (IdleTimeout)

- [ ] **Step 1: Update `SingleUseLifecycle::acquire`** in `core/src/worker_lifecycle/manager.rs`. Replace:

```rust
        let program = entry.binary.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &program,
            args: &[],
            wall_clock_ms: entry.wall_clock_ms,
        };
```

with:

```rust
        // Route through the lockdown shim when the manifest set one (Linux
        // pure-Python workers); otherwise spawn the binary directly.
        let (program, args) = crate::tool_host::build_program_and_args(
            &entry.binary,
            entry.lockdown_shim.as_deref(),
            &[],
        );
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let spec = WorkerSpec {
            policy: &policy,
            program: &program,
            args: &arg_refs,
            wall_clock_ms: entry.wall_clock_ms,
        };
```

- [ ] **Step 2: Update the idle-timeout cold-spawn path** in `core/src/worker_lifecycle/idle_timeout.rs`. Replace:

```rust
    let program = entry.binary.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &program,
        args: &[],
        wall_clock_ms: entry.wall_clock_ms,
    };
```

with:

```rust
    let (program, args) = crate::tool_host::build_program_and_args(
        &entry.binary,
        entry.lockdown_shim.as_deref(),
        &[],
    );
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let spec = WorkerSpec {
        policy: &policy,
        program: &program,
        args: &arg_refs,
        wall_clock_ms: entry.wall_clock_ms,
    };
```

- [ ] **Step 3: Build + run the worker-lifecycle tests**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --all-targets && cargo test -p kastellan-core worker_lifecycle 2>&1 | tail -20`
Expected: clean build; lifecycle unit tests PASS (no behavior change — all entries `None`).

- [ ] **Step 4: Commit**

```bash
git add core/src/worker_lifecycle/manager.rs core/src/worker_lifecycle/idle_timeout.rs
git commit -m "refactor(core): route both spawn sites through build_program_and_args (#281)

No behavior change (all entries pass lockdown_shim=None); prepares the
manifest flip for browser-driver.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Browser-driver manifest — discover the shim, fail-closed, disable Landlock

The behavior change: on Linux, browser-driver is spawned through the shim with the `browser_client` seccomp profile and `KASTELLAN_LANDLOCK_PROFILE=none`. macOS unchanged.

**Files:**
- Modify: `core/src/workers/browser_driver.rs` (`browser_driver_entry` signature + env; `resolve` discovery + fail-closed)
- Modify: `core/src/workers/browser_driver/tests.rs` (manifest assertions)

- [ ] **Step 1: Write the failing manifest test.** In `core/src/workers/browser_driver/tests.rs`, add (adjust the `make_env()`/helper names to match the file's existing fixtures — read the file first):

```rust
    /// Linux: the entry routes through the lockdown shim and disables Landlock
    /// (bwrap mounts remain the FS layer). macOS: no shim (Seatbelt contains
    /// from the parent), no extra env.
    #[test]
    fn entry_sets_lockdown_shim_and_landlock_none_on_linux() {
        let env = sample_env(); // existing helper that builds a BrowserDriverEnv
        let shim = std::path::PathBuf::from("/opt/kastellan/kastellan-worker-lockdown-exec");

        #[cfg(target_os = "linux")]
        {
            let entry = browser_driver_entry(&env, &["example.com".to_string()], Some(shim.clone()));
            assert_eq!(entry.lockdown_shim.as_deref(), Some(shim.as_path()));
            assert!(
                entry
                    .policy
                    .env
                    .iter()
                    .any(|(k, v)| k == "KASTELLAN_LANDLOCK_PROFILE" && v == "none"),
                "Linux browser-driver must disable Landlock for the shim's lock_down"
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            let entry = browser_driver_entry(&env, &["example.com".to_string()], None);
            assert!(entry.lockdown_shim.is_none());
            assert!(
                !entry
                    .policy
                    .env
                    .iter()
                    .any(|(k, _)| k == "KASTELLAN_LANDLOCK_PROFILE"),
                "macOS browser-driver must not add the Landlock-profile env"
            );
        }
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core entry_sets_lockdown_shim_and_landlock_none_on_linux 2>&1 | tail -20`
Expected: FAIL — `browser_driver_entry` takes 2 args, not 3.

- [ ] **Step 3: Update `browser_driver_entry`** in `core/src/workers/browser_driver.rs`. Change the signature and tail:

```rust
pub fn browser_driver_entry(
    env: &BrowserDriverEnv,
    allowlist: &[String],
    lockdown_shim: Option<PathBuf>,
) -> ToolEntry {
```

Inside, after the existing `env: vec![ … ]` policy field is built, the cleanest approach is to build the env into a local `let mut env_vec = vec![ … ];` then push the opt-out when a shim is present, before constructing `SandboxPolicy`. Add, right before the `let policy = SandboxPolicy {`:

```rust
    // When spawned through the lockdown shim (Linux), disable the shim's
    // Landlock layer: browser-driver's Chromium FS surface isn't validated
    // against a Landlock ruleset yet, and bwrap's mount namespace already
    // contains it. seccomp (browser_client) still applies. (#281; Landlock is
    // a tracked follow-up.) macOS passes None here and adds nothing.
    let mut policy_env = policy_env; // the existing Vec built above
    if lockdown_shim.is_some() {
        policy_env.push((
            crate::tool_host::ENV_LANDLOCK_PROFILE.to_string(),
            "none".to_string(),
        ));
    }
```

> Implementation note: rename the inline `env: vec![ … ]` to a preceding `let mut policy_env = vec![ … ];` and set `env: policy_env,` in the struct, so the conditional push above is well-placed. Set the new struct field `lockdown_shim,` (shorthand) at the end of the `ToolEntry { … }` literal (replacing the `lockdown_shim: None` placeholder from Task 5 Step 5).

- [ ] **Step 4: Wire discovery + fail-closed in `resolve`.** In `impl WorkerManifest for BrowserDriverManifest`, replace the `Ok(env) => { … }` arm with:

```rust
            Ok(env) => {
                let allowlist = (ctx.allowlist)(TOOL_NAME);
                // Linux: browser-driver is a pure-Python venv worker bwrap
                // spawns directly, so it needs the lockdown-exec shim to get the
                // browser_client seccomp filter. Fail-closed if the shim is
                // missing — never register an unfilterable browser. macOS uses
                // Seatbelt (applied from the parent), so no shim.
                #[cfg(target_os = "linux")]
                {
                    match crate::worker_manifest::discover_binary(
                        ctx,
                        "KASTELLAN_LOCKDOWN_EXEC_BIN",
                        "kastellan-worker-lockdown-exec",
                    ) {
                        Some(shim) => Resolution::Register(browser_driver_entry(
                            &env,
                            &allowlist,
                            Some(shim),
                        )),
                        None => Resolution::Misconfigured {
                            detail: "lockdown-exec shim not found \
                                     (KASTELLAN_LOCKDOWN_EXEC_BIN unset/invalid and no \
                                     exe-relative sibling); browser-driver requires it \
                                     for worker-side seccomp on Linux"
                                .to_string(),
                        },
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Resolution::Register(browser_driver_entry(&env, &allowlist, None))
                }
            }
```

- [ ] **Step 5: Update any other call site of `browser_driver_entry`.** Run `grep -rn browser_driver_entry core/` — update each call to pass the third arg (`None` in non-Linux test fixtures, or a fake shim path where the test asserts shim behavior).

- [ ] **Step 6: Build + run the browser-driver tests**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --all-targets && cargo test -p kastellan-core browser_driver 2>&1 | tail -30`
Expected: clean build; manifest tests PASS (the new one + existing ones).

- [ ] **Step 7: Commit**

```bash
git add core/src/workers/browser_driver.rs core/src/workers/browser_driver/tests.rs
git commit -m "feat(core): browser-driver routes through lockdown-exec shim on Linux (#281)

Discovers kastellan-worker-lockdown-exec (fail-closed if missing), sets
lockdown_shim + KASTELLAN_LANDLOCK_PROFILE=none so the worker gets the
browser_client seccomp filter (Landlock deferred; bwrap mounts remain the FS
layer). macOS unchanged (Seatbelt applies the profile from the parent).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Full Mac verification (compile + clippy + suite)

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build**

Run: `source "$HOME/.cargo/env" && cargo build --workspace --all-targets 2>&1 | tail -15`
Expected: clean.

- [ ] **Step 2: clippy `-D warnings`**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15`
Expected: no warnings.

- [ ] **Step 3: Workspace test suite (Mac skip-as-pass, no KASTELLAN_PG_BIN_DIR)**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -30`
Expected: all green; record passed/failed/ignored counts for the handover. (The new `lockdown_exec_smoke` is `cfg(linux)` → 0 tests on Mac; the seccomp behavior is verified on the DGX in Task 9.)

- [ ] **Step 4: Cross-clippy the Linux-gated prelude path (pure-Rust crate)**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-worker-prelude --target aarch64-unknown-linux-gnu 2>&1 | tail -15`
Expected: clean (compiles the Linux landlock/seccomp + the new shim under cfg(linux) without a linker; `core` can't be cross-checked — that's the DGX's job).

- [ ] **Step 5: Commit (if any clippy fixups were needed; else skip)**

```bash
git add -u
git commit -m "chore(#281): clippy/format fixups for the seccomp-shim wiring

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: DGX native-Linux acceptance — the real gate

Verifies the `browser_client` seccomp filter is **actually active** under a real Chromium render. Drive the DGX as `ssh dgx '<cmd>'` (prefix-match allow rule — no flags before the hostname).

**Files:** none (acceptance only); may produce a `seccomp_lock.rs` allowlist expansion if Chromium hits a missing syscall.

- [ ] **Step 1: Push the branch so the DGX can fetch it**

```bash
git push -u origin feat/281-python-worker-seccomp-shim
```

- [ ] **Step 2: On the DGX — sync, build the workspace (so the shim + worker bins exist), stage the browser-driver venv**

Run:
```bash
ssh dgx 'cd ~/src/kastellan && git fetch && git checkout feat/281-python-worker-seccomp-shim && git pull && source ~/.cargo/env && cargo build --workspace 2>&1 | tail -5'
ssh dgx 'cd ~/src/kastellan && bash scripts/workers/browser-driver/install.sh 2>&1 | tail -5'
```
Expected: clean build; install.sh stages the venv + Chromium (it `--force-reinstall`s current source — the #287 fix).

- [ ] **Step 3: Run the prelude shim smoke test natively (real seccomp)**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-worker-prelude --test lockdown_exec_smoke -- --nocapture 2>&1 | tail -20'`
Expected: 3 passed (no `[SKIP]`).

- [ ] **Step 4: Run the browser-driver e2e with the seccomp filter active**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && KASTELLAN_BROWSER_DRIVER_ENABLE=1 cargo test -p kastellan-core --test browser_driver_e2e -- --ignored --nocapture 2>&1 | tail -40'`
Expected: the render tests pass with the worker spawned through the shim. **If a render dies by SIGSYS:** Chromium hit a syscall missing from `browser_client`'s allow-list. Read the worker stderr (drained at debug) / dmesg for the blocked syscall number, add it to `allow_list_for(Profile::BrowserClient)` in `workers/prelude/src/seccomp_lock.rs` with a comment naming the Chromium subsystem, rebuild, re-run. Repeat until green. Commit each expansion:

```bash
git commit -am "fix(prelude): allow <syscall> in browser_client (Chromium <subsystem>) (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Full native-Linux workspace test + clippy (the DGX baseline)**

Run:
```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test --workspace 2>&1 | tail -5'
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5'
```
Expected: green; record the passed/failed count (update the handover's native-Linux baseline from 1790/0).

- [ ] **Step 6: Push any seccomp expansions**

```bash
git push
```

---

## Task 10: Handover, ROADMAP, PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Update HANDOVER.md** — header (`Last updated`, last commit, session-end test counts from Tasks 8 + 9); move this work into "Recently merged"/"This session" with file paths + the seccomp-allowlist expansions (if any) + the DGX result; refresh "Working state" (browser-driver now seccomp-filtered on Linux; the new `kastellan-worker-lockdown-exec` bin in the prelude crate); write a fresh "Next TODO" naming the two explicit follow-ups (Landlock-for-browser once the Chromium RO set is validated; gliner-relex wiring reusing the shim).

- [ ] **Step 2: Tick the #281 item in ROADMAP.md** with the merge commit hash (browser-driver portion; note gliner-relex + Landlock remain).

- [ ] **Step 3: Commit the docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): browser-driver Linux seccomp via lockdown-exec shim (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git push
```

- [ ] **Step 4: Open the PR** (link #281; note it resolves the browser-driver portion, gliner-relex + Landlock are tracked follow-ups; DGX acceptance green):

```bash
gh pr create --base main --title "Linux seccomp for browser-driver via lockdown-exec shim (#281)" --body "$(cat <<'EOF'
## Summary
Brings the pure-Python `browser-driver` worker under the worker-side
`browser_client` seccomp filter on Linux. bwrap spawns venv console scripts
directly (never running the Rust prelude), so they previously got no seccomp.
A new `kastellan-worker-lockdown-exec` shim applies the prelude lockdown then
`execve`s the venv script, which inherits the filter under `NO_NEW_PRIVS`.

- New `kastellan-worker-lockdown-exec` binary (prelude crate).
- `KASTELLAN_LANDLOCK_PROFILE=none` opt-out → browser-driver is **seccomp-only**
  (Landlock deferred; bwrap mounts remain the FS boundary — see the spec's
  residual-risk analysis).
- `ToolEntry.lockdown_shim` (default `None` = unchanged) + a pure
  `build_program_and_args` helper at both spawn sites.
- browser-driver manifest discovers the shim (fail-closed on Linux); macOS
  unchanged (Seatbelt applies the profile from the parent).

## Verification
- Mac: build + clippy `-D warnings` + workspace suite green.
- DGX: `lockdown_exec_smoke` (3/3) + `browser_driver_e2e --ignored` render with
  the filter active + full `cargo test --workspace`.

## Follow-ups (keep #281 open)
- Landlock for browser-driver once the Chromium-compatible RO path set is
  validated on the DGX.
- gliner-relex wiring (reuses this shim; needs its own profile validation).

Spec: `docs/superpowers/specs/2026-06-15-python-worker-linux-seccomp-design.md`
Plan: `docs/superpowers/plans/2026-06-15-python-worker-linux-seccomp.md`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-review notes

- **Spec coverage:** shim binary (T2), Landlock disable signal (T1), `ToolEntry` field + helper (T5), spawn-site wiring (T6), browser-driver manifest + fail-closed + LANDLOCK=none (T7), unit + prelude-integration tests (T1/T4/T5/T7), DGX gate (T9), out-of-scope follow-ups recorded in the PR + handover (T10). All spec sections map to a task.
- **Compilability:** the `ToolEntry` field lands with all literal updates in one commit (T5); browser-driver's literal is `None` until T7 flips it — every task ends green.
- **Type consistency:** `build_program_and_args(&Path, Option<&Path>, &[&str]) -> (String, Vec<String>)`, `lockdown_shim: Option<PathBuf>`, `ENV_LANDLOCK_PROFILE` / `LANDLOCK_PROFILE_ENV` (core mirror / prelude source-of-truth) used consistently across T1/T5/T7.
- **Line numbers drift:** every literal-edit step says to let `cargo build` flag any missed site rather than trusting the listed line numbers.
