# python-exec macOS micro-VM mode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in path that runs the `python-exec` worker under the macOS `MacosContainer` micro-VM backend, closing the macOS `mem_mb` parity gap and giving arbitrary agent code a separate-kernel boundary.

**Architecture:** Mirror gliner-relex Slice 2.5. A new `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1` (macOS-only) selects a `container_mode_entry` tagged `SandboxBackendKind::Container`, pointing `binary` at an in-image Linux build of `kastellan-worker-python-exec` and `KASTELLAN_PYTHON_EXEC_PYTHON` at the image's `/usr/local/bin/python3`. The image is a multi-stage build (Rust builder stage + `python:3.12-slim` runtime). Linux is untouched (stays on bwrap).

**Tech Stack:** Rust (core crate + sandbox crate), Apple `container` CLI 0.12.3, `python:3.12-slim` + `rust:1-slim` container images, bash.

**Spec:** `docs/superpowers/specs/2026-06-25-python-exec-macos-microvm-design.md`

## Global Constraints

- **AGPL-compatible deps only.** No new third-party Rust deps in this slice (reuses `kastellan-sandbox`). Image bases (`python:3.12-slim` Debian, `rust:1-slim`) are PSF/MIT/Apache — fine.
- **Cross-platform: Linux + macOS first-class.** Container mode is macOS-only by mechanism (Apple `container`); Linux keeps bwrap+seccomp+Landlock+cgroup (already the stronger baseline + already enforces `mem_mb`). All container-mode code is `#[cfg(target_os = "macos")]`-gated so the Linux build never references the macOS-only `SandboxBackendKind::Container` variant (the issue-#144 rule).
- **Every worker sandboxed before it runs.** Container mode is an *additional* boundary, never a bypass. `Net::Deny` + `WorkerStrict` are preserved verbatim.
- **Files ≤ 500 LOC where feasible.** `core/src/workers/python_exec.rs` is currently 317 LOC + tests external in `python_exec/tests.rs`; the additions keep it under cap.
- **TDD.** Pure unit tests precede implementation; the e2e is `#[ignore]` + skip-as-pass.
- **Worker is a pure stdio JSON-RPC server** (`workers/python-exec/src/main.rs` → `serve_stdio`, no argv). In container mode the `MacosContainer` backend appends `binary` as the container's sole program → **no `ENTRYPOINT`** in the Containerfile.
- **Commit messages** end with `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

---

### Task 1: Container image build infra (Containerfile + .containerignore + build-image.sh)

Builds the `kastellan/python-exec:dev` image: a Linux build of the worker binary + a Python interpreter. This task's deliverable is a working image build verified on this Mac.

**Files:**
- Create: `workers/python-exec/Containerfile`
- Create: `workers/python-exec/.containerignore`
- Create: `scripts/workers/python-exec/build-image.sh`

**Interfaces:**
- Produces: the image tag `kastellan/python-exec:dev` (override `KASTELLAN_PYTHON_EXEC_IMAGE`), containing `/usr/local/bin/kastellan-worker-python-exec` (the worker, runs as the program) and `/usr/local/bin/python3` (the interpreter). Consumed by Task 2's `container_mode_entry` and Task 3's e2e.

- [ ] **Step 1: Write the Containerfile**

Create `workers/python-exec/Containerfile`:

```dockerfile
# workers/python-exec/Containerfile
#
# Build image for the python-exec worker, consumed by the macOS
# `MacosContainer` SandboxBackend (Phase 4 micro-VM mode). Operator-built
# via scripts/workers/python-exec/build-image.sh; NOT used on Linux
# (LinuxBwrap stays the per-OS default there).
#
# Why multi-stage (vs gliner-relex's single-stage uv install): python-exec's
# worker is a Rust binary, not a Python console script. The builder stage
# compiles it Linux-native inside the image build, sidestepping the macOS
# cross-compile / `ring` C-dep problem entirely.
#
# Image layout decisions worth knowing:
#   1. python:3.12-slim runtime (glibc, matches gliner). The image's own
#      /usr/local/bin/python3 is the interpreter the worker drives; the
#      manifest injects KASTELLAN_PYTHON_EXEC_PYTHON=/usr/local/bin/python3.
#   2. NO ENTRYPOINT. The MacosContainer backend appends the manifest's
#      `binary` field verbatim as the container program, so the worker is
#      invoked directly as /usr/local/bin/kastellan-worker-python-exec.
#   3. USER nobody — defense-in-depth complement to build_container_argv's
#      `--user nobody`. If a future profile widening drops --user, the
#      image-baked USER still ensures non-root execution.
#   4. Build context is the WORKSPACE ROOT (the worker crate needs its
#      workspace siblings: prelude, protocol, matrix-wire, etc.). A
#      .containerignore keeps target/, .git, and worktrees out.

FROM rust:1-slim AS builder
WORKDIR /build
# Copy the whole workspace (context = repo root; see build-image.sh -f flag).
COPY . .
# Release build: smaller binary, and the worker's strict caps make debug
# overhead irrelevant. Build only the one worker crate + its deps.
RUN cargo build --release -p kastellan-worker-python-exec

FROM python:3.12-slim
COPY --from=builder /build/target/release/kastellan-worker-python-exec /usr/local/bin/
USER nobody
```

- [ ] **Step 2: Write the .containerignore**

Create `workers/python-exec/.containerignore` (keeps the build context small + reproducible):

```gitignore
target/
.git/
.claude/
**/*.md
docs/
assets/
```

- [ ] **Step 3: Write build-image.sh**

Create `scripts/workers/python-exec/build-image.sh` (mirror `scripts/workers/gliner-relex/build-image.sh`; context = repo root, `-f` points at the worker Containerfile):

```bash
#!/usr/bin/env bash
#
# Build the python-exec container image consumed by the macOS
# MacosContainer SandboxBackend (Phase 4 micro-VM mode). Companion to the
# host-mode path (Seatbelt/bwrap), which needs no image.
#
# Tag default: kastellan/python-exec:dev (overridable via
# KASTELLAN_PYTHON_EXEC_IMAGE, matching the daemon-side knob).
#
# Usage:
#     scripts/workers/python-exec/build-image.sh
#     KASTELLAN_PYTHON_EXEC_IMAGE=kastellan/python-exec:v0.0.1 \
#         scripts/workers/python-exec/build-image.sh
#
# Exits non-zero with a clear message if `container` CLI is missing or its
# system service is not running. Multi-stage Rust build: ~2-4 min cold.

set -euo pipefail

IMAGE_TAG="${KASTELLAN_PYTHON_EXEC_IMAGE:-kastellan/python-exec:dev}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
CONTAINERFILE="$REPO_ROOT/workers/python-exec/Containerfile"

if [[ ! -f "$CONTAINERFILE" ]]; then
    echo "error: Containerfile not found at $CONTAINERFILE" >&2
    exit 2
fi

if ! command -v container >/dev/null 2>&1; then
    echo "error: Apple \`container\` CLI not found on PATH." >&2
    echo "       Install with: brew install container" >&2
    exit 3
fi

if ! container system status >/dev/null 2>&1; then
    echo "error: \`container\` system service is not running." >&2
    echo "       Start it with: container system start" >&2
    exit 4
fi

echo "Building $IMAGE_TAG from $CONTAINERFILE (context: $REPO_ROOT) ..."
container build \
    --tag "$IMAGE_TAG" \
    --file "$CONTAINERFILE" \
    "$REPO_ROOT"

echo "Built $IMAGE_TAG. Verify the interpreter:"
echo "    container run --rm $IMAGE_TAG /usr/local/bin/python3 --version"
```

- [ ] **Step 4: Make it executable + build the image (real)**

Run:
```bash
chmod +x scripts/workers/python-exec/build-image.sh
container system start 2>/dev/null || true
scripts/workers/python-exec/build-image.sh
```
Expected: ends with `Built kastellan/python-exec:dev.` (cold build ~2-4 min).

- [ ] **Step 5: Verify the image runs the worker + interpreter**

Run:
```bash
container run --rm kastellan/python-exec:dev /usr/local/bin/python3 --version
container run --rm kastellan/python-exec:dev /usr/local/bin/kastellan-worker-python-exec --help 2>&1 | head -1 || true
```
Expected: `Python 3.12.x` from the first command (proves the in-image interpreter path the manifest will inject). The worker `--help` may error (it's a stdio server, not a CLI) — that's fine; the goal is to prove the binary exists and is executable in the VM.

- [ ] **Step 6: Commit**

```bash
git add workers/python-exec/Containerfile workers/python-exec/.containerignore scripts/workers/python-exec/build-image.sh
git commit -m "feat(python-exec): container image build infra for macOS micro-VM mode

Multi-stage Containerfile (Rust builder + python:3.12-slim runtime) bakes
a Linux build of the worker binary + interpreter; build-image.sh mirrors
gliner-relex's. Image consumed by the upcoming container_mode_entry.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `container_mode_entry` + env knobs + resolver branch + pure unit tests

Adds the macOS-only container-mode `ToolEntry` and the `USE_CONTAINER` selection, fully unit-tested without invoking `container`.

**Files:**
- Modify: `core/src/workers/python_exec.rs` (add consts, `container_mode_entry`, resolver branch)
- Modify: `core/src/workers/python_exec/tests.rs` (add macOS-gated unit tests)

**Interfaces:**
- Consumes: `kastellan_sandbox::SandboxBackendKind` (the `Container` variant, macOS-only); the existing `ToolEntry` struct.
- Produces: `pub fn container_mode_entry(binary: PathBuf, image: String, params_file_max: Option<String>) -> ToolEntry` (macOS-only, `pub` so the e2e in Task 3 builds the identical entry). New consts: `USE_CONTAINER_ENV: &str`, `IMAGE_ENV: &str`, `DEFAULT_IMAGE: &str`, `CONTAINER_WORKER_BIN: &str`, `CONTAINER_PYTHON: &str`.

- [ ] **Step 1: Write the failing unit tests**

Append to `core/src/workers/python_exec/tests.rs`:

```rust
// ---- container mode (macOS micro-VM) ----

/// Container-mode entry carries the Container backend tag + image, points
/// `binary` at the in-image worker, injects the in-image interpreter path,
/// preserves the strict policy, and binds NO host paths (code rides stdin,
/// scratch is the in-VM /tmp tmpfs).
#[cfg(target_os = "macos")]
#[test]
fn container_mode_entry_shape() {
    let entry = container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        "kastellan/python-exec:dev".to_string(),
        None,
    );
    assert_eq!(
        entry.sandbox_backend,
        Some(kastellan_sandbox::SandboxBackendKind::Container)
    );
    assert_eq!(
        entry.container_image.as_deref(),
        Some("kastellan/python-exec:dev")
    );
    assert_eq!(entry.binary, PathBuf::from(CONTAINER_WORKER_BIN));
    // Strict policy preserved.
    assert!(matches!(entry.policy.net, Net::Deny));
    assert_eq!(entry.policy.profile, Profile::WorkerStrict);
    assert_eq!(entry.policy.mem_mb, 512);
    assert_eq!(entry.policy.cpu_ms, 10_000);
    assert_eq!(entry.wall_clock_ms, Some(30_000));
    // No host binds in container mode.
    assert!(entry.policy.fs_read.is_empty(), "no host fs_read in container mode");
    assert!(entry.policy.fs_write.is_empty());
    // In-image interpreter injected; NO Landlock grant (Linux-prelude concept).
    assert!(entry
        .policy
        .env
        .contains(&(PYTHON_ENV.to_string(), CONTAINER_PYTHON.to_string())));
    assert!(!entry
        .policy
        .env
        .iter()
        .any(|(k, _)| k == ENV_LANDLOCK_RW));
    // No host scratch dir — the in-VM /tmp tmpfs serves params.json.
    assert!(!entry.ephemeral_scratch);
    assert!(matches!(
        entry.lifecycle,
        crate::worker_lifecycle::Lifecycle::SingleUse
    ));
}

/// The operator's params-file ceiling is forwarded into the jail only when set.
#[cfg(target_os = "macos")]
#[test]
fn container_mode_entry_forwards_params_file_max_only_when_set() {
    let without = container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        "img".to_string(),
        None,
    );
    assert!(!without
        .policy
        .env
        .iter()
        .any(|(k, _)| k == PARAMS_FILE_MAX_ENV));

    let with = container_mode_entry(
        PathBuf::from(CONTAINER_WORKER_BIN),
        "img".to_string(),
        Some("2097152".to_string()),
    );
    assert!(with
        .policy
        .env
        .contains(&(PARAMS_FILE_MAX_ENV.to_string(), "2097152".to_string())));
}

/// USE_CONTAINER=1 (macOS) routes the manifest to a Container-tagged entry,
/// with the default image when KASTELLAN_PYTHON_EXEC_IMAGE is unset.
#[cfg(target_os = "macos")]
#[test]
fn resolve_uses_container_backend_when_flag_set() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_USE_CONTAINER" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    // Only the worker binary needs to exist; NO host interpreter is probed
    // in container mode (the interpreter is in the image).
    let exists = |p: &Path| p == Path::new("/opt/python-exec");
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.sandbox_backend,
                Some(kastellan_sandbox::SandboxBackendKind::Container)
            );
            assert_eq!(entry.container_image.as_deref(), Some(DEFAULT_IMAGE));
            assert_eq!(entry.binary, PathBuf::from(CONTAINER_WORKER_BIN));
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

/// An explicit KASTELLAN_PYTHON_EXEC_IMAGE override is honoured.
#[cfg(target_os = "macos")]
#[test]
fn resolve_container_honours_image_override() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_USE_CONTAINER" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_IMAGE" => Some("kastellan/python-exec:v9".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    let exists = |p: &Path| p == Path::new("/opt/python-exec");
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(
                entry.container_image.as_deref(),
                Some("kastellan/python-exec:v9")
            );
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}

/// USE_CONTAINER unset (or != "1") stays in host mode: a host interpreter
/// IS probed and the entry carries no backend tag. (Runs on both OSes — on
/// Linux the flag is never even read.)
#[test]
fn resolve_stays_host_mode_without_use_container() {
    let get_env = |k: &str| match k {
        "KASTELLAN_PYTHON_EXEC_ENABLE" => Some("1".to_string()),
        "KASTELLAN_PYTHON_EXEC_BIN" => Some("/opt/python-exec".to_string()),
        _ => None,
    };
    let first = Path::new(PYTHON_CANDIDATES[0]);
    let exists = |p: &Path| p == Path::new("/opt/python-exec") || p == first;
    let c = ctx(&get_env, &exists);
    match PythonExecManifest.resolve(&c) {
        Resolution::Register(entry) => {
            assert_eq!(entry.sandbox_backend, None, "host mode carries no backend tag");
            assert!(entry.container_image.is_none());
        }
        other => panic!("expected Register, got {}", outcome_label(&other)),
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::python_exec 2>&1 | tail -20`
Expected: FAIL — `container_mode_entry`, `CONTAINER_WORKER_BIN`, `CONTAINER_PYTHON`, `DEFAULT_IMAGE` not found.

- [ ] **Step 3: Add the consts**

In `core/src/workers/python_exec.rs`, after the existing `PARAMS_FILE_MAX_ENV` const (line ~48), add:

```rust
/// Opt into the macOS micro-VM (`MacosContainer`) backend. macOS-only;
/// on Linux the flag is never read (the `Container` variant doesn't exist).
const USE_CONTAINER_ENV: &str = "KASTELLAN_PYTHON_EXEC_USE_CONTAINER";
/// Operator override for the container image tag.
const IMAGE_ENV: &str = "KASTELLAN_PYTHON_EXEC_IMAGE";
/// Default image tag built by scripts/workers/python-exec/build-image.sh.
pub const DEFAULT_IMAGE: &str = "kastellan/python-exec:dev";
/// In-image path of the worker binary (Containerfile copies it here). The
/// `MacosContainer` backend appends this as the container's program.
pub const CONTAINER_WORKER_BIN: &str = "/usr/local/bin/kastellan-worker-python-exec";
/// In-image python interpreter the worker drives (python:3.12-slim default).
pub const CONTAINER_PYTHON: &str = "/usr/local/bin/python3";
```

- [ ] **Step 4: Add `container_mode_entry`**

In `core/src/workers/python_exec.rs`, after `python_exec_entry` (line ~179), add:

```rust
/// Container-mode entry: routes python-exec through the macOS
/// `MacosContainer` micro-VM backend (opt-in via
/// `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1`). Closes the macOS `mem_mb`
/// parity gap (Seatbelt can't enforce memory; Apple `container` does via
/// `-m`) and gives arbitrary agent code a separate-kernel boundary.
///
/// Simpler than [`python_exec_entry`]: NO host interpreter discovery, NO
/// `interpreter_lib_dirs`, `fs_read` empty. Both the worker binary and the
/// interpreter live inside the image; code arrives over stdin and scratch
/// (incl. the >64 KiB `params.json` file channel) lands in the in-VM `/tmp`
/// tmpfs that `build_container_argv` mounts for `WorkerStrict`.
///
/// `mem_mb: 512` is now ENFORCED (the payoff). `cpu_quota_pct`/`tasks_max`
/// stay `None` (python-exec never set them; Apple `container` lacks the
/// primitive anyway). Latency: ~0.8 s container warm-spawn per call under
/// `SingleUse` — acceptable; freshness per call is the point for arbitrary
/// code.
///
/// macOS-only: emits `SandboxBackendKind::Container`, a
/// `#[cfg(target_os = "macos")]` variant. Compiling this on Linux is what
/// broke the core build before issue #144, so the whole fn is gated out
/// there and the resolver never reaches it.
#[cfg(target_os = "macos")]
pub fn container_mode_entry(
    binary: PathBuf,
    image: String,
    params_file_max: Option<String>,
) -> ToolEntry {
    let mut env = vec![(PYTHON_ENV.to_string(), CONTAINER_PYTHON.to_string())];
    if let Some(v) = params_file_max.filter(|v| !v.trim().is_empty()) {
        env.push((PARAMS_FILE_MAX_ENV.to_string(), v));
    }
    let policy = SandboxPolicy {
        fs_read: vec![],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerStrict,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: Some(kastellan_sandbox::SandboxBackendKind::Container),
        container_image: Some(image),
        lockdown_shim: None,
        ephemeral_scratch: false,
    }
}
```

- [ ] **Step 5: Add the resolver branch**

In `PythonExecManifest::resolve` (`core/src/workers/python_exec.rs`), right after the `ENABLE` gate is satisfied and before the host interpreter resolution. The cleanest seam: read the flag first, and when on, short-circuit to container mode. Insert at the top of `resolve` (after the existing `let is_runnable = ...` line, before the `resolve_env` call):

```rust
        // Container mode (macOS micro-VM) short-circuits host interpreter
        // resolution: the interpreter lives in the image, not on the host.
        // macOS-only — on Linux USE_CONTAINER is never read so the
        // `Container` variant is never referenced (issue #144).
        #[cfg(target_os = "macos")]
        {
            let enabled = (ctx.get_env)(ENABLE_ENV).unwrap_or_default().trim() == "1";
            let use_container =
                (ctx.get_env)(USE_CONTAINER_ENV).unwrap_or_default().trim() == "1";
            if enabled && use_container {
                let binary = PathBuf::from(CONTAINER_WORKER_BIN);
                let image = (ctx.get_env)(IMAGE_ENV)
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_IMAGE.to_string());
                let params_file_max = (ctx.get_env)(PARAMS_FILE_MAX_ENV);
                return Resolution::Register(container_mode_entry(
                    binary,
                    image,
                    params_file_max,
                ));
            }
            // enabled && !use_container, or !enabled: fall through to the
            // existing host-mode logic (which re-checks the ENABLE gate).
        }
```

Note: the host-mode path below already returns `Resolution::Disabled` when `ENABLE != 1`, so the `enabled` re-check here only guards against tagging a *disabled* worker as container-mode; the fall-through handles the `Disabled` reporting uniformly.

- [ ] **Step 6: Run the unit tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::python_exec 2>&1 | tail -20`
Expected: PASS (all existing + 5 new tests).

- [ ] **Step 7: Clippy + Linux cfg check**

Run:
```bash
source "$HOME/.cargo/env"
cargo clippy -p kastellan-core --lib --tests -- -D warnings 2>&1 | tail -5
cargo clippy -p kastellan-core --lib --target aarch64-unknown-linux-gnu 2>&1 | tail -5 || true
```
Expected: clean on macOS. The Linux cross-clippy may fail to link (`ring` C dep, per the memory note) — that's expected; what matters is no *compile* error in the `#[cfg(target_os="macos")]`-gated code, which the macOS clippy already proves and the gating guarantees. If the cross-target compiles far enough to type-check, confirm no reference to `Container` leaks outside the macOS cfg.

- [ ] **Step 8: Commit**

```bash
git add core/src/workers/python_exec.rs core/src/workers/python_exec/tests.rs
git commit -m "feat(python-exec): macOS container_mode_entry + USE_CONTAINER opt-in

KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1 (macOS-only) routes python-exec
through the MacosContainer micro-VM: in-image worker + interpreter, strict
policy preserved, mem_mb:512 now enforced, no host binds (in-VM /tmp tmpfs
serves params.json). 5 pure unit tests.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Real container e2e

Drives `python.exec` through the real micro-VM and pins the three behaviours: print round-trip, the `mem_mb` cap kill (the parity payoff), and `Net::Deny` containment. Skip-as-pass without `container`/image.

**Files:**
- Create: `core/tests/python_exec_container_e2e.rs`

**Interfaces:**
- Consumes: `container_mode_entry`, `DEFAULT_IMAGE`, `CONTAINER_WORKER_BIN` (Task 2); `tool_host::{dispatch, spawn_worker, WorkerSpec}`; `SandboxBackends::resolve(Some(Container), Some(image))` to get the container backend.

- [ ] **Step 1: Write the e2e (failing until the image exists / passes when it does)**

Create `core/tests/python_exec_container_e2e.rs`. Model the dispatch harness on `python_exec_e2e.rs` and the skip-guard + backend resolution on `lifecycle_container_routing_e2e.rs`:

```rust
//! End-to-end test: the agent core runs python-exec inside the macOS
//! `MacosContainer` micro-VM (Phase 4 container mode) and round-trips
//! `python.exec` through `tool_host::dispatch`.
//!
//! Pins what host mode can't on macOS: the `mem_mb: 512` cap is actually
//! ENFORCED by the VM (a >512 MiB allocation is SIGKILLed), and `Net::Deny`
//! + `--network none` contains a socket attempt inside the guest kernel.
//!
//! `[SKIP]`s cleanly when the `container` CLI / its system service / the
//! `kastellan/python-exec:dev` image are missing. Build the image first:
//!     scripts/workers/python-exec/build-image.sh

#![cfg(target_os = "macos")]

use std::sync::Arc;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::python_exec::{container_mode_entry, DEFAULT_IMAGE};
use kastellan_db::secrets::{MapKeyProvider, KEY_LEN};
use kastellan_sandbox::{macos_container::MacosContainer, SandboxBackendKind, SandboxBackends};
use serde_json::json;

const TEST_KEY_ID: &str = "test-keyring";

/// Skip when Apple `container` / the image aren't usable here.
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

/// Build a Vault (no secrets needed; dispatch requires one).
fn empty_vault() -> Vault {
    Vault::new(Arc::new(MapKeyProvider::new(TEST_KEY_ID, [42u8; KEY_LEN])))
}

/// Resolve the container backend for the python-exec image.
fn container_backend() -> Arc<dyn kastellan_sandbox::SandboxBackend> {
    SandboxBackends::default_for_current_os()
        .resolve(Some(SandboxBackendKind::Container), Some(DEFAULT_IMAGE))
}

/// Spawn the worker in the VM, dispatch one `python.exec`, return the result.
async fn run_in_container(code: &str) -> serde_json::Value {
    let entry = container_mode_entry(
        std::path::PathBuf::from(
            kastellan_core::workers::python_exec::CONTAINER_WORKER_BIN,
        ),
        DEFAULT_IMAGE.to_string(),
        None,
    );
    let backend = container_backend();
    let spec = WorkerSpec {
        program: entry.binary.to_str().unwrap(),
        args: &[],
        policy: &entry.policy,
        wall_clock_ms: entry.wall_clock_ms,
        // (Field set mirrors python_exec_e2e.rs's WorkerSpec construction;
        // adjust to the actual struct — see that file for the exact fields.)
    };
    let mut worker = spawn_worker(backend.as_ref(), &spec).expect("spawn worker in container");
    let result = dispatch(
        &mut worker,
        "python.exec",
        json!({ "code": code }),
        &empty_vault(),
        "python-exec",
    )
    .await;
    // Best-effort teardown.
    let _ = worker;
    result.expect("dispatch python.exec")
}

#[tokio::test]
async fn python_exec_round_trips_through_container() {
    if skip_if_no_container_image() {
        return;
    }
    let out = run_in_container("print('hello-from-microvm')").await;
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("hello-from-microvm"),
        "expected sentinel in stdout, got: {out}"
    );
    assert_eq!(out["exit_code"], 0);
}

#[tokio::test]
async fn container_enforces_mem_cap() {
    if skip_if_no_container_image() {
        return;
    }
    // Allocate ~900 MiB — above the 512 MiB cap. The VM SIGKILLs it; under
    // Seatbelt host mode this would succeed (the parity gap this closes).
    let code = "x = bytearray(900 * 1024 * 1024); print(len(x))";
    let out = run_in_container(code).await;
    // Killed by the cgroup/OOM inside the VM → non-zero exit and no success print.
    assert_ne!(out["exit_code"], 0, "expected non-zero exit on OOM, got: {out}");
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(
        !stdout.contains(&(900 * 1024 * 1024).to_string()),
        "the allocation print must not appear — it should be killed first: {out}"
    );
}

#[tokio::test]
async fn container_contains_socket_attempt() {
    if skip_if_no_container_image() {
        return;
    }
    // Net::Deny + --network none: any connect attempt fails inside the VM.
    let code = "\
import socket, sys
try:
    s = socket.create_connection(('1.1.1.1', 443), timeout=2)
    print('CONNECTED')
except Exception as e:
    print('blocked', file=sys.stderr)
";
    let out = run_in_container(code).await;
    let stdout = out["stdout"].as_str().unwrap_or_default();
    assert!(!stdout.contains("CONNECTED"), "network must be denied: {out}");
}
```

**Implementer note:** the `WorkerSpec` field set and `dispatch`/`spawn_worker` signatures above are approximate — read `core/tests/python_exec_e2e.rs` for the exact current construction and copy it verbatim, swapping only the backend (container vs `backend()`) and the entry (`container_mode_entry` vs `python_exec_entry`). Do **not** invent fields.

- [ ] **Step 2: Run the e2e (real, image built in Task 1)**

Run:
```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test python_exec_container_e2e -- --nocapture 2>&1 | tail -40
```
Expected: 3 passed. If any assertion is shaky (e.g. the OOM manifests as a different exit signature), adjust the assertion to match the real `python.exec` result shape — but keep all three behaviours pinned. Confirm there are **no** `[SKIP]` lines (the image exists from Task 1).

- [ ] **Step 3: Clippy the test**

Run: `cargo clippy -p kastellan-core --test python_exec_container_e2e -- -D warnings 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add core/tests/python_exec_container_e2e.rs
git commit -m "test(python-exec): real macOS micro-VM e2e (round-trip + mem cap + net deny)

Drives python.exec through the MacosContainer VM via tool_host::dispatch.
Pins the mem_mb:512 enforcement (the parity payoff, unenforceable under
Seatbelt) + Net::Deny containment + print round-trip. Skip-as-pass without
the container CLI/image.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Regression sweep + docs + PR

Confirm nothing else broke, update the handover/roadmap, open the PR.

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Targeted regression — host-mode python-exec still green**

Run:
```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib workers::python_exec 2>&1 | tail -5
cargo test -p kastellan-core --test python_exec_e2e -- --nocapture 2>&1 | tail -15
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
```
Expected: python_exec lib units pass; `python_exec_e2e` 4/0 (host mode under Seatbelt, unaffected); workspace clippy clean.

- [ ] **Step 2: Update HANDOVER.md**

Set the "Last updated" header to a one-paragraph summary of this slice (DONE on branch `feat/python-exec-macos-microvm`, PR pending). Fix the stale "PR pending" on the prior (#354) entry → it merged as `cc213ad`. Move the python-exec micro-VM item from "Next TODO" to recently-completed. Add a "Next TODO" pointing at the remaining Phase-4 picks (curated-wheels RO dir; Linux micro-VM `FirecrackerVm`; warm/idle container lifecycle for python-exec).

- [ ] **Step 3: Update ROADMAP.md**

Tick the Phase-4 micro-VM line for python-exec (terse, with the PR hash once merged). The Linux micro-VM stays an open `[ ]` item.

- [ ] **Step 4: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: handover + roadmap for python-exec macOS micro-VM mode

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 5: Push + open PR**

Push the branch and open a PR to `main` describing the slice (opt-in macOS micro-VM for python-exec; mem_mb parity payoff; Linux unchanged). Link the Phase-4 continuation context. If the Mac→github push is firewalled, use the DGX relay (`format-patch | ssh dgx git am` then push from the DGX; `gh pr create` from the Mac) per the memory note.

---

## Self-Review

**Spec coverage:**
- Containerfile + `.containerignore` + build-image.sh → Task 1 ✓
- `USE_CONTAINER`/`IMAGE` env knobs + `container_mode_entry` + resolver branch → Task 2 ✓
- Writable scratch via in-VM `--tmpfs /tmp` (no host bind, `ephemeral_scratch:false`) → Task 2 (policy) + asserted in unit test ✓
- `SingleUse` lifecycle + latency note → Task 2 doc comment ✓
- e2e: round-trip + mem cap + net deny, skip-as-pass → Task 3 ✓
- Cross-platform gating (macOS-only, Linux host mode) → Task 2 Step 5 + Step 7 ✓
- Docs (HANDOVER/ROADMAP) → Task 4 ✓
- Out-of-scope items (Linux micro-VM, wheels, warm lifecycle) → left as Next TODO, not built ✓

**Placeholder scan:** No TBD/TODO. The one approximate spot (the e2e's `WorkerSpec`/`dispatch` signatures) is explicitly flagged with an instruction to copy the exact construction from `python_exec_e2e.rs` — a deliberate "match the existing source" directive, not a vague placeholder, because the struct's current field set must not be guessed.

**Type consistency:** `container_mode_entry(binary: PathBuf, image: String, params_file_max: Option<String>)` — same signature in Task 2 (definition + tests) and Task 3 (e2e). Consts `DEFAULT_IMAGE`, `CONTAINER_WORKER_BIN`, `CONTAINER_PYTHON` used consistently. `sandbox_backend`/`container_image`/`ephemeral_scratch` field names match the `ToolEntry` struct confirmed in gliner's `entry.rs`.
