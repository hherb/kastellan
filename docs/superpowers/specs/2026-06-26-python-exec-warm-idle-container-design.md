# python-exec warm/idle container lifecycle — design

**Date:** 2026-06-26
**Status:** approved design, pre-plan
**Phase:** 4 (`python-exec` arc continuation)
**Scope:** one session

## Problem

`python-exec` (the worker that runs arbitrary agent-authored Python) runs under
the macOS Apple-`container` micro-VM when the operator opts in with
`KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1` (shipped in PR #355). That path is
`Lifecycle::SingleUse`: **every** `python.exec` call boots a fresh micro-VM,
costing **~0.7–0.8 s** of warm-spawn latency per call. For a tight agent loop
that calls python-exec repeatedly, that boot cost dominates.

The host Seatbelt (macOS) and bwrap (Linux) paths spawn an ordinary cheap
process, so the boot cost is specific to the micro-VM. This design amortises it
by keeping the booted VM warm between calls, mirroring the existing
GLiNER-Relex idle-timeout lifecycle.

## Key finding that shapes the design

The python-exec worker process is a **persistent Rust JSON-RPC server**
(`serve_stdio` loops over requests — `workers/python-exec/src/main.rs`), and it
runs each `python.exec` request by **spawning a brand-new `python3` subprocess**
(`run_code` — `workers/python-exec/src/exec/mod.rs:280`). The untrusted agent
code therefore **already runs as a throwaway subprocess per call**, even today.

Consequences:

- Keeping the VM "warm" keeps only the **trusted** Rust JSON-RPC server + the
  booted VM kernel alive. It does **not** reuse a Python interpreter across
  calls — so there is no Python-level state leakage (imported modules,
  monkeypatches, leftover threads) of the kind that would make in-process reuse
  dangerous.
- The **one** cross-call state surface in a reused VM is the in-VM `/tmp`
  tmpfs — the worker writes `<scratch>/params.json` there and the agent's Python
  has `TMPDIR`/`HOME`/cwd pointed at it (`run_code`), so files an earlier call
  left behind would be visible to a later call, and would eat into the VM's
  `mem_mb: 512` headroom. A fresh `SingleUse` VM gets a pristine `/tmp` each
  time; warm reuse does not, unless we restore that.

This makes the GLiNER-style `IdleTimeout` reuse model genuinely viable here, in
contrast to a hypothetical in-process reuse.

## Decisions (settled in brainstorming)

1. **Isolation posture: wipe `/tmp` between reused calls.** The worker clears
   its scratch-dir contents at the start of every `python.exec` call, before
   writing the new `params.json`. This restores pristine-`/tmp` parity with
   `SingleUse` for every call — the agent's Python sees no prior call's
   leftovers, and the VM's memory headroom is reset each call. Cost is a few
   milliseconds of directory cleanup.

2. **Scope: container (micro-VM) mode only.** Only the
   `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1` path gets the warm lifecycle, because
   that is where the ~0.7 s boot cost lives. The host Seatbelt and Linux bwrap
   paths stay `SingleUse` (cheap spawn; simplest per-spawn isolation
   semantics unchanged).

3. **Enablement: a dedicated opt-in knob, default off.** A single env var
   `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS` both enables and tunes the feature:
   `> 0` → warm reuse with that idle window; `0`/unset/unparseable → today's
   `SingleUse` behaviour. Layered on top of container mode (only consulted when
   `USE_CONTAINER=1`). This matches kastellan's conservative opt-in slice posture
   and lets the operator own the RAM-vs-latency tradeoff (a warm VM holds
   ~512 MiB idle for the idle window).

4. **Caps mirror GLiNER, overridable:** `max_requests` default `10_000`
   (`KASTELLAN_PYTHON_EXEC_MAX_REQUESTS`), `max_age_seconds` default `86_400`
   (`KASTELLAN_PYTHON_EXEC_MAX_AGE_SECONDS`), `grace_period_seconds` fixed `5`.
   The `Contract { stateless: true }` holds (fresh subprocess + wiped `/tmp`).

## Approach (chosen)

**Reuse the existing `IdleTimeout` lifecycle machinery.** The
`CompositeLifecycle` dispatcher already routes by `entry.lifecycle`
(`core/src/worker_lifecycle/composite.rs`), and `WarmRegistry` / `ToolSlot` /
idle-teardown / cap-evaluation / restart-backoff all exist and are exercised in
production by GLiNER-Relex. **No new lifecycle machinery is needed.** The change
is to (a) make python-exec's container entry declare `IdleTimeout` when the
operator opts in, (b) wipe the worker's scratch between calls, and (c) test it.

Rejected alternatives:

- **Pre-warm standby pool** (keep a booted-but-unused VM ready; consume one per
  call; never reuse a VM). Marginally stronger isolation (zero reuse), but it is
  net-new mechanism and far more code, and the extra isolation is redundant now
  that agent code is already subprocess-isolated *and* `/tmp` is wiped per call.
- **Worker-internal warm management.** Reinvents the registry; does not fit the
  `WorkerLifecycleManager` seam.

## Components & changes

### 1. Worker-side `/tmp` wipe (`workers/python-exec/src/exec/mod.rs`)

New pure helper, called at the top of `run_code` before `write_params_file`:

```rust
/// Remove the *contents* of the scratch dir (not the dir itself) so each
/// `python.exec` call starts from a pristine working area, restoring
/// SingleUse-parity isolation when the worker is reused under the
/// idle-timeout lifecycle. Idempotent: on a fresh VM the dir is already
/// empty, so this is a no-op. Best-effort per entry (a removal error on one
/// stale file does not abort the run — the params write below is the
/// fail-closed gate); returns the count removed for observability/tests.
pub fn wipe_scratch_contents(dir: &Path) -> std::io::Result<usize>
```

Placement: `run_code` resolves `scratch` (line 286) → **wipe** → write the new
`params.json` (line 292). Because the wipe runs in `run_code`, which is the
single chokepoint shared by host and container paths, it is lifecycle-agnostic
and harmless on the cheap paths (their per-spawn scratch is already empty).

The worker runs as `nobody` in the VM and owns the files it and its child
wrote (same uid), so it can always remove them. We remove directory *entries*
(files + subdirs) but keep the scratch directory itself (it is the mount point /
`TMPDIR`).

### 2. Container entry declares the lifecycle (`core/src/workers/python_exec.rs`)

- New pure helper (mirrors GLiNER's `build_idle_timeout_lifecycle`):

  ```rust
  /// Build the python-exec container lifecycle from the parsed idle window.
  /// `None` (or `Some(0)`) → SingleUse (today's behaviour). `Some(n)` with
  /// n > 0 → IdleTimeout with the given window and the request/age caps.
  fn container_lifecycle(idle_seconds: Option<u64>, max_requests: u64,
                         max_age_seconds: u64) -> Lifecycle
  ```

- New env constants: `IDLE_SECONDS_ENV = "KASTELLAN_PYTHON_EXEC_IDLE_SECONDS"`,
  `MAX_REQUESTS_ENV = "KASTELLAN_PYTHON_EXEC_MAX_REQUESTS"`,
  `MAX_AGE_SECONDS_ENV = "KASTELLAN_PYTHON_EXEC_MAX_AGE_SECONDS"`.

- A pure parse helper `parse_idle_caps(get_env) -> (Option<u64>, u64, u64)` that
  reads + parses the three vars (default `max_requests = 10_000`,
  `max_age_seconds = 86_400`; an unparseable/empty `IDLE_SECONDS` → `None` =
  SingleUse, fail-safe to the conservative default).

- `container_mode_entry` gains a `lifecycle: Lifecycle` parameter (built by the
  caller) instead of the hardcoded `Lifecycle::SingleUse` at line 248. All other
  fields are unchanged (`ephemeral_scratch: false`, `sandbox_backend:
  Some(Container)`, `mem_mb: 512`, …).

- The resolver (`resolve`, around line 346) parses the caps from env and passes
  the constructed lifecycle into `container_mode_entry`.

The whole container path stays `#[cfg(target_os = "macos")]`-gated (issue #144);
Linux never reads these vars.

### 3. Tests

**Pure unit tests (no VM):**

- `wipe_scratch_contents`: removes files + nested dirs, keeps the dir itself,
  no-op on an empty dir, returns the right count. (Use a `tempfile` dir.)
- `parse_idle_caps`: unset → `(None, 10_000, 86_400)`; `IDLE_SECONDS=120` →
  `(Some(120), …)`; `IDLE_SECONDS=0` → `None`; garbage → `None`; custom
  `MAX_REQUESTS`/`MAX_AGE_SECONDS` parsed; empty strings → defaults.
- `container_lifecycle`: `None`/`Some(0)` → `SingleUse`; `Some(n>0)` →
  `IdleTimeout` carrying the right caps + `stateless: true`. (Mirrors GLiNER's
  `entry_carries_idle_timeout_lifecycle_with_spec_caps`.)
- A resolver test: with `USE_CONTAINER=1` + `IDLE_SECONDS=120` the registered
  entry's lifecycle is `IdleTimeout`; without `IDLE_SECONDS` it stays
  `SingleUse`.

**Integration e2e (real micro-VM, macOS, `#[ignore]` or skip-as-pass gated like
the existing container e2e):** mirror `worker_lifecycle_idle_timeout_e2e.rs`
against the real python-exec image —

- **warm reuse**: wrap the Container backend in a spawn-counter; three
  acquire→dispatch(`print(6*7)`)→release cycles spawn the VM **once** and all
  return `42`.
- **/tmp wipe across reuse**: call 1 writes a sentinel file under `/tmp`
  (`open('/tmp/leak','w')`); call 2 on the **same warm VM** asserts the sentinel
  is **gone** (`os.path.exists('/tmp/leak') == False`) — the load-bearing
  isolation guarantee. (Jointly non-vacuous with a negative-control note: without
  the wipe, call 2 would see it.)
- **idle teardown**: after `idle_seconds` with no call, the warm slot clears
  (re-uses the lifecycle's `_test_slot_has_warm` inspector).

These reuse `tests-common` daemon/backend helpers and the existing
`build-image.sh`-built image (`kastellan/python-exec:dev`).

## Security analysis

- The threat-model invariant is unchanged. The micro-VM separate-kernel boundary
  + `--read-only --cap-drop ALL --user nobody --network none --tmpfs /tmp -m
  512M` flags are identical whether the VM is `SingleUse` or warm. Reuse does not
  widen any grant.
- Cross-call isolation is preserved by (a) the agent's Python already being a
  fresh subprocess per call and (b) the per-call `/tmp` wipe restoring a pristine
  working area. The `Contract { stateless: true }` assertion is therefore honest.
- `mem_mb: 512` enforcement is preserved per call: the wipe frees the prior
  call's tmpfs usage, so each call gets the full headroom (the existing
  `MemoryError`-at-900 MiB e2e behaviour is unaffected).
- `max_requests` / `max_age_seconds` rotation provides slow-leak hygiene against
  any state we did not anticipate (e.g. a future in-VM cache): the VM is torn
  down and rebooted periodically even under sustained load.
- Default **off**: absent `KASTELLAN_PYTHON_EXEC_IDLE_SECONDS`, behaviour is
  byte-identical to today's `SingleUse` container mode.

## Out of scope / follow-ups

- **Linux micro-VM warm lifecycle** — there is no Linux micro-VM backend yet
  (`FirecrackerVm`/Kata is a separate multi-session arc); python-exec on Linux
  stays bwrap `SingleUse`. The `IdleSeconds` knob simply has no effect there.
- **Host-path (Seatbelt/bwrap) warm reuse** — deliberately excluded; the spawn is
  already cheap.
- **Curated-wheels RO dir** — unrelated Phase-4 pick.

## Verification plan

- macOS, Apple `container`: pure units green; container warm-reuse + `/tmp`-wipe
  e2e green (real VM, image rebuilt if needed via `build-image.sh`); confirm a
  spawn-counter shows 1 boot for N calls.
- `cargo clippy --workspace --all-targets -D warnings` clean (incl. the macOS
  container cfg).
- Linux/DGX not required: the change is macOS-only mechanism (container-gated) +
  a pure worker helper that is a no-op on the bwrap path. A DGX workspace build
  confirms the worker helper compiles and the bwrap path is unaffected, but no
  new Linux behaviour is introduced.
