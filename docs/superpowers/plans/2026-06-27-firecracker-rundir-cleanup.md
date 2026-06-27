# Firecracker run-dir cleanup (#362) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the per-spawn Firecracker run-dirs (`/tmp/kastellan-microvm-*`, holding `fc.json`/`fc.log`/`vsock.sock`) from accumulating in a long-running daemon.

**Architecture:** Two layers. (1) The `kastellan-microvm-run` launcher — whose lifetime exactly matches the run-dir's need — removes its run-dir in its existing RAII teardown guard, on every graceful and panic exit. (2) A launcher-pid-keyed orphan sweep, run at the top of `LinuxFirecracker::spawn_under_policy`, GCs run-dirs left behind by SIGKILLed launchers. No `SandboxBackend` trait change.

**Tech Stack:** Rust, `std` only (no new dep; liveness via `/proc/<pid>` existence). Affected crates: `kastellan-sandbox` (Linux-gated) and `kastellan-microvm-run` (cross-platform binary).

## Global Constraints

- AGPL-3.0; AGPL-compatible deps only. **No new dependency** in this change (`std` + existing `libc` only; we use `/proc`, not `libc`).
- Cross-platform first-class, but `sandbox/src/linux_firecracker` is `#[cfg(target_os = "linux")]`: its tests run on the **DGX**; the Mac-side compile gate is `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings`. `kastellan-microvm-run` is **not** OS-gated → its tests run on macOS.
- Keep files under 500 LOC. `sandbox/src/linux_firecracker.rs` is 168 LOC today; new logic goes in a `cleanup.rs` sibling.
- Inline docs understandable to a junior contributor are mandatory.
- TDD: failing test first, then minimal implementation. Frequent commits.
- Stage **specific files** in every commit (`git add <path>`), never `git add -A` (an untracked `assets/agent_with_the_keys.png` must stay out).
- All tests must pass before committing.

---

## File Structure

- **Create** `sandbox/src/linux_firecracker/cleanup.rs` — the pure orphan-decision predicate, the I/O sweep, the `/proc` liveness check, and the two filename/prefix constants. Owns its own `#[cfg(test)]` module.
- **Modify** `sandbox/src/linux_firecracker.rs` — declare `mod cleanup` + re-export; write `<run_dir>/launcher.pid` after spawning the launcher `Child`; call the sweep at the top of `spawn_under_policy`; pass `--run-dir` in `launcher_argv`.
- **Modify** `workers/microvm-run/src/main.rs` — parse `--run-dir`; remove the run-dir in the teardown guard (subsuming today's base-UDS `remove_file`).

---

## Task 1: `cleanup` module — pure orphan-decision predicate

**Files:**
- Create: `sandbox/src/linux_firecracker/cleanup.rs`
- Modify: `sandbox/src/linux_firecracker.rs` (add `mod cleanup;` + re-export)

**Interfaces:**
- Produces:
  - `pub const LAUNCHER_PID_FILE: &str = "launcher.pid"`
  - `pub const RUN_DIR_PREFIX: &str = "kastellan-microvm-"`
  - `pub fn orphaned_run_dir_should_remove(pidfile: Option<String>, alive: impl Fn(u32) -> bool) -> bool`

- [ ] **Step 1: Create the module file with the constants + pure fn + failing tests**

Create `sandbox/src/linux_firecracker/cleanup.rs`:

```rust
//! Best-effort cleanup of orphaned per-spawn micro-VM run directories.
//!
//! Each micro-VM spawn gets a temp run-dir (`kastellan-microvm-<pid>-<seq>`)
//! holding `fc.json`, `fc.log`, and the per-spawn vsock UDS. The launcher
//! (`kastellan-microvm-run`) removes its own run-dir on every graceful/panic
//! exit (see `workers/microvm-run`). This module is the BACKSTOP for the one
//! case the launcher cannot self-clean: a launcher killed by SIGKILL (the
//! wall-clock watchdog, OOM, or PDEATHSIG when the daemon dies) never runs its
//! teardown, leaking its run-dir.
//!
//! The backstop is keyed on the launcher's OWN pid, written into
//! `<run_dir>/launcher.pid` by the backend right after spawn. The dir-NAME pid
//! is the daemon's pid (shared by every run-dir from one daemon), so it is
//! useless as a per-VM liveness signal; the pidfile is the authoritative one.

use std::path::Path;

/// Filename of the per-run pidfile each run-dir carries: the
/// `kastellan-microvm-run` launcher's PID, written by the backend after spawn.
pub const LAUNCHER_PID_FILE: &str = "launcher.pid";

/// Name prefix of every per-spawn run-dir under the system temp dir.
/// Kept in sync with `make_spawn_dir` in the parent module.
pub const RUN_DIR_PREFIX: &str = "kastellan-microvm-";

/// Pure decision: should an orphan sweep remove a run-dir, given the contents of
/// its pidfile (if any) and a liveness predicate?
///
/// Returns `true` ONLY when the pidfile is present AND parses to a PID the
/// `alive` predicate reports as dead. Every uncertain case returns `false` —
/// the sweep must never delete a dir it cannot prove belongs to a dead launcher:
/// - `None` (no pidfile yet — a dir still mid-spawn) → keep
/// - unparseable / whitespace-only contents → keep
/// - a live PID → keep
///
/// This conservatism is what makes the sweep safe to run concurrently with live
/// spawns: a false negative is a missed cleanup (caught next sweep); a false
/// positive would delete a running VM's dir, which this rules out.
pub fn orphaned_run_dir_should_remove(pidfile: Option<String>, alive: impl Fn(u32) -> bool) -> bool {
    match pidfile
        .as_deref()
        .map(str::trim)
        .and_then(|s| s.parse::<u32>().ok())
    {
        Some(pid) => !alive(pid),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_when_pidfile_names_a_dead_pid() {
        assert!(orphaned_run_dir_should_remove(Some("999".into()), |_| false));
    }

    #[test]
    fn keeps_when_pidfile_names_a_live_pid() {
        assert!(!orphaned_run_dir_should_remove(Some("999".into()), |_| true));
    }

    #[test]
    fn keeps_when_no_pidfile() {
        // A dir still mid-spawn (created, pidfile not yet written) must survive.
        assert!(!orphaned_run_dir_should_remove(None, |_| false));
    }

    #[test]
    fn keeps_when_pidfile_is_garbage() {
        assert!(!orphaned_run_dir_should_remove(Some("not-a-pid".into()), |_| false));
    }

    #[test]
    fn parses_pidfile_with_trailing_whitespace() {
        // Dead pid with a trailing newline (how the backend writes it) → remove.
        assert!(orphaned_run_dir_should_remove(Some("123\n".into()), |p| {
            assert_eq!(p, 123, "whitespace must be trimmed before parse");
            false
        }));
    }
}
```

Add to `sandbox/src/linux_firecracker.rs`, directly under the existing `mod probe; pub use probe::...;` block (around line 19):

```rust
mod cleanup;
pub use cleanup::{
    orphaned_run_dir_should_remove, sweep_orphaned_run_dirs, LAUNCHER_PID_FILE, RUN_DIR_PREFIX,
};
```

Note: `sweep_orphaned_run_dirs` is added in Task 2; this re-export references it ahead of time, so the crate will not compile until Task 2 lands. To keep Task 1 independently green, re-export only what exists now:

```rust
mod cleanup;
pub use cleanup::{orphaned_run_dir_should_remove, LAUNCHER_PID_FILE, RUN_DIR_PREFIX};
```

(Task 2 widens this re-export.)

- [ ] **Step 2: Run the tests to verify they pass (this is pure logic — they should pass immediately once written)**

Run (on the DGX, or via cross-clippy on the Mac to confirm it compiles):
```
cargo test -p kastellan-sandbox cleanup::tests
```
Expected on DGX: 5 passed. On the Mac the module is cfg'd out, so verify compilation instead:
```
cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings
```
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add sandbox/src/linux_firecracker/cleanup.rs sandbox/src/linux_firecracker.rs
git commit -m "feat(sandbox): pure orphan-run-dir decision for #362

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `cleanup` module — the I/O sweep + `/proc` liveness

**Files:**
- Modify: `sandbox/src/linux_firecracker/cleanup.rs`
- Modify: `sandbox/src/linux_firecracker.rs` (widen the re-export to include `sweep_orphaned_run_dirs`)

**Interfaces:**
- Consumes: `orphaned_run_dir_should_remove`, `LAUNCHER_PID_FILE`, `RUN_DIR_PREFIX` (Task 1).
- Produces:
  - `pub fn sweep_orphaned_run_dirs(temp_dir: &Path, alive: impl Fn(u32) -> bool) -> usize`
  - `pub fn pid_is_alive(pid: u32) -> bool`

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `sandbox/src/linux_firecracker/cleanup.rs`:

```rust
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Unique temp root per test so parallel runs don't collide.
    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);
    fn fresh_temp_root() -> std::path::PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "kastellan-sweeptest-{}-{}",
            std::process::id(),
            seq
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn make_run_dir(root: &Path, suffix: &str, pidfile: Option<&str>) -> std::path::PathBuf {
        let dir = root.join(format!("{RUN_DIR_PREFIX}{suffix}"));
        fs::create_dir_all(&dir).unwrap();
        if let Some(contents) = pidfile {
            fs::write(dir.join(LAUNCHER_PID_FILE), contents).unwrap();
        }
        dir
    }

    #[test]
    fn sweep_removes_dead_pid_dir_keeps_live_and_pidfileless() {
        let root = fresh_temp_root();
        let dead = make_run_dir(&root, "1-0", Some("100\n")); // dead
        let live = make_run_dir(&root, "1-1", Some("200\n")); // live
        let young = make_run_dir(&root, "1-2", None); // mid-spawn, no pidfile
        let other = root.join("unrelated-dir");
        fs::create_dir_all(&other).unwrap();

        // alive(): only pid 200 is "alive".
        let removed = sweep_orphaned_run_dirs(&root, |p| p == 200);

        assert_eq!(removed, 1, "exactly the dead-pid dir is removed");
        assert!(!dead.exists(), "dead-pid run-dir removed");
        assert!(live.exists(), "live-pid run-dir kept");
        assert!(young.exists(), "pidfile-less (mid-spawn) run-dir kept");
        assert!(other.exists(), "non-matching dir untouched");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sweep_on_missing_dir_returns_zero() {
        let missing = std::env::temp_dir().join("kastellan-sweeptest-does-not-exist-xyz");
        assert_eq!(sweep_orphaned_run_dirs(&missing, |_| false), 0);
    }

    #[test]
    fn pid_is_alive_true_for_self_false_for_unused() {
        // Our own pid is alive; pid 0 is never a normal process under /proc.
        assert!(pid_is_alive(std::process::id()));
        assert!(!pid_is_alive(0));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run (DGX): `cargo test -p kastellan-sandbox cleanup::tests`
Expected: FAIL — `cannot find function sweep_orphaned_run_dirs` / `pid_is_alive`.

- [ ] **Step 3: Implement the sweep + liveness**

Append to `sandbox/src/linux_firecracker/cleanup.rs` (after `orphaned_run_dir_should_remove`):

```rust
/// I/O: scan `temp_dir` for orphaned `kastellan-microvm-*` run-dirs and remove
/// them. A dir is orphaned when its `launcher.pid` names a dead PID (see
/// [`orphaned_run_dir_should_remove`]). Best-effort throughout: an unreadable
/// entry or a failed removal is skipped, never propagated. Returns the number of
/// dirs actually removed.
///
/// Called at the top of `spawn_under_policy` (before this spawn creates its own
/// dir), so it is naturally rate-matched to micro-VM use and never sees the
/// in-flight spawn's not-yet-created dir.
pub fn sweep_orphaned_run_dirs(temp_dir: &Path, alive: impl Fn(u32) -> bool) -> usize {
    let entries = match std::fs::read_dir(temp_dir) {
        Ok(e) => e,
        Err(_) => return 0, // temp dir unreadable/absent → nothing to do.
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_run_dir = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with(RUN_DIR_PREFIX));
        if !is_run_dir {
            continue;
        }
        let pidfile = std::fs::read_to_string(path.join(LAUNCHER_PID_FILE)).ok();
        if orphaned_run_dir_should_remove(pidfile, &alive) && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Linux liveness check via `/proc/<pid>` existence. No external dependency.
/// A reused pid (a dead launcher's pid now held by an unrelated process) reads
/// as alive → that dir is conservatively kept (a safe missed-cleanup).
pub fn pid_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}
```

Widen the re-export in `sandbox/src/linux_firecracker.rs`:

```rust
mod cleanup;
pub use cleanup::{
    orphaned_run_dir_should_remove, pid_is_alive, sweep_orphaned_run_dirs, LAUNCHER_PID_FILE,
    RUN_DIR_PREFIX,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run (DGX): `cargo test -p kastellan-sandbox cleanup::tests`
Expected: 8 passed.
Mac compile gate: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings` → clean.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_firecracker/cleanup.rs sandbox/src/linux_firecracker.rs
git commit -m "feat(sandbox): orphan run-dir sweep + /proc liveness for #362

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Launcher self-cleans its run-dir (`--run-dir`)

**Files:**
- Modify: `workers/microvm-run/src/main.rs`

**Interfaces:**
- Produces: a `--run-dir <path>` launcher flag; teardown removes that dir.
- Consumes: nothing from other tasks (this crate is standalone, std-only).

- [ ] **Step 1: Write the failing test**

The launcher's `main` spawns firecracker, so it is not unit-testable directly; extract the run-dir removal into a tiny pure-ish helper and test that. Add to the bottom of `workers/microvm-run/src/main.rs`:

```rust
/// Best-effort removal of the per-spawn run-dir on launcher exit. Separated from
/// the teardown closure so it is unit-testable without booting a VM. Removing
/// the whole dir subsumes removing the base vsock UDS (which lives inside it).
fn remove_run_dir(run_dir: &str) {
    let _ = std::fs::remove_dir_all(run_dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_run_dir_deletes_the_directory_tree() {
        let dir = std::env::temp_dir().join(format!(
            "kastellan-microvm-runtest-{}-{}",
            std::process::id(),
            "a"
        ));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("fc.json"), "{}").unwrap();
        assert!(dir.exists());

        remove_run_dir(&dir.to_string_lossy());

        assert!(!dir.exists(), "remove_run_dir must delete the whole tree");
    }

    #[test]
    fn remove_run_dir_is_noop_on_missing_dir() {
        // Must not panic when the dir is already gone.
        remove_run_dir("/tmp/kastellan-microvm-runtest-definitely-absent-zzz");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kastellan-microvm-run remove_run_dir`
Expected: FAIL — `cannot find function remove_run_dir`.

- [ ] **Step 3: Add `remove_run_dir` (above) and wire `--run-dir` into `main`**

In `workers/microvm-run/src/main.rs`, in `main()`, parse the new optional flag near the other `arg(...)` calls (after the `log` line, ~line 26):

```rust
    // Per-spawn run-dir to remove on exit (#362). Optional for backward
    // compatibility with callers that don't pass it; when absent we fall back
    // to removing just the base vsock UDS, as before.
    let run_dir = arg("--run-dir");
```

Replace the existing teardown guard block:

```rust
    let uds_for_guard = vsock_uds.clone();
    let teardown = scopeguard(move || {
        let _ = fc.kill();
        let _ = std::fs::remove_file(&uds_for_guard);
    });
```

with:

```rust
    let uds_for_guard = vsock_uds.clone();
    let run_dir_for_guard = run_dir.clone();
    let teardown = scopeguard(move || {
        let _ = fc.kill();
        // Remove the whole per-spawn run-dir when we know it (#362); this
        // subsumes the base-UDS removal since the UDS lives inside it. When the
        // flag is absent (older caller / a direct test), fall back to the UDS.
        match run_dir_for_guard {
            Some(dir) => remove_run_dir(&dir),
            None => {
                let _ = std::fs::remove_file(&uds_for_guard);
            }
        }
    });
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p kastellan-microvm-run`
Expected: PASS (the two new tests + the existing `boot`/`bridge` tests).

- [ ] **Step 5: Commit**

```bash
git add workers/microvm-run/src/main.rs
git commit -m "feat(microvm-run): self-clean per-spawn run-dir on exit (#362)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Backend wiring — pass `--run-dir`, write pidfile, sweep on spawn

**Files:**
- Modify: `sandbox/src/linux_firecracker.rs`

**Interfaces:**
- Consumes: `sweep_orphaned_run_dirs`, `pid_is_alive`, `LAUNCHER_PID_FILE` (Tasks 1–2); the `--run-dir` flag (Task 3).
- Produces: the fully-wired cleanup behavior; no new public surface.

- [ ] **Step 1: Update the `launcher_argv` test to assert `--run-dir` is passed**

In `sandbox/src/linux_firecracker.rs`, the `launcher_argv` signature gains a `run_dir` parameter. First update the existing test `launcher_argv_passes_config_and_vsock` (in `mod spawn_tests`) to call the new signature and assert the flag:

```rust
        let argv = launcher_argv(&plan, "/run/fc.json", "/run/fc.log", "/run");
        assert_eq!(argv[0], MICROVM_RUN_BIN);
        assert!(
            argv.windows(2).any(|w| w[0] == "--config-file" && w[1] == "/run/fc.json"),
            "argv must pass --config-file /run/fc.json"
        );
        assert!(
            argv.windows(2).any(|w| w[0] == "--run-dir" && w[1] == "/run"),
            "argv must pass --run-dir <dir>"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--vsock-port" && w[1] == plan.vsock_port.to_string()),
            "argv must pass --vsock-port <port>"
        );
```

- [ ] **Step 2: Run the test to verify it fails (signature mismatch)**

Run (DGX): `cargo test -p kastellan-sandbox launcher_argv_passes_config_and_vsock`
Expected: FAIL to compile — `launcher_argv` takes 3 args, 4 supplied.
Mac: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets` shows the same arity error.

- [ ] **Step 3: Add `run_dir` to `launcher_argv`**

Replace the `launcher_argv` fn (~lines 30–38):

```rust
/// Pure: the launcher argv for a plan + its rendered config/log/run-dir paths.
pub fn launcher_argv(
    plan: &FirecrackerLaunchPlan,
    config_path: &str,
    log_path: &str,
    run_dir: &str,
) -> Vec<String> {
    vec![
        MICROVM_RUN_BIN.into(),
        "--config-file".into(), config_path.into(),
        "--vsock-uds".into(), plan.vsock_uds.to_string_lossy().into_owned(),
        "--vsock-port".into(), plan.vsock_port.to_string(),
        "--log".into(), log_path.into(),
        "--run-dir".into(), run_dir.into(),
    ]
}
```

- [ ] **Step 4: Wire the sweep, the run-dir arg, and the pidfile into `spawn_under_policy`**

In `spawn_under_policy`, add the sweep as the **first** statement of the method body (before the image-dir lookup, ~line 97):

```rust
        // Backstop GC (#362): remove run-dirs left by SIGKILLed launchers whose
        // own pid is now dead. Runs before we create THIS spawn's dir, so it
        // never races the in-flight spawn. Best-effort; ignores the count.
        let _ = cleanup::sweep_orphaned_run_dirs(&std::env::temp_dir(), cleanup::pid_is_alive);
```

Update the `launcher_argv` call (it currently passes 3 args, ~line 124) to pass the run-dir:

```rust
        let argv = launcher_argv(
            &plan,
            &config_path.to_string_lossy(),
            &log_path.to_string_lossy(),
            &run_dir.to_string_lossy(),
        );
```

Replace the final `Command::new(...).spawn().map_err(...)` tail (~lines 129–135) so it captures the child, writes the pidfile, and returns the child:

```rust
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SandboxError::Backend(format!("microvm-run spawn failed: {e}")))?;
        // Record the launcher's own pid so the orphan sweep can later tell this
        // VM's run-dir from a dead one (#362). Best-effort: a write failure only
        // means this one dir won't be swept if its launcher is later SIGKILLed;
        // the launcher's own teardown still cleans the dir on a graceful exit.
        let _ = std::fs::write(
            run_dir.join(cleanup::LAUNCHER_PID_FILE),
            child.id().to_string(),
        );
        Ok(child)
```

- [ ] **Step 5: Run the tests to verify they pass**

Run (DGX): `cargo test -p kastellan-sandbox`
Expected: all green (the updated `launcher_argv` test + the `cleanup` suite + existing sandbox tests).
Mac compile gate: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings` → clean.

- [ ] **Step 6: Commit**

```bash
git add sandbox/src/linux_firecracker.rs
git commit -m "feat(sandbox): wire run-dir cleanup into firecracker spawn (#362)

Pass --run-dir to the launcher, write <run_dir>/launcher.pid after spawn,
and sweep dead-launcher orphan dirs at the top of spawn_under_policy.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: DGX e2e — a completed spawn leaves no run-dir behind

**Files:**
- Modify: `core/tests/python_exec_firecracker_e2e.rs`

**Interfaces:**
- Consumes: the wired cleanup (Tasks 1–4) end to end through a real VM boot.

This task runs **only on the DGX** (real KVM/firecracker). On macOS it is skip-as-pass like the rest of that suite.

- [ ] **Step 1: Read the existing e2e to find the post-call point + skip guard**

Run: open `core/tests/python_exec_firecracker_e2e.rs` and locate (a) the skip guard used when KVM/firecracker is unavailable, and (b) a successful `python.exec` round-trip test after which the worker/VM has exited. The new assertion attaches at the end of that existing successful test (or a sibling that reuses its harness), after the worker `Child` has been dropped/reaped.

- [ ] **Step 2: Add the no-leak assertion**

After the worker has exited in the chosen test, assert no run-dir remains. Use a snapshot-diff to avoid flaking on unrelated dirs from concurrent tests:

```rust
    // #362: after a completed micro-VM spawn, its per-spawn run-dir must be gone
    // (the launcher self-cleans on graceful exit). Count run-dirs before and
    // after is racy under parallel tests, so assert the specific dirs that exist
    // now all belong to STILL-LIVE launchers (pidfile pid alive) — a leaked dir
    // from this finished spawn would have a dead pidfile pid.
    let temp = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(&temp) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("kastellan-microvm-") || !path.is_dir() {
                continue;
            }
            if let Ok(pid_str) = std::fs::read_to_string(path.join("launcher.pid")) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    assert!(
                        std::path::Path::new(&format!("/proc/{pid}")).exists(),
                        "leaked run-dir {path:?}: launcher pid {pid} is dead but dir survived"
                    );
                }
            }
        }
    }
```

(Place this inside the existing `#[cfg(target_os = "linux")]` / KVM-gated test body so it is naturally skipped where firecracker can't run.)

- [ ] **Step 3: Run on the DGX**

Run (DGX): `cargo test -p kastellan-core --test python_exec_firecracker_e2e -- --nocapture`
Expected: the firecracker e2e suite passes (5/5 as in the slice-1 baseline) with the new assertion green; `0 orphaned VMs`.

- [ ] **Step 4: Commit**

```bash
git add core/tests/python_exec_firecracker_e2e.rs
git commit -m "test(firecracker): assert spawn leaves no orphan run-dir (#362)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Final verification + docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md` (session-end update)

- [ ] **Step 1: Full Mac gate**

Run:
```
cargo test -p kastellan-microvm-run
cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings
cargo clippy -p kastellan-microvm-run --all-targets -- -D warnings
```
Expected: microvm-run tests green; both clippy runs clean.

- [ ] **Step 2: Full DGX gate (over SSH)**

Run:
```
ssh dgx 'cd <repo> && source ~/.cargo/env && cargo test -p kastellan-sandbox && cargo test -p kastellan-microvm-run && cargo test -p kastellan-core --test python_exec_firecracker_e2e && cargo clippy --workspace --all-targets -- -D warnings'
```
Expected: sandbox + microvm-run + firecracker e2e all green; workspace clippy clean; 0 orphaned VMs.

- [ ] **Step 3: Update HANDOVER.md + ROADMAP.md**

Move the #362 entry from "Next TODO / remaining slice-1 follow-up" to a "Recently completed" summary (what shipped: launcher self-clean + pid-keyed orphan sweep, both-platform verification counts, file paths, the documented mid-spawn-SIGKILL residual). Prune older entries to keep under 500 lines. Mark #362 done in the slice-1 follow-up list.

- [ ] **Step 4: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: #362 firecracker run-dir cleanup done; handover + roadmap

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open PR**

```bash
git push -u origin feat/362-firecracker-rundir-cleanup
gh pr create --base main --title "feat(sandbox): Firecracker per-spawn run-dir cleanup (#362)" \
  --body "Closes #362. Launcher self-cleans its run-dir on every graceful/panic exit; a launcher-pid-keyed orphan sweep at the top of spawn_under_policy backstops the SIGKILL case. No SandboxBackend trait change. Spec + plan under docs/superpowers. Verified: macOS (microvm-run tests + sandbox cross-clippy) + DGX (sandbox/microvm-run/firecracker-e2e green, 0 orphaned VMs)."
```

(If `git push` from the Mac times out, use the DGX relay: `git format-patch origin/main..HEAD --stdout | ssh dgx 'cd <repo> && git am' && ssh dgx 'cd <repo> && git push'`, then `gh pr create` from the Mac.)

---

## Self-Review

- **Spec coverage:** Layer 1 (launcher self-clean) → Task 3 + the `--run-dir` wiring in Task 4. Layer 2 (pidfile + pure predicate + sweep + `/proc` liveness + top-of-spawn call) → Tasks 1, 2, 4. Concurrency-safety cases (no pidfile / live / dead / pid-reuse) → Task 1 tests + Task 2 sweep test. Testing reality (Linux-gated → DGX, launcher → Mac) → tasks specify both gates. E2e no-leak → Task 5. Residual (mid-spawn SIGKILL) → documented in spec, no task (out of scope by design). All covered.
- **Placeholder scan:** none — every code step shows full code; commands have expected output.
- **Type consistency:** `orphaned_run_dir_should_remove(Option<String>, impl Fn(u32)->bool) -> bool`, `sweep_orphaned_run_dirs(&Path, impl Fn(u32)->bool) -> usize`, `pid_is_alive(u32) -> bool`, `LAUNCHER_PID_FILE`/`RUN_DIR_PREFIX` consts, `launcher_argv(&plan, config, log, run_dir)`, launcher `--run-dir` + `remove_run_dir(&str)` — used consistently across Tasks 1–5. Task 1's re-export is deliberately narrow and widened in Task 2 (noted inline) so each task compiles green.
