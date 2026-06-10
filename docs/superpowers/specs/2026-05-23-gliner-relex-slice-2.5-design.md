# GLiNER-Relex Slice 2.5 — Containerfile + macOS image build

**Date:** 2026-05-23
**Parent issue:** [#55](https://github.com/hherb/kastellan/issues/55) (macOS micro-VM backend)
**Also closes:** [#107](https://github.com/hherb/kastellan/issues/107) (PID-1 signal handling)
**Predecessor slices:**
- Spike: [`2026-05-21-macos-container-spike-notes.md`](2026-05-21-macos-container-spike-notes.md)
- Slice 1 (`MacosContainer` skeleton): merged via PR #106 at `cc0b0de`
- Slice 2 (per-worker backend selection): merged via PR #108 at `1b86f84`
- GLiNER-Relex worker (host mode): merged via PR #88 at `715a882`
- HANDOVER Item 25 (this slice).

**Scope:** 1 session — first real workload on the `MacosContainer` backend.

## Context

Slice 1 (PR #106) shipped `MacosContainer: SandboxBackend`. Slice 2 (PR
#108) shipped per-worker backend selection so a `ToolEntry` can opt into
`sandbox_backend: Some(Container)`. Slice 2's smoke validated the
plumbing against plain `alpine:3.20`. Slice 2.5 is where the
end-to-end story actually lands: build a real container image for
`gliner-relex`, flip its manifest to container mode, and verify the
canonical `Dr Smith --[treats]--> asthma (0.994)` triple comes out the
other end on macOS.

**Why this matters:** Apple Seatbelt has no memory primitive. The
`gliner-relex` worker uses PyTorch + transformers + gliner — easily
2-3 GB resident in steady state, with model-load spikes. On macOS today
the `mem_mb: 4_096` cap in `gliner_relex_entry`'s `SandboxPolicy` is
silently a no-op. Slice 2.5 closes that gap for the first real workload.

This slice also closes Issue #107 (PID-1 signal handling) by adding
`--init` unconditionally to `build_container_argv` — gliner-relex is
the first long-lived container worker, so signal forwarding +
zombie reaping become load-bearing here.

## Scope

### In scope

- New `workers/gliner-relex/Containerfile` (Python 3.12 slim + `uv pip
  install --system` + `USER nobody` + `ENTRYPOINT
  ["kastellan-worker-gliner-relex"]`).
- New `scripts/workers/gliner-relex/build-image.sh` operator-runnable
  helper consistent with the existing `install.sh`.
- `gliner_relex_entry` branches on a new `GlinerRelexEnv.use_container_backend`
  field: container mode emits in-container `binary` path, weights-only
  `policy.fs_read`, `sandbox_backend = Some(Container)`,
  `container_image = Some(CONTAINER_IMAGE_DEFAULT)`. Host mode stays
  byte-equivalent to today.
- `resolve_env` reads two new env vars: `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`
  (gates container mode) and `KASTELLAN_GLINER_RELEX_IMAGE=<tag>` (image-tag
  override, defaults to `kastellan/gliner-relex:dev`). Container mode
  skips the host-venv existence check.
- New `container_image: Option<String>` field on `ToolEntry`.
- `SandboxBackends::resolve()` widens to `(kind, image: Option<&str>)
  -> Arc<dyn SandboxBackend>`. When `kind == Some(Container)` and
  `image == Some(tag)`, returns a per-call
  `Arc::new(MacosContainer::with_image(tag))`. All other arms unchanged.
- `--init` added unconditionally to `build_container_argv`. Closes #107.
- New happy-path e2e test in `core/tests/gliner_relex_e2e.rs`:
  `happy_path_container_extract_returns_entities_and_triples`.
- Unit tests for the resolver widening, `ToolEntry` widening, env-var
  parsing, container-mode manifest shape, and `--init` argv pin.

### Out of scope (deferred)

- **Linux Firecracker counterpart.** Linux already enforces `mem_mb` /
  `cpu_quota_pct` / `tasks_max` via cgroup v2. A Linux micro-VM backend
  would be defense-in-depth, not parity-fix.
- **`python-exec` worker on container.** Slice 3 (Phase 4) territory.
- **`cargo build` integration for image build.** Per spike notes,
  "future slice."
- **Multi-arch image (x86_64 alongside arm64).** Container on macOS is
  arm64-only today.
- **Image-tag namespacing for multiple model versions.** Single tag
  `kastellan/gliner-relex:dev` for this slice; future multi-model support
  would mean separate tags + per-tag manifests.
- **Operator CLI for runtime backend swap.** Daemon restart required
  after env-var flip — same posture as `KASTELLAN_GLINER_RELEX_ENABLE`.
- **Image rebuild automation in CI.** macOS CI runners would need to
  build the image once; this slice ships the operator helper, not the
  CI integration.
- **`gliner_relex.rs` 500-cap breach.** This slice adds ~80 LOC; the
  file remains over cap (1238 → ~1320 LOC). Item 26 (deferred
  test-module lift) addresses the cap separately.

## Design

### Data flow

```
operator: scripts/workers/gliner-relex/build-image.sh
  └─► container build -t kastellan/gliner-relex:dev workers/gliner-relex/
       └─► image: system-installed pkg + /usr/local/bin/kastellan-worker-gliner-relex

operator: export KASTELLAN_GLINER_RELEX_USE_CONTAINER=1

daemon startup
  └─► resolve_env() → GlinerRelexEnv { use_container_backend: true, ... }
  └─► gliner_relex_entry() branches:
       binary = /usr/local/bin/kastellan-worker-gliner-relex
       policy.fs_read = [weights_dir]
       sandbox_backend = Some(Container)
       container_image = Some("kastellan/gliner-relex:dev")

step dispatch (per request)
  └─► IdleTimeoutLifecycle.acquire("gliner-relex", &entry)
       └─► sandboxes.resolve(entry.sandbox_backend, entry.container_image.as_deref())
            └─► Arc::new(MacosContainer::with_image("kastellan/gliner-relex:dev"))
       └─► spawn_worker via:
            container run --rm -i --init --progress none \
              [policy flags from build_container_argv...] \
              kastellan/gliner-relex:dev /usr/local/bin/kastellan-worker-gliner-relex
```

**Two backwards-compatibility properties:**

1. `KASTELLAN_GLINER_RELEX_USE_CONTAINER` unset → byte-equivalent to
   today's host-mode (Seatbelt on macOS, bwrap on Linux). All existing
   e2e tests stay byte-equivalent.
2. `container_image: Option<String>` defaults to `None` everywhere;
   `SandboxBackends::resolve(kind, None)` returns the cached default-image
   backend exactly as Slice 2 shipped.

### Types

#### `ToolEntry` widening (in `core/src/scheduler/tool_dispatch.rs`)

```rust
pub struct ToolEntry {
    pub binary: PathBuf,
    pub policy: SandboxPolicy,
    pub wall_clock_ms: Option<u64>,
    pub lifecycle: crate::worker_lifecycle::Lifecycle,
    pub sandbox_backend: Option<SandboxBackendKind>,
    /// Container image tag for the `MacosContainer` backend. Only
    /// meaningful when `sandbox_backend == Some(Container)`; ignored
    /// otherwise (the field is `Option` rather than enum-coupled so
    /// future container-based backends on other platforms can reuse
    /// the same shape without enum widening).
    ///
    /// `None` with `sandbox_backend == Some(Container)` falls back to
    /// `MacosContainer`'s `DEFAULT_IMAGE` (`alpine:3.20`) — useful for
    /// Slice 1-style smoke tests. Production workers (gliner-relex,
    /// future python-exec) populate this with their per-worker image.
    pub container_image: Option<String>,
}
```

`shell_exec_entry` (only other shipping constructor) sets
`container_image: None` explicitly.

#### `SandboxBackends::resolve` widening (in `sandbox/src/lib.rs`)

```rust
impl SandboxBackends {
    /// Resolve a per-worker `SandboxBackendKind` + optional container
    /// image tag to a concrete backend.
    ///
    /// * `(None, _)` → per-OS default (Linux → bwrap, darwin → seatbelt).
    /// * `(Some(Container), None)` → cached default-image container
    ///   backend (used by Slice 1's smoke tests).
    /// * `(Some(Container), Some(tag))` → per-call
    ///   `Arc::new(MacosContainer::with_image(tag))`. Cheap (String +
    ///   Arc); the probe done at `default_for_current_os()` against
    ///   the default image is image-independent, so no re-probe needed.
    /// * Other `Some(kind)` arms → existing cached slots, image ignored.
    pub fn resolve(
        &self,
        kind: Option<SandboxBackendKind>,
        image: Option<&str>,
    ) -> Arc<dyn SandboxBackend> { ... }
}
```

**Why per-call construct is OK:**
- `MacosContainer::with_image` just stores a `String`; no I/O.
- Probe checks `container --version` + `container system status` — both
  image-independent. The default-image probe at startup covers any
  custom-image use.
- For `IdleTimeoutLifecycle` (gliner-relex's lifecycle), `resolve` is
  called once per cold-spawn, not per request. Amortised cost is zero.

#### `GlinerRelexEnv` widening (in `core/src/workers/gliner_relex.rs`)

```rust
pub struct GlinerRelexEnv {
    pub script_path: PathBuf,           // empty in container mode
    pub venv_dir: PathBuf,              // empty in container mode
    pub weights_dir: PathBuf,           // always populated
    pub model_id: String,
    pub device: String,
    /// True when the operator set `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1`.
    /// `gliner_relex_entry` branches on this field to emit the
    /// container-mode `ToolEntry` shape instead of the host-mode one.
    pub use_container_backend: bool,
    /// Operator-supplied image tag override, read from
    /// `KASTELLAN_GLINER_RELEX_IMAGE`. `None` → falls back to
    /// `CONTAINER_IMAGE_DEFAULT` at the `gliner_relex_entry` callsite.
    pub container_image: Option<String>,
}
```

### `gliner_relex_entry` branching

```rust
const CONTAINER_IMAGE_DEFAULT: &str = "kastellan/gliner-relex:dev";
const CONTAINER_BINARY: &str = "/usr/local/bin/kastellan-worker-gliner-relex";

pub fn gliner_relex_entry(env: &GlinerRelexEnv) -> ToolEntry {
    if env.use_container_backend {
        container_mode_entry(env)
    } else {
        host_mode_entry(env)  // today's body, unchanged
    }
}

fn container_mode_entry(env: &GlinerRelexEnv) -> ToolEntry {
    // Only weights are host data; venv + src baked into image. Mount
    // weights at SAME host path inside container (build_container_argv
    // convention: source=<P>,target=<P>). KASTELLAN_GLINER_RELEX_WEIGHTS_DIR
    // env var continues to point at the host path verbatim.
    let policy = SandboxPolicy {
        fs_read: vec![env.weights_dir.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 0,
        mem_mb: 4_096,
        profile: Profile::WorkerStrict,
        cpu_quota_pct: Some(400),
        tasks_max: Some(64),
        env: vec![
            ("KASTELLAN_GLINER_RELEX_WEIGHTS_DIR".into(),
             env.weights_dir.to_string_lossy().into_owned()),
            ("KASTELLAN_GLINER_RELEX_MODEL".into(), env.model_id.clone()),
            ("KASTELLAN_GLINER_RELEX_DEVICE".into(), env.device.clone()),
            ("HF_HUB_OFFLINE".into(), "1".into()),
            ("TRANSFORMERS_OFFLINE".into(), "1".into()),
            ("USER".into(), "kastellan".into()),
            ("TORCHINDUCTOR_CACHE_DIR".into(), "/tmp/torchinductor".into()),
        ],
    };

    let image = env.container_image.clone()
        .unwrap_or_else(|| CONTAINER_IMAGE_DEFAULT.to_string());

    ToolEntry {
        binary: PathBuf::from(CONTAINER_BINARY),
        policy,
        wall_clock_ms: None,
        lifecycle: build_idle_timeout_lifecycle(),  // shared with host mode
        sandbox_backend: Some(SandboxBackendKind::Container),
        container_image: Some(image),
    }
}
```

**Lifecycle stays identical between modes.** Same `IdleTimeoutCaps`
(10 min idle, 10 000 req, daily rotation, 5 s grace), same
`Contract { stateless: true }`. The container's `--init` handles
signal-forwarding; lifecycle manager kills the outer `container run`
process and `--init` propagates SIGTERM to the worker inside.

The `build_idle_timeout_lifecycle()` helper is a new private function
extracted from the existing inline body (`Lifecycle::idle_timeout(...).expect(...)`)
so both `host_mode_entry` and `container_mode_entry` share one source
of truth for the lifecycle caps. The extraction has no behaviour
change — the constants and the `.expect` message are unchanged. The
existing `gliner_relex_entry_lifecycle_*` unit tests remain the
regression pin.

### `resolve_env` changes

```rust
let use_container_backend = env_lookup("KASTELLAN_GLINER_RELEX_USE_CONTAINER")
    .map(|v| v.trim() == "1")
    .unwrap_or(false);

let container_image = env_lookup("KASTELLAN_GLINER_RELEX_IMAGE");

// Skip venv/script existence check in container mode.
let (venv_dir, script_path) = if use_container_backend {
    (PathBuf::new(), PathBuf::new())
} else {
    let venv_dir = resolve_venv_dir(&env_lookup)?;  // existing
    let script_path = venv_dir.join("bin").join("kastellan-worker-gliner-relex");
    if !exists(&script_path) {
        return Err(ResolveSkipReason::ScriptShimMissing { path: script_path });
    }
    (venv_dir, script_path)
};
```

**No new `ResolveSkipReason` variants.** Container CLI missing / image
not built → surfaced at spawn time by the lifecycle manager (same
posture as any other backend-side failure). Adding probe-time skip
variants would force `resolve_env` to be I/O-heavy and couple it to
the sandbox layer.

### `build_container_argv` `--init` change

```rust
// sandbox/src/macos_container.rs::build_container_argv
argv.push("container".into());
argv.push("run".into());
argv.push("--rm".into());
argv.push("-i".into());
argv.push("--init".into());                      // NEW (always on)
argv.push("--progress".into());
argv.push("none".into());
// ... rest unchanged
```

Parallel to `LinuxBwrap`'s unconditional `--as-pid-1`. Cost is one
extra small init process per container — negligible for both
short-lived smoke and long-lived workers. Removes the need for
`build_container_argv` to be lifecycle-aware.

### Containerfile

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
#      policy.fs_read host path. Image stays ~3 GB instead of ~4.5 GB;
#      weight refreshes don't require image rebuild.
#   4. ENTRYPOINT carries the worker shim. The MacosContainer backend
#      appends the binary path verbatim, so the manifest's `binary`
#      field doubles as the in-container `program` argument.

FROM python:3.12-slim

# uv is pinned for reproducible image builds. Bumping is a paired edit
# with the host install.sh's uv-version assumption.
RUN pip install --no-cache-dir uv==0.4.30

WORKDIR /build

# Build context is workers/gliner-relex/; pyproject.toml + uv.lock +
# README.md + src/ are everything we need.
COPY pyproject.toml uv.lock README.md ./
COPY src/ ./src/

# --system installs into /usr/lib/python3.12/site-packages and creates
# /usr/local/bin/<console-scripts>. --no-cache keeps the image small;
# --no-dev skips the [dev] optional deps (pytest, pytest-mock) which
# tests don't need (tests run against the host venv, not the container).
RUN uv pip install --system --no-cache --no-dev .

# Defense-in-depth complement to the policy-driven `--user nobody` flag.
# If a future profile widening drops --user, the image-baked USER still
# ensures non-root execution. python:3.12-slim ships with nobody (uid 65534).
USER nobody

ENTRYPOINT ["kastellan-worker-gliner-relex"]
```

### Build script

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

## Tests

TDD-ordered.

### Unit tests (sandbox crate)

1. **`argv_carries_init_for_signal_forwarding_and_zombie_reaping`** —
   `build_container_argv` always emits `--init` immediately after `-i`.
2. **`sandbox_backends_resolve_with_custom_image_returns_fresh_container`** —
   `resolve(Some(Container), Some("kastellan/gliner-relex:dev"))` returns
   a backend whose `image()` matches the requested tag; not
   Arc-pointer-equal to the cached default-image slot.
3. **`sandbox_backends_resolve_with_none_image_returns_cached_default`** —
   `resolve(Some(Container), None)` returns the cached `Arc::clone` of
   the default slot (Arc-pointer identity pin).
4. **Existing `build_container_argv` test updates** (2 tests) — add
   `--init` to the expected always-on argv prefix.

### Unit tests (core: `workers/gliner_relex`)

5. **`gliner_relex_entry_host_mode_byte_equivalent_to_today`** —
   `use_container_backend = false` produces the byte-equivalent
   `ToolEntry` shape as today (existing manifest tests are the
   regression pin; one new assertion: `container_image == None`).
6. **`gliner_relex_entry_container_mode_emits_in_container_binary_and_weights_only_fs_read`** —
   `use_container_backend = true` produces
   `binary = CONTAINER_BINARY`, `policy.fs_read = [weights_dir]`,
   `sandbox_backend = Some(Container)`,
   `container_image = Some(CONTAINER_IMAGE_DEFAULT)`.
7. **`gliner_relex_entry_container_mode_honours_custom_image_tag`** —
   `GlinerRelexEnv.container_image = Some("kastellan/gliner-relex:v0.0.1")`
   flows into `entry.container_image`.
8. **`resolve_env_sets_use_container_backend_when_env_var_is_one`** —
   pure test against in-memory env-lookup closure (existing pattern).
9. **`resolve_env_skips_venv_existence_check_in_container_mode`** —
   `use_container_backend = true` + missing host venv → returns
   `Ok(GlinerRelexEnv { script_path: empty, venv_dir: empty, .. })`,
   not `Err(ScriptShimMissing)`.
10. **`resolve_env_strict_about_use_container_value`** — only `"1"`
    (after trim) counts; `true` / `yes` / `0` / unset all →
    `use_container_backend = false`.

### Unit tests (core: `ToolEntry` widening)

11. **`tool_entry_container_image_defaults_to_none`** —
    `shell_exec_entry(...).container_image == None`.

### Integration test (sandbox crate, macOS-gated)

12. **`macos_container_argv_with_init_runs_alpine_cleanly`** (extends
    existing `macos_container_smoke.rs`) — `container run --init ...`
    against `alpine:3.20`'s `/bin/sh -c 'echo ok'` exits 0; pins that
    `--init` doesn't break the existing smoke envelope.

### Integration test (`core/tests/gliner_relex_e2e.rs`, macOS-gated)

13. **`happy_path_container_extract_returns_entities_and_triples`** —
    new test with new `build_test_entry_container()` fixture +
    `skip_if_container_unavailable()` + `container_image_exists("kastellan/gliner-relex:dev")`
    helpers. Mirrors the existing happy-path test body (PG bring-up,
    `IdleTimeoutLifecycle`, `tool_host::dispatch`, assert at-least-one
    entity); skip-as-pass when image/CLI/weights missing.

### Test count delta

| Platform | Before | After | Delta |
|---|---|---|---|
| macOS | 998 / 0 / 3 | ~1010 / 0 / 3+ | +12 |
| Linux DGX | 990 / 0 / 4 | 994 / 0 / 4+ | +4 (cross-platform unit tests on resolve_env strictness + ToolEntry widening + `--init` argv pin) |

Skip lines on hosts that haven't built the image:
```
[SKIP] kastellan/gliner-relex:dev image not built — run scripts/workers/gliner-relex/build-image.sh
[SKIP] container CLI not on PATH — install via 'brew install container'
[SKIP] container system service not running — start via 'container system start'
```

## File-size watch (post-slice)

- `core/src/workers/gliner_relex.rs`: 1238 → ~1320 LOC. Stays over
  cap; Item 26 (test-module lift) addresses separately.
- `sandbox/src/lib.rs`: ~330 → ~340 LOC (resolver widening). Under cap.
- `sandbox/src/macos_container.rs`: 815 → ~820 LOC. Stays over cap;
  not touched structurally.
- `core/tests/gliner_relex_e2e.rs`: 390 → ~480 LOC. Under cap.
- New `workers/gliner-relex/Containerfile`: ~20 LOC.
- New `scripts/workers/gliner-relex/build-image.sh`: ~50 LOC.

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| **Container image build is ~3 GB** (PyTorch + transformers + gliner wheels). | One-time per operator; documented in script's final message. CI cost is one-time-per-fresh-runner. |
| **`--init` interaction with worker's signal handling.** `IdleTimeoutLifecycle` kills the outer `container run`; `--init` should propagate SIGTERM cleanly to the worker inside. | Smoke-tested via existing `worker_lifecycle_idle_timeout_e2e` plus the new `happy_path_container_extract` (drop-cleanup at end of test exercises the kill path). |
| **Container probe is image-independent**, but `resolve(kind, Some(image))` constructs a per-call backend that isn't probed. | `MacosContainer::probe()` was called once at `default_for_current_os()` against the default image; the probe checks CLI version + system-service status (image-independent). First spawn against a missing image surfaces a clear "image not found" error. |
| **Image tag `:dev` is mutable** — operator rebuilds, daemon caches `MacosContainer::with_image(":dev")` per warm worker. | `IdleTimeoutLifecycle`'s warm cache is in-process and re-cold-spawns at daemon restart. Operator rebuild + daemon restart cycle is the documented refresh path. Production deployments should pin a semver tag (`:v0.0.1`) via `KASTELLAN_GLINER_RELEX_IMAGE` env. |
| **Operator forgets `KASTELLAN_GLINER_RELEX_USE_CONTAINER=1` after building image.** | Final message in `build-image.sh` explicitly tells the operator to set both env vars. |
| **Cumulative LOC growth on `gliner_relex.rs`** (currently 1238 LOC, already over cap). | This slice adds ~80 LOC. Item 26 (deferred test-module lift) addresses the cap separately. |
| **uv version drift between Containerfile (`0.4.30`) and host `install.sh`.** | Containerfile docstring flags the pairing; future uv bump is a paired edit. |

## Implementation order (TDD)

1. **Sandbox crate** — add `--init` to `build_container_argv` + 1 new
   unit test + update 2 existing argv tests. Independent change; can
   merge separately if useful for #107 hygiene.
2. **Sandbox crate** — widen `SandboxBackends::resolve()` signature +
   2 new unit tests. All existing `resolve(kind)` callers update to
   `resolve(kind, None)` (mechanical).
3. **Core: `ToolEntry` widening** — add `container_image: Option<String>`
   + 1 new unit test on `shell_exec_entry`. All existing constructors
   updated (mechanical).
4. **Core: lifecycle managers** — update `SingleUseLifecycle` and
   `IdleTimeoutLifecycle` to pass `entry.container_image.as_deref()`
   to `sandboxes.resolve()`. Tiny mechanical edit; existing tests are
   the regression pin.
5. **Core: `GlinerRelexEnv` + `resolve_env`** — add `use_container_backend`
   + `container_image` fields, read new env vars, skip venv check in
   container mode. +3 new unit tests.
6. **Core: `gliner_relex_entry` branching** — extract `host_mode_entry`
   from existing body, add `container_mode_entry`, top-level dispatch.
   +3 new unit tests.
7. **Containerfile + build script** — write both, commit together.
   No automated tests; smoke tested via step 9.
8. **Container smoke test** — `macos_container_argv_with_init_runs_alpine_cleanly`
   (extends `macos_container_smoke.rs`). Verifies `--init` doesn't
   break the existing smoke envelope.
9. **Operator manual build** — run `scripts/workers/gliner-relex/build-image.sh`
   to build the actual image. This is the unblocking step for the e2e
   in step 10.
10. **E2E** — `happy_path_container_extract_returns_entities_and_triples`
    in `core/tests/gliner_relex_e2e.rs`. Skip-as-pass on hosts without
    the image; green on the operator's macOS after step 9.

Each numbered item is a self-contained commit; the bundle ships as
one PR (operator preference for bundled refactor PRs per HANDOVER
convention).

## Closing notes

- Closes Issue [#107](https://github.com/hherb/kastellan/issues/107) as
  part of step 1.
- Unblocks future Slice 3 (`python-exec` on container) by proving the
  per-worker image-tag path works end-to-end.
- After merge, the `mem_mb` docstring on `SandboxPolicy` ought to be
  widened again to call out that gliner-relex is now the first real
  workload exercising container-side memory enforcement on darwin
  (Slice 1 widened it once to "macOS supported via container backend
  when selected"; Slice 2.5 makes that real).
