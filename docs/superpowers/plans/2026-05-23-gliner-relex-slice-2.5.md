# GLiNER-Relex Slice 2.5 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the `gliner-relex` worker through the macOS `MacosContainer` SandboxBackend with a real Containerfile + operator build script, closing the macOS memory-enforcement gap and Issue #107 (PID-1 signal handling) in one slice.

**Architecture:** Add `--init` unconditionally to `build_container_argv`; widen `SandboxBackends::resolve()` to take both backend kind + optional container image tag; widen `ToolEntry` with `container_image: Option<String>`; branch `gliner_relex_entry` on a new `GlinerRelexEnv.use_container_backend` field driven by `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`. Ship a Containerfile (`python:3.12-slim` + `uv pip install --system` + `USER nobody` + `ENTRYPOINT`) and an operator helper script. One new happy-path e2e + 11 unit/smoke tests verify end-to-end.

**Tech Stack:** Rust (workspace: kastellan-sandbox + kastellan-core + kastellan-tests-common), Apple `container` 0.12.3, Python 3.12, uv 0.4.30, gliner + transformers + torch.

**Spec:** [`docs/superpowers/specs/2026-05-23-gliner-relex-slice-2.5-design.md`](../specs/2026-05-23-gliner-relex-slice-2.5-design.md) at `845e8f7`.

**Branch:** `feat/gliner-relex-slice-2.5` (already created from `main` at `7c53af3`; spec committed at `845e8f7`).

**Pre-flight check (before Task 1):**
- Branch is `feat/gliner-relex-slice-2.5`. Confirm via `git branch --show-current`.
- Baseline workspace test count on macOS: **998 passed / 0 failed / 3 ignored**. Confirm via `source $HOME/.cargo/env && cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "TOTAL passed="p" failed="f" ignored="i}'`.
- Apple `container` CLI present: `container --version` returns `container CLI version 0.12.3 (...)` or newer.

---

## File Structure

**Files created:**
- `workers/gliner-relex/Containerfile` — image recipe (~20 LOC)
- `scripts/workers/gliner-relex/build-image.sh` — operator helper (~50 LOC)

**Files modified:**
- `sandbox/src/macos_container.rs` — `--init` in `build_container_argv` + 1 new unit test + 1 existing test updated
- `sandbox/src/lib.rs` — `SandboxBackends::resolve()` widened to take `image: Option<&str>` + 2 new unit tests
- `core/src/scheduler/tool_dispatch.rs` — add `container_image: Option<String>` to `ToolEntry`; update `shell_exec_entry` to set it to `None`
- `core/src/worker_lifecycle/manager.rs` — `SingleUseLifecycle` + `IdleTimeoutLifecycle` pass `entry.container_image.as_deref()` to resolver (2 call sites)
- `core/src/workers/gliner_relex.rs` — widen `GlinerRelexEnv` with `use_container_backend: bool` + `container_image: Option<String>`; update `resolve_env` to read new env vars + skip venv check in container mode; split `gliner_relex_entry` into host/container branches with shared `build_idle_timeout_lifecycle` helper; update existing test fixtures
- `core/tests/gliner_relex_e2e.rs` — new `build_test_entry_container` fixture + `happy_path_container_extract_returns_entities_and_triples` test
- `sandbox/tests/macos_container_smoke.rs` — new `macos_container_argv_with_init_runs_alpine_cleanly` smoke test (extends existing file)
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — session-end updates

**Files mechanically updated by ripple changes:**
- `core/src/main.rs::build_gliner_relex_entry` — no code change (passes generic `env_lookup`; new env vars flow through automatically)
- Any test fixture constructing `ToolEntry { ... }` literally — add `container_image: None`
- Any test fixture constructing `GlinerRelexEnv { ... }` literally — add `use_container_backend: false, container_image: None`

---

## Task 1: Add `--init` unconditionally to `build_container_argv` (closes Issue #107)

**Files:**
- Modify: `sandbox/src/macos_container.rs`

### Step 1.1: Write the failing new unit test

Open `sandbox/src/macos_container.rs`. Find the `mod tests { ... }` block (search for `fn argv_starts_with_container_run`). Add this new test immediately after the existing `argv_always_carries_rm_and_interactive_and_progress_none` test (around line 531):

- [ ] **Write the test:**

```rust
/// `--init` must appear in every container run argv: it forwards
/// signals (so the lifecycle manager's outer-process kill reaches the
/// in-VM worker) and reaps zombies (Python's multiprocessing fork). The
/// flag is parallel to LinuxBwrap's unconditional `--as-pid-1` posture.
/// Pinned by issue #107.
#[test]
fn argv_carries_init_for_signal_forwarding_and_zombie_reaping() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(
        argv.contains(&"--init".to_string()),
        "missing --init; got: {argv:?}"
    );
}
```

### Step 1.2: Run the new test to verify it fails

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-sandbox --lib argv_carries_init -- --nocapture
```

Expected: FAIL with `missing --init; got: [...]`.

### Step 1.3: Update the existing always-on test to expect `--init`

The existing `argv_always_carries_rm_and_interactive_and_progress_none` test (around line 517) doesn't currently assert `--init`. Tighten it so the always-on prefix is fully pinned in one place. Replace the existing function body with:

- [ ] **Edit the existing test:**

```rust
/// Always-on flags must appear regardless of policy: `--rm` (auto-remove),
/// `-i` (stdin open for JSON-RPC), `--init` (signal-forwarding + zombie-reap),
/// `--progress none` (suppress noisy stderr progress lines).
#[test]
fn argv_always_carries_rm_and_interactive_and_init_and_progress_none() {
    let argv = build_container_argv(&strict_policy(), DEFAULT_IMAGE, "/bin/true", &[]);
    assert!(argv.contains(&"--rm".to_string()), "missing --rm; got: {argv:?}");
    assert!(argv.contains(&"-i".to_string()), "missing -i; got: {argv:?}");
    assert!(argv.contains(&"--init".to_string()), "missing --init; got: {argv:?}");
    // --progress none must appear as adjacent argv elements (not just both present somewhere).
    let progress_idx = argv
        .iter()
        .position(|s| s == "--progress")
        .expect("missing --progress");
    assert_eq!(
        argv[progress_idx + 1],
        "none",
        "--progress not followed by `none`; got: {argv:?}"
    );
}
```

(Test renamed to include `_init_` so the failure message in grep history is unambiguous.)

### Step 1.4: Implement `--init` in `build_container_argv`

Find the always-on prefix block in `build_container_argv` (around lines 160-167, right after `argv.push("run".into());`). Add the `--init` push between `-i` and `--progress`:

- [ ] **Edit `build_container_argv`:**

```rust
    argv.push("container".into());
    argv.push("run".into());

    argv.push("--rm".into());
    argv.push("-i".into());
    // Always-on signal-forwarding + zombie-reaping init shim.
    // Parallel to LinuxBwrap's unconditional `--as-pid-1` posture. For
    // short-lived smoke containers the overhead is one extra small init
    // process (negligible); for long-lived `IdleTimeoutLifecycle`
    // workers (gliner-relex, future python-exec) this is load-bearing:
    // without it, the in-VM worker inherits PID 1 and ignores SIGTERM
    // by default. Closes issue #107.
    argv.push("--init".into());
    argv.push("--progress".into());
    argv.push("none".into());
```

Also update the docstring on `build_container_argv` (around lines 116-126). Find this block:

```rust
/// Always-on flags:
/// * `--rm` — container auto-removed on exit (mirrors bwrap's stateless
///   per-spawn posture).
/// * `-i` — keep stdin open for JSON-RPC stdio (otherwise `container run`
///   closes stdin and any worker speaking JSON-RPC over stdio hangs).
/// * `--progress none` — suppress the `[6/6] Starting container [0s]`
///   progress lines that `container run` emits on stderr by default.
///   They don't corrupt stdout (the JSON-RPC parser only reads stdout) but
///   they interleave noisily with worker `tracing` output in test
///   captures.
```

Replace with:

```rust
/// Always-on flags:
/// * `--rm` — container auto-removed on exit (mirrors bwrap's stateless
///   per-spawn posture).
/// * `-i` — keep stdin open for JSON-RPC stdio (otherwise `container run`
///   closes stdin and any worker speaking JSON-RPC over stdio hangs).
/// * `--init` — Apple `container`'s init-shim; forwards signals to the
///   worker process and reaps zombies. Parallel to LinuxBwrap's
///   unconditional `--as-pid-1`. Closes issue #107.
/// * `--progress none` — suppress the `[6/6] Starting container [0s]`
///   progress lines that `container run` emits on stderr by default.
///   They don't corrupt stdout (the JSON-RPC parser only reads stdout) but
///   they interleave noisily with worker `tracing` output in test
///   captures.
```

### Step 1.5: Run all `build_container_argv` tests to verify pass

- [ ] **Run:**

```sh
cargo test -p kastellan-sandbox --lib macos_container -- --nocapture
```

Expected: all `macos_container::tests::*` pass (including `argv_carries_init_for_signal_forwarding_and_zombie_reaping` and `argv_always_carries_rm_and_interactive_and_init_and_progress_none`). Test count on the macos_container module goes up by +1 net.

### Step 1.6: Run the macOS container smoke tests to confirm `--init` doesn't break the existing envelope

- [ ] **Run:**

```sh
cargo test -p kastellan-sandbox --test macos_container_smoke -- --nocapture
```

Expected: existing 7 smoke tests pass (skip-as-pass if `container` not running). The smoke tests spawn `alpine:3.20` via `container run` with the full real argv — `--init` going through is the structural verification.

### Step 1.7: Commit

- [ ] **Commit:**

```sh
git add sandbox/src/macos_container.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): add --init unconditionally to container run argv (closes #107)

Apple container's --init forwards signals to the worker process and
reaps zombies. Parallel to LinuxBwrap's unconditional --as-pid-1
posture. Required for long-lived IdleTimeoutLifecycle workers
(gliner-relex, future python-exec): without it the in-VM worker
inherits PID 1 and ignores SIGTERM by default; SIGTERM from the
lifecycle manager would not propagate to the worker on idle teardown
or cap-eval rotation.

+1 new unit test (argv_carries_init_for_signal_forwarding_and_zombie_reaping)
+1 existing test renamed + tightened (argv_always_carries_rm_and_interactive_and_init_and_progress_none)
docstring on build_container_argv updated.

Closes #107.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Widen `SandboxBackends::resolve()` to take an optional image tag

**Files:**
- Modify: `sandbox/src/lib.rs`

### Step 2.1: Write failing tests for the new resolver behaviour

Find the `mod tests { ... }` block in `sandbox/src/lib.rs` (around line 304). Add these two new tests (macOS-gated):

- [ ] **Add tests:**

```rust
#[cfg(target_os = "macos")]
#[test]
fn sandbox_backends_resolve_with_custom_image_returns_fresh_container() {
    // When the operator opts a worker into container mode with a custom
    // image tag (Slice 2.5: gliner-relex flips to kastellan/gliner-relex:dev),
    // resolve(Some(Container), Some("kastellan/gliner-relex:dev")) must
    // return a backend whose image() method reports that tag — NOT the
    // cached default-image backend's tag (DEFAULT_IMAGE = alpine:3.20).
    let backends = SandboxBackends::default_for_current_os();
    let backend = backends.resolve(
        Some(SandboxBackendKind::Container),
        Some("kastellan/gliner-relex:dev"),
    );
    // Downcast via Any is overkill — use the public surface of MacosContainer
    // by constructing one with the same image and checking the resolver
    // returned an Arc that holds the right tag.
    //
    // Since `dyn SandboxBackend` doesn't expose image(), we test via a
    // probe: the per-call MacosContainer::with_image(tag) path returns
    // a fresh Arc that is NOT pointer-equal to the cached default slot.
    let cached_default = backends.resolve(Some(SandboxBackendKind::Container), None);
    assert!(
        !Arc::ptr_eq(&backend, &cached_default),
        "resolve with custom image must return a fresh backend, not the cached default-image slot"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn sandbox_backends_resolve_with_none_image_returns_cached_default() {
    // resolve(Some(Container), None) — the smoke-test / Slice 1 posture —
    // must return the cached default-image slot (Arc-pointer identity).
    // Slice 1's tests rely on this: they don't pass a custom image, and
    // the per-call construction path would be a behaviour change.
    let backends = SandboxBackends::default_for_current_os();
    let first = backends.resolve(Some(SandboxBackendKind::Container), None);
    let second = backends.resolve(Some(SandboxBackendKind::Container), None);
    assert!(
        Arc::ptr_eq(&first, &second),
        "resolve with image=None must return the cached default-image slot (Arc-pointer identity)"
    );
}
```

### Step 2.2: Run the new tests to verify they fail to compile

- [ ] **Run:**

```sh
cargo test -p kastellan-sandbox --lib sandbox_backends_resolve_with -- --nocapture 2>&1 | head -30
```

Expected: compile error — `resolve()` takes 1 arg, not 2. This is the TDD signal.

### Step 2.3: Widen the `resolve` signature

Find the existing `resolve` impl in `sandbox/src/lib.rs` (lines 265-284). Replace the whole `impl SandboxBackends { ... }` block from `pub fn resolve` to the closing brace with:

- [ ] **Edit `resolve`:**

```rust
    /// Resolve a per-worker [`SandboxBackendKind`] (+ optional container
    /// image tag) to a concrete backend.
    ///
    /// * `(None, _)` → per-OS default (linux → `bwrap`, darwin → `seatbelt`).
    /// * `(Some(Container), None)` → cached default-image container backend
    ///   (the Slice 1 / smoke-test posture; `alpine:3.20`).
    /// * `(Some(Container), Some(tag))` → per-call
    ///   `Arc::new(MacosContainer::with_image(tag))`. Cheap (String + Arc);
    ///   `MacosContainer::probe()` was called once at construction against
    ///   the default image, and `probe` is image-independent (it checks
    ///   `container --version` + `container system status`), so no
    ///   re-probe needed here.
    /// * Other `Some(kind)` arms → existing cached slots, `image` ignored.
    ///
    /// The returned `Arc` is held for the lifetime of one acquire call
    /// (single-use lifecycle) or one warm-slot fill (idle-timeout
    /// lifecycle).
    pub fn resolve(
        &self,
        kind: Option<SandboxBackendKind>,
        image: Option<&str>,
    ) -> Arc<dyn SandboxBackend> {
        match (kind, image) {
            (None, _) => {
                #[cfg(target_os = "linux")]
                {
                    Arc::clone(&self.bwrap)
                }
                #[cfg(target_os = "macos")]
                {
                    Arc::clone(&self.seatbelt)
                }
            }
            #[cfg(target_os = "linux")]
            (Some(SandboxBackendKind::Bwrap), _) => Arc::clone(&self.bwrap),
            #[cfg(target_os = "macos")]
            (Some(SandboxBackendKind::Seatbelt), _) => Arc::clone(&self.seatbelt),
            #[cfg(target_os = "macos")]
            (Some(SandboxBackendKind::Container), None) => Arc::clone(&self.container),
            #[cfg(target_os = "macos")]
            (Some(SandboxBackendKind::Container), Some(tag)) => {
                Arc::new(macos_container::MacosContainer::with_image(tag))
            }
        }
    }
```

### Step 2.4: Run the workspace to find every existing `.resolve()` caller that breaks

- [ ] **Run:**

```sh
cargo build --workspace 2>&1 | grep -E "^error\[E" | head -20
```

Expected: ~4 compile errors in:
- `sandbox/src/lib.rs` mod tests (the existing tests use the old signature)
- `core/src/worker_lifecycle/manager.rs:224`
- `core/src/worker_lifecycle/manager.rs:309`
- Any other `.resolve(` callsite

### Step 2.5: Update existing `sandbox` lib tests to use the new signature

Find tests in `sandbox/src/lib.rs` that call `.resolve(`. There are at least four (around lines 375, 386, 394, 402 per the earlier grep). They all currently call `resolve(kind)`. Mechanical update:

- [ ] **Edit each call site:**

For each `.resolve(some_kind)` change to `.resolve(some_kind, None)`. Example:

Before:
```rust
let sbs = SandboxBackends::default_for_current_os();
let _backend = sbs.resolve(None);
```

After:
```rust
let sbs = SandboxBackends::default_for_current_os();
let _backend = sbs.resolve(None, None);
```

Do this for all 4 existing test call sites.

### Step 2.6: Run all sandbox lib tests to verify pass

- [ ] **Run:**

```sh
cargo test -p kastellan-sandbox --lib -- --nocapture
```

Expected: all sandbox lib tests pass, including the two new ones. The lifecycle-manager call-sites in `core` still don't compile — that's Task 3+4.

### Step 2.7: Commit

- [ ] **Commit:**

```sh
git add sandbox/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(sandbox): widen SandboxBackends::resolve to take optional image tag

resolve(kind: Option<SandboxBackendKind>, image: Option<&str>) ->
Arc<dyn SandboxBackend>. When (Some(Container), Some(tag)) is
requested, constructs a fresh Arc::new(MacosContainer::with_image(tag));
all other arms unchanged (return cached per-OS default or cached
default-image container slot). Per-call construct cost is negligible
(String + Arc); MacosContainer::probe() at default_for_current_os()
already validated CLI + system service (image-independent).

+2 new macOS-gated unit tests pin the per-call vs cached behaviour.

Slice 2.5 building block: enables per-worker image-tag plumbing so
gliner-relex can request kastellan/gliner-relex:dev while other workers
keep the default-image slot. core/ lifecycle managers will pick up
this signature in the next commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add `container_image: Option<String>` to `ToolEntry`

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs`

### Step 3.1: Write the failing test on `shell_exec_entry`

Find the `mod tests` block in `core/src/scheduler/tool_dispatch.rs` (search for `#[cfg(test)]` near the bottom of the file). If no test module exists, add one. Add this test:

- [ ] **Add test:**

```rust
#[test]
fn shell_exec_entry_defaults_container_image_to_none() {
    // Pin the default so a future operator-config plumbing pass that
    // adds image-tag inheritance for non-container backends has to
    // update this test deliberately — it must not silently start
    // populating container_image on workers that don't use container.
    let entry = shell_exec_entry(
        PathBuf::from("/usr/bin/true"),
        &["/usr/bin/true".to_string()],
    );
    assert!(
        entry.container_image.is_none(),
        "shell_exec_entry must default container_image to None; got {:?}",
        entry.container_image,
    );
}
```

If the test module doesn't exist, prepend:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ... existing tests if any ...
}
```

### Step 3.2: Run the test to verify it fails

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib shell_exec_entry_defaults_container_image -- --nocapture 2>&1 | head -20
```

Expected: compile error — no field `container_image` on `ToolEntry`.

### Step 3.3: Add the field to `ToolEntry`

Find `pub struct ToolEntry { ... }` in `core/src/scheduler/tool_dispatch.rs` (around line 93). Add the new field after `sandbox_backend` (last field today, around line 122):

- [ ] **Edit `ToolEntry`:**

```rust
    /// Per-worker sandbox-backend opt-in. `None` (current default for
    /// every shipping tool) uses the per-OS default backend (Seatbelt
    /// on darwin, Bwrap on linux). `Some(K)` requests a specific
    /// backend, validated at compile time by the cfg-gated enum.
    ///
    /// Slice 2.5 will set `Some(SandboxBackendKind::Container)` on
    /// the `gliner-relex` manifest to opt that worker into macOS
    /// memory enforcement (Seatbelt has no memory primitive). All
    /// other workers stay on `None` until they have a concrete
    /// reason to diverge. See
    /// `docs/superpowers/specs/2026-05-21-macos-container-slice-2-design.md`.
    pub sandbox_backend: Option<kastellan_sandbox::SandboxBackendKind>,
    /// Container image tag for the `MacosContainer` backend. Only
    /// meaningful when `sandbox_backend == Some(Container)`; ignored
    /// otherwise. Type is `Option<String>` rather than enum-coupled so
    /// future container-based backends on other platforms (e.g. a
    /// hypothetical Linux Firecracker backend) could reuse the same
    /// shape without enum widening.
    ///
    /// * `None` with `sandbox_backend == Some(Container)` →
    ///   `MacosContainer`'s `DEFAULT_IMAGE` (`alpine:3.20`). Useful for
    ///   Slice 1-style smoke tests.
    /// * `Some(tag)` → per-call
    ///   `Arc::new(MacosContainer::with_image(tag))` via
    ///   `SandboxBackends::resolve`. Production workers (gliner-relex,
    ///   future python-exec) populate this with their per-worker image.
    pub container_image: Option<String>,
```

### Step 3.4: Update `shell_exec_entry` to set the new field

Find `pub fn shell_exec_entry` (line 176). Locate the `ToolEntry { ... }` literal at the end (around lines 192-199). Add `container_image: None`:

- [ ] **Edit `shell_exec_entry`:**

```rust
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
```

### Step 3.5: Find and update every other `ToolEntry { ... }` struct literal in the workspace

The new field is required — every literal construction must add `container_image: None` (or `Some(tag)` for container-mode entries, which only Task 6 introduces).

- [ ] **Find call sites:**

```sh
grep -rn "ToolEntry {" core/ workers/ db/ sandbox/ supervisor/ 2>/dev/null
```

Expected hits include:
- `core/src/scheduler/tool_dispatch.rs` (the `shell_exec_entry` we just edited)
- `core/src/workers/gliner_relex.rs::gliner_relex_entry` (existing literal — add `container_image: None`; Task 6 splits this)
- `core/src/workers/gliner_relex.rs` test fixtures (multiple)
- `core/tests/*.rs` integration tests
- `sandbox/` won't have `ToolEntry` (different crate)

For each hit, add `container_image: None,` as the last struct field (or `container_image: None` at the position you prefer; matching the field order on the struct keeps `git blame` cleaner).

**Concrete pattern: where you see `sandbox_backend: None,` add `container_image: None,` on the next line. Where you see `sandbox_backend: Some(...)` (currently no such call sites outside future container-mode code), still add `container_image: None,` — Task 6 will retro-fit the gliner-relex container-mode construct to populate it.**

### Step 3.6: Run the new `shell_exec_entry_defaults_container_image_to_none` test to verify pass

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib shell_exec_entry_defaults_container_image -- --nocapture
```

Expected: PASS.

### Step 3.7: Run the workspace build to verify everything still compiles

- [ ] **Run:**

```sh
cargo build --workspace 2>&1 | grep -E "^error\[E" | head -20
```

Expected: errors in `core/src/worker_lifecycle/manager.rs` (the `.resolve()` callsites still use the old 1-arg signature — Task 4 fixes). All `ToolEntry { ... }` literal construction errors should be gone.

### Step 3.8: Commit

- [ ] **Commit:**

```sh
git add core/src/scheduler/tool_dispatch.rs core/src/workers/gliner_relex.rs core/tests/*.rs
# (Plus any other files where `ToolEntry { ... }` literal needed updating.)
git commit -m "$(cat <<'EOF'
feat(core): add container_image: Option<String> to ToolEntry

Per-worker container-image plumbing for the macOS MacosContainer
backend. `None` everywhere today (byte-equivalent default); Slice 2.5
Task 6 populates Some("kastellan/gliner-relex:dev") on gliner-relex's
container-mode entry.

shell_exec_entry sets it to None explicitly. Every existing ToolEntry
struct-literal construct (gliner-relex host-mode entry + integration
test fixtures) mechanically updated. +1 unit test pins the
shell_exec_entry default.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Wire lifecycle managers to pass `entry.container_image.as_deref()` to resolver

**Files:**
- Modify: `core/src/worker_lifecycle/manager.rs`

### Step 4.1: Run the workspace build to see the current errors

- [ ] **Run:**

```sh
cargo build --workspace 2>&1 | grep -E "manager.rs.*error\[E" | head -10
```

Expected: 2 errors at lines 224 + 309 — `resolve()` takes 2 args, given 1.

### Step 4.2: Update both `.resolve()` call sites

- [ ] **Edit `SingleUseLifecycle::acquire` (around line 224):**

Find:
```rust
        // Resolve per call: `entry.sandbox_backend == None` returns the
        // per-OS default; `Some(K)` returns the matching backend slot.
        // Resolution is an Arc::clone (refcount bump, nanoseconds).
        let backend = self.sandboxes.resolve(entry.sandbox_backend);
```

Replace with:
```rust
        // Resolve per call: `entry.sandbox_backend == None` returns the
        // per-OS default; `Some(K)` returns the matching backend slot.
        // For Container kind, `entry.container_image.as_deref()` picks
        // the per-worker image tag (or `None` → default-image cached slot).
        // Resolution is an Arc::clone (refcount bump, nanoseconds) for
        // the cached paths, or a fresh Arc::new(MacosContainer::with_image)
        // for per-worker images (still cheap — String + Arc).
        let backend = self
            .sandboxes
            .resolve(entry.sandbox_backend, entry.container_image.as_deref());
```

- [ ] **Edit `IdleTimeoutLifecycle::acquire` (around line 309):**

Find:
```rust
        // Resolve per-acquire: cold-fill paths pick up the right backend
        // for the entry. The warm cache below in `acquire_impl` is keyed
        // by tool name, so a warm worker spawned under one backend isn't
        // reused for a different tool with a different backend.
        let backend = self.sandboxes.resolve(entry.sandbox_backend);
```

Replace with:
```rust
        // Resolve per-acquire: cold-fill paths pick up the right backend
        // for the entry. The warm cache below in `acquire_impl` is keyed
        // by tool name, so a warm worker spawned under one backend isn't
        // reused for a different tool with a different backend. The
        // `entry.container_image.as_deref()` arg drives per-worker image
        // selection for the Container kind (see SandboxBackends::resolve
        // docs).
        let backend = self
            .sandboxes
            .resolve(entry.sandbox_backend, entry.container_image.as_deref());
```

### Step 4.3: Build the workspace to verify all compile errors are gone

- [ ] **Run:**

```sh
cargo build --workspace 2>&1 | grep -E "^error\[E"
```

Expected: empty output.

### Step 4.4: Run the lifecycle manager tests to verify the rerouting didn't break anything

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib worker_lifecycle -- --nocapture
```

Expected: all `worker_lifecycle::*` unit tests pass.

### Step 4.5: Run the lifecycle routing integration test (Slice 2's counter-backend pin)

- [ ] **Run:**

```sh
cargo test -p kastellan-core --test lifecycle_container_routing_e2e -- --nocapture
```

Expected: pass (or `[SKIP]` lines on hosts without container CLI). This is the structural pin that the routing-by-`entry.sandbox_backend` still works after the widening.

### Step 4.6: Commit

- [ ] **Commit:**

```sh
git add core/src/worker_lifecycle/manager.rs
git commit -m "$(cat <<'EOF'
feat(core): lifecycle managers pass container_image to resolve()

Both SingleUseLifecycle::acquire and IdleTimeoutLifecycle::acquire
now call self.sandboxes.resolve(entry.sandbox_backend,
entry.container_image.as_deref()). Mechanical edit that unblocks
per-worker container-image selection without altering any cached-path
behaviour (entries with container_image=None still hit the cached
per-OS default slot via the resolver's None arm).

Existing lifecycle routing e2e + manager unit tests stay byte-equivalent
green — they're the regression pin for the rerouting hygiene.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Widen `GlinerRelexEnv` + `resolve_env` for the new env vars

**Files:**
- Modify: `core/src/workers/gliner_relex.rs`

### Step 5.1: Write failing tests for the new env-var behaviour

Find the `mod tests` block in `core/src/workers/gliner_relex.rs` (line 660). Find the existing `resolve_env` tests near the bottom of the test module (search for `fn resolve_env_` or similar — they pass in-memory closures). Add these three new tests right after the last existing resolve_env test:

- [ ] **Add tests:**

```rust
#[test]
fn resolve_env_sets_use_container_backend_when_env_var_is_one() {
    let env_map = std::collections::HashMap::from([
        ("KASTELLAN_GLINER_RELEX_ENABLE", "1"),
        ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
        ("KASTELLAN_GLINER_RELEX_USE_CONTAINER", "1"),
    ]);
    let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
    let is_dir = |_: &Path| true;   // pretend /tmp/fake-weights exists
    let exists = |_: &Path| true;   // pretend any script_path exists
    let env = resolve_env(env_lookup, is_dir, exists).expect("resolve_env ok");
    assert!(
        env.use_container_backend,
        "KASTELLAN_GLINER_RELEX_USE_CONTAINER=1 must set use_container_backend = true"
    );
}

#[test]
fn resolve_env_strict_about_use_container_value() {
    // Only "1" (after trim) counts — symmetric with KASTELLAN_GLINER_RELEX_ENABLE
    // strictness. Surface dialect debate ("true", "yes", "on") would
    // creep in over time without this pin.
    for value in &["true", "yes", "on", "0", " 1 \n"] {
        let env_map = std::collections::HashMap::from([
            ("KASTELLAN_GLINER_RELEX_ENABLE", "1"),
            ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
            ("KASTELLAN_GLINER_RELEX_USE_CONTAINER", *value),
        ]);
        let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
        let is_dir = |_: &Path| true;
        let exists = |_: &Path| true;
        let env = resolve_env(env_lookup, is_dir, exists).expect("resolve_env ok");
        // " 1 \n" → trim() == "1" so it DOES count; others don't.
        let expected = value.trim() == "1";
        assert_eq!(
            env.use_container_backend, expected,
            "value {value:?} should yield use_container_backend = {expected}"
        );
    }
}

#[test]
fn resolve_env_skips_venv_existence_check_in_container_mode() {
    // In container mode the host venv is unused (the worker shim lives
    // inside the image at /usr/local/bin/...). Don't force operators to
    // maintain a host venv when they're running container-mode-only.
    let env_map = std::collections::HashMap::from([
        ("KASTELLAN_GLINER_RELEX_ENABLE", "1"),
        ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
        ("KASTELLAN_GLINER_RELEX_USE_CONTAINER", "1"),
        ("KASTELLAN_DATA_DIR", "/nonexistent/data-dir"),
    ]);
    let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
    let is_dir = |p: &Path| p == Path::new("/tmp/fake-weights");
    let exists = |_: &Path| false;  // host venv shim DOES NOT exist anywhere
    let result = resolve_env(env_lookup, is_dir, exists);
    let env = result.expect("container mode must skip venv check; got ScriptShimMissing");
    assert!(env.use_container_backend);
    assert_eq!(env.script_path, PathBuf::new(), "script_path empty in container mode");
    assert_eq!(env.venv_dir, PathBuf::new(), "venv_dir empty in container mode");
    assert_eq!(env.weights_dir, PathBuf::from("/tmp/fake-weights"));
}

#[test]
fn resolve_env_picks_up_container_image_override() {
    let env_map = std::collections::HashMap::from([
        ("KASTELLAN_GLINER_RELEX_ENABLE", "1"),
        ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR", "/tmp/fake-weights"),
        ("KASTELLAN_GLINER_RELEX_USE_CONTAINER", "1"),
        ("KASTELLAN_GLINER_RELEX_IMAGE", "kastellan/gliner-relex:v0.0.1"),
    ]);
    let env_lookup = |k: &str| env_map.get(k).map(|v| v.to_string());
    let is_dir = |_: &Path| true;
    let exists = |_: &Path| true;
    let env = resolve_env(env_lookup, is_dir, exists).expect("resolve_env ok");
    assert_eq!(
        env.container_image.as_deref(),
        Some("kastellan/gliner-relex:v0.0.1"),
        "KASTELLAN_GLINER_RELEX_IMAGE override must flow into GlinerRelexEnv.container_image"
    );
}
```

### Step 5.2: Run the new tests to verify they fail to compile

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib resolve_env_sets_use_container -- --nocapture 2>&1 | head -20
```

Expected: compile error — no field `use_container_backend` or `container_image` on `GlinerRelexEnv`.

### Step 5.3: Widen `GlinerRelexEnv`

Find `pub struct GlinerRelexEnv { ... }` (line 52). Add two new fields at the end:

- [ ] **Edit struct:**

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlinerRelexEnv {
    pub script_path: PathBuf,
    pub venv_dir: PathBuf,
    pub weights_dir: PathBuf,
    pub model_id: String,
    pub device: String,
    /// True when the operator set `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`
    /// (strict: only `"1"` after trim counts). `gliner_relex_entry`
    /// branches on this field to emit the container-mode `ToolEntry`
    /// shape (in-container binary, weights-only `fs_read`,
    /// `sandbox_backend = Some(Container)`, `container_image` populated)
    /// instead of the host-mode one.
    ///
    /// In container mode `resolve_env` also skips the host-venv
    /// existence check — the worker shim lives inside the image at
    /// `/usr/local/bin/kastellan-worker-gliner-relex`, so requiring a
    /// host venv would be a footgun for container-mode-only operators.
    pub use_container_backend: bool,
    /// Operator-supplied container image tag override, read from
    /// `KASTELLAN_GLINER_RELEX_IMAGE`. `None` (default) falls back to
    /// the `CONTAINER_IMAGE_DEFAULT` constant at the
    /// `gliner_relex_entry` callsite. Symmetric to
    /// `KASTELLAN_GLINER_RELEX_MODEL` override behaviour.
    pub container_image: Option<String>,
}
```

### Step 5.4: Update `resolve_env` to read the new env vars + skip venv check in container mode

Find `pub fn resolve_env` (line 294). Find the venv-resolution block (around lines 326-343) — it's the cascade of `if let Some(v) = env_lookup("KASTELLAN_GLINER_RELEX_VENV_DIR")`. Wrap the venv resolution in an `if !use_container_backend` branch.

Replace the entire `resolve_env` body from the `let enable = ...` line through the final `Ok(GlinerRelexEnv { ... })`:

- [ ] **Edit `resolve_env`:**

```rust
pub fn resolve_env<EnvLookup, IsDir, Exists>(
    env_lookup: EnvLookup,
    is_dir: IsDir,
    exists: Exists,
) -> Result<GlinerRelexEnv, ResolveSkipReason>
where
    EnvLookup: Fn(&str) -> Option<String>,
    IsDir: Fn(&Path) -> bool,
    Exists: Fn(&Path) -> bool,
{
    let enable = env_lookup("KASTELLAN_GLINER_RELEX_ENABLE").unwrap_or_default();
    if enable.trim() != "1" {
        return Err(ResolveSkipReason::Disabled);
    }

    let weights_dir = match env_lookup("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR") {
        Some(v) => PathBuf::from(v),
        None => return Err(ResolveSkipReason::WeightsDirEnvMissing),
    };
    if !is_dir(&weights_dir) {
        return Err(ResolveSkipReason::WeightsDirNotADir { path: weights_dir });
    }

    let model_id = env_lookup("KASTELLAN_GLINER_RELEX_MODEL")
        .unwrap_or_else(|| "knowledgator/gliner-relex-multi-v1.0".to_string());
    let device = env_lookup("KASTELLAN_GLINER_RELEX_DEVICE")
        .unwrap_or_else(|| "auto".to_string());

    // New env knobs (Slice 2.5):
    //   * `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1` → container-mode (strict on "1").
    //   * `KASTELLAN_GLINER_RELEX_IMAGE=<tag>` → operator-supplied image override.
    let use_container_backend = env_lookup("KASTELLAN_GLINER_RELEX_USE_CONTAINER")
        .map(|v| v.trim() == "1")
        .unwrap_or(false);
    let container_image = env_lookup("KASTELLAN_GLINER_RELEX_IMAGE");

    // Host venv resolution is skipped in container mode — the worker
    // shim lives inside the image, so no host venv is required.
    let (venv_dir, script_path) = if use_container_backend {
        (PathBuf::new(), PathBuf::new())
    } else {
        let venv_dir = if let Some(v) = env_lookup("KASTELLAN_GLINER_RELEX_VENV_DIR") {
            PathBuf::from(v)
        } else if let Some(data_dir) = env_lookup("KASTELLAN_DATA_DIR") {
            PathBuf::from(data_dir).join("workers/gliner-relex/.venv")
        } else if let Some(home) = env_lookup("HOME") {
            PathBuf::from(home)
                .join(".local/share/kastellan/workers/gliner-relex/.venv")
        } else {
            return Err(ResolveSkipReason::VenvDirUnresolvable);
        };
        let script_path = venv_dir.join("bin").join("kastellan-worker-gliner-relex");
        if !exists(&script_path) {
            return Err(ResolveSkipReason::ScriptShimMissing { path: script_path });
        }
        (venv_dir, script_path)
    };

    Ok(GlinerRelexEnv {
        script_path,
        venv_dir,
        weights_dir,
        model_id,
        device,
        use_container_backend,
        container_image,
    })
}
```

### Step 5.5: Update the existing test fixture `test_env()` to include the new fields

Find `fn test_env()` in the same `mod tests` block (around line 770). Add the two new fields with their host-mode defaults so existing tests stay byte-equivalent:

- [ ] **Edit `test_env`:**

```rust
fn test_env() -> GlinerRelexEnv {
    GlinerRelexEnv {
        script_path: PathBuf::from("/tmp/fake/.venv/bin/kastellan-worker-gliner-relex"),
        venv_dir: PathBuf::from("/tmp/fake/.venv"),
        weights_dir: PathBuf::from("/tmp/fake/weights/multi-v1.0"),
        model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
        device: "auto".to_string(),
        use_container_backend: false,
        container_image: None,
    }
}
```

### Step 5.6: Run the gliner_relex unit tests to verify all 11 tests pass (existing 7 + 4 new)

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib workers::gliner_relex -- --nocapture
```

Expected: all existing tests pass (byte-equivalent — the new fields default to host-mode defaults via `test_env`); the 4 new `resolve_env_*` tests pass.

### Step 5.7: Commit

- [ ] **Commit:**

```sh
git add core/src/workers/gliner_relex.rs
git commit -m "$(cat <<'EOF'
feat(workers): GlinerRelexEnv + resolve_env support container-mode opt-in

New env vars:
  KASTELLAN_GLINER_RELEX_USE_CONTAINER=1 — opt into container mode
      (strict: only "1" after trim counts; symmetric with the
      KASTELLAN_GLINER_RELEX_ENABLE convention).
  KASTELLAN_GLINER_RELEX_IMAGE=<tag>     — operator-supplied image
      override; flows into GlinerRelexEnv.container_image.

In container mode resolve_env() skips the host-venv existence check
entirely — the worker shim lives inside the image at
/usr/local/bin/kastellan-worker-gliner-relex; requiring an unused host
venv would be a footgun for container-mode-only operators.

New fields on GlinerRelexEnv:
  use_container_backend: bool
  container_image: Option<String>

test_env() fixture updated with host-mode defaults so existing 7
manifest tests stay byte-equivalent. +4 new tests pin the env-var
strictness + container-mode venv skip + image-override flow-through.

Task 6 (next commit) consumes use_container_backend in
gliner_relex_entry to actually branch the manifest shape.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Branch `gliner_relex_entry` into host-mode and container-mode

**Files:**
- Modify: `core/src/workers/gliner_relex.rs`

### Step 6.1: Write failing tests for the container-mode manifest shape

In the `mod tests` block of `core/src/workers/gliner_relex.rs`, add three new tests immediately after the existing `entry_*` tests:

- [ ] **Add tests:**

```rust
/// Pin the host-mode shape stays byte-equivalent to today:
/// container_image must be None on a host-mode entry (the existing 7
/// `entry_*` tests are the regression pin for everything else;
/// this one adds the new-field default to the suite).
#[test]
fn entry_host_mode_container_image_is_none() {
    let env = test_env();
    assert!(!env.use_container_backend, "test_env defaults to host mode");
    let entry = gliner_relex_entry(&env);
    assert!(
        entry.container_image.is_none(),
        "host-mode entry must have container_image == None; got {:?}",
        entry.container_image
    );
    assert!(
        entry.sandbox_backend.is_none(),
        "host-mode entry must have sandbox_backend == None; got {:?}",
        entry.sandbox_backend
    );
}

/// Container-mode entry emits the in-container binary path, mounts
/// only `weights_dir` (venv + src baked into image), and populates
/// sandbox_backend + container_image.
#[test]
fn entry_container_mode_emits_in_container_binary_and_weights_only_fs_read() {
    let env = GlinerRelexEnv {
        use_container_backend: true,
        ..test_env()
    };
    let entry = gliner_relex_entry(&env);

    assert_eq!(
        entry.binary,
        PathBuf::from("/usr/local/bin/kastellan-worker-gliner-relex"),
        "container-mode binary must be the in-container shim path"
    );
    assert_eq!(
        entry.policy.fs_read,
        vec![env.weights_dir.clone()],
        "container-mode fs_read must contain ONLY weights_dir (venv + src baked into image)"
    );
    assert_eq!(
        entry.sandbox_backend,
        Some(kastellan_sandbox::SandboxBackendKind::Container),
    );
    assert_eq!(
        entry.container_image.as_deref(),
        Some("kastellan/gliner-relex:dev"),
        "container_image defaults to CONTAINER_IMAGE_DEFAULT when env override absent"
    );
}

/// Operator-supplied image tag (KASTELLAN_GLINER_RELEX_IMAGE) flows
/// through GlinerRelexEnv.container_image into the entry.
#[test]
fn entry_container_mode_honours_custom_image_tag() {
    let env = GlinerRelexEnv {
        use_container_backend: true,
        container_image: Some("kastellan/gliner-relex:v0.0.1".to_string()),
        ..test_env()
    };
    let entry = gliner_relex_entry(&env);
    assert_eq!(
        entry.container_image.as_deref(),
        Some("kastellan/gliner-relex:v0.0.1"),
        "operator-supplied image tag must flow into entry.container_image"
    );
}
```

### Step 6.2: Run the new tests to verify they fail

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib entry_container_mode -- --nocapture 2>&1 | head -30
```

Expected: FAIL — `entry_container_mode_emits_in_container_binary` expects `binary == "/usr/local/bin/..."` but the unchanged `gliner_relex_entry` returns the host script_path. `entry_container_mode_honours_custom_image_tag` expects `container_image == Some(...)` but the entry has `container_image: None` (Task 3 default).

### Step 6.3: Split `gliner_relex_entry` body

Find `pub fn gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry { ... }` (line 132 — body runs to line 232). Replace the entire function (everything from `pub fn gliner_relex_entry` through the closing `}` at line 232) with this 3-function split:

- [ ] **Edit `gliner_relex_entry`:**

```rust
/// Default image tag for the gliner-relex container backend. Operator
/// can override via `KASTELLAN_GLINER_RELEX_IMAGE` env var (read by
/// `resolve_env`). Bumping this default is a paired edit with
/// `scripts/workers/gliner-relex/build-image.sh`.
const CONTAINER_IMAGE_DEFAULT: &str = "kastellan/gliner-relex:dev";

/// In-container path to the worker shim. Containerfile uses
/// `uv pip install --system .` which places the console-script from
/// pyproject's `[project.scripts]` at `/usr/local/bin/<name>`. Bumping
/// is a paired edit with the Containerfile's package install path.
const CONTAINER_BINARY: &str = "/usr/local/bin/kastellan-worker-gliner-relex";

/// Construct the [`ToolEntry`] for the gliner-relex worker.
///
/// Branches on `env.use_container_backend`:
///
/// * `false` → host-mode entry (the existing default): worker spawns
///   from the host venv shim, FS allowlist includes weights + venv +
///   editable-install src dir, runs under Seatbelt on darwin / bwrap
///   on Linux. Byte-equivalent to the pre-Slice-2.5 shape.
///
/// * `true` → container-mode entry (macOS-only opt-in via
///   `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`): worker spawns inside
///   the `kastellan/gliner-relex:dev` image (or operator override) via
///   `MacosContainer`, FS allowlist holds only `weights_dir` (venv +
///   src baked into the image), `sandbox_backend = Some(Container)`,
///   `container_image = Some(<image>)`.
///
/// Lifecycle stays identical between modes via the shared
/// `build_idle_timeout_lifecycle()` helper.
pub fn gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry {
    if env.use_container_backend {
        container_mode_entry(env)
    } else {
        host_mode_entry(env)
    }
}

/// Host-mode entry: the existing pre-Slice-2.5 shape. Worker runs from
/// the host venv shim; FS allowlist holds weights + venv + editable
/// src dir; per-OS default sandbox backend (Seatbelt darwin / bwrap
/// linux).
fn host_mode_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // The venv uses an editable install (uv's default for hatchling
    // workspace projects); `.venv/.../_editable_impl_*.pth` points at
    // `<worker_dir>/src`. Mounting only `.venv` would let Python start
    // but fail on `from kastellan_worker_gliner_relex.__main__ import
    // main` with ModuleNotFoundError. Compute the sibling `src/` from
    // the documented `<worker_dir>/.venv` contract on `venv_dir` and
    // bind it read-only too.
    let worker_src_dir = env
        .venv_dir
        .parent()
        .expect("GlinerRelexEnv.venv_dir must have a parent (got a root/relative path)")
        .join("src");

    let policy = SandboxPolicy {
        fs_read: vec![
            env.weights_dir.clone(),
            env.venv_dir.clone(),
            worker_src_dir,
        ],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
        env: build_runtime_env(env),
    };

    ToolEntry {
        binary: env.script_path.clone(),
        policy,
        wall_clock_ms: None,
        lifecycle: build_idle_timeout_lifecycle(),
        sandbox_backend: None,
        container_image: None,
    }
}

/// Container-mode entry: routes the worker through the macOS
/// `MacosContainer` SandboxBackend (Slice 2.5+; opt-in via
/// `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`). Only `weights_dir` mounts
/// from the host; venv + src are baked into the image. The image is
/// per-call constructed via `SandboxBackends::resolve(Some(Container),
/// Some(<image>))`.
fn container_mode_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // Container-mode policy: fs_read mounts host weights only.
    // build_container_argv uses source=<P>,target=<P> convention, so the
    // weights mount at the SAME host path inside the container — that
    // makes the existing KASTELLAN_GLINER_RELEX_WEIGHTS_DIR env value
    // work verbatim without a path rewrite.
    let policy = SandboxPolicy {
        fs_read: vec![env.weights_dir.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
        env: build_runtime_env(env),
    };

    let image = env
        .container_image
        .clone()
        .unwrap_or_else(|| CONTAINER_IMAGE_DEFAULT.to_string());

    ToolEntry {
        binary: PathBuf::from(CONTAINER_BINARY),
        policy,
        wall_clock_ms: None,
        lifecycle: build_idle_timeout_lifecycle(),
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::Container),
        container_image: Some(image),
    }
}

/// Shared env-var list for both host-mode and container-mode entries.
/// Single source of truth so a future PyTorch hygiene addition lands
/// in both branches automatically.
fn build_runtime_env(env: &GlinerRelexEnv) -> Vec<(String, String)> {
    vec![
        (
            "KASTELLAN_GLINER_RELEX_WEIGHTS_DIR".to_string(),
            env.weights_dir.to_string_lossy().into_owned(),
        ),
        (
            "KASTELLAN_GLINER_RELEX_MODEL".to_string(),
            env.model_id.clone(),
        ),
        (
            "KASTELLAN_GLINER_RELEX_DEVICE".to_string(),
            env.device.clone(),
        ),
        ("HF_HUB_OFFLINE".to_string(), "1".to_string()),
        ("TRANSFORMERS_OFFLINE".to_string(), "1".to_string()),
        // PyTorch's _dynamo (transitively imported by transformers)
        // calls getpass.getuser() at module-import time, which falls
        // back to pwd.getpwuid(os.getuid()) when no
        // LOGNAME/USER/LNAME/USERNAME is set. The sandbox has no
        // /etc/passwd, so that fallback raises KeyError and the worker
        // exits before serving any RPC. Setting USER skips the pwd
        // lookup entirely.
        ("USER".to_string(), "kastellan".to_string()),
        // TORCHINDUCTOR_CACHE_DIR pre-empts the home-dir cache
        // computation that triggers the getpass.getuser path above
        // (defense in depth — USER alone is sufficient today, but a
        // future torch refactor could re-route through getuid()).
        (
            "TORCHINDUCTOR_CACHE_DIR".to_string(),
            "/tmp/torchinductor".to_string(),
        ),
    ]
}

/// Shared lifecycle constructor: 10-minute idle window, 10 000 request
/// cap, daily age-out, 5 s grace. Identical between host-mode and
/// container-mode entries. Extracted from the inline body so both
/// branches use one source of truth; the existing
/// `entry_carries_idle_timeout_lifecycle_with_spec_caps` test pins the
/// caps.
fn build_idle_timeout_lifecycle() -> Lifecycle {
    Lifecycle::idle_timeout(
        IdleTimeoutCaps {
            idle_seconds: 600,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        },
        Contract { stateless: true },
    )
    .expect("manifest declares stateless = true; validator must accept")
}
```

### Step 6.4: Run the existing `entry_*` tests to verify host mode stays byte-equivalent

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib workers::gliner_relex::tests::entry_ -- --nocapture
```

Expected: all 10 `entry_*` tests pass (7 existing + 3 new container-mode tests).

### Step 6.5: Run the full gliner_relex module test suite

- [ ] **Run:**

```sh
cargo test -p kastellan-core --lib workers::gliner_relex -- --nocapture
```

Expected: all unit tests pass (including the 4 resolve_env tests from Task 5 + the 3 new entry tests + 7 existing entry tests + any other tests).

### Step 6.6: Commit

- [ ] **Commit:**

```sh
git add core/src/workers/gliner_relex.rs
git commit -m "$(cat <<'EOF'
feat(workers): gliner_relex_entry branches host/container by use_container_backend

gliner_relex_entry now dispatches on env.use_container_backend:

* false (default) -> host_mode_entry: today's shape, byte-equivalent
  to pre-Slice-2.5 (binary = host script_path, fs_read = [weights,
  venv, src], sandbox_backend = None, container_image = None).

* true -> container_mode_entry: binary = /usr/local/bin/<shim>
  (in-image path), fs_read = [weights_dir] only (venv + src baked
  into image), sandbox_backend = Some(Container), container_image =
  Some(CONTAINER_IMAGE_DEFAULT or operator-supplied override).

Shared helpers extracted:
  build_runtime_env(env)         — single source of truth for the
      env-var list across both modes; future PyTorch hygiene additions
      land once.
  build_idle_timeout_lifecycle() — single source of truth for the
      10-min idle / 10k req / daily / 5s-grace caps; existing
      entry_carries_idle_timeout_lifecycle_with_spec_caps test is the
      regression pin.

Constants pinned at module top:
  CONTAINER_IMAGE_DEFAULT = "kastellan/gliner-relex:dev"
  CONTAINER_BINARY        = "/usr/local/bin/kastellan-worker-gliner-relex"

+3 new entry tests pin the container-mode shape (default-image,
custom-image, container_image=None in host mode).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Write the Containerfile + build helper script

**Files:**
- Create: `workers/gliner-relex/Containerfile`
- Create: `scripts/workers/gliner-relex/build-image.sh`

### Step 7.1: Write the Containerfile

- [ ] **Create file:** `workers/gliner-relex/Containerfile`

```dockerfile
# workers/gliner-relex/Containerfile
#
# Build image for the gliner-relex worker, consumed by the macOS
# `MacosContainer` SandboxBackend (Slice 2.5+). Operator-built via
# scripts/workers/gliner-relex/build-image.sh; not used on Linux
# (LinuxBwrap stays the per-OS default there).
#
# Image layout decisions worth knowing:
#   1. Debian-slim base — PyTorch wheels are glibc-only (manylinux2014);
#      Alpine is OUT (musl libc).
#   2. `uv pip install --system` — no .venv indirection; console script
#      lands at /usr/local/bin/kastellan-worker-gliner-relex via
#      pyproject's [project.scripts] entry.
#   3. Weights NOT baked in — operator mounts them at runtime via the
#      policy.fs_read host path (build_container_argv uses
#      source=<P>,target=<P> convention, so the existing
#      KASTELLAN_GLINER_RELEX_WEIGHTS_DIR env value works verbatim).
#      Image stays ~3 GB instead of ~4.5 GB; weight refreshes don't
#      require image rebuild.
#   4. ENTRYPOINT carries the worker shim. The MacosContainer backend
#      appends the binary path verbatim, so the manifest's `binary`
#      field doubles as the in-container `program` argument.
#   5. uv version pin — paired edit with scripts/workers/gliner-relex/
#      install.sh's uv-version assumption. Bumping requires updating
#      both.

FROM python:3.12-slim

# uv is pinned for reproducible image builds.
RUN pip install --no-cache-dir uv==0.4.30

WORKDIR /build

# Build context is workers/gliner-relex/; pyproject + lockfile + src +
# README are everything we need.
COPY pyproject.toml uv.lock README.md ./
COPY src/ ./src/

# --system installs into /usr/lib/python3.12/site-packages and creates
# /usr/local/bin/<console-scripts>. --no-cache keeps the image small;
# --no-dev skips the [dev] optional deps (pytest, pytest-mock) which
# tests don't need (tests run against the host venv, not the container).
RUN uv pip install --system --no-cache --no-dev .

# Defense-in-depth complement to the policy-driven `--user nobody` flag.
# If a future profile widening drops --user from build_container_argv,
# the image-baked USER still ensures non-root execution.
# python:3.12-slim ships with nobody (uid 65534).
USER nobody

ENTRYPOINT ["kastellan-worker-gliner-relex"]
```

### Step 7.2: Write the build helper script

- [ ] **Create file:** `scripts/workers/gliner-relex/build-image.sh`

```bash
#!/usr/bin/env bash
#
# Build the gliner-relex container image consumed by the macOS
# MacosContainer SandboxBackend. Companion to install.sh — install.sh
# builds the host venv (native Seatbelt/bwrap mode); this builds the
# container image (macOS container mode).
#
# Tag default: kastellan/gliner-relex:dev (overridable via
# KASTELLAN_GLINER_RELEX_IMAGE env, matching the daemon-side knob).
#
# Usage:
#     scripts/workers/gliner-relex/build-image.sh
#     KASTELLAN_GLINER_RELEX_IMAGE=kastellan/gliner-relex:v0.0.1 \
#         scripts/workers/gliner-relex/build-image.sh
#
# Exits non-zero with a clear message if `container` CLI is missing or
# the `container` system service is not running. The image build itself
# takes ~3-5 minutes on a fresh M3 Max (PyTorch + transformers + gliner
# wheels are ~3 GB).

set -euo pipefail

IMAGE_TAG="${KASTELLAN_GLINER_RELEX_IMAGE:-kastellan/gliner-relex:dev}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKER_DIR="$(cd "$SCRIPT_DIR/../../../workers/gliner-relex" && pwd)"

if [[ ! -f "$WORKER_DIR/Containerfile" ]]; then
    echo "error: Containerfile not found at $WORKER_DIR/Containerfile" >&2
    exit 2
fi

if ! command -v container >/dev/null 2>&1; then
    echo "error: 'container' CLI not on PATH" >&2
    echo "  install via: brew install container" >&2
    echo "  then: container system start --enable-kernel-install" >&2
    exit 2
fi

if ! container system status >/dev/null 2>&1; then
    echo "error: 'container' system service is not running" >&2
    echo "  start via: container system start" >&2
    exit 2
fi

echo "Building $IMAGE_TAG from $WORKER_DIR"
container build -t "$IMAGE_TAG" "$WORKER_DIR"

cat <<EOF

Done. To enable container-mode in the daemon, set both:
    export KASTELLAN_GLINER_RELEX_ENABLE=1
    export KASTELLAN_GLINER_RELEX_USE_CONTAINER=1

If you used a non-default image tag, also set:
    export KASTELLAN_GLINER_RELEX_IMAGE=$IMAGE_TAG
EOF
```

### Step 7.3: Mark the script executable

- [ ] **Run:**

```sh
chmod +x scripts/workers/gliner-relex/build-image.sh
```

### Step 7.4: Verify the script's missing-CLI path is sane (without actually building yet)

- [ ] **Run** (in a context that fakes container missing — quick path check):

```sh
ls -l scripts/workers/gliner-relex/build-image.sh
bash -n scripts/workers/gliner-relex/build-image.sh  # syntax check
```

Expected: file is `-rwxr-xr-x`, syntax check passes silently.

### Step 7.5: Commit

- [ ] **Commit:**

```sh
git add workers/gliner-relex/Containerfile scripts/workers/gliner-relex/build-image.sh
git commit -m "$(cat <<'EOF'
feat(workers/gliner-relex): Containerfile + build-image.sh operator helper

Containerfile shape:
* python:3.12-slim base (PyTorch glibc wheels need it; Alpine is OUT)
* uv pip install --system .  (no .venv indirection; console-script
  lands at /usr/local/bin/kastellan-worker-gliner-relex)
* USER nobody                (defense-in-depth complement to
  build_container_argv's --user nobody flag)
* ENTRYPOINT ["kastellan-worker-gliner-relex"]
* Weights NOT baked — operator mounts at runtime via policy.fs_read
  (same host path inside container; KASTELLAN_GLINER_RELEX_WEIGHTS_DIR
  env works verbatim).

build-image.sh:
* Mirrors install.sh's shell-hygiene shape (set -euo pipefail, exit 2
  on operator-correctable misconfig with clear stderr messages).
* Probes both `container --version` AND `container system status` so
  the operator gets a clear diagnosis instead of a cryptic
  `container build` failure.
* Final message reminds operator to set both
  KASTELLAN_GLINER_RELEX_ENABLE=1 and KASTELLAN_GLINER_RELEX_USE_CONTAINER=1.
* KASTELLAN_GLINER_RELEX_IMAGE env overrides the default tag.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Add the container `--init` smoke test

**Files:**
- Modify: `sandbox/tests/macos_container_smoke.rs`

### Step 8.1: Read the current smoke test layout to find the skip helper convention

- [ ] **Run:**

```sh
grep -n "skip_if\|fn .*smoke\|#\[test\]\|#\[cfg" sandbox/tests/macos_container_smoke.rs | head -30
```

Note the existing skip helper names (e.g. `skip_if_container_unavailable`) and the `#[cfg]` gates used.

### Step 8.2: Write the new smoke test

Append to the end of `sandbox/tests/macos_container_smoke.rs`:

- [ ] **Add test:**

```rust
/// Slice 2.5 (Issue #107 follow-up): `--init` is always-on in
/// build_container_argv. This smoke verifies that the added flag
/// doesn't break Apple `container`'s short-lived run envelope. If
/// `--init` is rejected by an older `container` build, this test
/// fails loudly instead of letting the broken argv ship.
#[test]
fn macos_container_argv_with_init_runs_alpine_cleanly() {
    if MacosContainer::probe().is_err() {
        eprintln!("\n[SKIP] container CLI / system service not available\n");
        return;
    }
    let backend = MacosContainer::new();  // default image = alpine:3.20
    let policy = SandboxPolicy {
        // Minimal policy: just enough so --init has something to wrap
        // around and the spawn returns quickly.
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 256,  // above container's 200 MiB floor; no clamp warn
        profile: Profile::WorkerStrict,
        cpu_quota_pct: None,
        tasks_max: None,
        env: vec![],
    };
    let mut child = match backend.spawn_under_policy(
        &policy,
        "/bin/sh",
        &["-c", "echo init-ok"],
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\n[SKIP] alpine:3.20 image likely missing: {e}\n");
            return;
        }
    };
    let status = child.wait().expect("wait on container run");
    assert!(
        status.success(),
        "container run --init alpine /bin/sh -c 'echo init-ok' must exit 0; got {status:?}"
    );
}
```

If the imports at the top of `macos_container_smoke.rs` don't already cover `MacosContainer`, `SandboxPolicy`, `Profile`, `Net`, add them:

```rust
use kastellan_sandbox::{MacosContainer, Net, Profile, SandboxBackend, SandboxPolicy};
```

(Check existing imports first — keep the `use` block clean.)

### Step 8.3: Run the new smoke test

- [ ] **Run:**

```sh
cargo test -p kastellan-sandbox --test macos_container_smoke macos_container_argv_with_init -- --nocapture
```

Expected: PASS on this Mac (container CLI is running per pre-flight check + alpine:3.20 is in the cache from Slice 1's earlier work). If alpine:3.20 isn't cached, the test will exit-skip cleanly.

Confirm alpine:3.20 is present (or pull it):
```sh
container image list 2>&1 | grep alpine:3.20 || container image pull alpine:3.20
```

### Step 8.4: Run all macos_container smoke tests to make sure nothing else broke

- [ ] **Run:**

```sh
cargo test -p kastellan-sandbox --test macos_container_smoke -- --nocapture
```

Expected: all smoke tests pass.

### Step 8.5: Commit

- [ ] **Commit:**

```sh
git add sandbox/tests/macos_container_smoke.rs
git commit -m "$(cat <<'EOF'
test(sandbox): smoke-test --init in real `container run` against alpine

macos_container_argv_with_init_runs_alpine_cleanly spawns alpine:3.20
under MacosContainer with the always-on --init flag added in Task 1.
Verifies that the flag doesn't break the existing real-container
spawn envelope (older container CLIs might reject unknown flags;
this fails loudly if so instead of letting the broken argv ship).

Skip-as-pass when container CLI / system service / alpine:3.20 image
are missing.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Operator build of the gliner-relex container image (MANUAL)

This is the unblocking step for Task 10's e2e. It is a one-time operator action; no commit.

### Step 9.1: Build the image

- [ ] **Run:**

```sh
scripts/workers/gliner-relex/build-image.sh
```

Expected: clean run, no errors, final message printing "Done. To enable container-mode in the daemon, ..." and the right `export` commands.

If the script fails:
- "container CLI not on PATH" → `brew install container && container system start --enable-kernel-install`
- "container system service is not running" → `container system start`
- Network errors during `RUN pip install` → check connectivity; retry.
- `uv pip install --no-cache --no-dev .` hang → may be cold pytorch wheel download (~2-3 GB); allow ~5 minutes.

### Step 9.2: Verify the image is present

- [ ] **Run:**

```sh
container image list | grep kastellan/gliner-relex
```

Expected: a line like `kastellan/gliner-relex   dev    arm64   <digest>   ...   ~3.0 GB ...`.

### Step 9.3: Manually sanity-check the in-container worker shim path

- [ ] **Run:**

```sh
container run --rm kastellan/gliner-relex:dev which kastellan-worker-gliner-relex
```

Expected: `/usr/local/bin/kastellan-worker-gliner-relex` printed on stdout. Confirms the Containerfile installed the shim where the manifest expects it.

---

## Task 10: Add the gliner-relex container-mode e2e test

**Files:**
- Modify: `core/tests/gliner_relex_e2e.rs`

### Step 10.1: Read the existing test fixture pattern

The file has helpers `resolve_worker_script`, `resolve_weights_dir`, `build_test_entry`. The new fixture follows the same shape, gated on container preconditions instead of host venv.

### Step 10.2: Add the new helper functions + fixture

Insert the following near the existing skip helpers (after `resolve_weights_dir`, before `build_test_entry`):

- [ ] **Add helpers:**

```rust
/// Slice 2.5: gate container-mode e2e on the operator having built the
/// image. Mirrors the venv-staged `resolve_worker_script` skip pattern.
#[cfg(target_os = "macos")]
fn skip_if_container_unavailable() -> bool {
    if kastellan_sandbox::MacosContainer::probe().is_err() {
        eprintln!(
            "\n[SKIP] container CLI / system service not available — install via 'brew install container' and 'container system start'\n"
        );
        return true;
    }
    false
}

/// Probe whether `image_tag` is present in the local container image
/// store. Skip-as-pass when it's not — operator has to run
/// `scripts/workers/gliner-relex/build-image.sh` first.
#[cfg(target_os = "macos")]
fn skip_if_image_missing(image_tag: &str) -> bool {
    use std::process::Command;
    let output = Command::new("container")
        .args(["image", "list"])
        .output();
    let Ok(out) = output else {
        eprintln!("\n[SKIP] failed to spawn 'container image list'\n");
        return true;
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Image-list format prints "REPOSITORY  TAG  ..."; checking for the
    // "repo  tag" substring is robust to whitespace variations.
    let (repo, tag) = image_tag.split_once(':').unwrap_or((image_tag, "latest"));
    let needle_compact = format!("{repo}");
    let has_repo = stdout.contains(&needle_compact);
    let has_tag = stdout.contains(tag);
    if !(has_repo && has_tag) {
        eprintln!(
            "\n[SKIP] {image_tag} image not present — run scripts/workers/gliner-relex/build-image.sh\n"
        );
        return true;
    }
    false
}

/// Build the gliner-relex container-mode ToolEntry against the operator-
/// built image. Returns `None` if any precondition (sandbox / supervisor /
/// container CLI / image / weights) is missing — every caller converts
/// that into a `[SKIP]` early return.
#[cfg(target_os = "macos")]
fn build_test_entry_container() -> Option<ToolEntry> {
    if skip_if_sandbox_unavailable() {
        return None;
    }
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_container_unavailable() {
        return None;
    }
    if skip_if_image_missing("kastellan/gliner-relex:dev") {
        return None;
    }
    let weights = resolve_weights_dir()?;
    let env = GlinerRelexEnv {
        // Both paths empty in container mode — the worker shim lives
        // at /usr/local/bin inside the image; the manifest builder
        // ignores script_path + venv_dir on the container branch.
        script_path: PathBuf::new(),
        venv_dir: PathBuf::new(),
        weights_dir: weights,
        model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
        device: "auto".to_string(),
        use_container_backend: true,
        container_image: None,  // defaults to CONTAINER_IMAGE_DEFAULT
    };
    Some(gliner_relex_entry(&env))
}
```

### Step 10.3: Add the new happy-path e2e test

Append at the end of the file (after the existing `happy_path_extract_returns_entities_and_triples` and other tests):

- [ ] **Add test:**

```rust
/// Slice 2.5: end-to-end through the macOS Apple `container` micro-VM
/// backend. Spawns the real Python worker INSIDE the container,
/// dispatches one `extract` over JSON-RPC stdio, asserts at least one
/// entity is returned. Proves the canonical
/// `Dr Smith --[treats]--> asthma` triple lands through the new
/// backend.
///
/// Skip-as-pass when the operator hasn't built the image (run
/// `scripts/workers/gliner-relex/build-image.sh`) or container CLI /
/// system service is missing.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn happy_path_container_extract_returns_entities_and_triples() {
    let Some(entry) = build_test_entry_container() else {
        return;
    };
    let Some((_cluster, pool)) = bring_up_pg("happy-container").await else {
        return;
    };

    // Sanity-check the manifest is in container mode (paranoia: catches
    // a future refactor that silently regresses build_test_entry_container).
    assert_eq!(
        entry.sandbox_backend,
        Some(kastellan_sandbox::SandboxBackendKind::Container),
        "build_test_entry_container must produce a Container-backend entry"
    );
    assert_eq!(
        entry.container_image.as_deref(),
        Some("kastellan/gliner-relex:dev"),
    );

    let sandboxes = Arc::new(kastellan_sandbox::SandboxBackends::default_for_current_os());
    let lifecycle = IdleTimeoutLifecycle::new(sandboxes);

    let mut handle = lifecycle
        .acquire("gliner-relex", &entry)
        .await
        .expect("acquire gliner-relex worker via container backend");

    let req = ExtractRequest {
        text: "Dr Smith treats asthma in Mosman.".to_string(),
        entity_labels: vec!["person".into(), "disease".into(), "location".into()],
        relation_labels: vec!["treats".into(), "located_in".into()],
        threshold: Some(0.5),
        relation_threshold: Some(0.5),
        max_entities: Some(64),
    };
    let params = serde_json::to_value(&req).expect("serialise ExtractRequest");

    let result_value = tool_host::dispatch(
        &pool,
        handle.worker_mut(),
        "gliner-relex",
        "extract",
        params,
    )
    .await
    .expect("dispatch extract via container backend");

    let response: ExtractResponse =
        serde_json::from_value(result_value).expect("decode ExtractResponse");

    assert!(
        !response.entities.is_empty(),
        "model should find at least one entity in 'Dr Smith treats asthma in Mosman.'"
    );
    // If we got triples, sanity-check the nested shape.
    if let Some(t) = response.triples.first() {
        assert!(!t.head.r#type.is_empty(), "head.type must be populated");
        assert!(!t.relation.is_empty(), "triple.relation must be populated");
    }
}
```

### Step 10.4: Run the new e2e test

- [ ] **Run** (requires Task 9 image build complete, weights staged, PG bin dir resolvable):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test gliner_relex_e2e happy_path_container -- --nocapture
```

Expected: PASS in ~30-60 s (cold-spawn container + model load + one inference + tear down).

If it fails with a missing-weights skip but you HAVE staged weights, double-check `KASTELLAN_DATA_DIR` matches what `resolve_weights_dir` expects (`$KASTELLAN_DATA_DIR/workers/gliner-relex/weights/multi-v1.0/`).

If it fails inside the container with a Python error, drop into the container manually for debugging:
```sh
container run --rm -it --mount type=bind,source=$KASTELLAN_DATA_DIR/workers/gliner-relex/weights/multi-v1.0,target=/weights,readonly \
  -e KASTELLAN_GLINER_RELEX_WEIGHTS_DIR=/weights \
  -e KASTELLAN_GLINER_RELEX_MODEL=knowledgator/gliner-relex-multi-v1.0 \
  -e KASTELLAN_GLINER_RELEX_DEVICE=auto \
  -e HF_HUB_OFFLINE=1 -e TRANSFORMERS_OFFLINE=1 \
  kastellan/gliner-relex:dev
```
(Then paste an `ExtractRequest` JSON-RPC line into the worker's stdin.)

### Step 10.5: Run the entire gliner_relex_e2e suite to verify nothing regressed

- [ ] **Run:**

```sh
cargo test -p kastellan-core --test gliner_relex_e2e -- --nocapture
```

Expected: all existing host-mode tests + the new container-mode test pass (or skip cleanly).

### Step 10.6: Commit

- [ ] **Commit:**

```sh
git add core/tests/gliner_relex_e2e.rs
git commit -m "$(cat <<'EOF'
test(core): e2e — gliner-relex extract via macOS container backend

happy_path_container_extract_returns_entities_and_triples spawns the
real Python worker INSIDE Apple `container` (image
kastellan/gliner-relex:dev built by scripts/workers/gliner-relex/
build-image.sh), dispatches one `extract` over JSON-RPC stdio through
the production IdleTimeoutLifecycle + tool_host::dispatch chain, and
asserts at least one entity is returned.

This is the actual proof that Slice 2.5's end-to-end story works:
manifest branches to container mode, lifecycle manager resolves the
right per-worker image, MacosContainer spawns the worker with --init
+ all policy-driven flags, the worker loads the model from the
host-mounted weights and answers the request, and the response
decodes cleanly through ExtractResponse.

Skip-as-pass on hosts without container CLI / image built / weights
staged. Sibling helpers:
  skip_if_container_unavailable     — probes MacosContainer::probe()
  skip_if_image_missing(tag)        — checks `container image list`
  build_test_entry_container()      — assembles the container-mode entry

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Full workspace test + final session-end docs update

### Step 11.1: Run the full workspace test suite

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep -E "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "TOTAL passed="p" failed="f" ignored="i}'
```

Expected: `TOTAL passed=1010 failed=0 ignored=3` (or close; +12 over the 998 baseline).

If failures appear, do NOT proceed — debug + fix before continuing.

### Step 11.2: Run the Python worker test suite (sanity — no Python changes shipped)

- [ ] **Run:**

```sh
cd workers/gliner-relex && uv run pytest -v 2>&1 | tail -5
cd ../..
```

Expected: 35 passed (matching the HANDOVER baseline; Slice 2.5 has no Python changes).

### Step 11.3: Update HANDOVER.md

Edit `docs/devel/handovers/HANDOVER.md`. At the very top of the file (line 7 area, the `**Last updated:**` line), prepend a new session entry describing Slice 2.5. Demote the existing `**Last updated:**` line to a `(prior, earlier this session)` paragraph.

The replacement header should look like (substitute actual commit hashes from `git log --oneline`):

- [ ] **Edit the header:**

```markdown
**Last updated:** 2026-05-23 (Next-TODO Item 25 — GLiNER-Relex Slice 2.5 (Containerfile + macOS image build) shipped on branch `feat/gliner-relex-slice-2.5`, PR pending. 10 commits: 1 spec + 1 `--init` always-on (closes #107) + 1 `SandboxBackends::resolve` widening + 1 `ToolEntry.container_image` field + 1 lifecycle-manager rerouting + 1 `GlinerRelexEnv`/`resolve_env` env-var support + 1 `gliner_relex_entry` host/container split + 1 Containerfile + build-image.sh + 1 container `--init` smoke + 1 e2e through container. Operator built `kastellan/gliner-relex:dev` via `scripts/workers/gliner-relex/build-image.sh`; `happy_path_container_extract_returns_entities_and_triples` confirms canonical extraction through Apple `container` 0.12.3. Workspace **998 → 1010 (+12)** on macOS, all green. Container backend now has real memory enforcement (`mem_mb=4096` is no longer a no-op on darwin); PID-1 signal forwarding closed via #107. Spec at `docs/superpowers/specs/2026-05-23-gliner-relex-slice-2.5-design.md`; plan at `docs/superpowers/plans/2026-05-23-gliner-relex-slice-2.5.md`. Earlier this session: docs sync on `main` (`7c53af3`) backfilling PR #117 (Item 23(a)) merge.
```

Then add a new `## Recently completed (this session, 2026-05-23 — Slice 2.5)` section before the existing `## Recently completed (this session, 2026-05-23 — Item 23 ...)` section, summarising the slice (key decisions, what shipped, test count delta, file-size watch, what's deliberately deferred). Mirror the format of the existing Item 23(a) entry.

Also update the `**Last commit on \`main\`:**` line — `main` hasn't moved since `7c53af3`, but add a `**Last commit on \`feat/gliner-relex-slice-2.5\`:**` line with the head commit of this branch.

Tick off Item 25 in the Next-TODO list (search for "Item 25 — GLiNER-Relex Slice 2.5" or "**First idle-timeout worker — GLiNER-Relex Slice 2.5**") with a "SHIPPED 2026-05-23" prefix + PR link placeholder.

### Step 11.4: Update ROADMAP.md

Edit `docs/devel/ROADMAP.md`. Find the "Phase 0b" or "Phase 1" section that tracks macOS Container slices (search for "MacosContainer" or "Slice 2.5"). Add a new `[x]` line at the end of the most-recent Slice-2 entry:

- [ ] **Add line:**

```markdown
- [x] **Slice 2.5 — gliner-relex Containerfile + macOS image build (2026-05-23)** — branch `feat/gliner-relex-slice-2.5`, **merged to `main` via PR [#XXX](https://github.com/hherb/kastellan/pull/XXX) at `<hash>`**. Closes [#107](https://github.com/hherb/kastellan/issues/107) (PID-1 signal handling). First real workload routed through Apple `container` micro-VM on macOS, proving end-to-end the per-worker `SandboxBackendKind` selection shipped in Slice 2. New `workers/gliner-relex/Containerfile` (python:3.12-slim + `uv pip install --system`; image tag `kastellan/gliner-relex:dev`); new `scripts/workers/gliner-relex/build-image.sh` operator helper; new `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1` opt-in + `KASTELLAN_GLINER_RELEX_IMAGE` override; `gliner_relex_entry` branches on `GlinerRelexEnv.use_container_backend`; `SandboxBackends::resolve` widens to take `image: Option<&str>`; `ToolEntry.container_image: Option<String>` new field; `--init` added unconditionally to `build_container_argv`. **Workspace 998 → 1010 (+12) on macOS, all green**: +1 `--init` argv pin, +1 existing always-on test renamed/tightened, +2 resolver widening (image-Arc-identity pins), +1 `shell_exec_entry` container_image default, +4 `resolve_env` env-var support (use_container strictness + venv-skip + image-override), +3 `gliner_relex_entry` container-mode shape, +1 macos_container_smoke `--init` envelope, +1 `happy_path_container_extract_returns_entities_and_triples` e2e.
```

(Replace `XXX` + `<hash>` after the PR is open and merged.)

### Step 11.5: Commit the doc updates

- [ ] **Commit:**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): GLiNER-Relex Slice 2.5 shipped (closes #107)

Workspace 998 → 1010 (+12) on macOS, all green. First real workload
routed through Apple `container` micro-VM, closing the macOS
memory-enforcement gap and Issue #107 (PID-1 signal handling) in one
slice. Spec + plan committed earlier in session.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Push the branch and open the PR

### Step 12.1: Push the branch

- [ ] **Run:**

```sh
git push -u origin feat/gliner-relex-slice-2.5
```

### Step 12.2: Open the PR via `gh`

- [ ] **Run:**

```sh
gh pr create --title "GLiNER-Relex Slice 2.5 — Containerfile + macOS image build (closes #107)" --body "$(cat <<'EOF'
## Summary

- First real workload routed through Apple `container` micro-VM on macOS: `gliner-relex` worker now opts into `MacosContainer` via `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`, with `mem_mb=4096` actually enforced (Seatbelt has no memory primitive).
- Closes [#107](https://github.com/hherb/kastellan/issues/107) by adding `--init` unconditionally to `build_container_argv` (parallel to bwrap's `--as-pid-1`).
- New `workers/gliner-relex/Containerfile` (python:3.12-slim + `uv pip install --system .` + `USER nobody` + `ENTRYPOINT`); new operator helper `scripts/workers/gliner-relex/build-image.sh`.
- Per-worker container image plumbing: new `container_image: Option<String>` field on `ToolEntry`; `SandboxBackends::resolve()` widens to take `(kind, image: Option<&str>)`.
- `gliner_relex_entry` branches on a new `GlinerRelexEnv.use_container_backend` field; host-mode shape stays byte-equivalent (default).
- Workspace **998 → 1010 (+12) on macOS, all green**.

Spec: [`docs/superpowers/specs/2026-05-23-gliner-relex-slice-2.5-design.md`](docs/superpowers/specs/2026-05-23-gliner-relex-slice-2.5-design.md).
Plan: [`docs/superpowers/plans/2026-05-23-gliner-relex-slice-2.5.md`](docs/superpowers/plans/2026-05-23-gliner-relex-slice-2.5.md).

## Test plan

- [x] `cargo test --workspace` on macOS → 1010 / 0 / 3
- [x] `cargo test -p kastellan-sandbox --test macos_container_smoke` → real `container run` envelope green with `--init`
- [x] `cargo test -p kastellan-core --test gliner_relex_e2e happy_path_container` → canonical extraction through Apple `container`
- [x] `cd workers/gliner-relex && uv run pytest` → 35 / 0 unchanged (no Python changes shipped)
- [ ] CI on Linux → workspace stays at 994 / 0 / 4 (cross-platform unit tests on resolve_env strictness + ToolEntry widening + `--init` argv pin)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed. Report it back to the user.

---

## Self-Review

After the plan was written, walking back through with fresh eyes:

**Spec coverage:**
- `--init` always-on: Task 1 ✓
- `SandboxBackends::resolve` widening: Task 2 ✓
- `ToolEntry.container_image`: Task 3 ✓
- Lifecycle manager rerouting: Task 4 ✓
- `GlinerRelexEnv` + `resolve_env` widening: Task 5 ✓
- `gliner_relex_entry` host/container branching: Task 6 ✓
- Containerfile + `build-image.sh`: Task 7 ✓
- `--init` smoke test against real container: Task 8 ✓
- Operator image build: Task 9 ✓
- Container-mode happy-path e2e: Task 10 ✓
- HANDOVER + ROADMAP updates: Task 11 ✓
- PR: Task 12 ✓

All 13 tests from the spec's test plan are covered (4 sandbox unit + 5 core unit on `resolve_env` + 1 unit on `shell_exec_entry` + 1 unit on `entry_host_mode_container_image_is_none` + 2 unit on container-mode entries + 1 smoke + 1 e2e = 15 total counted; spec said ~12 — small overage from the explicit pin tests is fine).

**Placeholder scan:** None. Every step shows the actual code or command.

**Type consistency:**
- `GlinerRelexEnv.use_container_backend: bool` referenced consistently in Tasks 5, 6, 10.
- `GlinerRelexEnv.container_image: Option<String>` referenced consistently in Tasks 5, 6, 10.
- `ToolEntry.container_image: Option<String>` referenced consistently in Tasks 3, 4, 6, 10.
- `SandboxBackends::resolve(kind, image: Option<&str>)` referenced consistently in Tasks 2, 4.
- Constants `CONTAINER_IMAGE_DEFAULT` (= `"kastellan/gliner-relex:dev"`) and `CONTAINER_BINARY` (= `"/usr/local/bin/kastellan-worker-gliner-relex"`) consistent across Tasks 6, 7 (in Containerfile install path), 10 (e2e test).

No issues found.
