# Matrix-worker seccomp/Landlock enforcement flip — design

**Date:** 2026-06-24
**Status:** approved (brainstorming) → implementation
**Branch:** `feat/matrix-worker-sandbox-enforcement`

## Problem

The live Matrix channel worker (`kastellan-worker-matrix`, `live-matrix`
feature) runs today with **seccomp + Landlock disabled**: the daemon's
`install` step writes `KASTELLAN_MATRIX_ENFORCE_SANDBOX=0`
(`core/src/install/plan.rs:142`), and the spawn path
(`core/src/channel/matrix.rs::spawn_matrix_worker`) responds by pushing
`KASTELLAN_SECCOMP_PROFILE=none` + `KASTELLAN_LANDLOCK_PROFILE=none` into the
worker's jail env. So the only containment the Matrix worker has is the bwrap /
Seatbelt mount + net namespace — not the worker-side syscall filter that every
other worker (shell-exec, web-fetch, web-search, egress-proxy, python-exec,
browser-driver, gliner-relex) now runs under.

This was the deliberate first-bring-up posture: get the matrix-rust-sdk
integration correct before adding the syscall filter on top. The SDK is now
DGX-verified live (PR #320 / #321), so this is the follow-up that closes the
gap.

## Goal

Run the live Matrix worker under `net_client` seccomp **+** Landlock by default
in the supervised deployment, with the `enforce_sandbox=0` path retained as an
explicit operator debug escape hatch.

## Key finding: the enforcement plumbing already exists

No new plumbing is needed. The mechanism is fully wired:

- `build_matrix_policy` (`core/src/channel/matrix.rs:334`) already sets
  `Profile::WorkerNetClient`, `fs_write=[store_dir]`, and
  `fs_read=[binary, /etc/resolv.conf, /etc/hosts, /etc/nsswitch.conf, CA bundle paths]`.
- `spawn_worker_client` calls `derive_lockdown_env(policy)`
  (`core/src/tool_host/lockdown_env.rs`), which translates that policy into
  `KASTELLAN_SECCOMP_PROFILE=net_client` + `KASTELLAN_LANDLOCK_RW=[store_dir]` +
  `KASTELLAN_LANDLOCK_RO=[fs_read…]`.
- The worker's `main.rs` (live-matrix) calls `rlimit::apply_from_env()` then
  `lock_down()` **after** the network init, so the filter is applied once the
  SDK login + first sync are done and the background sync task keeps running
  under `net_client` (which permits the BSD-socket family).
- The `enforce_sandbox=false` branch (`matrix.rs:599`) only **overrides** the
  derived values with `none`/`none`.

So "enabling enforcement" reduces to: **stop emitting the `none`/`none`
override, and prove the worker survives under the filter.** The risk is purely
empirical — whether bare `net_client` covers everything matrix-rust-sdk needs.

## What matrix-rust-sdk brings that bare `net_client` may not cover

`net_client` = `BASE_ALLOW` + `NET_CLIENT_ADDITIONS` (the BSD-socket family).
matrix-rust-sdk 0.18 additionally exercises:

- **A multi-thread tokio runtime** (`LiveSdk` owns a `Runtime`) — thread
  creation/teardown, `futex`, `epoll`, `eventfd`. (The egress-proxy + web-fetch
  already run tokio under `net_client`, so most of this is likely covered.)
- **A SQLite crypto store** (`matrix-sdk-sqlite` → `rusqlite`, bundled sqlite) —
  `mmap`/`munmap`/`mremap`, `fsync`/`fdatasync`, `fcntl`, `statx`, possibly
  `fallocate`. This is the most likely source of gaps; no other `net_client`
  worker uses an embedded SQLite DB.
- **rustls** native-cert TLS — `getrandom` (in `BASE_ALLOW`).

The actual set is determined empirically on the DGX, exactly as the `ml_client`
profile (#281) was enumerated.

## Architecture decisions (locked in brainstorming)

1. **Dedicated profile if (and only if) additions are needed.** If DGX
   enumeration shows `net_client` needs extra syscalls for matrix-sdk, add a
   new `Profile::WorkerMatrixClient` + `MATRIX_CLIENT_ADDITIONS`, mirroring the
   existing `BrowserClient`/`MlClient` precedent. This keeps least-privilege:
   only the Matrix worker gets the extra syscalls; web-fetch / web-search /
   egress-proxy keep the tighter bare `net_client`. If bare `net_client`
   survives unchanged, **no new profile** is added.

2. **Explicit `=1` in the rendered env file.** `install/plan.rs` writes
   `KASTELLAN_MATRIX_ENFORCE_SANDBOX=1` (not omitted). The runtime already
   defaults on when the var is absent (`parse_daemon_spawn_config`,
   `matrix.rs:444`), but an explicit `=1` is self-documenting and gives the
   operator an obvious knob to flip to `0` for debugging.

3. **Fail-closed sequencing.** Enumerate + verify on a throwaway dev-e2e
   homeserver FIRST; flip the install default LAST, only after the live e2e
   passes under enforcement. If we flipped the default while the worker couldn't
   survive the filter, the supervised channel would enter an endless
   respawn loop (spawn → `SIGSYS` death → backoff → respawn). The production
   channel stays on `enforce_sandbox=0` until the final deploy step.

## Components / work breakdown

### A. Empirical enumeration (DGX — the real work)

Drive the DGX over `ssh dgx` against the **throwaway** loopback homeserver from
`scripts/matrix/dev-e2e-bootstrap.sh up` (never the live production channel).
Loop, per the #281 / `dgx-seccomp-syscall-enumeration` memory note:

1. Build the worker with `--features live-matrix` and the seccomp filter in
   **kill mode** (the default — a denied syscall `SIGSYS`-kills the worker).
2. Run the `#[ignore]` `core/tests/matrix_live_e2e.rs` round-trip under
   `enforce_sandbox=true`.
3. On a kill: `journalctl -k | grep type=1326` names the first missing syscall
   (`syscall=<nr>`). (`adm` group on the DGX gives unprivileged access; `dmesg`
   is unusable — `dmesg_restrict=1`.)
4. Add the syscall to `MATRIX_CLIENT_ADDITIONS`, rebuild `--workspace`, repeat
   until the worker survives login + sync + a send/recv round-trip.
5. Watch the worker **stderr** for Landlock `EACCES` (a *different* failure mode
   from `SIGSYS` — Landlock denials are permission errors, not kernel-audit
   records). If matrix-sdk's SQLite writes outside the store dir, add that path
   to `fs_write`. (Expected: none — SQLite keeps WAL/journal beside the DB.)

Every syscall added is documented with its captured audit record in the profile
const's doc comment, same as `ML_CLIENT_ADDITIONS`.

### B. Seccomp profile (TDD — only if A finds gaps)

In `workers/prelude/src/seccomp_lock.rs`:

- Add `Profile::WorkerMatrixClient` variant (parse arm `"matrix_client"`,
  display/round-trip).
- Add `MATRIX_CLIENT_ADDITIONS: &[i64]` = the DGX-enumerated syscalls, with a
  doc comment listing each + why.
- Extend `allow_list_for`: `matches!(profile, … | MatrixClient)` for the
  net-socket family, plus a `MatrixClient` arm for the additions.
- **Tests:** `seccomp_smoke` pins `build_bpf(MatrixClient)` succeeds; a unit
  test pins the additions are exactly `net_client` ∪ `MATRIX_CLIENT_ADDITIONS`
  (mirrors the existing `ml_client` difference test at line ~806).

In the prelude's `Profile` parse table (`KASTELLAN_SECCOMP_PROFILE`):
add `"matrix_client" => MatrixClient`.

### C. Core wiring (TDD)

- `sandbox` crate `Profile` enum: add `WorkerMatrixClient` (if a new profile is
  used). It renders byte-identical to `WorkerNetClient` off Linux (only the
  Linux seccomp layer differs), same as `WorkerMlClient`.
- `core/src/tool_host/lockdown_env.rs::derive_lockdown_env`: add the
  `WorkerMatrixClient => "matrix_client"` arm + a unit test pinning the env
  string.
- `core/src/channel/matrix.rs::build_matrix_policy`: point `profile` at
  `WorkerMatrixClient` (if a new profile is used) + update the doc comment.
  Update the existing `build_matrix_policy` unit test's profile assertion.

### D. Install default flip

- `core/src/install/plan.rs:142`: write `KASTELLAN_MATRIX_ENFORCE_SANDBOX=1`.
- `core/src/install/plan.rs:437`: update the test assertion to `=1`.
- `core/src/channel/matrix.rs`: `MatrixSpawnConfig.enforce_sandbox` +
  `from_env` doc comments reflect production-on.

### E. Verification

1. **DGX live e2e under enforcement:** `dev-e2e-bootstrap.sh up` → the
   `#[ignore]` `matrix_live_e2e` tests pass with `enforce_sandbox=true` (login +
   E2E sync + send/recv round-trip survives the filter), reproducibly.
2. **Negative control:** confirm the filter is load-bearing — force a tighter
   profile (`strict`, or drop one addition) and observe the `SIGSYS` kill, so we
   are not shipping a no-op filter. (Mirrors #321's negative-control approach.)
3. **Mac hermetic + clippy:** `cargo test -p kastellan-worker-prelude`,
   `-p kastellan-core` channel + lockdown_env units, `cargo clippy
   --workspace --all-targets -D warnings` (and `--features live-matrix` for the
   worker crate). Linux-gated seccomp code verified via
   `cargo clippy -p kastellan-worker-prelude --target aarch64-unknown-linux-gnu`
   on the Mac (pure-Rust crate) where feasible.
4. **Production deploy LAST:** deploy to the DGX, flip the install default,
   restart, confirm the live channel runs under enforcement (`NRestarts=0`, no
   respawn loop, `matrix channel bus running`).

## Error handling

- A denied syscall `SIGSYS`-kills the worker (fail-closed) → the supervised
  `MatrixChannel` respawns with capped backoff. During enumeration this is the
  signal; in production (post-verification) it should never fire.
- A Landlock `EACCES` surfaces as a worker error, not a kill; the worker logs it
  and the supervised channel respawns. Same fail-closed posture.
- Inbound messages during any respawn downtime are recovered via the #321
  sync-token mechanism (already shipped), so an enforcement-induced restart
  during the transition window is not lossy.

## Testing strategy (TDD)

| Test | Where | Pins |
| ---- | ----- | ---- |
| `build_bpf(MatrixClient)` smoke | `seccomp_lock.rs` tests / `seccomp_smoke` | filter builds |
| additions = net_client ∪ MATRIX_CLIENT_ADDITIONS | `seccomp_lock.rs` tests | exact diff |
| `Profile::parse("matrix_client")` | `seccomp_lock.rs` tests | round-trip |
| `derive_lockdown_env` → `matrix_client` | `lockdown_env.rs` tests | env string |
| `build_matrix_policy` profile | `channel/matrix.rs` tests | profile field |
| install plan env line | `install/plan.rs` tests | `=1` |
| live round-trip under enforcement | `matrix_live_e2e.rs` (`#[ignore]`, DGX) | survives filter |

If bare `net_client` survives with zero gaps, the seccomp-profile rows collapse
to nothing and the change is just D + the `build_matrix_policy` doc + the live
verification.

## Out of scope

- Egress force-routing coupling for the Matrix worker (it runs direct
  `--share-net` `Net::Allowlist` today) — orthogonal to the syscall filter,
  tracked separately in the ROADMAP Matrix-hardening line.
- In-daemon password materialization (the keyring-outside-tokio follow-up).
- macOS Seatbelt changes — the profile already applies on macOS via the parent;
  this work is the Linux seccomp/Landlock layer.
