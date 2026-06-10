# Issue #130 — serialize launchd bring-up in `bring_up_pg_cluster` (macOS)

**Date:** 2026-05-30
**Issue:** [#130](https://github.com/hherb/kastellan/issues/130) — parallel-launchd bring-up contention under `KASTELLAN_PG_BIN_DIR` override
**Scope:** test-infra only (`kastellan-tests-common`); no production code touched.

## Problem

When `KASTELLAN_PG_BIN_DIR` points at a Postgres.app `bin/` and the full
workspace runs (`cargo test --workspace`), some `*_e2e.rs` tests time out at
`pg active: timeout 30s; last=Inactive`, even though each file passes
individually under the same override. Widening the 30 s cap
(`PG_BRING_UP_TIMEOUT_SECS`) only defers the symptom.

## Diagnosis

Two distinct contention sources touch the launchd `gui/<uid>` domain:

1. **Within a binary.** Files like `secret_vault_e2e` (9 tests),
   `postgres_e2e` (8), `injection_guard_e2e` (6) run their `#[test]`s on
   parallel threads, so 6–9 launchd `bootstrap`s fire simultaneously. This is
   genuine in-process concurrency.
2. **Across binaries.** `cargo test` runs integration-test binaries
   *sequentially*, but `bootout` (the `ServiceGuard` drop) is asynchronous, so
   unregistration tails overlap the next binary's bootstrap, and the
   `gui/<uid>` domain accumulates churn over a ~32-binary run.

`tests-common::serial::serial_lock()` is a **process-local** `static Mutex<()>`
— it can serialize source (1) but never (2). `bring_up_pg_cluster` currently
takes **no** lock at all; only 4 test files take `serial_lock()` manually.

"Passes alone, times out in the workspace" fits: the within-binary N-way
bring-up is constant, but only tips past 30 s once the domain is degraded by
cumulative cross-binary churn. Reducing peak concurrent launchd registration
to **1** (source 1) is the highest-leverage, lowest-risk lever, and is the fix
the operator selected (issue Option 1).

## Approved approach

Serialize the launchd-touching window of `bring_up_pg_cluster` against the
*same* lock that daemon-spawning tests already use, releasing it before the
cluster handle is returned (so `initdb` and the post-bring-up test body stay
parallel). Two coupled parts:

### Part 1 — `serial.rs`: make the lock reentrant

`supervisor_e2e.rs:106` and `observation_capture.rs:469` take `serial_lock()`
and **then** call `bring_up_pg_cluster` (line 127 / 514) on the same thread.
If `bring_up` locks the same *non-reentrant* `std::Mutex`, that is an instant
self-deadlock.

Switch the macOS `serial_mutex()` from `Mutex<()>` to
`std::sync::ReentrantLock<()>` (stable since Rust 1.78; toolchain is 1.96).
`serial_lock()` returns `ReentrantLockGuard<'static, ()>`. `ReentrantLock` does
not poison, so the `unwrap_or_else(into_inner)` poison handling is dropped.

Behavioral delta for the existing 4 callers: same-thread re-acquire no longer
deadlocks; cross-thread mutual exclusion is **unchanged**. Strictly more
permissive. Linux stays a `()` no-op.

A *separate* PG lock was rejected: daemon-spawning tests and PG tests in the
same binary would then race the `gui/<uid>` domain against each other — the
exact contention being fixed. PG bring-up must share the one launchd lock.

### Part 2 — `pg.rs`: take the lock for the launchd window

In `bring_up_pg_cluster_with_timeout`, immediately before `sup.install`:

```rust
// Serialize the launchd gui/<uid> registration window against every other
// daemon-spawning test in this process (issue #130). cfg-gated so Linux has
// no unit-binding (clippy-clean under the -D warnings gate) and no import.
#[cfg(target_os = "macos")]
let _serial = crate::serial::serial_lock();
```

The guard drops as the function returns the handle, after the `Active`/socket
waits + 500 ms recheck. `initdb` / socket-dir / `auto.conf` stay **outside** the
lock (no launchd interaction → kept parallel for throughput).

Why `cfg`-gate the `let` rather than rely on `serial_lock()` → `()` on Linux:
avoids any `clippy::let_unit_value` / unused-import risk now that `linux-check`
runs `-- -D warnings` (#153). No `use` is added; the full path is inlined.

## TDD

New tests in `serial.rs` (macOS-only, fast, deterministic):

- `serial_lock_is_reentrant_on_same_thread` — **RED-first.** A spawned thread
  acquires `serial_lock()` twice; assert it completes within 5 s via
  `mpsc::recv_timeout`. On today's `Mutex` the second acquire deadlocks → the
  thread never signals → timeout → fail. On `ReentrantLock` → passes.
- `serial_lock_excludes_across_threads` — regression pin. 4 threads each
  increment a holder counter under the lock, sleep briefly, record the running
  max via `fetch_max`, decrement. Assert the max observed == 1 (proves the
  switch did not weaken cross-thread exclusion). GREEN on both old and new.

The live "no deadlock under real PG" pin already exists: `supervisor_e2e` and
`observation_capture` hold `serial_lock()` across the `bring_up` call and would
hang under live PG if `bring_up`'s lock were not reentrant.

## Files changed

- `tests-common/src/serial.rs` — `Mutex` → `ReentrantLock`; drop poison
  handling; doc update; +2 tests (~60 LOC). Stays well under the 500-LOC cap.
- `tests-common/src/pg.rs` — +1 `cfg`-gated lock acquisition (~5 LOC incl.
  comment); doc note on the macOS serialization. 317 → ~325 LOC.

## Verification

- `cargo test -p kastellan-tests-common` — the two new `serial` tests pass.
- `cargo test --workspace` — stays **1153 / 0 / 3** on macOS (no behavior delta
  to existing tests; skip-as-pass posture without `KASTELLAN_PG_BIN_DIR`).
- `cargo clippy --workspace --all-targets -- -D warnings` — exit 0 (macOS 1.96).
- Optional operator validation: full-workspace run with
  `KASTELLAN_PG_BIN_DIR='/Applications/Postgres 2.app/Contents/Versions/18/bin/'`
  to confirm the flake is gone (and the `--test-threads=1` workaround is no
  longer required).

## Implementation amendments (2026-05-30, post-design)

Two deviations surfaced during implementation; both operator-approved:

1. **Mechanism: `parking_lot::ReentrantMutex`, not `std::sync::ReentrantLock`.**
   The std type is still unstable on the 1.96 toolchain (feature
   `reentrant_lock`). `parking_lot` 0.12 is already in the build graph
   (MIT/Apache-2.0), has a stable `ReentrantMutex`, and was added as a direct
   dev-dependency of `tests-common`. Same semantics (reentrant, no poison).

2. **Bundled fix for a separate bug found during live validation
   ([#163](https://github.com/hherb/kastellan/issues/163)).** Validating live
   against Postgres.app v18 revealed `injection_guard_e2e` could never come up
   live — but the cause was *not* #130 contention (it reproduces with
   `--test-threads=1`). Its fixture built an over-long data label, overflowing
   macOS's 104-byte `sun_path`, so postgres silently failed to bind the socket
   and bring-up timed out. Bundled here (same theme — macOS PG bring-up
   reliability under the override):
   - `injection_guard_e2e`: shortened labels (`ig-{label}-d` / `-l`).
   - `bring_up_pg_cluster`: new pure `check_socket_path_fits` guard
     (per-OS `SUN_PATH_MAX` = 104 macOS / 108 Linux) that fails fast with a
     clear message instead of a 30 s timeout. TDD: stub → RED → implement.

   Live result after the fix: `injection_guard_e2e` 6/6, `secret_vault_e2e`
   11/11, `postgres_e2e` 57/57, zero bring-up timeouts.

## Out of scope

- Cross-process serialization of source (2) — `cargo` already runs binaries
  sequentially, so a process-local lock plus the bootstrap-window
  serialization is expected to be sufficient. A `flock(2)`-based cross-process
  lock (issue Option 2) was considered and deferred; revisit only if the
  workspace flake persists after this change.
- `supervisor/tests/launchd_agents_smoke.rs` keeps its own local `serial_lock`
  copy (separate Mutex, no PG bring-up) — pre-existing, unaffected.
