# Firecracker micro-VM per-spawn run-dir cleanup — design

**Issue:** [#362](https://github.com/hherb/kastellan/issues/362)
**Date:** 2026-06-27
**Status:** design approved; implementation pending

## Problem

`LinuxFirecracker::spawn_under_policy` (`sandbox/src/linux_firecracker.rs`)
creates a per-spawn temp directory `/tmp/kastellan-microvm-<daemon-pid>-<seq>/`
holding the rendered `fc.json`, the firecracker `fc.log`, and the per-spawn
`vsock.sock`. The directory is never removed. For a long-running daemon these
accumulate, one per micro-VM spawn.

Cleanup is non-trivial for two reasons the issue calls out:

1. The directory must **outlive the spawned `Child`** — firecracker reads
   `fc.json` at boot and writes `fc.log` for the VM's whole life, and the vsock
   UDS lives there too.
2. `SandboxBackend::spawn_under_policy` returns a **bare `std::process::Child`**
   with no teardown hook, and the trait is `dyn`-safe with ~40 call sites, so
   changing its return type to thread an RAII run-dir out is a wide ripple we
   want to avoid.

## Key insight

The run-dir's lifetime requirement matches **exactly one process**: the
`kastellan-microvm-run` launcher that the backend spawns as the `Child`.
Firecracker is *that launcher's* own child; once the launcher exits, firecracker
is killed and nothing else ever touches `fc.json`/`fc.log`/`vsock.sock`. The
launcher already owns an RAII `scopeguard` teardown that runs on every
normal-return and panic path (today it kills firecracker and removes the base
UDS). That guard is the natural, trait-change-free place to remove the run-dir.

The dir-name pid is the **daemon's** pid (`std::process::id()` in
`make_spawn_dir`), shared by every run-dir from one daemon, so it is useless as a
liveness signal *within* a single daemon run. The authoritative per-VM liveness
signal is the **launcher Child's own pid**.

## Design

Two layers, mapping to the two options named in the issue.

### Layer 1 — self-cleaning launcher (steady-state fix)

- The backend passes the run-dir to the launcher explicitly via a new
  `--run-dir <path>` flag (clearer and more testable than deriving
  parent-of-`--config-file`).
- The launcher's existing teardown guard is extended to
  `std::fs::remove_dir_all(run_dir)` **after** firecracker is killed. This
  subsumes today's single `remove_file(base_uds)` because the base UDS lives
  inside the run-dir.

Result: on every graceful close (worker stdin EOF → `pump` returns) and every
panic path, the run-dir is removed precisely when the VM is gone. No
`SandboxBackend` trait change. This alone closes the accumulation the issue is
about for normal operation.

### Layer 2 — orphan sweep backstop (SIGKILL case)

When the launcher is SIGKILLed (wall-clock watchdog kill, OOM, or `PDEATHSIG`
when the daemon dies) its guard cannot run, leaking that one run-dir. Backstop:

- After spawning the launcher `Child`, the backend writes
  `<run_dir>/launcher.pid` = `child.id()` (best-effort; a write failure is
  logged, not fatal — the spawn already succeeded).
- New module `sandbox/src/linux_firecracker/cleanup.rs`:
  - **pure** `orphaned_run_dir_should_remove(pidfile: Option<String>, alive: impl Fn(u32) -> bool) -> bool`
    — returns `true` **only** when the pidfile parses to a pid that is *not*
    alive. `None` (no pidfile yet) or unparseable contents → `false`
    (conservative: never delete what we cannot prove dead).
  - **I/O** `sweep_orphaned_run_dirs(temp_dir: &Path, alive: impl Fn(u32) -> bool) -> usize`
    — scans `temp_dir` for `kastellan-microvm-*` entries, reads each
    `launcher.pid`, applies the predicate, `remove_dir_all`s the orphans,
    returns the count removed (best-effort per entry; a remove error is skipped,
    not propagated).
  - `pid_is_alive(pid: u32) -> bool` — `/proc/<pid>` existence check
    (Linux-only, no new dependency).
- `spawn_under_policy` calls `sweep_orphaned_run_dirs(&std::env::temp_dir(), pid_is_alive)`
  at its **top**, before creating this spawn's dir. Self-contained in the
  sandbox crate (no `main.rs`/cross-crate wiring), and naturally rate-matched to
  micro-VM use (a sweep only happens when a micro-VM is actually spawned).

### Concurrency & correctness

The sweep removes a dir only when it has a present `launcher.pid` naming a *dead*
pid. Therefore:

- A live VM's dir → pidfile names a live pid → **skipped**.
- A sibling spawn still mid-flight (dir created, pidfile not yet written) →
  no pidfile → **skipped**.
- This spawn's own dir does not exist yet when the sweep runs → not seen.
- Pid reuse (a dead launcher's pid reused by an unrelated live process) →
  `alive` returns `true` → **skipped** (a safe false-negative: a missed
  cleanup, never a wrongful delete).

The sweep can therefore never remove a live VM's run-dir — no false positives.

## Testing (TDD)

**Test-exec reality:** `sandbox/src/linux_firecracker` (hence `cleanup.rs` and
its tests) is `#[cfg(target_os = "linux")]`, so these tests **compile and run
only on Linux (the DGX)**; the Mac-side gate is
`cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu
--all-targets -D warnings` (pure-Rust crate → cross-clippy works without a
linker). The `microvm-run` launcher crate is **not** OS-gated, so its tests run
directly in `cargo test` on macOS.

- `orphaned_run_dir_should_remove` — pure unit tests (dead pid → true; live pid
  → false; `None` → false; garbage/whitespace → false). DGX (Linux-gated module).
- `sweep_orphaned_run_dirs` — tests over real temp dirs with an **injected**
  `alive` closure: a dead-pid dir is removed, a live-pid dir is kept, a
  pidfile-less dir is kept, a non-matching dir name is ignored, the returned
  count matches. The injected closure keeps the assertions deterministic
  (no real pids); only the real `pid_is_alive` (`/proc`) is exercised by the e2e.
  DGX (Linux-gated module).
- launcher (`microvm-run`) — `--run-dir` arg parsing + a run-dir-removal helper
  unit test (create a temp dir, run the removal, assert gone). **Runs on macOS**
  (crate is not OS-gated).
- End-to-end wiring (pidfile written after a real spawn; teardown removes the
  dir on real VM exit; the top-of-spawn sweep) is covered by the existing DGX
  `python_exec_firecracker_e2e` suite plus an assertion that a completed spawn
  leaves no `kastellan-microvm-*` dir behind.

## File-size / structure

- `sandbox/src/linux_firecracker.rs` (168 LOC today) gains the pidfile write +
  the top-of-spawn sweep call + the `mod cleanup; pub use …` (~12 LOC); stays
  well under the 500-LOC cap.
- New `sandbox/src/linux_firecracker/cleanup.rs` holds the pure predicate, the
  sweep, `pid_is_alive`, the `LAUNCHER_PID_FILE` constant, and their tests.
- `workers/microvm-run/src/main.rs` gains `--run-dir` parsing and the run-dir
  removal in the teardown guard (~8 LOC).

## Residual (documented, out of scope)

A launcher SIGKILLed in the microseconds between dir-create and pidfile-write
leaves a pidfile-less dir the conservative sweep will never touch. This is
vanishingly rare. An `mtime`-age fallback (sweep pidfile-less dirs older than N
minutes) is a future option if ever observed in practice; not implemented now
(YAGNI).

## Non-goals

- No change to the `SandboxBackend` trait signature.
- No periodic/timer-based reaper thread — the top-of-spawn sweep plus the
  launcher self-clean cover the realistic cases.
- No new crate dependency (`/proc` existence check, not `libc`/`nix`).
