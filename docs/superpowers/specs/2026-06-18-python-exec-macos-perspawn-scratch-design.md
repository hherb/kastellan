# python-exec per-spawn writable scratch (macOS parity, #283)

**Date:** 2026-06-18
**Status:** Approved (design)
**Phase:** 4 (python-exec arc, slice #2 — writable scratch)
**Tracks:** [#283](https://github.com/hherb/kastellan/issues/283) (per-spawn macOS scratch)

## Problem

python-exec runs agent-authored Python under the strictest policy of any worker
(`Net::Deny`, `Profile::WorkerStrict`, `SingleUse`). Its scratch story is
asymmetric across platforms:

- **Linux (bwrap):** each spawn gets a fresh ephemeral `/tmp` tmpfs
  (`--tmpfs /tmp`, #89), made writable to the in-jail Landlock layer by the
  manifest's `KASTELLAN_LANDLOCK_RW=["/tmp"]`. Genuinely per-spawn isolated.
- **macOS (Seatbelt):** there is no tmpfs and the manifest sets `fs_write=[]`,
  so the Landlock-RW env is a no-op and **the worker has no writable scratch at
  all**. Agent Python that writes any temp file (`tempfile`, a scratch `.csv`,
  etc.) succeeds on Linux and fails on macOS.

This violates the hard cross-platform constraint (no Linux-only / macOS-only
behaviour without a counterpart of equivalent guarantee).

The worker side is already prepared: `exec.rs::run_code` sets the child's
`TMPDIR`/`HOME`/`cwd` to a scratch dir, and the `SCRATCH_DIR` doc comment
explicitly records that on macOS slice #1 "it exists but is not writable". The
only missing piece is the host granting a writable scratch on macOS.

A naive fix (`fs_write=["/tmp"]`, as browser-driver did) grants the **shared
host `/tmp`** — two invocations (or a concurrent browser-driver) can read each
other's leftover files. #283 tracks replacing that with a true per-spawn dir.
This design does the per-spawn fix for python-exec.

## Approach

A **reusable per-spawn-scratch mechanism at the `spawn_worker` chokepoint**,
opted into per-worker via a declarative `ToolEntry` flag. python-exec adopts it
now; browser-driver can adopt it later (closing #283 for both) without further
core changes. Rejected alternatives:

- **python-exec-only path** — would not generalise; the handover explicitly
  frames this as wiring browser-driver should share.
- **Creation inside the macOS `SandboxBackend`** — the backend returns a
  `Child` and is `dyn`-safe; making it own + RAII-clean a dir and inject a
  dynamic env var would require a trait change (forbidden by the invariants).
  Core's `spawn_worker` already owns the resulting `SupervisedWorker`, so the
  RAII guard belongs there (exactly where `EgressSidecar` already lives).
- **Field on `SandboxPolicy`** — `SandboxPolicy` lives in the `sandbox` crate;
  a flag that only core interprets (the backend never reads it) is worse
  layering than a flag on the core-owned `ToolEntry`/`WorkerSpec`.

## Components and seams

### 1. Opt-in flag (core types)

- `ToolEntry.ephemeral_scratch: bool` — new additive field, default `false`.
  Set `true` only on the python-exec manifest. Mirrors the existing additive
  per-worker fields (`sandbox_backend`, `lockdown_shim`).
- `WorkerSpec.ephemeral_scratch: bool` — sourced from the `ToolEntry` at each
  spec-construction site (the established pattern, same as `lockdown_shim`
  shaping `program`). Net-worker / test sites set `false`.

### 2. New `core/src/tool_host/scratch.rs` sibling

Keeps `tool_host.rs` (already 627 LOC, over the 500 cap) from growing — rule 4.
Contains:

- **`EphemeralScratch`** — RAII guard owning the created dir; `Drop` =
  best-effort `std::fs::remove_dir_all`. Mirrors `EgressSidecar.scratch`.
- **pure `scratch_subdir(root: &Path, pid: u32, seq: u64) -> PathBuf`** —
  builds `<root>/pyexec-<pid>-<seq>`. Unit-testable, no I/O.
- **pure `apply_scratch(policy: &mut SandboxPolicy, dir: &Path)`** — pushes
  `dir` onto `policy.fs_write` and sets the `KASTELLAN_WORKER_SCRATCH` env
  entry. Unit-testable, no I/O.
- a small `create_scratch(root) -> io::Result<EphemeralScratch>` that picks the
  pid + an atomic seq, `create_dir_all`s the subdir, and returns the guard.

Scratch **root** defaults to `std::env::temp_dir()` (per-user, private
`/var/folders/...` on macOS). Seatbelt's existing not-yet-created-path
canonicalization (`canonicalize_policy_paths`) already resolves the dir into the
real `(allow file-read* file-write* (subpath ...))` rule.

### 3. `spawn_worker` (macOS-gated only)

```text
let mut derived = derive_lockdown_env(spec.policy);   // already owned
#[cfg(target_os = "macos")]
let scratch = if spec.ephemeral_scratch {
    let s = scratch::create_scratch(&std::env::temp_dir())?;  // fail-closed
    scratch::apply_scratch(&mut derived, s.path());
    Some(s)
} else { None };
#[cfg(not(target_os = "macos"))]
let scratch = None;   // Linux already has the tmpfs; nothing to create
... spawn_under_policy(&derived, ...) ...
SupervisedWorker { client, _watchdog, egress: None, scratch }
```

- `SupervisedWorker` gains `scratch: Option<EphemeralScratch>`, declared **after**
  `egress` so the dir is removed last, after the worker's pipes have closed.
- The **Linux branch is untouched** → byte-identical to today.
- Fail-closed: a dir-create error aborts the spawn (`ToolHostError::Io`).

### 4. python-exec worker (`workers/python-exec/src/exec.rs`)

- New shared const `WORKER_SCRATCH_ENV = "KASTELLAN_WORKER_SCRATCH"` with a
  "keep in sync with core's `tool_host`" note (same convention as `PARAMS_ENV`).
- `run_code` resolves the scratch dir from `KASTELLAN_WORKER_SCRATCH`, falling
  back to the existing `SCRATCH_DIR` (`/tmp`) when unset. This value drives the
  child's `TMPDIR`/`HOME`/`cwd`. A pure helper
  `scratch_dir_from_env(lookup) -> String` makes the fallback unit-testable.
- Env unset on Linux → `/tmp` → **byte-identical**; set on macOS → per-spawn dir.

### 5. python-exec manifest (`core/src/workers/python_exec.rs`)

- `python_exec_entry` sets `ephemeral_scratch: true`.
- `fs_write` stays `[]` (the macOS dir is added at spawn; Linux keeps its
  `KASTELLAN_LANDLOCK_RW=["/tmp"]`).
- Refresh the manifest + `SCRATCH_DIR` doc comments to state macOS now gets a
  per-spawn writable scratch.

## Cross-platform guarantee

- **Linux:** zero behavioural change — `spawn_worker`'s scratch branch is
  `cfg`-gated to macOS, and the worker's env-unset fallback is `/tmp`.
- **macOS:** gains a per-spawn, per-worker-isolated, RAII-cleaned writable
  scratch — strictly stronger than browser-driver's shared `/tmp` (Seatbelt
  grants only the spawn's own subpath, so invocations cannot read each other).

## Testing (TDD)

- **core unit (`tool_host/scratch.rs` tests):**
  - `scratch_subdir` produces `<root>/pyexec-<pid>-<seq>` with distinct names
    for distinct seq.
  - `apply_scratch` appends the dir to `fs_write` and sets exactly one
    `KASTELLAN_WORKER_SCRATCH` env entry pointing at it.
  - `EphemeralScratch::Drop` removes a real temp dir (create under
    `std::env::temp_dir()`, drop, assert gone).
- **worker unit (`exec.rs` tests):** `scratch_dir_from_env` truth table — unset
  → `/tmp`; set → the value.
- **e2e (`core/tests/python_exec_e2e.rs`):** new test — agent Python writes a
  file under its temp dir and reads it back; asserts success **on macOS**
  (fails today) and Linux; host-side asserts the per-spawn dir is gone after the
  worker is dropped (macOS). Existing Linux `/tmp` scratch-write test stays
  green.
- **Regression:** full `python_exec_e2e` + `tool_host` units on macOS (live PG +
  real Seatbelt jail); DGX native-Linux re-run is **not required** — the change
  is macOS-gated and the Linux path is byte-identical (state this in the
  handover, carry the 1839/0/15 baseline forward).

## Out of scope (follow-ups)

- Browser-driver adopting `ephemeral_scratch` and dropping its
  `fs_write=["/tmp"]` — closes #283 fully.
- Unifying the Linux `KASTELLAN_LANDLOCK_RW=["/tmp"]` grant under the same flag.
- The `>64 KiB` scratch-file runtime-param channel (separate design; this
  unblocks it by giving the worker a host-writable scratch).
