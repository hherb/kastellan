# python-exec macOS micro-VM mode — design

**Date:** 2026-06-25
**Status:** approved (brainstorming)
**Phase:** 4 (`python-exec` arc continuation)
**Precedent:** gliner-relex Slice 2.5 (`2026-05-21-macos-container-slice-2-design.md`)

## Problem

`python-exec` runs **arbitrary agent-authored Python** — the highest-risk
worker in the system. On Linux (the DGX, primary host) it is contained by
bwrap namespaces + `WorkerStrict` seccomp + Landlock + a cgroup that enforces
`mem_mb`. On macOS it runs under Seatbelt, a **same-kernel** MAC policy that
**cannot enforce `mem_mb`** — so the manifest's `mem_mb: 512` is a silent
no-op there, and the isolation boundary is weaker than Linux's.

The `MacosContainer` `SandboxBackend` (Apple `container` /
`Virtualization.framework`, Linux guest) already exists and is consumed in
production by gliner-relex (Slice 2.5). It gives a **separate-kernel** micro-VM
boundary and enforces `mem_mb` via `-m <N>M` with SIGKILL on overrun.

## Goal

Add an **opt-in** path that runs `python-exec` under `MacosContainer` on macOS,
closing the `mem_mb` parity gap and giving arbitrary agent code a
separate-kernel boundary. Default behaviour is unchanged (Seatbelt host mode).

### Cross-platform framing (why this is not a macOS-only divergence)

Linux already has the stronger baseline (namespaces + seccomp + Landlock +
cgroup-enforced `mem_mb`). This slice brings macOS **up toward** that baseline;
it does not leave Linux weaker. A Linux micro-VM
(`SandboxBackendKind::FirecrackerVm` / Kata — already anticipated in the
`SandboxBackendKind` enum comment) is a separately-tracked future item, not a
counterpart missing from this slice. This mirrors how gliner-relex Slice 2.5
shipped container mode for macOS only while Linux stayed on bwrap.

## Key architectural fact

`python-exec`'s worker is a **Rust binary** (`kastellan-worker-python-exec`)
that speaks JSON-RPC over stdio and drives a CPython child
(`python3 -I -S -B -`). The Apple `container` runs a **Linux** guest, so a
macOS Mach-O worker binary cannot execute in it, and the host interpreter
cannot be bind-mounted and run either. Therefore **both** a Linux build of the
worker binary **and** a Python interpreter must live **inside the image**. This
is the only structural difference from gliner-relex, whose worker is a pure
Python console script.

## Approach (chosen)

Bake a Linux build of the worker binary + a Python interpreter into a container
image. The worker keeps speaking JSON-RPC over stdio **inside** the VM and
drives the in-image `python3`, preserving the full worker contract — capture
caps, the inline/scratch-file param channel, the handler, output secret-scrub.

Rejected alternative: run the in-image `python3` directly as the container's
`program` with no Rust worker. That discards `exec/mod.rs`, the JSON-RPC
protocol, the param channel, and the capture caps — it breaks the uniform
worker model. Not viable.

## Components

### 1. `workers/python-exec/Containerfile` (new)

Multi-stage, `python:3.12-slim` runtime (match gliner — glibc, proven):

```
FROM rust:1-slim AS builder
  # build the worker Linux-native inside the image build — sidesteps the
  # Mac cross-compile / `ring` C-dep problem entirely
  COPY . .
  RUN cargo build --release -p kastellan-worker-python-exec

FROM python:3.12-slim
  COPY --from=builder .../kastellan-worker-python-exec /usr/local/bin/
  USER nobody                       # defense-in-depth complement to --user nobody
  ENTRYPOINT ["kastellan-worker-python-exec"]
```

The image's own `/usr/local/bin/python3` is the interpreter. The build context
is the **workspace root** (the worker crate needs its workspace siblings:
`prelude`, `protocol`, etc.) — unlike gliner whose context is the worker dir.
A `.containerignore` keeps `target/`, `.git/`, worktrees out of the context.

**ENTRYPOINT vs. appended program (resolve in planning):** the `MacosContainer`
backend appends `binary` verbatim as the container's program/CMD. gliner sets
both an `ENTRYPOINT` and relies on this; under OCI semantics the appended path
would become an *arg* to the entrypoint. python-exec will resolve this cleanly
— most likely **no `ENTRYPOINT`**, letting the appended `binary`
(`/usr/local/bin/kastellan-worker-python-exec`) be the sole program — verified
during implementation against how `build_container_argv` actually invokes
`container run` and how gliner runs in practice.

### 2. `scripts/workers/python-exec/build-image.sh` (new)

Mirror `scripts/workers/gliner-relex/build-image.sh`: tag default
`kastellan/python-exec:dev`, override via `KASTELLAN_PYTHON_EXEC_IMAGE`,
fail-clear (exit non-zero) if `container` CLI is missing or its system service
is down. Builds from the workspace root with `-f workers/python-exec/Containerfile`.

### 3. `core/src/workers/python_exec.rs` (edit)

New env knobs (mirror gliner's `USE_CONTAINER` / `IMAGE`):

* `KASTELLAN_PYTHON_EXEC_USE_CONTAINER=1` — opt into container mode. **macOS
  only**; on Linux the resolver forces host mode (the `Container` variant is
  `#[cfg(target_os = "macos")]`, so the container branch is `cfg`-gated out and
  Linux never reaches it — same pattern that fixed gliner's #144 Linux build).
* `KASTELLAN_PYTHON_EXEC_IMAGE` — image tag override; default
  `kastellan/python-exec:dev`.

New `#[cfg(target_os = "macos")] container_mode_entry(...)`. It is **simpler**
than host mode:

* `sandbox_backend: Some(SandboxBackendKind::Container)`, `container_image: Some(image)`.
* `binary` = the in-image worker path (`/usr/local/bin/kastellan-worker-python-exec`).
  The `MacosContainer` backend appends `binary` verbatim as the in-container
  program (gliner Containerfile note #4).
* `env`: `KASTELLAN_PYTHON_EXEC_PYTHON = /usr/local/bin/python3` (in-image),
  plus `KASTELLAN_PYTHON_PARAMS_FILE_MAX` when the operator set it. **No**
  `KASTELLAN_LANDLOCK_RW` (Landlock is a Linux-prelude concept; inside the VM
  the boundary is the VM + `--read-only` + the RW scratch bind).
* `fs_read: []` — code arrives via stdin; no host interpreter to bind, no
  `interpreter_lib_dirs`, no stdlib bind (all in-image).
* `Net::Deny`, `Profile::WorkerStrict`, `cpu_ms: 10_000`, `mem_mb: 512`
  (**now enforced** — the payoff; 512 > the 200 MiB container floor),
  `cpu_quota_pct`/`tasks_max`: kept on the policy for parity though Apple
  `container` doesn't enforce them yet (same acknowledged gap as gliner).
* `wall_clock_ms: Some(30_000)`, `lifecycle: SingleUse`, `ephemeral_scratch: false`.

The resolver gains a `use_container` branch selecting `container_mode_entry`
vs the existing `python_exec_entry`, gated on macOS.

### 4. Writable scratch / param-file channel in the VM

**No host scratch bind, no `ephemeral_scratch`.** `build_container_argv`
already adds `--tmpfs /tmp` for the `WorkerStrict` profile (verified
2026-06-25, `macos_container.rs:209`), so even under `--read-only` root the
in-VM `/tmp` is a writable tmpfs — exactly mirroring Linux host mode (bwrap's
per-spawn `/tmp` tmpfs). The worker's `scratch_dir_from_env` falls back to
`SCRATCH_DIR = "/tmp"` when `KASTELLAN_WORKER_SCRATCH` is unset, so
`params.json` (>64 KiB params) and authored scratch writes land in the in-VM
tmpfs with **zero** new plumbing. Therefore container mode sets
`ephemeral_scratch: false` and does **not** set `KASTELLAN_WORKER_SCRATCH`
(the macOS host-dir mechanism is a Seatbelt-only need; the VM has its own
tmpfs). `tool_host` is untouched.

### 5. Lifecycle

Keep `SingleUse`. Freshness per call is the point for arbitrary code. Accept
the ~0.8 s container warm-spawn per call; document it in the manifest doc
comment (as gliner did). Warm/idle container lifecycle for python-exec is a
deferred follow-up.

## Data flow (container mode)

```
core dispatch ── python.exec {code, params}
  │  resolve → container_mode_entry (macOS, USE_CONTAINER=1)
  │  prepare_ephemeral_scratch → host scratch dir (bound RW into VM)
  ▼
MacosContainer.spawn_under_policy
  container run --rm -i --init --read-only --cap-drop ALL --user nobody
    --network none -m 512M --tmpfs /tmp          (writable in-VM tmpfs)
    <image> /usr/local/bin/kastellan-worker-python-exec
  ▼  (inside the Linux micro-VM)
worker (JSON-RPC over stdio) → python3 -I -S -B -  (code piped on stdin)
    params.json (>64 KiB) → /tmp/params.json on the in-VM tmpfs
  ▼
{exit_code, stdout, stderr, *_truncated} ── back over stdio ── core
  ▼  output secret-scrub + injection screen (unchanged, host-side in core)
```

## Testing (TDD)

**Pure units** (`core/src/workers/python_exec.rs` tests, no container):

* `container_mode_entry` shape: `sandbox_backend == Some(Container)`,
  `container_image == Some(expected)`, in-image `binary`, in-image
  `KASTELLAN_PYTHON_EXEC_PYTHON`, `Net::Deny`, `WorkerStrict`, `mem_mb == 512`,
  `fs_read == []`, `SingleUse`, `ephemeral_scratch == true`.
* `USE_CONTAINER` resolve truth-table: macOS on → container entry; macOS off /
  unset → host entry; image override honoured; default image when unset.
* Linux: container branch is `cfg`-gated out (compile-time guarantee, asserted
  by the existing `#[cfg]` pattern; a Linux test pins host mode).

**e2e** `core/tests/python_exec_container_e2e.rs` (`#[ignore]`, skip-as-pass
without `container` CLI / image / `container system status`, mirroring
`lifecycle_container_routing_e2e.rs`):

* print round-trip through the real VM (`python.exec` echoes a sentinel).
* memory-cap kill: a snippet allocating > 512 MiB is SIGKILLed by the VM
  (proves the parity payoff — this would NOT be enforced under Seatbelt).
* socket-attempt contained: `Net::Deny` + `--network none` blocks egress
  (parity with the host-mode `python_exec_e2e` net-deny assertion).

The image is built on this Mac via `build-image.sh` and the e2e run for real
before the PR.

## Out of scope (tracked, not built here)

* Linux micro-VM backend (`SandboxBackendKind::FirecrackerVm` / Kata).
* Curated read-only wheels dir for third-party packages.
* Warm/idle container lifecycle for python-exec (stays `SingleUse`).
* `cpu_quota_pct` / `tasks_max` enforcement (Apple `container` lacks the
  primitive; kept on the policy for forward parity).
* macOS supervised-deployment default: container mode stays opt-in; whether
  `core_service_spec` should carry `USE_CONTAINER=1` by default is an operator
  decision, deferred.

## Files touched

* `workers/python-exec/Containerfile` (new)
* `workers/python-exec/.containerignore` (new)
* `scripts/workers/python-exec/build-image.sh` (new)
* `core/src/workers/python_exec.rs` (edit: env knobs + `container_mode_entry` + resolver branch + tests)
* `core/tests/python_exec_container_e2e.rs` (new)
* docs: HANDOVER.md + ROADMAP.md at session end

(`tool_host` is **not** touched — container mode uses the in-VM `--tmpfs /tmp`,
no host scratch bind.)
