# Matrix-worker seccomp/Landlock enforcement flip — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the live Matrix worker under `net_client` seccomp + Landlock by default in the supervised deployment, retaining `KASTELLAN_MATRIX_ENFORCE_SANDBOX=0` as an explicit operator debug escape hatch.

**Architecture:** The enforcement plumbing already exists (`build_matrix_policy` → `Profile::WorkerNetClient` → `derive_lockdown_env` → `KASTELLAN_SECCOMP_PROFILE=net_client` + Landlock RW/RO). The `enforce_sandbox=false` branch only *overrides* it with `none`/`none`. So the work is: (1) empirically determine on the DGX whether matrix-rust-sdk's SQLite crypto store + multi-thread tokio + rustls survive `net_client`; (2) if not, add a dedicated `Profile::WorkerMatrixClient` + `MATRIX_CLIENT_ADDITIONS` mirroring the `ml_client` (#281) precedent; (3) flip the install default — **last**, after live verification, to avoid a production respawn loop.

**Tech Stack:** Rust (`kastellan-worker-prelude` seccomp, `kastellan-sandbox` `Profile`, `kastellan-core` channel + install), matrix-rust-sdk 0.18 (`live-matrix` feature), bwrap on the DGX (aarch64 Linux), `dev-e2e-bootstrap.sh` throwaway homeserver.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new dependencies expected.
- **Cross-platform: Linux + macOS first-class.** A new `Profile::WorkerMatrixClient` must render byte-identical to `WorkerNetClient` off Linux (only the Linux seccomp layer differs) — same as `WorkerMlClient`.
- **Files under 500 LOC where feasible.** `seccomp_lock.rs` is already 650 LOC (over cap, tests external); additions only — do not grow the inline test block.
- **Every syscall added to `MATRIX_CLIENT_ADDITIONS` is justified by a captured `journalctl -k` type=1326 audit record**, documented in the const's doc comment (same rigor as `ML_CLIENT_ADDITIONS`). Escape primitives (namespace/mount/ptrace/bpf/io_uring/keyring) are NEVER added.
- **Fail-closed sequencing:** verify on the throwaway dev-e2e homeserver FIRST; flip the install default + deploy to production LAST.
- **DGX driving convention:** `ssh dgx '<cmd>'` exactly (the `Bash(ssh dgx *)` allow rule is a prefix match — flags before the hostname get denied). Source `$HOME/.cargo/env` first in DGX shells.

---

### Task 1: DGX baseline — does matrix-rust-sdk survive bare `net_client`?

Discovery task (a measurement, not test-first). Output: the set of syscalls (possibly empty) matrix-rust-sdk needs beyond bare `net_client`, captured from kernel audit records. This drives whether Tasks 2–3 are needed at all.

**Files:** none modified (measurement only). Records the gap list in the task's commit message / handover notes.

**Interfaces:**
- Consumes: the existing `live-matrix` worker, `build_matrix_policy` (`Profile::WorkerNetClient`), `scripts/matrix/dev-e2e-bootstrap.sh`, `core/tests/matrix_live_e2e.rs` (`#[ignore]`).
- Produces: `MATRIX_CLIENT_ADDITIONS` candidate syscall numbers (or "none — net_client suffices") consumed by Task 2.

- [ ] **Step 1: Bring `feat/matrix-worker-sandbox-enforcement` to the DGX.**

The Mac→github push is firewalled; relay via the DGX (per the `mac-github-push-blocked-relay-via-dgx` memory note). From the Mac:

```bash
git format-patch origin/main..HEAD --stdout | ssh dgx 'cd ~/src/kastellan && git checkout -B feat/matrix-worker-sandbox-enforcement origin/main && git am'
```

Expected: patches apply cleanly on the DGX (only the spec commit so far).

- [ ] **Step 2: Build the live-matrix worker on the DGX.**

```bash
ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo build -p kastellan-worker-matrix --features live-matrix 2>&1 | tail -5'
```

Expected: `Finished` (a successful aarch64 build).

- [ ] **Step 3: Bring up the throwaway dev-e2e homeserver on the DGX.**

```bash
ssh dgx 'bash -s up' < scripts/matrix/dev-e2e-bootstrap.sh
```

Expected: a loopback `matrix-conduit` container + two bootstrapped accounts + an encrypted room; prints the env to `source ~/.matrix-e2e.env`. (`… down` tears it down — run at session end.)

- [ ] **Step 4: Run the live round-trip under `net_client` enforcement (kill mode).**

The default daemon path runs `enforce_sandbox=0`; we need to force enforcement for the e2e. The `matrix_live_e2e.rs` test drives `spawn_matrix_worker` with a `MatrixSpawnConfig`. Run it with `enforce_sandbox=true` (the test should already build the config; if it hardcodes `enforce_sandbox: false`, temporarily flip it for this measurement — do NOT commit that flip):

```bash
ssh dgx 'source $HOME/.cargo/env $HOME/.matrix-e2e.env && cd ~/src/kastellan && cargo test -p kastellan-core --features live-matrix --test matrix_live_e2e -- --ignored --nocapture 2>&1 | tail -40'
```

Expected outcomes (record which one):
- **PASS** → bare `net_client` suffices. Gap list is EMPTY. Skip Tasks 2–3; go to Task 4.
- **FAIL with the worker dying ~immediately** → seccomp `SIGSYS` kill. Proceed to Step 5.

- [ ] **Step 5: Read the first missing syscall from kernel audit.**

```bash
ssh dgx 'journalctl -k --since "2 min ago" | grep "type=1326" | tail -5'
```

Expected: lines of the form `… SECCOMP … syscall=<NR> …`. The `<NR>` is the first missing syscall (kill mode dies on the first denial per run). Map `<NR>` → name (aarch64 numbers: `arch/arm64/include/uapi/asm-generic/unistd.h`). Record it.

- [ ] **Step 6: Iterate (deferred to Task 2's build loop).**

Each missing syscall is added in Task 2, the worker rebuilt, and Steps 4–5 repeated until the round-trip PASSes. Also watch the worker stderr (in the `--nocapture` output) for **Landlock `EACCES`** errors (a *different* failure mode — permission errors, not `SIGSYS`); if matrix-sdk writes outside `store_dir`, note the path for Task 3's `fs_write`. Record the final candidate set:

```
MATRIX_CLIENT_ADDITIONS candidates: [<names + audit-record evidence>]
Landlock fs_write additions: [<paths or "none">]
```

- [ ] **Step 7: Commit the discovery notes.**

```bash
git commit --allow-empty -m "chore(matrix-sandbox): DGX net_client enumeration — <N> gaps found

Candidates: <list or 'none, net_client suffices'>.
Captured via kill-mode + journalctl -k type=1326 on the DGX dev-e2e homeserver."
```

---

### Task 2: `Profile::WorkerMatrixClient` + `MATRIX_CLIENT_ADDITIONS` in the prelude

**SKIP THIS TASK if Task 1 found zero gaps** (bare `net_client` survives). In that case the Matrix worker keeps `Profile::WorkerNetClient` and there is no new profile.

**Files:**
- Modify: `workers/prelude/src/seccomp_lock.rs` (Profile enum + parse + `allow_list_for` + `MATRIX_CLIENT_ADDITIONS` const)
- Test: `workers/prelude/src/seccomp_lock.rs` inline tests + `workers/prelude/tests/seccomp_smoke.rs`

**Interfaces:**
- Consumes: the Task 1 candidate syscall list.
- Produces: `Profile::MatrixClient` (parse string `"matrix_client"`), `pub const MATRIX_CLIENT_ADDITIONS: &[i64]`, `allow_list_for(Profile::MatrixClient)` = `net_client` ∪ `MATRIX_CLIENT_ADDITIONS`.

- [ ] **Step 1: Write the failing additions-diff test.**

In `seccomp_lock.rs` tests module (after the `ml_client` diff test ~line 815), add — substitute the **actual** Task 1 syscalls for the example `SYS_fdatasync`/`SYS_statx`:

```rust
#[test]
fn matrix_client_is_net_client_plus_additions() {
    let net_client = allow_list_for(Profile::NetClient);
    let matrix_client = allow_list_for(Profile::MatrixClient);
    // Every net_client syscall is still present.
    for nr in &net_client {
        assert!(matrix_client.contains(nr), "matrix_client dropped net_client syscall {nr}");
    }
    // The difference is exactly MATRIX_CLIENT_ADDITIONS.
    for nr in MATRIX_CLIENT_ADDITIONS {
        assert!(matrix_client.contains(nr), "matrix_client missing addition {nr}");
        assert!(!net_client.contains(nr), "addition {nr} already in net_client");
    }
}

#[test]
fn matrix_client_parses() {
    assert_eq!(Profile::parse("matrix_client").unwrap(), Some(Profile::MatrixClient));
}
```

- [ ] **Step 2: Run to verify it fails (compile error — `Profile::MatrixClient` undefined).**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-prelude matrix_client 2>&1 | tail -15
```
Expected: FAIL — `no variant named MatrixClient`.

- [ ] **Step 3: Add the `MatrixClient` variant + parse arm.**

In the `Profile` enum (after `MlClient`):

```rust
    /// `"matrix_client"` — `net_client` **plus** [`MATRIX_CLIENT_ADDITIONS`]:
    /// the syscalls matrix-rust-sdk's SQLite crypto store + multi-thread tokio
    /// runtime issue beyond the net-client base, enumerated empirically on the
    /// DGX (aarch64) via the kill-mode `journalctl -k | grep type=1326` loop
    /// (design spec 2026-06-24). The worker is `Net::Allowlist` (homeserver
    /// only); the socket family comes from the `net_client` base.
    MatrixClient,
```

In `Profile::parse`, add before the `"none"` arm:

```rust
            "matrix_client" => Ok(Some(Profile::MatrixClient)),
```

And extend the error message string to include `'matrix_client'`.

- [ ] **Step 4: Add `MATRIX_CLIENT_ADDITIONS` + wire `allow_list_for`.**

Add the const near `ML_CLIENT_ADDITIONS` (substitute the **actual** Task 1 syscalls; example shape below):

```rust
/// Syscalls matrix-rust-sdk 0.18 issues beyond the bare `net_client`
/// allow-list, permitted only under [`Profile::MatrixClient`].
///
/// **Enumerated empirically** on the DGX (aarch64) by running the live
/// `kastellan-worker-matrix` (login + E2E sync + send/recv) against the
/// dev-e2e homeserver under the kill-mode seccomp filter, reading each first
/// missing syscall from `journalctl -k | grep type=1326` (design spec
/// 2026-06-24 §A). Source: the SQLite crypto store (mmap/sync/fcntl) + the
/// multi-thread tokio runtime. Escape primitives are NEVER added here.
pub const MATRIX_CLIENT_ADDITIONS: &[i64] = &[
    // <one entry per Task-1-observed syscall, each with its evidence>
    // e.g. libc::SYS_fdatasync,  // SQLite WAL fsync (audit syscall=83 ×N)
];
```

In `allow_list_for`, add the net-socket family to `matrix_client` and its own additions:

```rust
    if matches!(profile, Profile::NetClient | Profile::BrowserClient | Profile::MlClient | Profile::MatrixClient) {
        out.extend_from_slice(NET_CLIENT_ADDITIONS);
    }
    // ... existing BrowserClient + MlClient arms ...
    if matches!(profile, Profile::MatrixClient) {
        out.extend_from_slice(MATRIX_CLIENT_ADDITIONS);
    }
```

- [ ] **Step 5: Add the `build_bpf` smoke assertion.**

In `seccomp_smoke.rs` (or the inline `build_bpf_*` tests), add:

```rust
#[test]
fn build_bpf_matrix_client_succeeds() {
    let bpf = build_bpf(Profile::MatrixClient).expect("matrix_client bpf must build");
    assert!(!bpf.is_empty());
}
```

- [ ] **Step 6: Run the tests to verify they pass.**

```bash
cargo test -p kastellan-worker-prelude 2>&1 | tail -15
```
Expected: PASS (all prelude tests, including the 3 new ones).

- [ ] **Step 7: Clippy (Linux-gated seccomp code, checked on the Mac for the pure-Rust prelude).**

```bash
cargo clippy -p kastellan-worker-prelude --all-targets --target aarch64-unknown-linux-gnu -- -D warnings 2>&1 | tail -5
```
Expected: clean (per the `cross-clippy-pure-rust-crates` memory note — prelude is pure-Rust so this works without a Linux linker).

- [ ] **Step 8: Commit.**

```bash
git add workers/prelude/src/seccomp_lock.rs workers/prelude/tests/seccomp_smoke.rs
git commit -m "feat(prelude): matrix_client seccomp profile (net_client + SQLite/tokio additions)"
```

---

### Task 3: Wire `WorkerMatrixClient` through sandbox + core

**SKIP THIS TASK if Task 1 found zero gaps.** (Otherwise the Matrix worker keeps `WorkerNetClient` and Task 4 is the only remaining change.)

**Files:**
- Modify: `sandbox/src/lib.rs` (`Profile` enum + any `match` arms — `macos_container.rs`)
- Modify: `core/src/tool_host/lockdown_env.rs` (`derive_lockdown_env` seccomp arm + test)
- Modify: `core/src/channel/matrix.rs` (`build_matrix_policy` profile + doc + test)

**Interfaces:**
- Consumes: `Profile::WorkerMatrixClient` (sandbox), `KASTELLAN_SECCOMP_PROFILE="matrix_client"`.
- Produces: `build_matrix_policy(...).profile == Profile::WorkerMatrixClient`; `derive_lockdown_env` emits `matrix_client`.

- [ ] **Step 1: Add `WorkerMatrixClient` to the sandbox `Profile` enum.**

In `sandbox/src/lib.rs` (after `WorkerMlClient`, ~line 50):

```rust
    /// For the `matrix` worker: `WorkerNetClient` **plus** the matrix-rust-sdk
    /// SQLite/crypto/tokio syscalls (`MATRIX_CLIENT_ADDITIONS` in the prelude).
    /// `Net::Allowlist` (homeserver only). On macOS this renders identically to
    /// `WorkerNetClient` (Seatbelt has no seccomp layer); only the Linux seccomp
    /// filter differs.
    WorkerMatrixClient,
```

- [ ] **Step 2: Fix the non-exhaustive `match` in `macos_container.rs`.**

`WorkerMatrixClient` is a net-client tier on macOS, so group it with `WorkerNetClient | WorkerBrowserClient` (line ~216):

```rust
        Profile::WorkerNetClient | Profile::WorkerBrowserClient | Profile::WorkerMatrixClient => {
```

- [ ] **Step 3: Build to find any other non-exhaustive matches.**

```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-sandbox 2>&1 | tail -15
```
Expected: compiles (or names the exact remaining `match` to extend — add a `WorkerMatrixClient` arm that mirrors `WorkerNetClient` at each site).

- [ ] **Step 4: Write the failing `derive_lockdown_env` test.**

In `core/src/tool_host/lockdown_env.rs` tests (after `derive_adds_ml_client_profile`):

```rust
    #[test]
    fn derive_adds_matrix_client_profile() {
        let mut p = base_policy();
        p.profile = Profile::WorkerMatrixClient;
        let derived = derive_lockdown_env(&p);
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .expect("seccomp env must be derived");
        assert_eq!(seccomp.1, "matrix_client");
    }
```

- [ ] **Step 5: Run to verify it fails.**

```bash
cargo test -p kastellan-core --lib derive_adds_matrix_client 2>&1 | tail -10
```
Expected: FAIL — non-exhaustive `match` in `derive_lockdown_env` won't compile, or the arm is missing.

- [ ] **Step 6: Add the `derive_lockdown_env` arm.**

In `lockdown_env.rs` (~line 87, the seccomp match):

```rust
            Profile::WorkerMatrixClient => "matrix_client",
```

- [ ] **Step 7: Point `build_matrix_policy` at the new profile + update its test.**

In `core/src/channel/matrix.rs::build_matrix_policy` (~line 372): `profile: Profile::WorkerMatrixClient,` and update the doc bullet (line ~328) from `WorkerNetClient` to `WorkerMatrixClient`. Update the `build_matrix_policy` unit test that asserts the profile (search `Profile::WorkerNetClient` in the `matrix.rs` test module).

- [ ] **Step 8: Run the tests to verify they pass.**

```bash
cargo test -p kastellan-core --lib lockdown_env 2>&1 | tail -10
cargo test -p kastellan-core --lib channel::matrix 2>&1 | tail -10
```
Expected: PASS.

- [ ] **Step 9: Clippy + commit.**

```bash
cargo clippy -p kastellan-sandbox -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5
git add sandbox/src/lib.rs sandbox/src/macos_container.rs core/src/tool_host/lockdown_env.rs core/src/channel/matrix.rs
git commit -m "feat(matrix): route matrix worker through WorkerMatrixClient profile"
```

---

### Task 4: Flip the install default to enforced

**Files:**
- Modify: `core/src/install/plan.rs:142` + the doc comment at ~138 + the test at ~437
- Modify: `core/src/channel/matrix.rs` (`MatrixSpawnConfig.enforce_sandbox` + `parse_daemon_spawn_config` doc comments)

**Interfaces:**
- Consumes: nothing new.
- Produces: `render_env_file` writes `KASTELLAN_MATRIX_ENFORCE_SANDBOX=1`.

- [ ] **Step 1: Update the failing test to expect `=1`.**

In `core/src/install/plan.rs` test `env_file_writes_matrix_block_when_configured` (~line 437):

```rust
        assert!(s.contains("KASTELLAN_MATRIX_ENFORCE_SANDBOX=1\n"), "{s}");
```

- [ ] **Step 2: Run to verify it fails.**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib env_file_writes_matrix_block 2>&1 | tail -10
```
Expected: FAIL — still writes `=0`.

- [ ] **Step 3: Flip the writer + update the doc comment.**

In `plan.rs:142`:

```rust
        s.push_str("KASTELLAN_MATRIX_ENFORCE_SANDBOX=1\n");
```

Update the comment at ~138 from "Worker-side seccomp/Landlock stays off for now (first bring-up); enabling it + egress force-routing is a hardening follow-up." to note enforcement is now ON by default (`net_client`/`matrix_client` seccomp + Landlock), `=0` is the operator debug escape hatch, and egress force-routing remains the separate follow-up.

- [ ] **Step 4: Run to verify it passes.**

```bash
cargo test -p kastellan-core --lib env_file_writes_matrix_block 2>&1 | tail -10
```
Expected: PASS.

- [ ] **Step 5: Update the `MatrixSpawnConfig` / `from_env` doc comments.**

In `core/src/channel/matrix.rs`: the `enforce_sandbox` field doc (~534) and the `from_env` env-contract line (~414) — note production default is ON (`net_client`/`matrix_client` + Landlock); `0`/`false` is the explicit debug opt-out. (The `daemon_cfg_default_enforce_sandbox_is_on` test at ~695 already pins default-on — no change needed.)

- [ ] **Step 6: Commit.**

```bash
git add core/src/install/plan.rs core/src/channel/matrix.rs
git commit -m "feat(install): enforce matrix-worker sandbox by default (=1)"
```

---

### Task 5: DGX live verification, negative control, production deploy

**Files:** none (verification + deploy + handover).

**Interfaces:**
- Consumes: Tasks 1–4 on the branch.
- Produces: green live e2e under enforcement + a running production channel with `NRestarts=0`.

- [ ] **Step 1: Sync the branch to the DGX (post-Task-2/3/4 commits).**

```bash
git format-patch origin/main..HEAD --stdout | ssh dgx 'cd ~/src/kastellan && git checkout feat/matrix-worker-sandbox-enforcement && git reset --hard origin/main && git am'
```
Expected: all commits apply.

- [ ] **Step 2: Full hermetic test + clippy on the DGX (the standing Linux gate).**

```bash
ssh dgx 'source $HOME/.cargo/env && cd ~/src/kastellan && cargo test -p kastellan-worker-prelude && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -8'
```
Expected: prelude tests green, clippy clean. (The `--features live-matrix` worker crate also: `cargo clippy -p kastellan-worker-matrix --all-targets --features live-matrix -- -D warnings`.)

- [ ] **Step 3: Live round-trip under enforcement (the acceptance gate).**

```bash
ssh dgx 'bash -s up' < scripts/matrix/dev-e2e-bootstrap.sh
ssh dgx 'source $HOME/.cargo/env $HOME/.matrix-e2e.env && cd ~/src/kastellan && cargo test -p kastellan-core --features live-matrix --test matrix_live_e2e -- --ignored --nocapture 2>&1 | tail -30'
```
Expected: PASS — login + E2E sync + send/recv survive under `matrix_client` (or `net_client`) seccomp + Landlock. Run twice to confirm reproducibility.

- [ ] **Step 4: Negative control — prove the filter is load-bearing.**

Temporarily force a tighter profile (e.g. set `KASTELLAN_SECCOMP_PROFILE=strict` for the worker, or drop one `MATRIX_CLIENT_ADDITIONS` entry) and re-run Step 3. Expected: the worker `SIGSYS`-dies (kill mode) and the e2e FAILs at the round-trip assertion — confirming the added syscalls are actually exercised and the filter is not a no-op. Revert the temporary change.

- [ ] **Step 5: Deploy to production (LAST — fail-closed sequencing).**

```bash
ssh dgx 'bash -s down' < scripts/matrix/dev-e2e-bootstrap.sh   # tear down throwaway hs first
ssh dgx 'cd ~/src/kastellan && ./scripts/upgrade_from_git.sh'   # build-release + install + restart
```
The flipped `install/plan.rs` re-renders `~/.config/kastellan/kastellan.env` with `KASTELLAN_MATRIX_ENFORCE_SANDBOX=1` (recall the install regenerates env from CLI flags — re-pass `--matrix-homeserver-url`/`--matrix-user` if `upgrade_from_git.sh` doesn't preserve them, per the `render_env_file` operator gotcha).

- [ ] **Step 6: Confirm the production channel runs under enforcement, no respawn loop.**

```bash
ssh dgx 'systemctl --user status kastellan-core --no-pager | grep -E "Active|NRestarts" ; journalctl --user -u kastellan-core --since "3 min ago" | grep -iE "matrix channel bus running|seccomp|landlock|SIGSYS" | tail -10'
```
Expected: `Active: active`, `matrix channel bus running`, no `SIGSYS`/respawn-loop churn. Verify a real DM to `@kastellan` still gets a reply (sanity).

- [ ] **Step 7: Update HANDOVER.md + ROADMAP.md and commit.**

Move this work into "Recently completed", tick the ROADMAP Matrix-hardening seccomp/Landlock line, note egress force-routing remains the residual Matrix follow-up. Fix the stale `#340` reference in the Next TODO preamble while here.

---

## Self-Review

**Spec coverage:**
- Empirical enumeration (spec §A) → Task 1. ✓
- Dedicated `matrix_client` profile if gaps (spec §B) → Task 2 (conditional). ✓
- Core wiring (spec §C) → Task 3 (conditional). ✓
- Install default flip (spec §D) → Task 4. ✓
- Verification incl. negative control + production deploy (spec §E) → Task 5. ✓
- Fail-closed sequencing (spec architecture #3) → Task 5 ordered last; enumeration on dev-e2e. ✓
- Landlock `EACCES` watch + `fs_write` additions → Task 1 Step 6 + Task 3 (if needed). ✓

**Placeholder scan:** The only deliberate "fill in" is the `MATRIX_CLIENT_ADDITIONS` syscall list — it is *necessarily* empirical (Task 1's output), and the plan shows the exact const shape + how each entry is justified. Not a placeholder failure; it is the discovery deliverable feeding Task 2.

**Type consistency:** `Profile::MatrixClient` (prelude) vs `Profile::WorkerMatrixClient` (sandbox) — these are two distinct enums in two crates, matching the existing `MlClient`/`WorkerMlClient` split; the env-string bridge `"matrix_client"` is consistent across `Profile::parse` (Task 2), `derive_lockdown_env` (Task 3), and the worker's `apply_from_env`. ✓

**Conditional structure:** Tasks 2–3 carry an explicit SKIP banner keyed on Task 1's result; Tasks 4–5 run regardless.
