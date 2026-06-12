# python-exec worker — slice #1 design (Phase 4 entry)

**Date:** 2026-06-12
**Status:** approved (operator picked Phase 4; this is its first item)
**Roadmap:** Phase 4 — "`python-exec` worker: scratch FS only, no net, hard
CPU/mem/wallclock; curated stdlib bind" (ROADMAP:202)

## 1. Goal

The first executor for agent-authored Python: a sandboxed tool worker exposing
one JSON-RPC method, `python.exec`, that runs a supplied source string under
the strictest containment any kastellan worker has — no network, no
persistent filesystem, hard CPU/memory/wall-clock caps — and returns
`{exit_code, stdout, stderr}`. Everything later in Phase 4 (skill catalog,
trust ceilings, delegation tiers) invokes code *through* this worker; the
worker itself stays a dumb, policy-free pipe, exactly like shell-exec.

## 2. Shape

Rust worker crate `workers/python-exec` (`kastellan-worker-python-exec`),
mirroring shell-exec: ~150 LOC of handler around `std::process::Command`,
locked down by `kastellan-worker-prelude::serve_stdio`. Rust (not a Python
package like gliner-relex/browser-driver) because the worker is a thin
spawn-and-capture wrapper and must not itself be written in the language it
executes — the CPython process is a *child* of the worker, so a wedged or
malicious payload can never corrupt the JSON-RPC server loop.

### 2.1 Wire contract

Method `python.exec`:

```jsonc
// params
{ "code": "<python source, UTF-8, ≤ 262_144 bytes>" }
// result
{
  "exit_code": 0,            // null if killed by a signal
  "stdout": "...",           // ≤ 262_144 bytes, lossy UTF-8, char-boundary cap
  "stderr": "...",           // same cap
  "stdout_truncated": false,
  "stderr_truncated": false
}
```

Errors: `INVALID_PARAMS` (missing/non-string `code`, code over the byte cap),
`OPERATION_FAILED` (interpreter failed to spawn), `METHOD_NOT_FOUND`. A
*Python* error (exception, SyntaxError) is **not** an RPC error — it comes
back as `exit_code: 1` + the traceback on `stderr`, which is what the planner
needs to iterate on its own code. Deliberately deferred params: `stdin`,
`argv`, per-request `timeout_ms` (the policy's cpu/wall caps are the timeout;
a worker-side child reaper adds threads for no slice-1 value on a `SingleUse`
worker).

### 2.2 Interpreter invocation

```
<python> -I -S -B -        # code piped over stdin, then EOF
```

* **`-I`** (isolated): implies `-E` (ignore `PYTHON*` env) + `-s` (no user
  site dir), and removes the script dir/cwd from `sys.path`.
* **`-S`**: skip the `site` module → system site-/dist-packages never join
  `sys.path`. **This is the "curated stdlib bind"** from the roadmap line:
  the code sees the standard library and nothing else. It is a
  *determinism/predictability* measure, not a security boundary — the
  security boundary is the jail (a payload appending to `sys.path` finds
  only what bwrap/Seatbelt let it read anyway).
* **`-B`**: no `.pyc` writes.
* **Code over stdin** (`-` argv): no scratch write needed to deliver the
  program, no argv-size limit, nothing in `/proc/*/cmdline`. CPython reads
  the whole program to EOF before executing, so there is no write-read
  deadlock; the worker still writes stdin from a helper thread as
  belt-and-braces.
* Child env is **cleared** (`env_clear`), then exactly `TMPDIR=/tmp` +
  `HOME=/tmp`; cwd `/tmp`. The lockdown env vars the jail carries
  (`KASTELLAN_*`) are not the child's business.

### 2.3 Containment (the roadmap line, mechanism by mechanism)

| Roadmap requirement | Mechanism |
| --- | --- |
| scratch FS only | `fs_write = []` — **no host path is ever bound writable**. On Linux the jail's `/tmp` is bwrap's per-spawn ephemeral tmpfs (#89); the policy grants it through the worker-side Landlock layer by carrying an explicit `KASTELLAN_LANDLOCK_RW=["/tmp"]` in `policy.env` (which `derive_lockdown_env` respects instead of deriving an empty list from `fs_write`). Scratch is therefore anonymous, per-spawn, and gone at exit, with zero host-side dir lifecycle. On macOS, Seatbelt's `(deny default)` means slice #1 simply has **no writable scratch** (strictly tighter; see §5). |
| no net | `Net::Deny` + seccomp `strict` (no `socket(2)`), both inherited by the CPython child. |
| hard CPU | `cpu_ms = 10_000` → `RLIMIT_CPU` (rlimits inherit across fork/exec). |
| hard mem | `mem_mb = 512` → Linux cgroup `MemoryMax` (whole jail, child included). macOS Seatbelt gap is the standing platform note; `MacosContainer` is the existing opt-in answer when it matters. |
| hard wallclock | `wall_clock_ms = Some(30_000)` → host-side watchdog kills the jail. |
| curated stdlib bind | `-I -S` (§2.2). |

`Profile::WorkerStrict` throughout: seccomp filters survive `execve`, so the
python child runs under the same `strict` allowlist as the worker. The
`BASE_ALLOW` set already carries everything CPython needs at startup
(`getrandom`, `futex`, `rt_sig*`, `ioctl`, `sysinfo`); the coreutils-smoke
suite gains a python case to pin that empirically (§6).

Lifecycle `SingleUse`: one spawn per step, no state across calls — the
`stateless` contract is structural, and a heap poisoned by one payload never
serves the next.

## 3. Host-side manifest (`core/src/workers/python_exec.rs`)

* **Opt-in gate:** `KASTELLAN_PYTHON_EXEC_ENABLE=1`, else
  `Resolution::Disabled`. Mirrors browser-driver. Rationale: shell-exec is
  deny-by-default through its empty argv allowlist; python-exec has no
  equivalent operational knob (arbitrary code is the *point*), so the
  deny-by-default posture moves to registration itself. No
  `allowlist_tool()`.
* **Interpreter discovery** (pure, via `ResolveCtx` probes):
  1. `KASTELLAN_PYTHON_EXEC_PYTHON` override — authoritative; set-but-invalid
     **fails closed** to `Misconfigured` (same contract as `discover_binary`).
  2. Candidate cascade: `/usr/bin/python3`, `/usr/local/bin/python3`,
     `/opt/homebrew/bin/python3` — first existing non-dir wins.
  3. None found → `Misconfigured`.
* **Worker binary:** standard `discover_binary` (`KASTELLAN_PYTHON_EXEC_BIN`
  override → exe-relative sibling `kastellan-worker-python-exec`).
* **Policy `fs_read`:** worker binary + interpreter + the interpreter's
  derived `<prefix>/lib` (when the interpreter sits in `<prefix>/bin/`).
  Redundant on Linux (`/usr` is always bound RO by bwrap and implicit in
  Landlock) but it is what makes a non-`/usr` prefix (Homebrew,
  `/usr/local`) readable under Seatbelt/Landlock.
* **Policy env:** `KASTELLAN_PYTHON_EXEC_PYTHON=<interpreter>` (the worker
  fails closed at startup without it) + `KASTELLAN_LANDLOCK_RW=["/tmp"]`
  (§2.3 — inert on macOS).
* Injection guard: `GuardProfile::Strict` (default — no `for_tool` change).
  The output is whatever agent-authored code printed, which may launder
  fetched content; strict screening is the right default.

## 4. What this slice does NOT do

* **No package access** — stdlib only. A curated-wheels story (vendored
  pure-Python wheel dir bound RO, offline `pip --no-index`) is a future
  slice *if* the skill catalog demands it.
* **No skill persistence** — that's the next Phase-4 item, built on top.
* **No micro-VM backend** — `sandbox_backend: None` (per-OS default).
  The Firecracker/`container` option stays a separate roadmap line.
* **No per-request timeout / stdin / argv params** (§2.1).
* **No prompt-assembly surfacing** — same posture as web-fetch/web-search
  (none of the net workers are named in `agent_planner.md` yet; tool
  surfacing is a separate concern).

## 5. macOS notes

The manifest + worker compile and register identically on darwin (candidate
cascade covers CLT + Homebrew pythons; `fs_read` carries the prefix). Two
documented platform gaps, both *tighter* not looser:

* No writable scratch (Seatbelt `(deny default)` + `fs_write = []`): code
  that writes temp files fails on macOS in slice #1. Follow-up: a per-spawn
  host scratch dir (the same wiring browser-driver Phase 2 needs) or the
  `MacosContainer` backend.
* `mem_mb` unenforced under Seatbelt (standing gap, documented on
  `SandboxPolicy.mem_mb`).

Runtime validation on a Mac (Seatbelt profile vs CPython framework layout)
is an operator/next-Mac-session step, same as browser-driver slice #1 was.

## 6. Tests

1. **Worker unit** (`workers/python-exec`): param validation (missing code,
   over-cap code), truncation helper (char-boundary, flag), command shape
   (`-I -S -B -` argv pinned).
2. **Worker integration** (`tests/real_python.rs`): real system python3 if
   present (`[SKIP]` otherwise) — happy path, traceback→exit 1 path, stdout
   cap, env isolation (`os.environ` near-empty).
3. **Manifest unit** (in-module): Disabled without the gate; Register shape
   (policy pins: Net::Deny, strict profile, env pair, landlock-RW pair, caps);
   override-invalid fails closed; no-python `Misconfigured`.
4. **Prelude smoke** (`coreutils_smoke.rs` python case): CPython runs a
   read+write+subprocess-free script under seccomp `strict` + Landlock —
   the BASE_ALLOW empirical pin the suite was built to host.
5. **Core e2e** (`core/tests/python_exec_e2e.rs`, mirrors shell_exec_e2e):
   real sandbox + PG + dispatch chokepoint — `print(6*7)` round-trip and a
   no-net negative (`urllib` connect fails inside the jail). Skip-as-pass
   without PG/sandbox.
