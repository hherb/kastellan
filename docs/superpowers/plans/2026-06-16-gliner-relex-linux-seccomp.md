# gliner-relex Linux seccomp Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route gliner-relex's host-mode venv spawn through the `kastellan-worker-lockdown-exec` shim on Linux so a real, torch-compatible seccomp filter (`ml_client`) is installed, closing the #281 gap for gliner-relex without breaking model load.

**Architecture:** Reuse the proven #281 shim. Add a dedicated `WorkerMlClient` sandbox profile (renders like `WorkerStrict` on macOS — byte-identical) and an `ml_client` seccomp profile (`net_client` base + an empirically-enumerated `ML_CLIENT_ADDITIONS`). The gliner manifest discovers the shim on Linux (fail-closed) and threads it into the host-mode `ToolEntry`; `derive_lockdown_env` already injects the env the shim reads. Seccomp-only (Landlock deferred).

**Tech Stack:** Rust (workspace crates `kastellan-sandbox`, `kastellan-worker-prelude`, `kastellan-core`), `seccompiler`, bwrap (Linux), Seatbelt (macOS). DGX Spark (aarch64) over `ssh dgx` for empirical enumeration + acceptance.

**Spec:** `docs/superpowers/specs/2026-06-16-gliner-relex-linux-seccomp-design.md`

**Build prelude (every session):** `source "$HOME/.cargo/env"` before any cargo command.

---

## File structure

- `sandbox/src/lib.rs` — add `Profile::WorkerMlClient` variant.
- `sandbox/src/macos_container.rs` — group `WorkerMlClient` with `WorkerStrict`.
- `workers/prelude/src/seccomp_lock.rs` — `Profile::MlClient` + `parse` + `ML_CLIENT_ADDITIONS` + `allow_list_for` arm + tests.
- `core/src/tool_host/lockdown_env.rs` — exhaustive-match arm `WorkerMlClient => "ml_client"` + test.
- `core/src/workers/gliner_relex/entry.rs` — `lockdown_shim` param, profile flip, shim bind, `LANDLOCK_PROFILE=none`.
- `core/src/workers/gliner_relex/manifest.rs` — Linux shim discovery, fail-closed.
- `core/src/workers/gliner_relex/tests.rs` — update call sites + new wiring tests.

---

## Task 1: Add `Profile::WorkerMlClient` to the sandbox enum

**Files:**
- Modify: `sandbox/src/lib.rs:28-42` (enum), `sandbox/src/macos_container.rs:200`
- Test: `sandbox/src/lib.rs` (inline `mod tests` near line 354-389)

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `sandbox/src/lib.rs`:

```rust
#[test]
fn ml_client_profile_is_distinct_and_serialises() {
    // WorkerMlClient is a real variant (torch/ML worker seccomp tier).
    let p = Profile::WorkerMlClient;
    let json = serde_json::to_string(&p).expect("serialise");
    let back: Profile = serde_json::from_str(&json).expect("deserialise");
    assert_eq!(back, Profile::WorkerMlClient);
    assert_ne!(Profile::WorkerMlClient, Profile::WorkerStrict);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kastellan-sandbox ml_client_profile_is_distinct -- --nocapture`
Expected: FAIL to compile — `no variant named WorkerMlClient`.

- [ ] **Step 3: Add the variant**

In `sandbox/src/lib.rs`, after the `WorkerBrowserClient` variant (line 41), add:

```rust
    /// For heavy torch/transformers inference workers (gliner-relex): the
    /// `WorkerNetClient` syscall set (torch creates sockets even fully offline)
    /// **plus** an empirically-enumerated ML-additions set (Linux seccomp
    /// `ml_client`). The worker stays `Net::Deny` — the socket syscalls are
    /// permitted at the seccomp layer but have no route out of the private
    /// netns. On macOS this renders identically to `WorkerStrict` (Seatbelt has
    /// no per-syscall layer and the worker is net-denied). See the
    /// gliner-relex Linux-seccomp design spec (2026-06-16) and issue #281.
    WorkerMlClient,
```

In `sandbox/src/macos_container.rs`, change line 200 from
`Profile::WorkerStrict => {` to:

```rust
        // gliner-relex (WorkerMlClient) is Net::Deny like WorkerStrict, so it
        // gets the same read-only-root container hardening; the ml_client
        // seccomp widening is a Linux-only host-backend concern.
        Profile::WorkerStrict | Profile::WorkerMlClient => {
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kastellan-sandbox ml_client_profile_is_distinct -- --nocapture`
Expected: PASS. Also run `cargo test -p kastellan-sandbox` — all green (the exhaustive `match` in `macos_container.rs` now compiles on macOS; Linux build unaffected since that file is macOS-gated).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/lib.rs sandbox/src/macos_container.rs
git commit -m "feat(sandbox): add Profile::WorkerMlClient (renders as strict off Linux) (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Add the `ml_client` seccomp profile

**Files:**
- Modify: `workers/prelude/src/seccomp_lock.rs` (Profile enum ~69-115, `allow_list_for` ~216-232, additions consts ~544-605)
- Test: `workers/prelude/src/seccomp_lock.rs` (inline `mod tests` ~655-849)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `seccomp_lock.rs`:

```rust
#[test]
fn profile_parse_recognises_ml_client() {
    assert_eq!(Profile::parse("ml_client").unwrap(), Some(Profile::MlClient));
}

#[test]
fn build_bpf_ml_client_succeeds() {
    let bpf = build_bpf(Profile::MlClient).expect("ml_client bpf must build");
    assert!(!bpf.is_empty(), "ml_client filter must emit instructions");
}

#[test]
fn ml_client_is_a_superset_of_net_client() {
    // ml_client = net_client + ML additions, so it must allow everything
    // net_client does (notably the socket family torch needs even offline).
    let net_client = allow_list_for(Profile::NetClient);
    let ml = allow_list_for(Profile::MlClient);
    for nr in net_client {
        assert!(ml.contains(&nr), "MlClient missing NetClient syscall {nr}");
    }
    assert!(ml.contains(&libc::SYS_socket), "MlClient must allow socket()");
}

#[test]
fn ml_client_excludes_escape_primitives() {
    // The threat-model invariant: even a torch-tier worker must never be able
    // to escape its namespace / inspect other processes / load BPF.
    let ml = allow_list_for(Profile::MlClient);
    for nr in [
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_ptrace,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
    ] {
        assert!(!ml.contains(&nr), "MlClient must never allow {nr}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p kastellan-worker-prelude ml_client -- --nocapture`
Expected: FAIL to compile — `no variant named MlClient`.

(Note: on macOS the prelude's seccomp code is still compiled — `seccompiler` builds cross-platform; only *installation* is Linux-gated. `build_bpf`/`allow_list_for`/`Profile::parse` are pure and testable on macOS.)

- [ ] **Step 3: Implement the profile**

In `seccomp_lock.rs`, add the variant after `BrowserClient` (line 99):

```rust
    /// `"ml_client"` — `net_client` **plus** [`ML_CLIENT_ADDITIONS`]: the
    /// syscalls a torch/transformers inference worker (gliner-relex) issues
    /// beyond the net-client base, enumerated empirically on the DGX via a
    /// log-mode seccomp run (design spec 2026-06-16 §4). The worker is
    /// `Net::Deny`; the socket family is permitted at the syscall layer (torch
    /// opens sockets even fully offline) but the private netns gives it no route.
    MlClient,
```

Extend `Profile::parse` (line 104) — add an arm before `"none" | ""`:

```rust
            "ml_client" => Ok(Some(Profile::MlClient)),
```

and update the error message string to list `'ml_client'` among the valid values.

Add the additions const after `BROWSER_IO_URING` (after line 613):

```rust
/// torch/transformers-specific syscalls beyond [`NET_CLIENT_ADDITIONS`].
/// Permitted only under [`Profile::MlClient`] (gliner-relex).
///
/// **Populated empirically** by a log-mode seccomp run on the DGX (see the
/// design spec §4 + this plan's Task 7) — each entry is a syscall a real
/// `knowledgator/gliner-relex-multi-v1.0` model load + `extract` was observed
/// to issue and SIGSYS on under the bare `net_client` set. Starts empty: a
/// fresh enumeration fills it. Escape primitives (namespace/mount/ptrace/bpf/
/// io_uring) are NEVER added here — they stay killed by the default action.
pub const ML_CLIENT_ADDITIONS: &[i64] = &[];
```

Extend `allow_list_for` (after the `NetClient | BrowserClient` block, before the `BrowserClient`-only block, ~line 224):

```rust
    if matches!(profile, Profile::MlClient) {
        out.extend_from_slice(ML_CLIENT_ADDITIONS);
    }
```

and add `Profile::MlClient` to the existing net-family condition so it gets the socket set:

```rust
    if matches!(profile, Profile::NetClient | Profile::BrowserClient | Profile::MlClient) {
        out.extend_from_slice(NET_CLIENT_ADDITIONS);
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-worker-prelude ml_client -- --nocapture`
Expected: PASS (all four). Run `cargo test -p kastellan-worker-prelude` — all green.

- [ ] **Step 5: Commit**

```bash
git add workers/prelude/src/seccomp_lock.rs
git commit -m "feat(prelude): add ml_client seccomp profile (net_client + empty ML additions) (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Map `WorkerMlClient` to `"ml_client"` in lockdown-env derivation

**Files:**
- Modify: `core/src/tool_host/lockdown_env.rs:88-93` (the `match out.profile` block)
- Test: `core/src/tool_host/lockdown_env.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `lockdown_env.rs` (next to `derive_adds_browser_client_profile`):

```rust
#[test]
fn derive_adds_ml_client_profile() {
    let mut p = base_policy();
    p.profile = Profile::WorkerMlClient;
    let derived = derive_lockdown_env(&p);
    let seccomp = derived
        .env
        .iter()
        .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
        .expect("seccomp env must be derived");
    assert_eq!(seccomp.1, "ml_client");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p kastellan-core --lib derive_adds_ml_client_profile -- --nocapture`
Expected: FAIL to compile — non-exhaustive `match` (the `match out.profile` arm is missing `WorkerMlClient`).

- [ ] **Step 3: Add the match arm**

In `lockdown_env.rs`, in the `let value = match out.profile { ... }` block, add:

```rust
            Profile::WorkerMlClient => "ml_client",
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p kastellan-core --lib derive_adds_ml_client_profile -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/tool_host/lockdown_env.rs
git commit -m "feat(core): map WorkerMlClient -> ml_client seccomp env (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Thread `lockdown_shim` into the gliner-relex host-mode entry

**Files:**
- Modify: `core/src/workers/gliner_relex/entry.rs` (`gliner_relex_entry` ~95-108, `host_mode_entry` ~114-175)
- Modify: `core/src/workers/gliner_relex/manifest.rs:57` (the one call site)
- Test: `core/src/workers/gliner_relex/tests.rs`

- [ ] **Step 1: Update all existing call sites to the new signature (mechanical, keeps the tree compiling)**

In `core/src/workers/gliner_relex/tests.rs`, replace every `gliner_relex_entry(&env)` with `gliner_relex_entry(&env, None)` (≈15 occurrences — use replace_all). In `manifest.rs:57` change `gliner_relex_entry(&env)` to `gliner_relex_entry(&env, lockdown_shim)` (the `lockdown_shim` local is introduced in Task 5; for now, to keep this task self-contained, pass `None` and Task 5 swaps it):

`manifest.rs:57`:
```rust
                Resolution::Register(gliner_relex_entry(&env, None))
```

- [ ] **Step 2: Write the failing wiring tests**

Add to `tests.rs`. First update the existing profile test:

```rust
// REPLACE entry_uses_strict_profile with:
#[test]
fn entry_uses_ml_client_profile() {
    let env = test_env();
    let entry = gliner_relex_entry(&env, None);
    match entry.policy.profile {
        Profile::WorkerMlClient => {}
        other => panic!("expected Profile::WorkerMlClient, got {other:?}"),
    }
}
```

Then add the shim-wiring tests:

```rust
#[test]
fn entry_without_shim_sets_no_lockdown_shim_and_no_landlock_optout() {
    // macOS / container path: no shim, no KASTELLAN_LANDLOCK_PROFILE override.
    let env = test_env();
    let entry = gliner_relex_entry(&env, None);
    assert!(entry.lockdown_shim.is_none());
    assert!(
        !entry
            .policy
            .env
            .iter()
            .any(|(k, _)| k == crate::tool_host::ENV_LANDLOCK_PROFILE),
        "no Landlock opt-out without a shim"
    );
}

#[test]
fn entry_with_shim_binds_it_and_opts_out_of_landlock() {
    let env = test_env();
    let shim = PathBuf::from("/tmp/fake/target/debug/kastellan-worker-lockdown-exec");
    let entry = gliner_relex_entry(&env, Some(shim.clone()));
    assert_eq!(entry.lockdown_shim.as_deref(), Some(shim.as_path()));
    assert!(
        entry.policy.fs_read.contains(&shim),
        "shim must be bound read-only so bwrap can exec it"
    );
    let landlock = entry
        .policy
        .env
        .iter()
        .find(|(k, _)| k == crate::tool_host::ENV_LANDLOCK_PROFILE)
        .expect("shim path must opt out of Landlock (seccomp-only)");
    assert_eq!(landlock.1, "none");
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p kastellan-core --lib gliner_relex::tests::entry_with_shim -- --nocapture`
Expected: FAIL to compile — `gliner_relex_entry` takes 1 argument.

- [ ] **Step 4: Implement the signature + body changes**

In `entry.rs`, change `gliner_relex_entry`:

```rust
pub fn gliner_relex_entry(
    env: &GlinerRelexEnv,
    lockdown_shim: Option<std::path::PathBuf>,
) -> ToolEntry {
    #[cfg(target_os = "macos")]
    if env.use_container_backend {
        // Container mode never uses the host-spawn shim (the image bakes its
        // own entrypoint); the param is host-mode-only.
        return container_mode_entry(env);
    }
    host_mode_entry(env, lockdown_shim)
}
```

Change `host_mode_entry`'s signature and body. New signature:

```rust
fn host_mode_entry(env: &GlinerRelexEnv, lockdown_shim: Option<std::path::PathBuf>) -> ToolEntry {
```

After the `fs_read.extend(env.interpreter_lib_dirs.iter().cloned());` line, add:

```rust
    // Bind the lockdown-exec shim into the jail read-only so bwrap can exec it
    // (it lives in target/debug/ in dev, outside the base /usr bind). Linux
    // only — macOS/container pass None (Seatbelt is applied from the parent).
    if let Some(shim) = &lockdown_shim {
        fs_read.push(shim.clone());
    }
```

Change the local `let policy = SandboxPolicy { ... profile: Profile::WorkerStrict, ... }` to `profile: Profile::WorkerMlClient,`.

Build `policy.env` so the Landlock opt-out is appended when a shim is present. Replace the `env: build_runtime_env(env),` field with a pre-built local. Just before `let policy = SandboxPolicy {`, add:

```rust
    let mut policy_env = build_runtime_env(env);
    // When spawned through the lockdown shim (Linux), run seccomp-only: the
    // shim's lock_down() reads KASTELLAN_LANDLOCK_PROFILE=none and skips the
    // Landlock layer. gliner's FS surface isn't validated against a Landlock
    // ruleset yet and bwrap's mount namespace already bounds it (Landlock is a
    // tracked #281 follow-up). macOS/container pass None and add nothing.
    if lockdown_shim.is_some() {
        policy_env.push((
            crate::tool_host::ENV_LANDLOCK_PROFILE.to_string(),
            "none".to_string(),
        ));
    }
```

and set the policy field to `env: policy_env,`. Finally set `lockdown_shim,` on the returned `ToolEntry` (was `lockdown_shim: None`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p kastellan-core --lib gliner_relex -- --nocapture`
Expected: PASS (all gliner unit tests, including the updated profile + new shim tests).

- [ ] **Step 6: Commit**

```bash
git add core/src/workers/gliner_relex/entry.rs core/src/workers/gliner_relex/manifest.rs core/src/workers/gliner_relex/tests.rs
git commit -m "feat(gliner-relex): thread lockdown_shim into host-mode entry; flip to WorkerMlClient (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Fail-closed shim discovery in the gliner-relex manifest (Linux)

**Files:**
- Modify: `core/src/workers/gliner_relex/manifest.rs` (the `Ok(mut env) => { ... }` arm, ~41-58)
- Test: `core/src/workers/gliner_relex/tests.rs`

- [ ] **Step 1: Write the failing test**

The manifest's `resolve()` takes a `ResolveCtx`. Mirror the browser-driver manifest tests' `ResolveCtx` construction. Add to `tests.rs` (check the existing manifest tests for the exact `ResolveCtx` field shape and reuse their helper if present):

```rust
#[cfg(target_os = "linux")]
#[test]
fn manifest_is_fail_closed_when_shim_missing_on_linux() {
    use crate::worker_manifest::{Resolution, ResolveCtx, WorkerManifest};
    // Enabled + venv shim present, but the lockdown-exec shim cannot be found:
    // resolve() must refuse (Misconfigured), never Register an unfiltered worker.
    let ctx = ResolveCtx {
        get_env: &|k: &str| match k {
            "KASTELLAN_GLINER_RELEX_ENABLE" => Some("1".to_string()),
            "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR" => Some("/tmp/fake/weights".to_string()),
            "KASTELLAN_GLINER_RELEX_VENV_DIR" => Some("/tmp/fake/.venv".to_string()),
            // No KASTELLAN_LOCKDOWN_EXEC_BIN set.
            _ => None,
        },
        // weights dir + venv shim "exist"; lockdown-exec sibling does not.
        exists: &|p| {
            p == std::path::Path::new("/tmp/fake/.venv/bin/kastellan-worker-gliner-relex")
        },
        is_dir: &|p| p == std::path::Path::new("/tmp/fake/weights"),
        // exe_dir None ⇒ no current_exe()-relative sibling lookup, so with no
        // override env the shim cannot be discovered ⇒ fail-closed.
        exe_dir: None,
        canonicalize: &|p| Some(p.to_path_buf()),
        allowlist: &|_| vec![],
    };
    match GlinerRelexManifest.resolve(&ctx) {
        Resolution::Misconfigured { detail } => {
            assert!(detail.contains("lockdown-exec"), "detail: {detail}");
        }
        other => panic!("expected Misconfigured, got {other:?}"),
    }
}
```

> Adjust the `ResolveCtx { .. }` literal to match the real struct (field names/closure signatures) — copy the shape from `core/src/workers/browser_driver.rs`'s `mod tests` if the inline literal here drifts.

- [ ] **Step 2: Run test to verify it fails**

Run (on the DGX, since it is `cfg(target_os = "linux")` — or via cross-clippy for compile-check):
`ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib manifest_is_fail_closed_when_shim_missing_on_linux'`
Expected: FAIL — currently `resolve()` registers unconditionally with `gliner_relex_entry(&env, None)`.

- [ ] **Step 3: Implement Linux shim discovery**

In `manifest.rs`, replace the `Resolution::Register(gliner_relex_entry(&env, None))` line (Task 4 left it `None`) with the platform split:

```rust
                // Linux: gliner-relex is a pure-Python venv worker bwrap spawns
                // directly, so it needs the lockdown-exec shim to actually apply
                // its ml_client seccomp filter. Fail-closed if the shim is
                // missing — never register an unfiltered torch worker. macOS uses
                // Seatbelt (applied from the parent), so no shim.
                #[cfg(target_os = "linux")]
                {
                    match crate::worker_manifest::discover_binary(
                        ctx,
                        "KASTELLAN_LOCKDOWN_EXEC_BIN",
                        "kastellan-worker-lockdown-exec",
                    ) {
                        Some(shim) => {
                            Resolution::Register(gliner_relex_entry(&env, Some(shim)))
                        }
                        None => Resolution::Misconfigured {
                            detail: "lockdown-exec shim not found (KASTELLAN_LOCKDOWN_EXEC_BIN unset/invalid and no exe-relative sibling); gliner-relex requires it for worker-side seccomp on Linux".to_string(),
                        },
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Resolution::Register(gliner_relex_entry(&env, None))
                }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo test -p kastellan-core --lib gliner_relex'`
Expected: PASS. (macOS: `cargo test -p kastellan-core --lib gliner_relex` — the non-linux arm compiles + the macOS tests pass.)

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/gliner_relex/manifest.rs core/src/workers/gliner_relex/tests.rs
git commit -m "feat(gliner-relex): fail-closed lockdown-exec shim discovery on Linux (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Local verification (macOS + cross-clippy)

**Files:** none (verification only)

- [ ] **Step 1: Full macOS build + test (skip-as-pass posture, no `KASTELLAN_PG_BIN_DIR`)**

Run: `cargo build --workspace && cargo test --workspace`
Expected: green (the standing macOS skip-as-pass baseline; gliner unit tests pass; no regressions). Record the passed/ignored counts.

- [ ] **Step 2: Workspace clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Cross-clippy the Linux-gated sandbox code**

Run: `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu`
Expected: clean (verifies the macOS-gated `macos_container.rs` arm change + the Linux build of the sandbox crate; `core`'s Linux path is DGX/CI-verified — it can't cross-compile here, the `ring` C dep).

- [ ] **Step 4: Commit (if clippy required any fixes; otherwise skip)**

```bash
git add -p
git commit -m "chore: clippy/test fixups for ml_client wiring (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Empirically enumerate `ML_CLIENT_ADDITIONS` on the DGX (log-mode)

**Files:**
- Temporary (revert before commit): `workers/prelude/src/seccomp_lock.rs::build_bpf` (Log action)
- Modify (final): `workers/prelude/src/seccomp_lock.rs` (`ML_CLIENT_ADDITIONS` contents)

This is the load-bearing empirical step. The DGX has the model cached
(`~/.cache/huggingface/hub/models--knowledgator--gliner-relex-multi-v1.0`),
`dmesg` readable, auditd off. Drive everything as `ssh dgx '<cmd>'`.

- [ ] **Step 1: Push the branch and sync the DGX working tree**

```bash
git push -u origin feat/281-gliner-relex-seccomp
ssh dgx 'cd ~/src/kastellan && git fetch origin && git checkout feat/281-gliner-relex-seccomp && git reset --hard origin/feat/281-gliner-relex-seccomp'
```
Expected: DGX tree on the branch HEAD.

- [ ] **Step 2: Confirm gliner-relex is staged + enabled on the DGX**

```bash
ssh dgx 'ls ~/src/kastellan/workers/gliner-relex/.venv/bin/kastellan-worker-gliner-relex && find ~ -maxdepth 7 -type d -path "*workers/gliner-relex/weights*" 2>/dev/null | head'
```
Expected: the venv shim exists and a weights dir is present. If weights are absent, run `ssh dgx 'cd ~/src/kastellan && bash scripts/workers/gliner-relex/install.sh'` first (it stages weights from the HF cache).

- [ ] **Step 3: Temporarily switch the `ml_client` main filter to Log action**

Edit `build_bpf` in `seccomp_lock.rs` so that **for `MlClient` only** the
mismatch action is `Log` instead of `KillProcess`. Apply this minimal diff:

```rust
    // TEMPORARY (Task 7 enumeration — revert before commit): log, don't kill,
    // unlisted syscalls under MlClient so a real torch run surfaces everything
    // it needs in dmesg instead of dying on the first one.
    let mismatch = if matches!(profile, Profile::MlClient) {
        SeccompAction::Log
    } else {
        SeccompAction::KillProcess
    };
    let filter = SeccompFilter::new(
        rules,
        mismatch,
        SeccompAction::Allow,
        target_arch()?,
    )
```

Push to the DGX:
```bash
git stash || true   # keep this diff LOCAL to the DGX run; do NOT commit it
# OR: apply the edit directly on the DGX via the editor of choice, then:
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --workspace'
```
(Simplest: make the temporary edit directly on the DGX tree so it never touches the committed branch. `cargo build --workspace` — NOT `-p ... --tests` — so the shim bin is rebuilt fresh, the #281 process gotcha.)
Expected: clean build on the DGX.

- [ ] **Step 4: Run a real model-load + extract under the log-mode filter and harvest denials**

First ensure `SECCOMP_RET_LOG` actually reaches `dmesg` (it is silent otherwise —
the `log` action must be enabled in the kernel sysctl, and auditd is off here):

```bash
ssh dgx 'cat /proc/sys/kernel/seccomp/actions_logged; echo log | sudo tee /proc/sys/kernel/seccomp/actions_logged; sudo dmesg -C'
```
Expected: `actions_logged` now contains `log`; the kernel ring buffer is cleared
so only this run's denials appear. (Fallback if RET_LOG still doesn't surface:
`strace -f -e trace=all` the worker and diff the observed syscall set against
`allow_list_for(Profile::MlClient)` — but the dmesg path is primary.)

Run the gliner real-model e2e (or the manifest-registered worker via a dispatch e2e) with the worker enabled, then read the kernel log:

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  KASTELLAN_GLINER_RELEX_ENABLE=1 \
  cargo test -p kastellan-core --test gliner_relex_e2e -- --ignored --nocapture; \
  echo "=== seccomp denials ==="; \
  sudo dmesg | grep -i "seccomp" | tail -80'
```

Expected: the extract completes (Log doesn't block); `dmesg` lists
`audit: type=1326 ... syscall=<nr>` lines for each denied syscall. Collect the
distinct `syscall=<nr>` numbers. (If `gliner_relex_e2e` isn't the right entry,
use `entity_extraction_e2e` — whichever drives a real model load + extract;
grep the test list with `cargo test -p kastellan-core -- --list 2>/dev/null | grep -i gliner`.)

- [ ] **Step 5: Map the syscall numbers to names and curate the list**

For each `syscall=<nr>` (aarch64 ABI), map to its name (cross-reference
`arch/arm64/include/uapi/asm-generic/unistd.h` or `ausyscall <nr>` if available:
`ssh dgx 'ausyscall <nr> 2>/dev/null'`). Keep only **safe** syscalls. **Reject**
(do not add) any of: `unshare`, `setns`, the mount family, `ptrace`,
`process_vm_readv`/`writev`, `bpf`, `perf_event_open`, `kexec*`, `keyctl`/
`add_key`/`request_key`, `io_uring_*`. If `io_uring_*` appears, wire the
EPERM-downgrade carve-out (mirror `build_io_uring_eperm_bpf` + the BrowserClient
install order in `apply`) instead of allowing it — and add a unit test like
`io_uring_is_allowed_in_the_main_filter_but_eperm_listed_separately` for MlClient.
If a genuinely dangerous syscall is load-bearing for torch, STOP and reassess
with the operator before proceeding.

- [ ] **Step 6: Populate `ML_CLIENT_ADDITIONS` (committed edit, on the local Mac tree)**

Back on the Mac branch, fill the const with the curated names (use `libc::SYS_*`;
for any aarch64-unexposed number follow the `SYS_SENDFILE` local-const precedent),
each with a one-line comment naming the torch/lib reason it was observed:

```rust
pub const ML_CLIENT_ADDITIONS: &[i64] = &[
    // <example shape — replace with the actual enumerated set>
    // libc::SYS_mlock,   // torch pinned-memory allocator
    // libc::SYS_mbind,   // OpenMP/NUMA thread placement
    // ...
];
```

Update the `ml_client_excludes_escape_primitives` test if needed (it already
covers the rejects). Add a test asserting a representative observed syscall is
present, e.g.:

```rust
#[test]
fn ml_client_includes_enumerated_additions() {
    let ml = allow_list_for(Profile::MlClient);
    let strict = allow_list_for(Profile::Strict);
    for nr in ML_CLIENT_ADDITIONS {
        assert!(ml.contains(nr), "MlClient missing enumerated syscall {nr}");
        // additions are ML-specific: not already in strict.
        assert!(!strict.contains(nr), "enumerated syscall {nr} already in Strict");
    }
}
```

- [ ] **Step 7: Revert the temporary Log diff; rebuild on the DGX**

Ensure `build_bpf` is back to unconditional `SeccompAction::KillProcess` (the
temporary edit was DGX-local and never committed; confirm `git diff` on the DGX
shows nothing but pull the now-populated const from the branch):

```bash
git add workers/prelude/src/seccomp_lock.rs
git commit -m "feat(prelude): populate ml_client ML_CLIENT_ADDITIONS from DGX enumeration (#281)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
git push
ssh dgx 'cd ~/src/kastellan && git checkout -- . && git fetch origin && git reset --hard origin/feat/281-gliner-relex-seccomp'
```

---

## Task 8: DGX kill-mode acceptance + full workspace gate

**Files:** none (verification only)

- [ ] **Step 1: Fresh full build (shim bin must be current — #281 gotcha)**

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && cargo build --workspace'
```
Expected: clean.

- [ ] **Step 2: Real extract under the KILL-mode `ml_client` filter**

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  KASTELLAN_GLINER_RELEX_ENABLE=1 \
  cargo test -p kastellan-core --test gliner_relex_e2e -- --ignored --nocapture'
```
Expected: the real model load + `extract` PASSES under the kill-mode filter —
proving `ml_client` doesn't break torch. If it SIGSYS-kills (worker dies
mid-load), a syscall was missed: re-run Task 7 Step 4-6 for the new denial.

- [ ] **Step 3: Full native-Linux workspace gate**

```bash
ssh dgx 'cd ~/src/kastellan && source ~/.cargo/env && \
  cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings'
```
Expected: all green (baseline was 1829/0 on the #281 branch; expect ≥ that, +
the new unit tests). Record the exact passed/failed/ignored counts for the
HANDOVER `Session-end verification` line.

- [ ] **Step 4: No commit (verification task).** If any fix was needed, commit it
with a `fix(...)` message and re-run Steps 1-3.

---

## Self-review notes

- **Spec coverage:** §1 mechanism → Tasks 4/5; §2 ml_client profile → Tasks 1/2/3; §3 manifest wiring → Tasks 4/5; §4 enumeration → Task 7; §5 testing → Tasks 1-6 (unit) + 7/8 (DGX); §6 files → all tasks; §7 risks → Task 7 Step 5 (stop-and-reassess) + Task 8 Step 2 (missed-syscall loop).
- **Empirical const:** `ML_CLIENT_ADDITIONS` ships empty in Task 2 and is populated in Task 7 — not a placeholder; the structural unit tests (superset, excludes escape primitives) are valid at every stage.
- **Signature ripple:** Task 4 Step 1 updates all ≈15 `gliner_relex_entry(&env)` call sites to the 2-arg form before any behavioural change, keeping every intermediate commit compiling.
- **macOS safety:** `WorkerMlClient` renders as `WorkerStrict` on Seatbelt/container; gliner's macOS path is byte-identical (Task 1 + Task 6 verify).
