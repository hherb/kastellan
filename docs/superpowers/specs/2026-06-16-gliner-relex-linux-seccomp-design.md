# gliner-relex Linux seccomp via the `ml_client` profile

**Date:** 2026-06-16
**Status:** Design — approved, pending spec review
**Issue:** [#281](https://github.com/hherb/kastellan/issues/281) (the gliner-relex half; browser-driver half shipped in PR #292 / `80de534`)
**Branch (planned):** `feat/281-gliner-relex-seccomp`

---

## Problem

gliner-relex is a heavy torch/transformers inference worker. On Linux it runs in
**host mode**: a pure-Python venv console-script that `linux_bwrap` spawns
**directly**. Because bwrap execs the venv shim rather than a Rust binary, the
worker **never runs the `kastellan-worker-prelude`** — so the seccomp filter its
manifest nominally selects (`Profile::WorkerStrict` → `KASTELLAN_SECCOMP_PROFILE=strict`)
is **never actually installed**. On Linux the worker today runs with **no
worker-side seccomp filter at all**. (macOS is unaffected: Seatbelt is applied
from the parent, independent of the prelude.)

This is the same gap that #281 closed for browser-driver. The fix infrastructure
— the `kastellan-worker-lockdown-exec` shim — already exists and is proven on the
DGX. This slice reuses it for gliner-relex.

## Goal

On Linux, route gliner-relex's spawn through the lockdown-exec shim so a real
seccomp filter is installed and inherited by the torch worker, **without breaking
model load or inference**. Land a dedicated `ml_client` seccomp profile sized for
torch/transformers, enumerated empirically on the DGX.

**Non-goals (deferred):**
- Worker-side **Landlock** for gliner-relex (seccomp-only this slice, mirroring
  browser-driver's #281 posture; bwrap's mount namespace remains the FS boundary).
  Filed as a tracked follow-up.
- Any change to gliner-relex's lifecycle, memory caps, `Net::Deny` posture, or the
  macOS Seatbelt / Apple-`container` paths (must stay byte-identical).

---

## Design

### 1. Mechanism — reuse the lockdown-exec shim (mirror browser-driver #281)

The shim (`workers/prelude/src/bin/lockdown_exec.rs`) applies the prelude lockdown
from env (`rlimit::apply_from_env()` → `lock_down()`) then `execve`s its target.
seccomp filters survive `execve` under `PR_SET_NO_NEW_PRIVS` (which `lock_down`
sets), so the torch worker inherits the filter. The host already injects the exact
env the shim reads (`KASTELLAN_SECCOMP_PROFILE`, `KASTELLAN_CPU_MS`,
`KASTELLAN_LANDLOCK_*`) via `tool_host::derive_lockdown_env` — **no new host-side
plumbing**. `tool_host::build_program_and_args` already wraps the spawn through a
shim when `ToolEntry.lockdown_shim.is_some()`.

### 2. New `ml_client` profile — two layers, one name

**Sandbox layer** (`sandbox/src/lib.rs` + per-OS backends):
- Add `Profile::WorkerMlClient`.
- `macos_container.rs`: group it with `Profile::WorkerStrict` (read-only root,
  `--cap-drop ALL --user nobody`) — gliner is `Net::Deny`.
- `macos_seatbelt.rs`: no new arm needed — the builder only special-cases
  `WorkerBrowserClient` via `matches!`; every other profile (incl. the new one)
  renders via the strict path. **macOS Seatbelt output is byte-identical to
  today's `WorkerStrict`.**
- bwrap (`linux_bwrap.rs`) does not match on `Profile` for seccomp — the prelude
  does, via the env var — so no bwrap change.

**Seccomp layer** (`workers/prelude/src/seccomp_lock.rs`):
- Add `Profile::MlClient` + `Profile::parse("ml_client")`.
- Add `pub const ML_CLIENT_ADDITIONS: &[i64]` — the torch/transformers-specific
  syscalls beyond `net_client`, **enumerated empirically on the DGX** (§4).
- `allow_list_for(MlClient)` = `BASE_ALLOW` (+ `BASE_ALLOW_X86_64_LEGACY` on x86_64)
  + `NET_CLIENT_ADDITIONS` (torch creates sockets even fully offline; `Net::Deny`
  still blocks the actual route at the netns layer) + `ML_CLIENT_ADDITIONS`.
- **io_uring:** if torch probes `io_uring_setup`/`io_uring_enter`, reuse the
  browser_client EPERM-downgrade pattern (`build_io_uring_eperm_bpf` + install the
  permissive filter first) rather than plain-allowing it — io_uring is a known
  sandbox-escape primitive. Resolve empirically; only wire if the trace shows it.

**Mapping** (`core/src/tool_host/lockdown_env.rs`): the exhaustive match gains
`Profile::WorkerMlClient => "ml_client"` (compiler-enforced — adding the variant
forces this edit).

### 3. Manifest wiring (`core/src/workers/gliner_relex/`)

`entry.rs::host_mode_entry`:
- Flip `policy.profile` from `Profile::WorkerStrict` to `Profile::WorkerMlClient`.
- Add a `lockdown_shim: Option<PathBuf>` parameter (mirrors
  `browser_driver_entry`). On Linux, when `Some`:
  - bind the shim into `fs_read` (it lives in `target/debug/` in dev, outside the
    base `/usr` bind);
  - push `KASTELLAN_LANDLOCK_PROFILE=none` into `policy.env` (seccomp-only).
- Container-mode (`container_mode_entry`) and the macOS host path pass `None` —
  unchanged.

`manifest.rs::resolve()`:
- On Linux, after a successful `resolve_env`, `discover_binary(ctx,
  "KASTELLAN_LOCKDOWN_EXEC_BIN", "kastellan-worker-lockdown-exec")`. `Some(shim)`
  ⇒ `Register(gliner_relex_entry(&env, Some(shim)))`; `None` ⇒
  `Resolution::Misconfigured` (**fail-closed** — never register an unfiltered
  torch worker on Linux).
- On non-Linux, `Register(gliner_relex_entry(&env, None))` — unchanged behaviour.

The interpreter-bind logic (host-mode `resolve_host_interpreter_binds`, #284) is
untouched.

### 4. Empirical enumeration (DGX — the load-bearing step)

`ML_CLIENT_ADDITIONS` cannot be guessed; it is derived by watching torch run under
a log-mode filter. The DGX has the model cached
(`~/.cache/huggingface/hub/models--knowledgator--gliner-relex-multi-v1.0`),
`dmesg` is readable, and auditd is off (so `dmesg` is the enumeration channel —
the kill is otherwise silent).

Loop:
1. Temporarily build `ml_client`'s main filter with mismatch action
   `SeccompAction::Log` instead of `KillProcess` (a one-line local diff in
   `build_bpf`, gated to the diagnostic run).
2. `cargo build --workspace` (fresh shim bin — the #281 process gotcha) and run a
   **real model-load + one `extract`** through the shim under that filter.
3. `dmesg | grep -i seccomp` → collect denied `syscall=<nr>`; map numbers → names.
4. Add **only the legitimate** ones to `ML_CLIENT_ADDITIONS`. Escape primitives
   (`unshare`, `setns`, `mount` family, `ptrace`, `process_vm_*`, `bpf`,
   `perf_event_open`, `kexec*`, `keyctl`/`add_key`/`request_key`, raw `io_uring`)
   stay killed by default — if torch hits one, stop and reassess (likely the
   EPERM-downgrade carve-out for io_uring, never a plain allow for the rest).
5. Revert to `KillProcess`; re-run; confirm a real `extract` succeeds end-to-end.

### 5. Testing

**macOS / cross (TDD, authored now, run locally):**
- `seccomp_lock` unit tests: `parse("ml_client")`; `build_bpf(MlClient)` builds;
  `allow_list_for(MlClient)` contains `socket` (net family) and the ML additions,
  is a superset of `Strict`, and excludes the escape primitives (extend the
  existing `unshare_is_not_in_allow_list`-style assertions to `MlClient`).
- `lockdown_env` unit test: `WorkerMlClient` derives `KASTELLAN_SECCOMP_PROFILE=ml_client`.
- gliner-relex manifest/entry tests: Linux entry sets `lockdown_shim` + binds it
  into `fs_read` + pushes `KASTELLAN_LANDLOCK_PROFILE=none`; `resolve()`
  fail-closed `Misconfigured` when the shim is absent on Linux; macOS/container
  entry unchanged (`lockdown_shim: None`, no `LANDLOCK_PROFILE`). Pin
  `policy.profile == WorkerMlClient`.
- `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu` for the
  Linux-gated sandbox code (pure crate; `core` can't cross-compile — its Linux
  path is DGX/CI-verified).

**DGX acceptance:**
- `cargo build --workspace` (fresh shim), then the gliner real-model path
  (`KASTELLAN_GLINER_RELEX_ENABLE=1` + weights staged) renders a real `extract`
  under the **kill-mode** `ml_client` filter — proving the filter doesn't break
  model load.
- Full `cargo test --workspace` + `cargo clippy --workspace --all-targets
  -D warnings` green (baseline was 1829/0 on the #281 branch).

### 6. Files touched

- `sandbox/src/lib.rs` — `Profile::WorkerMlClient` variant + doc.
- `sandbox/src/macos_container.rs` — group with `WorkerStrict` arm.
- `workers/prelude/src/seccomp_lock.rs` — `Profile::MlClient`, `parse`,
  `ML_CLIENT_ADDITIONS`, `allow_list_for`, tests.
- `core/src/tool_host/lockdown_env.rs` — exhaustive-match arm + test.
- `core/src/workers/gliner_relex/entry.rs` — `lockdown_shim` param, profile flip,
  shim bind, `LANDLOCK_PROFILE=none`.
- `core/src/workers/gliner_relex/manifest.rs` — Linux shim discovery, fail-closed.
- `core/src/workers/gliner_relex/tests.rs` (or sibling test module) — manifest/entry tests.

### 7. Risks

- **Empirical syscall set.** The additions are discovered, not designed. If torch
  needs a genuinely dangerous primitive, we stop rather than allow it.
- **Stale-shim trap.** A `-p <crate> --tests` build leaves the shim bin stale and
  the e2e silently runs the old binary. Always `cargo build --workspace` first.
- **Warm-worker lifecycle.** gliner uses `IdleTimeout` (long-lived). The filter is
  installed once at spawn (pre-`execve`) and persists for the worker's life —
  consistent with the existing model; no per-request re-application.
```
