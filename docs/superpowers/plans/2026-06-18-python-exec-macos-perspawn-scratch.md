# python-exec per-spawn writable scratch (macOS parity) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the python-exec worker a per-spawn, isolated, RAII-cleaned writable scratch dir on macOS (Seatbelt), matching Linux's per-spawn `/tmp` tmpfs — closing the cross-platform parity gap.

**Architecture:** A reusable per-spawn-scratch mechanism in a new `core/src/tool_host/scratch.rs`, opted into by a typed `ToolEntry.ephemeral_scratch: bool` flag (python-exec sets it). The scratch is composed *around* `spawn_worker` (mutate the cloned policy + create the guard before spawn, attach the guard to `SupervisedWorker` after) at the production cold-spawn sites and in the e2e harness — both share one helper, mirroring how egress attaches its sidecar post-spawn. `WorkerSpec`/`spawn_worker`/the Linux path are untouched (byte-identical).

**Tech Stack:** Rust workspace (rustc 1.96.0), `kastellan-core`, `kastellan-sandbox`, `kastellan-worker-python-exec`. macOS Seatbelt (`sandbox-exec`) + Linux bwrap. PG 18 + real jail for e2e.

**Spec:** `docs/superpowers/specs/2026-06-18-python-exec-macos-perspawn-scratch-design.md`

## Global Constraints

- **rustc 1.96.0**; `cargo clippy --workspace --all-targets -D warnings` must stay clean.
- **AGPL-compatible deps only** — this plan adds **no new dependencies** (stdlib only).
- **Cross-platform parity** — Linux behaviour must be byte-identical; the macOS branch is the only behavioural change. Verified by: `prepare_ephemeral_scratch` returns `None` off-macOS, the worker's scratch-dir env fallback is `/tmp`.
- **Files ≤ 500 LOC where feasible** — `core/src/tool_host.rs` is already 627 (over cap); put new code in the `tool_host/scratch.rs` sibling, do not grow `tool_host.rs`.
- **Build/test setup:** `source "$HOME/.cargo/env"` first. Dev box is macOS (Seatbelt). Run live-PG/jail e2e individually (full-workspace PG runs flake — see HANDOVER). DGX native-Linux re-run is **not required** (macOS-gated change, Linux byte-identical).
- **Env-var name** `KASTELLAN_WORKER_SCRATCH` is duplicated across `core` and the worker crate (no shared crate); both sides carry a "keep in sync" note, the convention already used for `KASTELLAN_PYTHON_PARAMS`.
- **TDD, frequent commits, one logical change per commit.** Stage specific files (`git add <paths>`) — never `git add -A` (untracked `assets/agent_with_the_keys.png` must stay out).

---

### Task 1: Scratch mechanism (`core/src/tool_host/scratch.rs`)

**Files:**
- Create: `core/src/tool_host/scratch.rs`
- Modify: `core/src/tool_host.rs` (add `mod scratch;` + re-exports; ~2 lines)

**Interfaces:**
- Consumes: `kastellan_sandbox::SandboxPolicy`; `crate::tool_host::ToolHostError`.
- Produces:
  - `pub const ENV_WORKER_SCRATCH: &str = "KASTELLAN_WORKER_SCRATCH";`
  - `pub fn scratch_subdir(root: &Path, pid: u32, seq: u64) -> PathBuf`
  - `pub fn apply_scratch(policy: &mut SandboxPolicy, dir: &Path)`
  - `pub struct EphemeralScratch` with `pub fn path(&self) -> &Path` (RAII, `Drop` removes the dir)
  - `pub fn prepare_ephemeral_scratch(policy: &mut SandboxPolicy, ephemeral: bool) -> Result<Option<EphemeralScratch>, ToolHostError>`

- [ ] **Step 1: Write the failing tests**

Create `core/src/tool_host/scratch.rs` with only the tests + empty module:

```rust
//! Per-spawn writable scratch for sandboxed workers (macOS parity, #283).
//!
//! On Linux every writable-scratch worker gets a fresh ephemeral `/tmp` tmpfs
//! from bwrap (#89); macOS Seatbelt has no tmpfs, so the host must create a
//! per-spawn dir, grant it via `fs_write`, tell the worker where it is, and
//! clean it up. This module is that mechanism; it is composed around
//! `spawn_worker` by the cold-spawn sites (and the python-exec e2e harness).

use std::path::{Path, PathBuf};

use kastellan_sandbox::SandboxPolicy;

use crate::tool_host::ToolHostError;

/// Env var carrying the per-spawn scratch dir to a worker process. The worker
/// uses it for `TMPDIR`/`HOME`/cwd, falling back to `/tmp` when unset (the
/// Linux tmpfs path). **Keep in sync** with the worker-side constant
/// `kastellan_worker_python_exec::exec::WORKER_SCRATCH_ENV`.
pub const ENV_WORKER_SCRATCH: &str = "KASTELLAN_WORKER_SCRATCH";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_subdir_is_pid_seq_named_under_root() {
        let d = scratch_subdir(Path::new("/var/tmp"), 1234, 7);
        assert_eq!(d, PathBuf::from("/var/tmp/pyexec-1234-7"));
    }

    #[test]
    fn scratch_subdir_distinct_for_distinct_seq() {
        let a = scratch_subdir(Path::new("/r"), 9, 1);
        let b = scratch_subdir(Path::new("/r"), 9, 2);
        assert_ne!(a, b);
    }

    #[test]
    fn apply_scratch_adds_fs_write_and_env() {
        let mut p = SandboxPolicy::default();
        apply_scratch(&mut p, Path::new("/var/tmp/pyexec-1-1"));
        assert!(p.fs_write.contains(&PathBuf::from("/var/tmp/pyexec-1-1")));
        let hits: Vec<_> = p.env.iter().filter(|(k, _)| k == ENV_WORKER_SCRATCH).collect();
        assert_eq!(hits.len(), 1, "exactly one scratch env entry");
        assert_eq!(hits[0].1, "/var/tmp/pyexec-1-1");
    }

    #[test]
    fn ephemeral_scratch_drop_removes_the_dir() {
        let root = std::env::temp_dir();
        let dir = scratch_subdir(&root, std::process::id(), 424_242);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir.exists());
        {
            let guard = EphemeralScratch { dir: dir.clone() };
            assert_eq!(guard.path(), dir);
        } // drop here
        assert!(!dir.exists(), "Drop must remove the scratch dir");
    }

    #[test]
    fn prepare_returns_none_when_not_requested() {
        let mut p = SandboxPolicy::default();
        let before = p.clone();
        let got = prepare_ephemeral_scratch(&mut p, false).unwrap();
        assert!(got.is_none());
        assert_eq!(p.fs_write, before.fs_write, "policy untouched when flag off");
        assert_eq!(p.env.len(), before.env.len());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn prepare_is_noop_on_non_macos_even_when_requested() {
        // Linux already has the bwrap tmpfs; the host creates nothing.
        let mut p = SandboxPolicy::default();
        let got = prepare_ephemeral_scratch(&mut p, true).unwrap();
        assert!(got.is_none(), "no host scratch off macOS");
        assert!(p.fs_write.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_creates_grants_and_cleans_on_macos() {
        let mut p = SandboxPolicy::default();
        let guard = prepare_ephemeral_scratch(&mut p, true).unwrap().expect("Some on macOS");
        let dir = guard.path().to_path_buf();
        assert!(dir.exists(), "dir created on disk");
        assert!(p.fs_write.contains(&dir), "granted via fs_write");
        assert!(p.env.iter().any(|(k, v)| k == ENV_WORKER_SCRATCH && Path::new(v) == dir));
        drop(guard);
        assert!(!dir.exists(), "cleaned on drop");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib tool_host::scratch 2>&1 | tail -20`
Expected: FAIL to compile — `scratch_subdir`, `apply_scratch`, `EphemeralScratch`, `prepare_ephemeral_scratch` not found.

(First add `mod scratch;` to `core/src/tool_host.rs` near the other `mod` lines, e.g. beside `mod lockdown_env;` / `mod secret_scrub;`, so the file is part of the build.)

- [ ] **Step 3: Implement the mechanism**

Insert above the `#[cfg(test)]` block in `core/src/tool_host/scratch.rs`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process counter so two scratch dirs spawned in the same
/// millisecond by the same pid still get distinct names.
static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Build the per-spawn scratch path `<root>/pyexec-<pid>-<seq>`. Pure (no I/O).
pub fn scratch_subdir(root: &Path, pid: u32, seq: u64) -> PathBuf {
    root.join(format!("pyexec-{pid}-{seq}"))
}

/// Grant `dir` to the worker: a writable `fs_write` entry (→ Seatbelt
/// `(allow file-read* file-write* (subpath ...))`) and the
/// [`ENV_WORKER_SCRATCH`] env entry telling the worker where it is. Pure.
pub fn apply_scratch(policy: &mut SandboxPolicy, dir: &Path) {
    policy.fs_write.push(dir.to_path_buf());
    policy
        .env
        .push((ENV_WORKER_SCRATCH.to_string(), dir.to_string_lossy().into_owned()));
}

/// RAII owner of a host-created per-spawn scratch dir. `Drop` best-effort
/// removes the whole subtree — mirrors `crate::egress::net_worker`'s scratch
/// cleanup. Held inside `SupervisedWorker` so the dir outlives the worker
/// exactly and no longer.
pub struct EphemeralScratch {
    dir: PathBuf,
}

impl EphemeralScratch {
    /// The granted scratch directory.
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for EphemeralScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Create + grant a per-spawn scratch dir when a worker requests one, returning
/// the RAII guard the caller must hold for the worker's lifetime.
///
/// * **macOS, `ephemeral == true`:** create `<temp_dir>/pyexec-<pid>-<seq>`,
///   [`apply_scratch`] it onto `policy`, return `Some(guard)`. Fail-closed: a
///   `create_dir_all` error aborts the spawn.
/// * **Otherwise** (off macOS, or `ephemeral == false`): `Ok(None)` — Linux's
///   bwrap tmpfs already provides per-spawn `/tmp`, so the host creates nothing.
///
/// Cross-platform-callable (runtime `cfg!`) so there is no dead code on Linux.
pub fn prepare_ephemeral_scratch(
    policy: &mut SandboxPolicy,
    ephemeral: bool,
) -> Result<Option<EphemeralScratch>, ToolHostError> {
    if !ephemeral || !cfg!(target_os = "macos") {
        return Ok(None);
    }
    let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = scratch_subdir(&std::env::temp_dir(), std::process::id(), seq);
    std::fs::create_dir_all(&dir).map_err(ToolHostError::Io)?;
    apply_scratch(policy, &dir);
    Ok(Some(EphemeralScratch { dir }))
}
```

Confirm `ToolHostError::Io(std::io::Error)` exists (it is used in `core/src/egress/net_worker.rs`). If the variant name differs, match the existing one.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib tool_host::scratch 2>&1 | tail -20`
Expected: PASS (6 tests on Linux: the `non_macos` one runs; 6 on macOS: the `macos` one runs).

- [ ] **Step 5: Re-export the public names from `tool_host`**

In `core/src/tool_host.rs`, beside the existing `pub use lockdown_env::{...}` line, add:

```rust
pub use scratch::{prepare_ephemeral_scratch, EphemeralScratch, ENV_WORKER_SCRATCH};
```

(`scratch_subdir`/`apply_scratch` stay module-public for tests; no need to re-export.)

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add core/src/tool_host/scratch.rs core/src/tool_host.rs
git commit -m "feat(tool_host): per-spawn ephemeral scratch mechanism (macOS, #283)"
```

---

### Task 2: `SupervisedWorker` scratch field + `with_scratch` attach seam

**Files:**
- Modify: `core/src/tool_host.rs` (the `SupervisedWorker` struct ~574-581 + an `impl SupervisedWorker` block)

**Interfaces:**
- Consumes: `scratch::EphemeralScratch` (Task 1).
- Produces: `pub fn SupervisedWorker::with_scratch(self, Option<EphemeralScratch>) -> Self` (the post-spawn attach seam, mirroring how `worker.egress` is set after `spawn_worker`).

> **No new unit test in this task — by design, not omission.** `SupervisedWorker`
> wraps a live `Client` and has no public/test constructor, so a unit test cannot
> build one without spawning a real worker. A test that only calls
> `prepare_ephemeral_scratch` + `drop` would duplicate **Task 1's** Drop test and
> assert nothing about `with_scratch`. The builder's real behaviour — guard moved
> in, dir cleaned when the worker drops — is verified end-to-end by **Task 6's
> e2e** (a real worker spawned `.with_scratch(...)`, then the host-side
> `no leaked scratch dirs` check). This task is field + builder plumbing; its gate
> is "compiles, clippy clean, existing `tool_host` tests still green".

- [ ] **Step 1: Add the field + builder**

In `core/src/tool_host.rs`, add to `SupervisedWorker` (after the `egress` field, so it drops last):

```rust
    /// `Some` only for a worker that requested per-spawn scratch
    /// (`ToolEntry.ephemeral_scratch`, macOS). Set post-spawn via
    /// [`SupervisedWorker::with_scratch`], mirroring how `egress` is attached.
    /// Its `Drop` removes the host scratch dir after the worker's pipes close.
    /// `None` for every worker on Linux and every non-scratch worker.
    pub(crate) scratch: Option<scratch::EphemeralScratch>,
```

Update the `SupervisedWorker { client, _watchdog, egress: None }` literal in `spawn_worker` (~524-528) to add `scratch: None,`. Update any other `SupervisedWorker { .. }` literal the compiler flags (e.g. in `core/src/egress/net_worker.rs`) with `scratch: None,`.

Add an `impl SupervisedWorker` method:

```rust
    /// Attach an optional per-spawn scratch guard, returning `self` for
    /// chaining. The guard's `Drop` cleans the host dir when this worker is
    /// dropped. `None` is a no-op (Linux / non-scratch workers).
    pub fn with_scratch(mut self, scratch: Option<scratch::EphemeralScratch>) -> Self {
        self.scratch = scratch;
        self
    }
```

- [ ] **Step 2: Run tests + clippy**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib tool_host 2>&1 | tail -10 && cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -5`
Expected: PASS + clean. (If clippy flags `field 'scratch' is never read`, mirror the `egress` field's exact treatment — it is the same shape and compiles clean today; if needed the established silencer in this struct is the `_`-prefix used by `_watchdog`, but prefer matching `egress`.)

- [ ] **Step 3: Commit**

```bash
git add core/src/tool_host.rs core/src/egress/net_worker.rs
git commit -m "feat(tool_host): SupervisedWorker scratch field + with_scratch attach seam"
```

---

### Task 3: `ToolEntry.ephemeral_scratch` flag + python-exec opt-in

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs` (`ToolEntry` struct ~95-148)
- Modify: `core/src/workers/python_exec.rs` (`python_exec_entry` ~148-157)
- Modify: the ~31 other `ToolEntry { .. }` literals the compiler flags (mechanical fill). Files (from `grep -rln "ToolEntry {" core/`): `core/tests/worker_lifecycle_idle_timeout_e2e.rs`, `core/tests/scheduler_step_dispatch_e2e.rs`, `core/tests/lifecycle_container_routing_e2e.rs`, `core/src/sandbox_health.rs`, `core/src/scheduler/tool_dispatch/tests.rs`, `core/src/workers/web_fetch.rs`, `core/src/workers/web_search.rs`, `core/src/workers/shell_exec.rs`, `core/src/workers/browser_driver.rs`, `core/src/workers/gliner_relex/entry.rs`, `core/src/worker_lifecycle/composite.rs`, `core/src/worker_lifecycle/manager/tests.rs`.
- Test: `core/src/workers/python_exec.rs` tests (`core/src/workers/python_exec/tests.rs`)

**Interfaces:**
- Produces: `ToolEntry.ephemeral_scratch: bool` — `true` only from `python_exec_entry`.

- [ ] **Step 1: Write the failing test**

In `core/src/workers/python_exec/tests.rs`, add:

```rust
#[test]
fn python_exec_entry_opts_into_ephemeral_scratch() {
    let e = python_exec_entry(
        std::path::PathBuf::from("/bin/worker"),
        std::path::PathBuf::from("/usr/bin/python3"),
        vec![],
    );
    assert!(e.ephemeral_scratch, "python-exec must request per-spawn scratch");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib python_exec_entry_opts_into 2>&1 | tail -10`
Expected: FAIL — no field `ephemeral_scratch` on `ToolEntry`.

- [ ] **Step 3: Add the field + flip python-exec + fill the rest (compiler-guided)**

In `core/src/scheduler/tool_dispatch.rs`, add to `ToolEntry` after `lockdown_shim`:

```rust
    /// When `true`, the worker is granted a per-spawn writable scratch dir on
    /// macOS (host-created, Seatbelt-granted, RAII-cleaned) — the parity
    /// counterpart of Linux's bwrap `/tmp` tmpfs. `false` for every worker
    /// except python-exec today. See `tool_host::prepare_ephemeral_scratch`.
    pub ephemeral_scratch: bool,
```

In `core/src/workers/python_exec.rs::python_exec_entry`, add to the returned `ToolEntry { .. }`:

```rust
        ephemeral_scratch: true,
```

Then `cargo build -p kastellan-core --all-targets 2>&1 | grep "missing field"` lists every other literal. Add `ephemeral_scratch: false,` to **each** (all are non-python-exec workers / tests that need no scratch).

- [ ] **Step 4: Run tests + build to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib python_exec 2>&1 | tail -10`
Expected: PASS (the new test + existing python_exec manifest tests).

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --all-targets 2>&1 | tail -5`
Expected: builds (no more `missing field`).

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add core/src/scheduler/tool_dispatch.rs core/src/workers/python_exec.rs core/src/workers/python_exec/tests.rs \
        core/src/sandbox_health.rs core/src/scheduler/tool_dispatch/tests.rs \
        core/src/workers/web_fetch.rs core/src/workers/web_search.rs core/src/workers/shell_exec.rs \
        core/src/workers/browser_driver.rs core/src/workers/gliner_relex/entry.rs \
        core/src/worker_lifecycle/composite.rs core/src/worker_lifecycle/manager/tests.rs \
        core/tests/worker_lifecycle_idle_timeout_e2e.rs core/tests/scheduler_step_dispatch_e2e.rs \
        core/tests/lifecycle_container_routing_e2e.rs
git commit -m "feat(tool_host): ToolEntry.ephemeral_scratch flag; python-exec opts in"
```

---

### Task 4: Wire the production cold-spawn sites

**Files:**
- Modify: `core/src/worker_lifecycle/manager.rs` (`SingleUseLifecycle::spawn` ~232-264)
- Modify: `core/src/worker_lifecycle/idle_timeout.rs` (cold-spawn path ~464-483)

**Interfaces:**
- Consumes: `crate::tool_host::prepare_ephemeral_scratch`, `SupervisedWorker::with_scratch`, `ToolEntry.ephemeral_scratch` (Tasks 1-3).

> No new unit test — `SupervisedWorker` can't be built without spawning, and the behaviour is e2e-covered (Task 6). This is a reviewer-gated wiring task. Both edits are the same shape.

- [ ] **Step 1: Edit `manager.rs::SingleUseLifecycle::spawn`**

Change `let policy = entry.policy.clone();` to `let mut policy = entry.policy.clone();`, then immediately after it add:

```rust
        // macOS per-spawn writable scratch (#283): host-create + grant + RAII.
        // No-op on Linux (bwrap tmpfs) and for non-scratch workers.
        let scratch = crate::tool_host::prepare_ephemeral_scratch(
            &mut policy,
            entry.ephemeral_scratch,
        )?;
```

Change the final `Ok(WorkerHandle::single_use(worker))` to attach the guard:

```rust
        Ok(WorkerHandle::single_use(worker.with_scratch(scratch)))
```

- [ ] **Step 2: Edit `idle_timeout.rs` cold-spawn path**

Change `let policy = entry.policy.clone();` to `let mut policy = entry.policy.clone();`, then immediately after add the identical `let scratch = crate::tool_host::prepare_ephemeral_scratch(&mut policy, entry.ephemeral_scratch)?;` block.

Change `let worker = spawn_worker_maybe_forced(force, sandbox, &spec, tool_name)?;` to be followed by attaching the guard before the `WorkerHandle::idle_timeout(...)` call:

```rust
    let worker = spawn_worker_maybe_forced(force, sandbox, &spec, tool_name)?.with_scratch(scratch);
```

- [ ] **Step 3: Build + clippy**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core 2>&1 | tail -5 && cargo clippy -p kastellan-core --all-targets -D warnings 2>&1 | tail -5`
Expected: builds + clean. (`prepare_ephemeral_scratch` returns `Result`, so the `?` requires the enclosing fns return `Result<_, ToolHostError>` — they already do.)

- [ ] **Step 4: Run the worker-lifecycle unit tests (regression — must stay green on Linux/macOS)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_lifecycle 2>&1 | tail -10`
Expected: PASS (no behavioural change on the test platform; scratch is `None` for non-python-exec workers and off-macOS).

- [ ] **Step 5: Commit**

```bash
git add core/src/worker_lifecycle/manager.rs core/src/worker_lifecycle/idle_timeout.rs
git commit -m "feat(worker_lifecycle): attach per-spawn scratch on cold spawn"
```

---

### Task 5: Worker reads the scratch dir from env

**Files:**
- Modify: `workers/python-exec/src/exec.rs` (add `WORKER_SCRATCH_ENV` + `scratch_dir_from_env`; `run_code` ~167-180 uses the resolved dir)

**Interfaces:**
- Consumes: the `KASTELLAN_WORKER_SCRATCH` env var set by core's `apply_scratch`.
- Produces: `pub fn scratch_dir_from_env(lookup: impl Fn(&str) -> Option<String>) -> String`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `workers/python-exec/src/exec.rs`:

```rust
    #[test]
    fn scratch_dir_defaults_to_tmp_when_unset() {
        let s = scratch_dir_from_env(|_| None);
        assert_eq!(s, "/tmp");
    }

    #[test]
    fn scratch_dir_uses_env_when_set() {
        let s = scratch_dir_from_env(|k| {
            (k == WORKER_SCRATCH_ENV).then(|| "/var/folders/xx/pyexec-1-1".to_string())
        });
        assert_eq!(s, "/var/folders/xx/pyexec-1-1");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec scratch_dir 2>&1 | tail -10`
Expected: FAIL — `scratch_dir_from_env` / `WORKER_SCRATCH_ENV` not found.

- [ ] **Step 3: Implement**

In `workers/python-exec/src/exec.rs`, near `PARAMS_ENV` add:

```rust
/// Env var by which the host hands this worker its per-spawn scratch dir
/// (macOS). Unset on Linux (the bwrap `/tmp` tmpfs is the scratch). **Keep in
/// sync** with core's `kastellan_core::tool_host::ENV_WORKER_SCRATCH`.
pub const WORKER_SCRATCH_ENV: &str = "KASTELLAN_WORKER_SCRATCH";

/// Resolve the scratch dir: the host-provided [`WORKER_SCRATCH_ENV`] value, or
/// the default [`SCRATCH_DIR`] (`/tmp`) when unset. Pure (no I/O) so the
/// fallback is unit-testable; the worker reads the real env at the call site.
pub fn scratch_dir_from_env(lookup: impl Fn(&str) -> Option<String>) -> String {
    lookup(WORKER_SCRATCH_ENV)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| SCRATCH_DIR.to_string())
}
```

In `run_code`, replace the hard-coded `SCRATCH_DIR` uses. At the top of the fn body:

```rust
    let scratch = scratch_dir_from_env(|k| std::env::var(k).ok());
```

Then change the three `SCRATCH_DIR` references to `scratch.as_str()` / `&scratch`:

```rust
    cmd.args(python_args())
        .env_clear()
        .env("TMPDIR", &scratch)
        .env("HOME", &scratch)
        .env(PARAMS_ENV, params_json)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if Path::new(&scratch).is_dir() {
        cmd.current_dir(&scratch);
    }
```

Update the `SCRATCH_DIR` doc comment (lines ~19-23) to note macOS now receives a per-spawn dir via `WORKER_SCRATCH_ENV` (falling back to `/tmp` when unset, the Linux tmpfs path).

- [ ] **Step 4: Run tests to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-python-exec 2>&1 | tail -15`
Expected: PASS (new scratch tests + existing exec tests; `run_code` behaviour unchanged when env unset → still `/tmp`).

- [ ] **Step 5: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-worker-python-exec --all-targets -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add workers/python-exec/src/exec.rs
git commit -m "feat(python-exec): resolve scratch dir from KASTELLAN_WORKER_SCRATCH"
```

---

### Task 6: e2e — macOS scratch write now succeeds

**Files:**
- Modify: `core/tests/python_exec_e2e.rs` (`dispatch_in_jail` ~123-156 + `scratch_tmp_write_round_trip_inside_jail` ~214-250)

**Interfaces:**
- Consumes: `prepare_ephemeral_scratch` + `with_scratch` + `ToolEntry.ephemeral_scratch` (Tasks 1-3).

- [ ] **Step 1: Make the harness apply per-spawn scratch (the production composition)**

In `dispatch_in_jail`, after `let entry = python_exec_entry(...)` and before building the `WorkerSpec`, clone the policy and prepare scratch (the entry now sets `ephemeral_scratch: true`):

```rust
    let entry = python_exec_entry(
        env.worker_path.clone(),
        env.python.clone(),
        interpreter_lib_dirs,
    );
    let mut policy = entry.policy.clone();
    let scratch =
        kastellan_core::tool_host::prepare_ephemeral_scratch(&mut policy, entry.ephemeral_scratch)
            .expect("prepare scratch");
    let backend = backend();
    let worker_str = env.worker_path.to_string_lossy().into_owned();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: None,
    };
    let mut sworker = spawn_worker(&*backend, &spec)
        .expect("spawn python-exec under sandbox")
        .with_scratch(scratch);
```

(Leave the rest of `dispatch_in_jail` — the `dispatch(...)` call and `sworker.close()` — unchanged. On Linux `scratch` is `None`, so this is byte-identical to today.)

- [ ] **Step 2: Enable the scratch write test on macOS**

Replace the body of `scratch_tmp_write_round_trip_inside_jail` so it runs on **both** platforms (delete the macOS `[SKIP]` arm and the `#[cfg(target_os = "linux")]` gate):

```rust
#[test]
fn scratch_tmp_write_round_trip_inside_jail() {
    // Linux: bwrap's per-spawn `/tmp` tmpfs (#89). macOS: a host-created
    // per-spawn dir granted via Seatbelt `fs_write` + handed to the worker
    // through KASTELLAN_WORKER_SCRATCH (#283). Either way the agent code can
    // write + read a temp file inside the jail.
    let env = match ready_or_skip() {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let code = concat!(
            "import tempfile\n",
            "with tempfile.NamedTemporaryFile('w+', delete=True) as f:\n",
            "    f.write('jail-scratch-ok')\n",
            "    f.flush()\n",
            "    f.seek(0)\n",
            "    print(f.read())\n",
        );
        let r = exec_in_jail(&pool, &env, code)
            .await
            .expect("python.exec round trip");
        assert_eq!(r["exit_code"], 0, "stderr: {}", r["stderr"]);
        assert_eq!(r["stdout"].as_str().unwrap().trim_end(), "jail-scratch-ok");
        pool.close().await;
    });
}
```

- [ ] **Step 3: Run the e2e on this box (macOS, real Seatbelt jail + PG 18)**

Run: `source "$HOME/.cargo/env" && KASTELLAN_PG_BIN_DIR="<pg18-bin>" cargo test -p kastellan-core --test python_exec_e2e 2>&1 | tail -25`
(Use the PG 18 bin dir from memory: `/Applications/Postgres 2.app/Contents/Versions/18/bin/`.)
Expected: all `python_exec_e2e` tests PASS — crucially `scratch_tmp_write_round_trip_inside_jail` now runs and passes on macOS (it printed `[SKIP]` before). The other tests (print round-trip, socket containment, secret-scrub) stay green.

- [ ] **Step 4: Verify host scratch cleanup (no leak)**

After the run: `ls "${TMPDIR:-/tmp}" | grep pyexec- || echo "no leaked scratch dirs"`
Expected: `no leaked scratch dirs` — every per-spawn dir was RAII-removed when its worker dropped.

- [ ] **Step 5: Commit**

```bash
git add core/tests/python_exec_e2e.rs
git commit -m "test(python-exec): per-spawn scratch write round-trips under Seatbelt (#283)"
```

---

### Task 7: Docs, handover, roadmap, final verification

**Files:**
- Modify: `core/src/workers/python_exec.rs` (module + `python_exec_entry` doc comments — macOS now has writable scratch)
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Refresh the python-exec manifest docs**

In `core/src/workers/python_exec.rs`, update the module-level doc (lines ~6-11) and `python_exec_entry`'s doc to state: scratch is the jail's ephemeral `/tmp` tmpfs on Linux **and a host-created per-spawn dir on macOS** (granted via `fs_write` at spawn by `prepare_ephemeral_scratch`, RAII-cleaned), no longer "not writable on macOS". Note `ephemeral_scratch: true`.

- [ ] **Step 2: Full regression on this box**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -D warnings 2>&1 | tail -5`
Expected: clean.

Run the worker + core lib suites and the python-exec e2e (full-workspace PG runs flake on the Mac — run the live suite individually):
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-python-exec 2>&1 | tail -5
cargo test -p kastellan-core --lib tool_host 2>&1 | tail -5
KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin/" \
  cargo test -p kastellan-core --test python_exec_e2e 2>&1 | tail -10
```
Expected: all PASS.

- [ ] **Step 3: Update HANDOVER.md**

Per its own "How to update" checklist: bump `Last updated` to 2026-06-18; move this work into "Recently merged"/"Recently completed" with file paths + the macOS test-count delta; write a fresh "Next TODO"; note **DGX native-Linux not re-run** (macOS-gated change, Linux byte-identical — carry the 1839/0/15 baseline forward); record the macOS `python_exec_e2e` now runs the scratch test on macOS (one fewer `[SKIP]`). Flag the follow-ups: browser-driver adopting `ephemeral_scratch` + dropping its `fs_write=["/tmp"]` (closes #283 fully); the >64 KiB scratch-file param channel.

- [ ] **Step 4: Tick ROADMAP.md**

Add the python-exec macOS per-spawn scratch item under Phase 4 with the merge commit (fill after squash-merge) and a pointer to the spec.

- [ ] **Step 5: Commit docs**

```bash
git add core/src/workers/python_exec.rs docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover,roadmap): python-exec macOS per-spawn scratch (#283)"
```

- [ ] **Step 6: Push + open PR**

```bash
git push -u origin <branch>
gh pr create --base main --title "feat(python-exec): per-spawn writable scratch on macOS (#283)" \
  --body "<summary + spec link + 'closes part of #283' + verification: macOS python_exec_e2e green incl. the now-unskipped scratch test; DGX not re-run (macOS-gated, Linux byte-identical)>"
```

(If `git push` from the Mac times out, use the DGX relay: `git format-patch` → `ssh dgx git am` → push from the DGX; `gh pr create` still works from the Mac. See memory note.)

---

## Self-Review

**Spec coverage:**
- §Approach generic opt-in flag → Task 3 (`ToolEntry.ephemeral_scratch`). ✓
- §Component 2 `scratch.rs` (helpers + RAII + prepare) → Task 1. ✓
- §Component 3 composition around `spawn_worker` (`with_scratch`, lifecycle wiring, e2e shares helper) → Tasks 2, 4, 6. ✓
- §Component 4 worker resolves scratch from env → Task 5. ✓
- §Component 5 manifest opt-in + docs → Tasks 3, 7. ✓
- §Cross-platform guarantee (Linux byte-identical) → `prepare_ephemeral_scratch` returns `None` off macOS (Task 1 test) + worker `/tmp` fallback (Task 5 test) + Task 4/6 no-op on Linux. ✓
- §Testing: core unit (Task 1), worker unit (Task 5), e2e (Task 6), regression (Tasks 4, 7). ✓
- §Out of scope (browser-driver adoption, Linux LANDLOCK_RW unification, param channel) → recorded in Task 7 handover. ✓

**Placeholder scan:** No TBD/TODO. The Task 3 "fill every literal" is compiler-guided and deterministic (one rule: `false` everywhere, `true` only in `python_exec_entry`), not a placeholder. PR body has explicit angle-bracket fill points (intentional, author-supplied at PR time).

**Type consistency:** `prepare_ephemeral_scratch(&mut SandboxPolicy, bool) -> Result<Option<EphemeralScratch>, ToolHostError>`, `EphemeralScratch::path()`, `SupervisedWorker::with_scratch(self, Option<EphemeralScratch>) -> Self`, `ENV_WORKER_SCRATCH` / `WORKER_SCRATCH_ENV` (the two synced sides), `ToolEntry.ephemeral_scratch: bool`, `scratch_dir_from_env(impl Fn(&str)->Option<String>) -> String` — names used consistently across Tasks 1-7.
