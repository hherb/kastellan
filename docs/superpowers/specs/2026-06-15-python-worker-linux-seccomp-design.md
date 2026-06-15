# Linux seccomp for pure-Python workers (#281) — browser-driver first

**Date:** 2026-06-15
**Issue:** [#281](https://github.com/hherb/kastellan/issues/281)
**Status:** design approved, pre-implementation

## Problem

The Linux syscall-filter (seccomp) and Landlock layers are installed by the
**Rust** `kastellan-worker-prelude` (`lock_down` / `apply_from_env`), which only
**Rust** workers run. **Pure-Python** workers — `gliner-relex` and
`browser-driver` — are plain venv console scripts that `linux_bwrap` spawns
**directly**, and bwrap does **not** pass `--seccomp`. So on Linux these workers
run with **no seccomp and no Landlock**, contained only by bwrap (namespaces + fs
binds + private/loopback netns) + cgroup + `RLIMIT_CPU`.

The concrete consequence that motivated the issue: `Profile::WorkerBrowserClient`'s
**seccomp** half (the `browser_client` profile + the io_uring→EPERM two-filter) is
built and unit/smoke-tested in the prelude but is **not actually applied** to
`browser-driver` on Linux. Its Seatbelt half *is* applied on macOS.

This is a pre-existing condition; `browser-driver` makes it visible because it is
the first chatty, large-attack-surface Python worker.

## Scope (this session)

- **browser-driver only.** Build the shim **generically** so `gliner-relex` (and
  future Python workers) reuse it later without further infrastructure.
- **seccomp-only for browser-driver now.** Apply the `browser_client` seccomp
  profile (the actual missing layer). Landlock for browser-driver is deferred —
  it needs a Chromium-validated read-only path set and is the riskier half. bwrap
  mounts remain browser-driver's filesystem-containment layer in the meantime.
- gliner-relex wiring and browser-driver Landlock are **explicit follow-ups**.

## Approach decision

Three options were on the table (from the issue):

1. **Rust exec-shim** that runs `prelude::lock_down` then `execve`s the venv
   interpreter — the child inherits the filter (filters survive `execve` under
   `PR_SET_NO_NEW_PRIVS`, which `lock_down` already sets). **CHOSEN.**
2. **bwrap `--seccomp <bpf>`** generated host-side. **Rejected:** `--seccomp`
   takes a single fd, but `browser_client` needs **two** filters (the main
   kill-filter plus the io_uring→EPERM filter); and this path applies seccomp
   only, never Landlock — so the shim is strictly more capable.
3. **Accept bwrap-only and document.** Rejected for browser-driver — the issue
   exists precisely because we want the built-and-tested seccomp profile actually
   enforced on the highest-attack-surface worker.

The exec-shim is already proven in-tree: the prelude's `lockdown-probe
exec-after-lockdown <bin> <args>` test fixture does exactly this, and
`coreutils_smoke` relies on the inheritance. python-exec relies on the same
child-inheritance for its CPython child. The shim is the production form of that
fixture.

## Design

### 1. The shim binary — `kastellan-worker-lockdown-exec`

A new production binary in the `kastellan-worker-prelude` crate (sibling to the
existing `kastellan-lockdown-probe` test fixture; add a second `[[bin]]`).

```
kastellan-worker-lockdown-exec <target-binary> [<target-args>…]
  1. rlimit::apply_from_env()    # RLIMIT_CPU from KASTELLAN_CPU_MS — matches serve_stdio order
  2. lock_down()                 # Landlock (env-gated, §2) + seccomp from KASTELLAN_SECCOMP_PROFILE
  3. execve(target, [target, …args])   # child inherits the seccomp filter under NO_NEW_PRIVS
```

- Reads the **exact env `derive_lockdown_env` already injects** for every worker
  (`KASTELLAN_SECCOMP_PROFILE`, `KASTELLAN_CPU_MS`, `KASTELLAN_LANDLOCK_RW/RO`).
  No new env plumbing on the host side for these.
- Fail-closed: a missing target arg, an `rlimit`/`lock_down` error, or an
  `execve` failure exits non-zero with a distinct code and a stderr line (mirrors
  the probe's exit-code discipline). A successful `execve` never returns.
- Order matches `serve_stdio`: rlimit before lock_down (so the CPU ceiling is
  armed before any seccomp restriction on the prlimit family).
- **rlimit is deliberately included** (faithful prelude wrapper). This newly
  enforces `RLIMIT_CPU=30s` worker-side for browser-driver — a containment win,
  but a behavior change beyond pure seccomp, so it is a DGX validation point (a
  heavy render must finish within the 30 CPU-second budget; raise `cpu_ms` if
  not).
- On non-Linux the shim is not inserted at all (macOS = Seatbelt from the
  parent). `lock_down` is already a no-op off-Linux, but we keep the shim out of
  the macOS spawn path entirely so macOS behavior is byte-identical.

### 2. Landlock disable signal (additive)

So that "seccomp-only" is expressible without ripping Landlock out of the shared
path, `landlock_lock::apply_from_env` learns one new env var:

- **`KASTELLAN_LANDLOCK_PROFILE`**
  - `"none"` → skip the ruleset entirely; return a new
    `LandlockReport::Disabled`.
  - unset / any other value → **unchanged** behavior (existing workers stay
    byte-identical — they never set this var).

`LandlockReport` gains a `Disabled` variant; `lock_down` and `serve_stdio` thread
it through. browser-driver's manifest sets `KASTELLAN_LANDLOCK_PROFILE=none` in
`policy.env`, so it gets seccomp + rlimit worker-side but no Landlock. This same
var is the seam gliner-relex / future workers flip back **on** (by simply not
setting it).

### 3. Wiring — `ToolEntry` + spawn chokepoint

- **`ToolEntry` gains `lockdown_shim: Option<PathBuf>`.**
  - `None` (default) → spawn the binary directly. **Every existing Rust worker
    stays `None`** → byte-identical behavior. ~10 mechanical
    `lockdown_shim: None` additions at existing `ToolEntry { … }` constructors
    (web_fetch, shell_exec, web_search, python_exec, gliner_relex host+container,
    sandbox_health, browser_driver, and the two test fixtures in
    `worker_lifecycle/composite.rs`).
  - `Some(shim)` → wrap: spawn `shim` with the real binary as its first argv.
- **New pure helper** `build_program_and_args(binary: &Path, shim:
  Option<&Path>, base_args: &[&str]) -> (String, Vec<String>)`. Returns owned
  values the caller borrows into `WorkerSpec` (the existing
  `to_string_lossy().into_owned()` pattern). Used at **both** spawn sites:
  - `worker_lifecycle/manager.rs` `SingleUseLifecycle::acquire` (browser-driver
    is `SingleUse`).
  - `worker_lifecycle/idle_timeout.rs` (future gliner-relex is `IdleTimeout`).
  - Unit-testable with no process spawn.
- **`browser_driver.rs` manifest** (`resolve` / `browser_driver_entry`):
  - On **Linux only**, discover the shim via `discover_binary(ctx,
    "KASTELLAN_LOCKDOWN_EXEC_BIN", "kastellan-worker-lockdown-exec")` (same
    override / exe-relative-sibling / fail-closed semantics as worker discovery).
    Set `lockdown_shim = Some(shim)` and add `("KASTELLAN_LANDLOCK_PROFILE",
    "none")` to `policy.env`.
  - **Fail-closed:** Linux + shim not found → `Resolution::Misconfigured` (do not
    register an unfilterable browser). The shim is a normal workspace binary
    always built beside `kastellan`, so this only fires on a genuine packaging
    bug — exactly when we want a loud failure.
  - On **macOS**, `lockdown_shim = None`; Seatbelt path unchanged.

### 4. Tests & verification

- **Pure unit:**
  - `build_program_and_args` — `None` (direct) vs `Some(shim)` (wrapped, real
    binary becomes argv[0] of the target list).
  - `landlock_lock`: `KASTELLAN_LANDLOCK_PROFILE=none` → `Disabled`; unset →
    existing behavior.
  - `browser_driver` manifest: `lockdown_shim` set on Linux / `None` on macOS;
    `KASTELLAN_LANDLOCK_PROFILE=none` present in the entry's env; Linux +
    missing-shim → `Misconfigured`.
- **prelude Linux integration smoke** (new, `cfg(target_os = "linux")`): spawn
  `kastellan-worker-lockdown-exec` with a target that, **after exec** (no
  re-lockdown), attempts a banned syscall → SIGSYS (proves inheritance), and an
  allowed syscall survives. Mirrors the existing `seccomp_smoke` style.
- **The real gate (DGX, Linux, bwrap + staged venv):** `browser_driver_e2e
  --ignored` renders a page with the `browser_client` seccomp filter **actually
  active**. This is the first time that profile meets real Chromium on Linux.
  Expected iteration: a SIGSYS means the profile misses a syscall Chromium needs
  → expand the `browser_client` allowlist in `seccomp_lock.rs` and re-run.
- **macOS:** compile + unit tests only (the feature is Linux-effective); no Mac
  behavior change. Cross-clippy the Linux-gated prelude/core paths where feasible
  (pure-Rust crates only — `core` needs the DGX).

### 5. Out of scope (explicit follow-ups, keep #281 open)

- **Landlock for browser-driver** — needs a Chromium-validated read-only path set
  (`/sys` probing, the force-routed `proxy_uds` path under Landlock, fonts).
  Flip `KASTELLAN_LANDLOCK_PROFILE` off and validate the RO set on the DGX.
- **gliner-relex wiring** — reuses this shim; needs its own seccomp-profile
  choice (torch/ML surface) and DGX validation that the filter doesn't break the
  model load. Heavy ML worker → separate session.

## Files touched

New:
- `workers/prelude/src/bin/lockdown_exec.rs` — the shim (~60 LOC).
- prelude Linux integration smoke test (new test file under `workers/prelude/tests/`).

Modified:
- `workers/prelude/Cargo.toml` — second `[[bin]]`.
- `workers/prelude/src/landlock_lock.rs` — `KASTELLAN_LANDLOCK_PROFILE=none`
  handling.
- `workers/prelude/src/lib.rs` — `LandlockReport::Disabled` variant; thread it
  through `lock_down` / `serve_stdio`.
- `core/src/scheduler/tool_dispatch.rs` — `ToolEntry.lockdown_shim` field.
- all `ToolEntry { … }` constructor sites — `lockdown_shim: None`.
- `core/src/worker_lifecycle/manager.rs` + `idle_timeout.rs` — call the new pure
  helper.
- new pure helper (placement: `core/src/tool_host.rs` or a small sibling) +
  the `KASTELLAN_LOCKDOWN_EXEC_BIN` override const.
- `core/src/workers/browser_driver.rs` — shim discovery + `lockdown_shim` +
  `KASTELLAN_LANDLOCK_PROFILE=none`; fail-closed on missing shim.

## Invariants preserved

- **Every worker is sandboxed before it runs.** The shim *strengthens* this —
  browser-driver gains the worker-side seccomp layer it was missing. No
  "spawn unsandboxed" escape hatch is added.
- **Cross-platform parity.** Linux gains the shim; macOS keeps Seatbelt (which
  already applies the equivalent profile from the parent). No OS regresses.
- **Default-`None` field** → all existing workers byte-identical; the only
  runtime behavior change is browser-driver on Linux.
- **Fail-closed** posture matches the rest of the daemon (force-routing, worker
  discovery).
