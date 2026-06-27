# Firecracker micro-VM: forward `policy.env` into the guest (#360)

**Date:** 2026-06-27
**Issue:** [#360](https://github.com/hherb/kastellan/issues/360)
**Status:** design approved; follow-up to Firecracker micro-VM slice 1 (PR #364).

## Problem

Slice 1 threads worker env (`KASTELLAN_PYTHON_PARAMS_FILE_MAX`, `KASTELLAN_MICROVM_DIR`,
`KASTELLAN_PYTHON_EXEC_PYTHON`) through `firecracker_mode_entry → policy.env →
build_launch_plan → FirecrackerLaunchPlan.env`, but **`plan.env` is never rendered
into the Firecracker config and never reaches the guest**. `render_firecracker_config`
ignores it; the launcher only bridges stdio; and `kastellan-microvm-init` bakes a
fixed env and `exec`s the worker without forwarding policy env.

Consequence: in micro-VM mode the operator `params_file_max` override is **inert** —
the worker falls back to `PARAMS_FILE_MAX_DEFAULT` (1 MiB). Safe-by-default, but the
threading + the `firecracker_mode_entry_forwards_params_file_max_only_when_set` test
prove a property that does not hold end-to-end.

### Latent bug surfaced while tracing this

`firecracker_mode_entry` sets `KASTELLAN_PYTHON_EXEC_PYTHON=/usr/local/bin/python3`,
but the guest `microvm-init` bakes `/usr/bin/python3` (the rootfs reality — the
stdlib is copied at the native `/usr` prefix; the e2e `print(6*7)→42` runs against
`/usr/bin/python3`). Today this is harmless *only because env is never forwarded*, so
the baked value wins. The moment env forwards, the host's wrong value would override
the correct one and break the worker. #360 must reconcile this.

## Constraints

- The python-exec worker is `Net::Deny` → the guest has **no NIC**, so Firecracker's
  MMDS metadata service is unavailable.
- The rootfs is attached **read-only and shared** across all spawns (slice-1 security
  invariant) → no per-spawn writable rootfs channel.
- `microvm-init` must stay **`libc`-only** (it must not depend on the sandbox crate;
  `WORKER_VSOCK_PORT` is already a manually-kept-in-sync constant across the boundary).

## Decision: kernel cmdline transport, generic forwarding, fail-safe guest

### Transport — kernel cmdline (hex token)

The launcher boots Firecracker with `--no-api --config-file`, so
`boot-source.boot_args` (the kernel cmdline) is the one host-controlled channel that
reaches a NIC-less guest. The host appends a single token; the guest reads
`/proc/cmdline`.

Encoding: the env block `K1=V1\nK2=V2\n…` (UTF-8) is **hex-encoded** and appended as
`" kastellan.env=<hex>"`. Hex (`[0-9a-f]`) is whitespace/quote/`=`-safe for any value
(env values may contain `[`, `"`, `/`, `=`, even spaces in the generic case), and is
trivial to hand-roll dependency-free on both sides. Base64 was rejected: it needs a
codec in the `libc`-only guest and the env is tiny, so hex's 2× size is irrelevant.

The hex codec is a **manually-kept-in-sync pair** (host encode in `sandbox`, guest
decode in `microvm-init`) — same pattern as `WORKER_VSOCK_PORT`. Roundtrip unit tests
pin the scheme on both sides.

### Scope — forward all of `plan.env` generically

The sandbox crate forwards **every** `plan.env` pair with no per-worker key knowledge
(matches the "any worker can opt in" backend goal). Correctness of the forwarded
values is the **entry's** responsibility:

- Fix `firecracker_mode_entry` to set `KASTELLAN_PYTHON_EXEC_PYTHON=/usr/bin/python3`
  (the rootfs reality), so the now-live value is guest-valid.
- `KASTELLAN_MICROVM_DIR` also forwards; it is meaningless in-guest but harmless (the
  worker never reads it).

### Guest fail-mode — fail-safe baked fallback

`microvm-init` applies its baked defaults **first**, then overlays the forwarded env
(forwarded overrides). An absent or undecodable `kastellan.env` token leaves the baked
defaults in place and still boots a working worker — a transient cmdline glitch never
bricks the VM; worst case a knob silently stays at its default. This matches the
existing safe-by-default posture.

### Length cap — fail closed in `build_launch_plan`

`build_launch_plan` (already fallible) enforces a conservative cap: if
`BASE_BOOT_ARGS + encoded-env` exceeds `MAX_CMDLINE_BYTES` (1024, well under arm64's
2048-byte `COMMAND_LINE_SIZE`), it returns `SandboxError`. The slice-1 env is ~3 small
vars (≈120 hex chars), so the cap only ever trips on a pathological policy.

## Components (pure + unit-testable)

| Fn | Crate / file | Purpose |
| --- | --- | --- |
| `encode_env_cmdline(&[(String,String)]) -> Option<String>` | `sandbox/src/linux_firecracker/plan.rs` | Hex-encode `\n`-joined `k=v`; `None` when env empty. Returns the `" kastellan.env=<hex>"` suffix. |
| `build_launch_plan` (extended) | same | Compute `boot_args = BASE + encode_env_cmdline(...)`; fail closed over `MAX_CMDLINE_BYTES`. |
| `parse_env_cmdline(&str) -> Vec<(String,String)>` | `workers/microvm-init/src/main.rs` | Find the `kastellan.env=` token in `/proc/cmdline`, hex-decode, split into pairs. Pure → testable on any platform. |
| guest `main` (Linux, extended) | same | Read `/proc/cmdline` → `parse_env_cmdline` → `set_var` each (after the baked fallback) → `exec_worker`. |
| `firecracker_mode_entry` (one-line fix) | `core/src/workers/python_exec/entries.rs` | PYTHON value `/usr/local/bin/python3` → `/usr/bin/python3`; update the stale "provisioning-only" NOTE comment (env IS forwarded now). |

`render_firecracker_config` is **unchanged** — it already renders `plan.boot_args`,
which now carries the env suffix.

## Testing

- **Pure unit (sandbox):** `encode_env_cmdline` shape, empty→`None`; `build_launch_plan`
  over-cap → `Err`; `boot_args` contains the token when env non-empty.
- **Pure unit (microvm-init):** `parse_env_cmdline` extracts pairs; missing token →
  empty; malformed hex → empty (fail-safe); a host-encode→guest-decode roundtrip
  pinned in **both** crates against the same fixture bytes.
- **DGX e2e** (`core/tests/python_exec_firecracker_e2e.rs`, `#[ignore]`): a forwarded-env
  **differential** — set `KASTELLAN_PYTHON_PARAMS_FILE_MAX` to a small ceiling and prove
  an over-ceiling param is now **rejected in-guest** (the default 1 MiB would accept it),
  demonstrating the override is live end-to-end. `[SKIP]`s via `probe()` like the other
  scenarios.

## Out of scope

- Per-spawn run-dir cleanup (#362) and the `python_exec.rs` split (#363 — done) are
  separate follow-ups.
- Net workers (a 2nd vsock / NIC), block-device fs-sharing, and the jailer are
  slices 2–5.

## Task breakdown (TDD)

1. `sandbox`: `encode_env_cmdline` + `build_launch_plan` cap, RED→GREEN units.
2. `microvm-init`: `parse_env_cmdline` + roundtrip, RED→GREEN units; wire into guest `main`.
3. `core`: `firecracker_mode_entry` PYTHON fix + comment; pin via the existing
   `firecracker_mode_entry_*` units (value assertion update).
4. DGX: build + clippy `--all-targets -D warnings` + the differential e2e; macOS clippy
   for the cross-platform-compiling pieces.
