# python-exec warm/idle container lifecycle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the operator keep the macOS python-exec micro-VM warm between `python.exec` calls (opt-in), amortising the ~0.7 s VM boot, while preserving per-call isolation by wiping the in-VM scratch between calls.

**Architecture:** Reuse the existing `IdleTimeout` worker lifecycle (already used by GLiNER-Relex; `CompositeLifecycle` routes by `entry.lifecycle`). Container-mode python-exec declares `IdleTimeout` when `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS > 0`, else stays `SingleUse` (today's behaviour). The worker wipes its scratch-dir contents at the start of every call so a reused VM starts each call from a pristine `/tmp`.

**Tech Stack:** Rust (workspace crates `kastellan-worker-python-exec` and `kastellan-core`), Apple `container` micro-VM backend on macOS, JSON-RPC over stdio.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new third-party deps are needed for this plan (std + existing `tempfile` dev-dep only).
- **Cross-platform.** The warm lifecycle is a macOS-only mechanism (container-gated, `#[cfg(target_os = "macos")]`). The worker-side scratch wipe lives in shared `run_code` and MUST be a no-op on the Linux/bwrap and macOS-host paths (their per-spawn scratch is already empty).
- **TDD.** Every task is failing-test-first, minimal implementation, green, commit.
- **File size target ≤ 500 LOC.** `workers/python-exec/src/exec/mod.rs` is currently ~330 LOC; `core/src/workers/python_exec.rs` is ~450 LOC. Stay under 500; if a change would breach it, lift the new pure helpers into a sibling module.
- **Build/test prelude (non-interactive shells):** `source "$HOME/.cargo/env"` before any cargo command.
- **Worker runs as `nobody` in the VM**, owns the files it and its `python3` child wrote (same uid), so it can always remove them.
- **Commit hygiene:** `git add <specific paths>` only — never `git add -A` (untracked `assets/agent_with_the_keys.png` must stay out). End every commit message with the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer.
- **Branch:** all work lands on `feat/python-exec-warm-idle-container` (already created; the design spec is its first commit).

---

## File Structure

- `workers/python-exec/src/exec/mod.rs` — **modify**: add pure `wipe_scratch_contents`; call it at the top of `run_code`.
- `workers/python-exec/src/exec/tests.rs` — **modify**: unit tests for `wipe_scratch_contents`.
- `core/src/workers/python_exec.rs` — **modify**: new env constants, pure `parse_idle_caps` + `container_lifecycle`, give `container_mode_entry` a `lifecycle` parameter, parse caps in the resolver. Unit tests in the existing inline `mod tests`.
- `core/tests/python_exec_warm_idle_e2e.rs` — **create**: real-micro-VM integration tests (warm reuse, `/tmp` wipe across reuse, idle teardown).

---

## Task 1: Worker-side scratch wipe (`wipe_scratch_contents`)

Restores pristine-`/tmp` parity for every call when the worker is reused. Lives in the worker crate so it ships inside the VM image.

**Files:**
- Modify: `workers/python-exec/src/exec/mod.rs` (add helper; call it inside `run_code` at line ~286)
- Test: `workers/python-exec/src/exec/tests.rs`

**Interfaces:**
- Produces: `pub fn wipe_scratch_contents(dir: &std::path::Path) -> std::io::Result<usize>` — removes every entry (files + nested dirs) directly under `dir`, leaves `dir` itself in place, returns the count of top-level entries removed. Best-effort per entry: a per-entry removal error is logged to stderr and skipped (not fatal), so a single stale file can't abort the run; the count reflects successful removals.

- [ ] **Step 1: Write the failing tests**

Add to `workers/python-exec/src/exec/tests.rs` (the module already has `use super::*;` and uses `tempfile` — confirm `tempfile` is a dev-dependency of the crate; the existing `write_params_file_writes_exact_content_mode_0600` test at line ~223 already uses a temp dir, so the pattern is present):

```rust
#[test]
fn wipe_scratch_contents_removes_files_and_subdirs_keeps_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join("params.json"), b"{}").unwrap();
    std::fs::write(root.join("leak.txt"), b"secret").unwrap();
    let sub = root.join("nested");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("inner.bin"), b"x").unwrap();

    let removed = wipe_scratch_contents(root).expect("wipe ok");

    assert_eq!(removed, 3, "params.json + leak.txt + nested/ are 3 top-level entries");
    assert!(root.is_dir(), "the scratch dir itself must remain");
    assert_eq!(
        std::fs::read_dir(root).unwrap().count(),
        0,
        "scratch dir must be empty after wipe"
    );
}

#[test]
fn wipe_scratch_contents_is_noop_on_empty_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let removed = wipe_scratch_contents(dir.path()).expect("wipe ok");
    assert_eq!(removed, 0, "empty dir → nothing removed (the fresh-VM no-op case)");
    assert!(dir.path().is_dir());
}

#[test]
fn wipe_scratch_contents_missing_dir_is_ok_zero() {
    // A not-yet-created scratch dir must not error — run_code tolerates it
    // (it only sets cwd when the dir exists). Treat absent as "nothing to wipe".
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist");
    let removed = wipe_scratch_contents(&missing).expect("missing dir is ok");
    assert_eq!(removed, 0);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-python-exec wipe_scratch_contents
```

Expected: FAIL — `cannot find function wipe_scratch_contents in this scope` (compile error).

- [ ] **Step 3: Implement `wipe_scratch_contents`**

Add to `workers/python-exec/src/exec/mod.rs`, just above `pub fn run_code` (around line 280). Keep the doc comment junior-readable:

```rust
/// Remove the *contents* of the scratch directory (files and nested
/// directories) while leaving the directory itself in place.
///
/// Called at the start of every `python.exec` run so that when the worker is
/// **reused** under the idle-timeout lifecycle (a warm micro-VM serving many
/// calls) each call starts from a pristine working area — exactly as a fresh
/// `SingleUse` VM would. This is the isolation guarantee that makes warm reuse
/// safe: a file an earlier call left under `/tmp` cannot be observed by a later
/// call, and the VM's memory headroom is reset each call.
///
/// **Idempotent:** on a fresh VM (or any `SingleUse` spawn) the scratch dir is
/// already empty, so this is a no-op — which is why it can live unconditionally
/// in `run_code` regardless of lifecycle.
///
/// **Best-effort per entry:** the worker runs as the same uid as its `python3`
/// child, so it owns every file either wrote and removal normally succeeds. If
/// one entry can't be removed we log to stderr (captured by the parent's
/// stderr drain) and continue rather than aborting the whole run; the
/// subsequent `params.json` write is the fail-closed gate for the call.
/// A missing directory is treated as "nothing to wipe" (returns `Ok(0)`).
///
/// Returns the number of top-level entries successfully removed.
pub fn wipe_scratch_contents(dir: &Path) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // A not-yet-created scratch dir is not an error: there is nothing to wipe.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        // Symlinks and files: remove_file. Directories: remove_dir_all. We
        // check the *symlink* metadata so a symlinked dir is unlinked (not
        // traversed), which is both correct and safer.
        let result = match entry.file_type() {
            Ok(ft) if ft.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) => eprintln!(
                "python-exec: failed to wipe scratch entry {}: {e}",
                path.display()
            ),
        }
    }
    Ok(removed)
}
```

- [ ] **Step 4: Call it from `run_code`**

In `workers/python-exec/src/exec/mod.rs`, modify `run_code` (currently lines 286–293) so the wipe runs **before** the new params file is written:

```rust
    let scratch = scratch_dir_from_env(|k| std::env::var(k).ok());
    let scratch_path = Path::new(&scratch);
    // Restore pristine-scratch isolation for this call. No-op on a fresh VM /
    // SingleUse spawn (dir already empty); load-bearing only under warm reuse.
    let _ = wipe_scratch_contents(scratch_path);
    let file_path = scratch_path.join(PARAMS_FILE_NAME);
    if matches!(channel, ParamChannel::File) {
        // Fail-closed: a scratch-write error aborts the run rather than
        // falling back to the oversize env channel (which would exceed the
        // execve wall).
        write_params_file(&file_path, params_json)?;
    }
```

(We deliberately swallow the wipe's `Result` with `let _ =` here: an unexpected `read_dir` error on the scratch root must not block the call — the per-entry failures are already logged inside the helper, and the helper's `Result` is exercised directly by the unit tests. The params write below remains the fail-closed gate.)

- [ ] **Step 5: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-python-exec wipe_scratch_contents
```

Expected: PASS (3 tests). Also run the whole worker crate to confirm no regression in the existing exec tests:

```sh
cargo test -p kastellan-worker-python-exec
```

Expected: PASS (all existing + 3 new).

- [ ] **Step 6: Clippy + commit**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-worker-python-exec --all-targets -- -D warnings
git add workers/python-exec/src/exec/mod.rs workers/python-exec/src/exec/tests.rs
git commit -m "feat(python-exec): per-call scratch wipe for warm-VM reuse

wipe_scratch_contents clears the scratch dir before each python.exec run so
a reused (warm) micro-VM starts every call from a pristine /tmp, matching
SingleUse isolation. Idempotent no-op on fresh spawns.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Core-side caps parsing + lifecycle wiring

Makes container-mode python-exec declare `IdleTimeout` when the operator opts in via `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS`, defaulting to today's `SingleUse`.

**Files:**
- Modify: `core/src/workers/python_exec.rs` (env constants near line 56; new pure helpers; `container_mode_entry` signature at line 223; resolver at line 346; inline `mod tests`)

**Interfaces:**
- Consumes: `crate::worker_lifecycle::{Lifecycle, IdleTimeoutCaps, Contract}` (existing).
- Produces (all `#[cfg(target_os = "macos")]`, used only by the resolver + tests):
  - `fn parse_idle_caps(get_env: impl Fn(&str) -> Option<String>) -> (Option<u64>, u64, u64)` returning `(idle_seconds, max_requests, max_age_seconds)`. `idle_seconds` is `None` when `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS` is unset/empty/unparseable/`0`.
  - `fn container_lifecycle(idle_seconds: Option<u64>, max_requests: u64, max_age_seconds: u64) -> Lifecycle` — `None`/`Some(0)` → `Lifecycle::SingleUse`; `Some(n>0)` → `Lifecycle::idle_timeout(...)` with `grace_period_seconds = 5` and `Contract { stateless: true }`.
  - `container_mode_entry(binary, image, params_file_max, lifecycle: Lifecycle)` — new fourth parameter replacing the hardcoded `Lifecycle::SingleUse`.

- [ ] **Step 1: Add the env constants**

In `core/src/workers/python_exec.rs`, after the existing `IMAGE_ENV` const (line ~60), add:

```rust
/// Opt-in knob for the warm/idle container lifecycle. `> 0` keeps the macOS
/// micro-VM warm for that many idle seconds between calls; `0`/unset/garbage →
/// today's per-call `SingleUse` boot. Container-mode only (host paths are
/// already cheap to spawn).
const IDLE_SECONDS_ENV: &str = "KASTELLAN_PYTHON_EXEC_IDLE_SECONDS";
/// Override for the warm worker's cumulative request cap (slow-leak hygiene).
/// Default `DEFAULT_MAX_REQUESTS`.
const MAX_REQUESTS_ENV: &str = "KASTELLAN_PYTHON_EXEC_MAX_REQUESTS";
/// Override for the warm worker's max-age cap in seconds (drift hygiene).
/// Default `DEFAULT_MAX_AGE_SECONDS`.
const MAX_AGE_SECONDS_ENV: &str = "KASTELLAN_PYTHON_EXEC_MAX_AGE_SECONDS";

/// Default cumulative-request cap, mirroring GLiNER-Relex's manifest.
const DEFAULT_MAX_REQUESTS: u64 = 10_000;
/// Default max-age cap (24 h), mirroring GLiNER-Relex's manifest.
const DEFAULT_MAX_AGE_SECONDS: u64 = 86_400;
/// SIGTERM grace before SIGKILL on warm-worker teardown (fixed; matches GLiNER).
const IDLE_GRACE_SECONDS: u64 = 5;
```

> Note: these are gated to where they're used. If the crate warns about unused consts on Linux, wrap the four idle-specific items in `#[cfg(target_os = "macos")]` (the resolver that reads them is already macOS-gated). Add the cfg attribute if `cargo build` on Linux warns.

- [ ] **Step 2: Write the failing unit tests**

In the existing `#[cfg(test)] mod tests` block at the bottom of `core/src/workers/python_exec.rs`, add (these are macOS-only because they exercise macOS-gated items — guard the new tests with `#[cfg(target_os = "macos")]`):

```rust
#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_unset_yields_single_use_defaults() {
    let (idle, max_req, max_age) = parse_idle_caps(|_| None);
    assert_eq!(idle, None, "no IDLE_SECONDS → SingleUse");
    assert_eq!(max_req, DEFAULT_MAX_REQUESTS);
    assert_eq!(max_age, DEFAULT_MAX_AGE_SECONDS);
}

#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_reads_idle_seconds_and_overrides() {
    let env = |k: &str| match k {
        IDLE_SECONDS_ENV => Some("120".to_string()),
        MAX_REQUESTS_ENV => Some("50".to_string()),
        MAX_AGE_SECONDS_ENV => Some("3600".to_string()),
        _ => None,
    };
    let (idle, max_req, max_age) = parse_idle_caps(env);
    assert_eq!(idle, Some(120));
    assert_eq!(max_req, 50);
    assert_eq!(max_age, 3600);
}

#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_zero_and_garbage_fall_back_to_single_use() {
    assert_eq!(parse_idle_caps(|k| (k == IDLE_SECONDS_ENV).then(|| "0".to_string())).0, None);
    assert_eq!(parse_idle_caps(|k| (k == IDLE_SECONDS_ENV).then(|| "abc".to_string())).0, None);
    assert_eq!(parse_idle_caps(|k| (k == IDLE_SECONDS_ENV).then(String::new)).0, None);
}

#[cfg(target_os = "macos")]
#[test]
fn parse_idle_caps_garbage_overrides_use_defaults() {
    // A garbage max_requests/max_age must not panic — fall back to the default.
    let env = |k: &str| match k {
        IDLE_SECONDS_ENV => Some("60".to_string()),
        MAX_REQUESTS_ENV => Some("notnum".to_string()),
        _ => None,
    };
    let (idle, max_req, max_age) = parse_idle_caps(env);
    assert_eq!(idle, Some(60));
    assert_eq!(max_req, DEFAULT_MAX_REQUESTS);
    assert_eq!(max_age, DEFAULT_MAX_AGE_SECONDS);
}

#[cfg(target_os = "macos")]
#[test]
fn container_lifecycle_none_is_single_use() {
    assert!(matches!(container_lifecycle(None, 10_000, 86_400), Lifecycle::SingleUse));
    assert!(matches!(container_lifecycle(Some(0), 10_000, 86_400), Lifecycle::SingleUse));
}

#[cfg(target_os = "macos")]
#[test]
fn container_lifecycle_positive_is_idle_timeout_with_caps() {
    match container_lifecycle(Some(120), 50, 3600) {
        Lifecycle::IdleTimeout { caps, contract } => {
            assert_eq!(caps.idle_seconds, 120);
            assert_eq!(caps.max_requests, 50);
            assert_eq!(caps.max_age_seconds, 3600);
            assert_eq!(caps.grace_period_seconds, IDLE_GRACE_SECONDS);
            assert!(contract.stateless);
        }
        other => panic!("expected IdleTimeout, got {other:?}"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn resolve_container_entry_is_idle_timeout_when_idle_seconds_set() {
    // USE_CONTAINER + ENABLE + IDLE_SECONDS=120 → registered entry is IdleTimeout.
    let entry = resolve_container_entry_for_test(|k: &str| match k {
        ENABLE_ENV => Some("1".to_string()),
        USE_CONTAINER_ENV => Some("1".to_string()),
        IDLE_SECONDS_ENV => Some("120".to_string()),
        _ => None,
    });
    assert!(matches!(entry.lifecycle, Lifecycle::IdleTimeout { .. }));
}

#[cfg(target_os = "macos")]
#[test]
fn resolve_container_entry_is_single_use_without_idle_seconds() {
    let entry = resolve_container_entry_for_test(|k: &str| match k {
        ENABLE_ENV => Some("1".to_string()),
        USE_CONTAINER_ENV => Some("1".to_string()),
        _ => None,
    });
    assert!(matches!(entry.lifecycle, Lifecycle::SingleUse));
}
```

The two `resolve_*` tests need a tiny test helper that builds the container entry from an env closure without the full `ResolveCtx`. Add it inside the `mod tests` block:

```rust
/// Build the container-mode entry the resolver would register, from an env
/// closure. Mirrors the resolver's container short-circuit (binary/image/
/// params_file_max/caps) so the lifecycle wiring can be asserted without a
/// full `ResolveCtx`.
#[cfg(target_os = "macos")]
fn resolve_container_entry_for_test(get_env: impl Fn(&str) -> Option<String>) -> ToolEntry {
    let image = get_env(IMAGE_ENV)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_IMAGE.to_string());
    let params_file_max = get_env(PARAMS_FILE_MAX_ENV);
    let (idle, max_req, max_age) = parse_idle_caps(&get_env);
    container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        image,
        params_file_max,
        container_lifecycle(idle, max_req, max_age),
    )
}
```

- [ ] **Step 3: Run the tests to verify they fail**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib workers::python_exec
```

Expected: FAIL — compile errors (`parse_idle_caps`, `container_lifecycle` not found; `container_mode_entry` arity mismatch).

- [ ] **Step 4: Implement the pure helpers**

Add to `core/src/workers/python_exec.rs`, `#[cfg(target_os = "macos")]`-gated (place them just above `container_mode_entry`):

```rust
/// Parse the warm/idle env knobs into `(idle_seconds, max_requests, max_age_seconds)`.
///
/// `idle_seconds` is `None` (→ `SingleUse`) unless `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS`
/// parses to a value `> 0`. The two cap overrides fall back to their defaults on
/// absent/unparseable input — fail-safe to the conservative GLiNER-mirrored values.
#[cfg(target_os = "macos")]
fn parse_idle_caps(get_env: impl Fn(&str) -> Option<String>) -> (Option<u64>, u64, u64) {
    let parse_u64 = |key: &str| -> Option<u64> {
        get_env(key)
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    let idle_seconds = parse_u64(IDLE_SECONDS_ENV).filter(|&n| n > 0);
    let max_requests = parse_u64(MAX_REQUESTS_ENV).unwrap_or(DEFAULT_MAX_REQUESTS);
    let max_age_seconds = parse_u64(MAX_AGE_SECONDS_ENV).unwrap_or(DEFAULT_MAX_AGE_SECONDS);
    (idle_seconds, max_requests, max_age_seconds)
}

/// Build the container-mode lifecycle from the parsed idle window.
///
/// `None`/`Some(0)` → `SingleUse` (today's per-call boot). `Some(n>0)` →
/// `IdleTimeout` keeping the warm VM for `n` idle seconds, with the request/age
/// caps and a fixed 5 s SIGTERM grace. The `Contract { stateless: true }` holds:
/// the agent's Python runs as a fresh subprocess per call and the worker wipes
/// its scratch between calls (see `wipe_scratch_contents` in the worker crate).
#[cfg(target_os = "macos")]
fn container_lifecycle(
    idle_seconds: Option<u64>,
    max_requests: u64,
    max_age_seconds: u64,
) -> Lifecycle {
    match idle_seconds {
        Some(n) if n > 0 => Lifecycle::idle_timeout(
            IdleTimeoutCaps {
                idle_seconds: n,
                max_requests,
                max_age_seconds,
                grace_period_seconds: IDLE_GRACE_SECONDS,
            },
            Contract { stateless: true },
        )
        .expect("stateless = true; validator must accept"),
        _ => Lifecycle::SingleUse,
    }
}
```

Add the import near the top (line ~30). To avoid an unused-import warning on Linux (where these items aren't referenced), gate it:

```rust
#[cfg(target_os = "macos")]
use crate::worker_lifecycle::{Contract, IdleTimeoutCaps, Lifecycle};
```

(The existing code references `crate::worker_lifecycle::Lifecycle::SingleUse` by full path in `python_exec_entry`; leave that line as-is so the host-mode entry needs no import on Linux.)

- [ ] **Step 5: Give `container_mode_entry` a `lifecycle` parameter**

Change the signature (line 223) and the `ToolEntry` literal (line 248):

```rust
#[cfg(target_os = "macos")]
pub fn container_mode_entry(
    binary: PathBuf,
    image: String,
    params_file_max: Option<String>,
    lifecycle: Lifecycle,
) -> ToolEntry {
    // ... policy unchanged ...
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::Container),
        container_image: Some(image),
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}
```

Update the doc comment's "`SingleUse`" latency sentence (lines ~214-216) to note the opt-in warm path:

```rust
/// Latency: ~0.8 s container warm-spawn per call under `SingleUse`. Set
/// `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS > 0` to keep the VM warm between calls
/// (the `IdleTimeout` lifecycle) and amortise that boot; the worker wipes its
/// scratch between calls so each call still sees a pristine `/tmp`.
```

- [ ] **Step 6: Wire the resolver**

In the resolver's container short-circuit (lines 340–350), parse caps and pass the lifecycle:

```rust
            if enabled && use_container {
                let binary = PathBuf::from(CONTAINER_WORKER_BIN);
                let image = (ctx.get_env)(IMAGE_ENV)
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_IMAGE.to_string());
                let params_file_max = (ctx.get_env)(PARAMS_FILE_MAX_ENV);
                let (idle, max_req, max_age) = parse_idle_caps(|k| (ctx.get_env)(k));
                return Resolution::Register(container_mode_entry(
                    binary,
                    image,
                    params_file_max,
                    container_lifecycle(idle, max_req, max_age),
                ));
            }
```

- [ ] **Step 7: Fix the other `container_mode_entry` caller**

`core/tests/python_exec_container_e2e.rs` (around line 91) calls `container_mode_entry(binary, image, None)`. Update it to pass `SingleUse` so that suite's behaviour is unchanged:

```rust
    let entry = container_mode_entry(
        std::path::PathBuf::from(
            kastellan_core::workers::python_exec::CONTAINER_WORKER_BIN,
        ),
        DEFAULT_IMAGE.to_string(),
        None,
        kastellan_core::worker_lifecycle::Lifecycle::SingleUse,
    );
```

Grep for any other caller before building:

```sh
grep -rn "container_mode_entry(" core/ workers/ | grep -v "fn container_mode_entry"
```

Update each hit to add the trailing `Lifecycle::SingleUse` argument (there should be exactly the resolver, the resolver-test helper from Step 2, and the container e2e).

- [ ] **Step 8: Run the tests to verify they pass**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib workers::python_exec
```

Expected: PASS (existing python_exec unit tests + the 8 new ones).

- [ ] **Step 9: Clippy (both OS cfgs) + commit**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --lib --tests -- -D warnings
# Confirm the macOS-gated code doesn't break the Linux build (pure-Rust cfg check):
cargo clippy -p kastellan-core --lib --target aarch64-unknown-linux-gnu 2>/dev/null || \
  echo "NOTE: core has a ring C dep; Linux cross-clippy may not link — rely on DGX/CI for the Linux gate"
git add core/src/workers/python_exec.rs core/tests/python_exec_container_e2e.rs
git commit -m "feat(python-exec): opt-in IdleTimeout for container mode

KASTELLAN_PYTHON_EXEC_IDLE_SECONDS>0 makes container-mode python-exec declare
the IdleTimeout lifecycle (warm VM reuse), else SingleUse. parse_idle_caps +
container_lifecycle are pure + unit-tested; caps mirror GLiNER (10k requests,
24h age, 5s grace), overridable via _MAX_REQUESTS/_MAX_AGE_SECONDS.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Real-micro-VM integration e2e

Proves the end-to-end behaviour: a warm VM is reused across calls, the `/tmp` wipe isolates calls, and the idle timer tears the VM down. `[SKIP]`s cleanly without the Apple `container` service + image.

**Files:**
- Create: `core/tests/python_exec_warm_idle_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_core::worker_lifecycle::{IdleTimeoutLifecycle, IdleTimeoutCaps, Contract, Lifecycle, WorkerLifecycleManager}`; `kastellan_core::workers::python_exec::{container_mode_entry, DEFAULT_IMAGE, CONTAINER_WORKER_BIN}`; `kastellan_core::tool_host::{dispatch_with_sink, AuditSink}`; `kastellan_core::secrets::Vault`; `kastellan_sandbox::{SandboxBackend, SandboxBackends, SandboxBackendKind, SandboxPolicy, SandboxError, macos_container::MacosContainer}`.
- The handle's worker is reached via `handle.worker_mut() -> &mut SupervisedWorker` (manager.rs:93), which `dispatch_with_sink` takes by `&mut`.

- [ ] **Step 1: Write the test file (failing — image-gated)**

Create `core/tests/python_exec_warm_idle_e2e.rs`:

```rust
//! End-to-end: python-exec under the macOS micro-VM with the warm/idle
//! lifecycle (`KASTELLAN_PYTHON_EXEC_IDLE_SECONDS > 0`).
//!
//! Pins the three properties warm reuse must hold:
//!   1. **Warm reuse** — N acquire→dispatch→release cycles boot the VM ONCE
//!      (asserted via a spawn-counting backend).
//!   2. **/tmp wipe across reuse** — a sentinel file written under /tmp by call
//!      1 is GONE for call 2 on the same warm VM (the isolation guarantee).
//!   3. **Idle teardown** — after `idle_seconds` with no call, the warm slot
//!      clears.
//!
//! `[SKIP]`s when Apple `container` / its service / the python-exec image are
//! missing. Build the image first:
//!     scripts/workers/python-exec/build-image.sh

#![cfg(target_os = "macos")]

use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch_with_sink, AuditSink};
use kastellan_core::worker_lifecycle::{
    Contract, IdleTimeoutCaps, IdleTimeoutLifecycle, Lifecycle, WorkerLifecycleManager,
};
use kastellan_core::workers::python_exec::{
    container_mode_entry, CONTAINER_WORKER_BIN, DEFAULT_IMAGE,
};
use kastellan_db::DbError;
use kastellan_sandbox::macos_container::MacosContainer;
use kastellan_sandbox::{
    SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxError, SandboxPolicy,
};

const TOOL_NAME: &str = "python-exec";

struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    async fn insert(
        &self,
        _actor: &str,
        _action: &str,
        _payload: serde_json::Value,
    ) -> Result<i64, DbError> {
        Ok(1)
    }
}

/// Spawn-counting wrapper over the real Container backend.
struct CountingBackend {
    inner: Arc<dyn SandboxBackend>,
    count: Arc<AtomicUsize>,
}

impl SandboxBackend for CountingBackend {
    fn spawn_under_policy(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[&str],
    ) -> Result<Child, SandboxError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn_under_policy(policy, program, args)
    }
}

fn skip_if_no_container_image() -> bool {
    if let Err(e) = MacosContainer::probe() {
        eprintln!("\n[SKIP] container probe failed: {e}\n");
        return true;
    }
    let listed = std::process::Command::new("container")
        .args(["image", "list"])
        .output();
    let has_image = matches!(
        listed,
        Ok(o) if String::from_utf8_lossy(&o.stdout).contains("python-exec")
    );
    if !has_image {
        eprintln!(
            "\n[SKIP] {DEFAULT_IMAGE} image not present; run \
             scripts/workers/python-exec/build-image.sh\n"
        );
        return true;
    }
    false
}

/// Build an idle-timeout lifecycle whose Container slot is the counting backend.
fn lifecycle_with_counter(count: Arc<AtomicUsize>) -> IdleTimeoutLifecycle {
    let real = SandboxBackends::default_for_current_os()
        .resolve(Some(SandboxBackendKind::Container), Some(DEFAULT_IMAGE));
    let counting: Arc<dyn SandboxBackend> = Arc::new(CountingBackend { inner: real, count });
    // The python-exec entry sets sandbox_backend: Some(Container), so only the
    // container slot is consulted; fill it with the counting backend. The other
    // slots are unused by this entry but must be present — reuse the same arc.
    let bundle = Arc::new(SandboxBackends {
        seatbelt: Arc::clone(&counting),
        container: counting,
    });
    IdleTimeoutLifecycle::new(bundle)
}

/// A container entry with an explicit idle window (overrides the env-driven default).
fn warm_entry(idle_seconds: u64) -> kastellan_core::scheduler::ToolEntry {
    let lifecycle = Lifecycle::idle_timeout(
        IdleTimeoutCaps {
            idle_seconds,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        },
        Contract { stateless: true },
    )
    .expect("valid lifecycle");
    container_mode_entry(
        std::path::PathBuf::from(CONTAINER_WORKER_BIN),
        DEFAULT_IMAGE.to_string(),
        None,
        lifecycle,
    )
}

/// Dispatch one `python.exec` over an already-acquired warm handle.
async fn dispatch_over_handle(
    handle: &mut kastellan_core::worker_lifecycle::WorkerHandle,
    code: &str,
) -> serde_json::Value {
    dispatch_with_sink(
        &NoopAuditSink,
        &Vault::new(),
        handle.worker_mut(),
        TOOL_NAME,
        "python.exec",
        serde_json::json!({ "code": code }),
    )
    .await
    .expect("dispatch python.exec")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_reuse_three_calls_boot_vm_once() {
    if skip_if_no_container_image() {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(60);

    for cycle in 1..=3 {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire");
        let out = dispatch_over_handle(&mut handle, "print(6*7)").await;
        assert_eq!(out["stdout"].as_str().unwrap_or_default().trim(), "42",
            "cycle {cycle}: expected 42");
        assert_eq!(out["exit_code"], 0);
        drop(handle);
        assert!(lifecycle._test_slot_has_warm(TOOL_NAME).await,
            "cycle {cycle}: slot should be warm after release");
    }
    assert_eq!(count.load(Ordering::SeqCst), 1,
        "three warm calls must boot the VM exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tmp_is_wiped_between_warm_calls() {
    if skip_if_no_container_image() {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(60);

    // Call 1: write a sentinel under /tmp (the in-VM scratch tmpfs).
    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 1");
        let out = dispatch_over_handle(
            &mut handle,
            "open('/tmp/leak','w').write('secret'); print('wrote')",
        )
        .await;
        assert_eq!(out["exit_code"], 0, "call 1 should write the sentinel: {out}");
        drop(handle);
    }

    // Call 2 on the SAME warm VM: the sentinel must be gone (wiped at call start).
    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire 2");
        let out = dispatch_over_handle(
            &mut handle,
            "import os; print('EXISTS' if os.path.exists('/tmp/leak') else 'GONE')",
        )
        .await;
        let stdout = out["stdout"].as_str().unwrap_or_default();
        assert!(stdout.contains("GONE"),
            "call 2 must not see call 1's /tmp sentinel (per-call wipe), got: {out}");
        drop(handle);
    }

    assert_eq!(count.load(Ordering::SeqCst), 1,
        "both calls ran on one warm VM (else the wipe assertion is vacuous)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_teardown_clears_warm_slot() {
    if skip_if_no_container_image() {
        return;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let lifecycle = lifecycle_with_counter(Arc::clone(&count));
    let entry = warm_entry(1); // 1-second idle window

    {
        let mut handle = lifecycle.acquire(TOOL_NAME, &entry).await.expect("acquire");
        let _ = dispatch_over_handle(&mut handle, "print('ok')").await;
        drop(handle);
    }
    assert!(lifecycle._test_slot_has_warm(TOOL_NAME).await, "warm right after release");

    tokio::time::sleep(Duration::from_millis(2_000)).await;

    assert!(!lifecycle._test_slot_has_warm(TOOL_NAME).await,
        "after the idle window the warm slot must be torn down");
}
```

- [ ] **Step 2: Verify the file compiles and tests are discovered**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test python_exec_warm_idle_e2e --no-run
```

Expected: compiles. If the `SandboxBackends` literal fields differ (confirm the exact field set with `grep -n "pub struct SandboxBackends" -A8 sandbox/src/*.rs` — on macOS it has `seatbelt` + `container`; there is no `bwrap` field under `#[cfg(target_os = "macos")]`), adjust the `bundle` literal to match. Fix any field/type mismatch until it compiles.

- [ ] **Step 3: Build the image if needed, then run the e2e**

```sh
source "$HOME/.cargo/env"
# Build the worker image once (cross-builds the worker + lone-file runtime image):
bash scripts/workers/python-exec/build-image.sh
cargo test -p kastellan-core --test python_exec_warm_idle_e2e -- --nocapture
```

Expected: 3 tests PASS (not `[SKIP]`). If they `[SKIP]`, the image/service is missing — resolve before claiming success (a green `[SKIP]` is a false positive per the project's testing rules).

- [ ] **Step 4: Clippy + commit**

```sh
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --test python_exec_warm_idle_e2e -- -D warnings
git add core/tests/python_exec_warm_idle_e2e.rs
git commit -m "test(python-exec): warm/idle micro-VM e2e (reuse + /tmp wipe + teardown)

Real Apple-container e2e: 3 calls boot the VM once; a /tmp sentinel from call
1 is gone for call 2 on the same warm VM (per-call wipe isolation gate); the
warm slot tears down after the idle window. Skips cleanly without the image.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Operator docs + workspace verification

**Files:**
- Modify: `docs/deploy/` python-exec / container operator note if one exists (grep below); otherwise add a short paragraph to the spec's "operator" surface in the worker module doc (already done in Task 2 Step 5) and the deploy doc.

- [ ] **Step 1: Document the new env knobs where python-exec deployment is described**

```sh
grep -rln "KASTELLAN_PYTHON_EXEC_USE_CONTAINER" docs/ scripts/
```

For each operator-facing doc that mentions `USE_CONTAINER`, add the warm knobs next to it, e.g.:

```markdown
- `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS` — keep the micro-VM warm between calls
  for this many idle seconds (opt-in; unset/`0` = a fresh VM per call). Amortises
  the ~0.7 s boot. Optional caps: `KASTELLAN_PYTHON_EXEC_MAX_REQUESTS`
  (default 10000), `KASTELLAN_PYTHON_EXEC_MAX_AGE_SECONDS` (default 86400).
```

If no such operator doc exists, skip this step (the module doc-comment from Task 2 carries it) and note that in the commit.

- [ ] **Step 2: Full-workspace build + targeted test sweep**

```sh
source "$HOME/.cargo/env"
cargo build --workspace
cargo test -p kastellan-worker-python-exec
cargo test -p kastellan-core --lib workers::python_exec
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: all green. (The container e2e tests `[SKIP]` unless the image is staged — that's expected on CI; run them locally on macOS per Task 3 Step 3.)

- [ ] **Step 3: Commit any doc changes**

```sh
git add docs/   # only the specific doc files you touched
git commit -m "docs(python-exec): document warm/idle container env knobs

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review (completed during planning)

**Spec coverage:**
- Decision 1 (wipe `/tmp`) → Task 1. ✓
- Decision 2 (container-only scope) → Task 2 (lifecycle only set in `container_mode_entry`; host entry untouched). ✓
- Decision 3 (opt-in `IDLE_SECONDS`, default off) → Task 2 (`parse_idle_caps`/`container_lifecycle`, `None` → `SingleUse`). ✓
- Decision 4 (caps mirror GLiNER, overridable; `stateless` contract) → Task 2 (`DEFAULT_MAX_REQUESTS`/`DEFAULT_MAX_AGE_SECONDS`/`IDLE_GRACE_SECONDS`, `Contract { stateless: true }`). ✓
- Component 1 (worker wipe) → Task 1. ✓ Component 2 (entry/resolver) → Task 2. ✓ Component 3 (tests) → Tasks 1–3. ✓
- Security analysis (wipe restores isolation; default off; rotation hygiene) → enforced by Task 1 + Task 3's wipe-across-reuse gate. ✓
- Verification plan → Task 3 (e2e) + Task 4 (workspace clippy/build). ✓

**Placeholder scan:** No TBD/TODO; every code step shows real code. The one conditional ("if no operator doc exists, skip") is a genuine branch with a defined fallback, not a placeholder.

**Type consistency:** `wipe_scratch_contents(&Path) -> io::Result<usize>` used identically in Task 1 def + tests. `parse_idle_caps` returns `(Option<u64>, u64, u64)` and `container_lifecycle(Option<u64>, u64, u64) -> Lifecycle` — consistent across Task 2 def, tests, resolver, and the Task 3 `warm_entry` helper (which builds the lifecycle directly). `container_mode_entry`'s new 4-arg form is updated at every call site (resolver, test helper, container e2e — Task 2 Step 7 greps to confirm). `handle.worker_mut()` matches manager.rs:93.

**Open risk flagged for the implementer:** the `SandboxBackends` struct literal in Task 3 Step 1 assumes macOS fields `{ seatbelt, container }`; Step 2 explicitly greps the real definition and adjusts. The Linux cross-clippy in Task 2 Step 9 may not link (core's `ring` C dep) — that's expected; the real Linux gate is the DGX/CI, and this change introduces no new Linux behaviour (the wipe is a no-op there, the lifecycle is macOS-gated).
