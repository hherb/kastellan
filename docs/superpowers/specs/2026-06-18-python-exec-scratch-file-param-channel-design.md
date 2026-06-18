# python-exec: >64 KiB scratch-file param channel — design

**Date:** 2026-06-18
**Status:** approved (brainstorm), ready for implementation plan
**Phase:** 4 (python-exec arc continuation)
**Related:** runtime-params (`2026-06-14-python-exec-runtime-params-design.md`),
per-spawn scratch (PR #307/#308), output secret-scrub
(`2026-06-17-python-exec-output-secret-scrub-design.md`)

## Problem

Runtime params reach an agent-authored Python skill through the env var
`KASTELLAN_PYTHON_PARAMS` (compact JSON), capped at **64 KiB**. The cap is not a
transport limit on the host→worker hop — params travel over JSON-RPC stdio,
whose read path is unbounded (`BufRead::read_line` into a dynamic `String`, both
`protocol/src/server.rs` and `client.rs`). The cap exists because the worker
hands params to the child CPython process **as an env var**, and `execve`
env-string size is OS-limited (Linux `MAX_ARG_STRLEN` ≈ 128 KiB per string; the
total arg+env budget is also bounded). 64 KiB is the conservative ceiling that
keeps the single env string safely under that wall.

So a skill that needs a larger input — a medium JSON dataset, a page of fetched
text, a config blob, a CSV — has no way in. Large worker *output* (>64 KiB) is
already handled by the `HandoffCache`; this is the symmetric gap on the *input*
side.

The per-spawn writable scratch shipped in PR #307/#308 unblocks the fix: the
worker now has a writable directory inside its jail on **both** platforms (macOS
host-created `KASTELLAN_WORKER_SCRATCH`, Linux bwrap `/tmp` tmpfs). The worker
can write large params to a file there and hand the child a *path* instead of a
value.

## Approach (chosen)

**Worker-writes-to-scratch.** Params arrive at the worker over stdio exactly as
today. The worker decides, by serialized size:

- **≤ 64 KiB** → inline env `KASTELLAN_PYTHON_PARAMS` (today's behavior,
  byte-identical).
- **> 64 KiB and ≤ ceiling** → write the JSON to `<scratch>/params.json`, set
  `KASTELLAN_PYTHON_PARAMS_FILE` to that in-jail path, and set
  `KASTELLAN_PYTHON_PARAMS` to `"{}"` (stable empty default so legacy
  unconditional reads never `KeyError`).
- **> ceiling** → fail-closed (`INVALID_PARAMS`).

The ceiling is operator-configurable via `KASTELLAN_PYTHON_PARAMS_FILE_MAX`
(default **1 MiB**, clamped to an absolute max of **16 MiB**).

### Approaches considered and rejected

- **Host RO-binds a staged param file** (host writes a temp file, `--ro-bind` /
  Seatbelt read-grant into the jail, injects the path). Avoids stdio for the
  large case but adds a new host-side RAII guard, a `policy.fs_read` mutation,
  and briefly materializes secrets onto host disk — machinery with no gain,
  since stdio is already unbounded and the worker already owns a writable
  scratch.
- **Just raise the env cap.** Rejected: `execve` total arg+env size is
  OS-limited and not a reliable channel for multi-MiB payloads.

## Components

### Host side

**`core/src/memory/l3py_invoke/pure.rs`**
- Make the validation cap a parameter:
  `validate_python_params(params: &Value, max_bytes: usize) -> Result<Value, PyParamError>`.
  Snake_case top-level-key checks and the null/empty passthrough are unchanged;
  only the size gate now compares against `max_bytes` instead of the hardcoded
  `MAX_PARAMS_BYTES`.
- Add a thin boundary helper `params_file_max_from_env()` that reads
  `KASTELLAN_PYTHON_PARAMS_FILE_MAX` (default 1 MiB, parse failure → default,
  clamp to the 16 MiB absolute max). The pure validator stays I/O-free; the env
  read happens at the call site.
- The host no longer has an inline-vs-file notion — that is a worker-only
  concern. So `pure.rs`'s current `MAX_PARAMS_BYTES` (64 KiB) is **retired** as
  the rejection cap; the host validates only against the configurable file
  ceiling. Any existing references to `pure::MAX_PARAMS_BYTES` (tests, the
  doc-comment cross-reference) migrate to the new helper / the worker's
  `INLINE_PARAMS_MAX`.

**`core/src/workers/python_exec.rs`**
- Inject `KASTELLAN_PYTHON_PARAMS_FILE_MAX` into the worker's `policy.env`
  (sourced from the operator env, default 1 MiB) so the worker enforces the
  **same** ceiling. Defense-in-depth: the worker is the authoritative boundary
  (the secrets/sandbox principle), and a direct or malformed call must never
  reach `execve` or the filesystem with an oversize payload.

### Worker side

**`workers/python-exec/src/exec.rs`**
- New constants:
  - `INLINE_PARAMS_MAX: usize = 64 * 1024` — the execve-safe env threshold (the
    env-vs-file decision point; this is today's `MAX_PARAMS_BYTES` renamed for
    clarity of intent).
  - `PARAMS_FILE_MAX_DEFAULT: usize = 1024 * 1024` and
    `PARAMS_FILE_MAX_ABS: usize = 16 * 1024 * 1024`.
  - `PARAMS_FILE_ENV: &str = "KASTELLAN_PYTHON_PARAMS_FILE"` — the in-jail path
    handed to the child when the file channel is used.
  - `PARAMS_FILE_MAX_ENV: &str = "KASTELLAN_PYTHON_PARAMS_FILE_MAX"` — the
    operator-config ceiling (kept in sync with core).
  - `PARAMS_FILE_NAME: &str = "params.json"`.
- Pure `params_file_max(lookup: impl Fn(&str) -> Option<String>) -> usize` —
  mirrors the host helper (default + clamp), unit-testable.
- Pure `decide_param_channel(serialized_len: usize, inline_max: usize, file_max: usize)
  -> Result<ParamChannel, ParamsError>` where
  `enum ParamChannel { Inline, File }`. Returns `Err(TooLarge)` above
  `file_max`. No I/O — the full truth table is unit-testable.
- `serialize_params` keeps producing the compact JSON string and the `NotObject`
  check; the size rejection moves into `decide_param_channel` (so serialize is
  purely "to string", decide is purely "where does it go / is it too big").
- `run_code` gains a small delivery seam: given the serialized JSON and the
  channel decision, on `Inline` it sets `PARAMS_ENV` as today; on `File` it
  writes `<scratch>/params.json` (created with mode 0600), sets
  `PARAMS_FILE_ENV` to the path and `PARAMS_ENV` to `"{}"`. The file write is the
  only new I/O — fail-closed (`io::Result` propagates; no silent fallback to the
  env channel, which would exceed the execve wall).

**`workers/python-exec/src/handler.rs`**
- Compute `serialized = serialize_params(&p.params)?`, then
  `channel = decide_param_channel(serialized.len(), INLINE_PARAMS_MAX, params_file_max(env))?`,
  then call `run_code` with the serialized JSON and the channel. Both error arms
  map to `INVALID_PARAMS` as today.

### Agent (skill-author) contract — "file only when large"

- `KASTELLAN_PYTHON_PARAMS` is **always** present (real JSON ≤ 64 KiB, else
  `"{}"`).
- `KASTELLAN_PYTHON_PARAMS_FILE` is present **only** for the large case.
- Documented idiom (placed in the `exec.rs` `PARAMS_FILE_ENV` doc-comment,
  matching where the existing `PARAMS_ENV` idiom is documented — no new doc
  file):

  ```python
  import json, os
  if p := os.environ.get("KASTELLAN_PYTHON_PARAMS_FILE"):
      with open(p) as f:
          params = json.load(f)
  else:
      params = json.loads(os.environ.get("KASTELLAN_PYTHON_PARAMS", "{}"))
  ```

## Data flow & security

`L3 invoke → validate_python_params(value, file_max_from_env) → python_exec_step
→ dispatch substitutes secret:// refs (unchanged) → worker.call over stdio
(carries the large payload) → handler → decide_param_channel → run_code writes
<scratch>/params.json inside the jail (Linux tmpfs / macOS RAII-cleaned scratch)
→ child reads it`.

- **Secrets:** `secret://` refs are substituted host-side in `dispatch` **before**
  the worker, so the file holds exactly the same materialized params the env var
  would have. The output secret-scrub fingerprints this dispatch's secrets and
  redacts them from the result regardless of how params entered — so it is
  **unaffected**. The file is no more sensitive than the env var was.
- **Lifetime:** python-exec is `SingleUse`, so the scratch dir (and
  `params.json`) is destroyed after the call — Linux tmpfs torn down with the
  jail, macOS dir RAII-cleaned via `SupervisedWorker.scratch`.
- **Cross-platform parity:** identical observable contract on both OSes; the only
  difference is where the writable scratch physically lives, which the existing
  `scratch_dir_from_env` already abstracts.
- **Fail-closed at both layers:** host rejects above the ceiling before dispatch
  (clean refusal); the worker re-enforces the ceiling as the real boundary.

## Testing (TDD-first)

- **Pure unit (`exec.rs`):** `decide_param_channel` truth table — exactly 64 KiB
  (Inline), one byte over (File), at `file_max` (File), one over `file_max`
  (TooLarge). `params_file_max` parsing — unset → default, valid value, garbage →
  default, over-absolute → clamp.
- **Pure unit (`l3py_invoke/pure.rs`):** `validate_python_params` accepts up to
  the passed ceiling, rejects above, snake_case + null passthrough unchanged.
- **Worker unit (`exec.rs`):** inline path sets `PARAMS_ENV` only and leaves
  `PARAMS_FILE_ENV` unset; file path writes `params.json` with exact content +
  sets `PARAMS_FILE_ENV` to the path + sets `PARAMS_ENV` to `"{}"`; over-ceiling
  errors. Uses a temp dir as the scratch root.
- **e2e (`core/tests/python_exec_e2e.rs`):** a > 64 KiB param round-trips through
  the real worker + real jail under the production policy — the agent reads the
  file channel and echoes back a marker derived from the large payload, proving
  it received the full value. Runs on macOS Seatbelt and DGX bwrap.

## Files touched

- `core/src/memory/l3py_invoke/pure.rs`
- `core/src/workers/python_exec.rs`
- `workers/python-exec/src/exec.rs`
- `workers/python-exec/src/handler.rs`
- `core/tests/python_exec_e2e.rs`
- (session end) `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

All files stay under the 500-LOC cap (`exec.rs` is ~230 LOC today; the additions
are small and the decision logic is pure).

## Out of scope / deferred

- A writable *result*-file channel for > 64 KiB output — already covered by the
  `HandoffCache`; not duplicated here.
- Curated-wheels RO package dir for skills that need third-party imports
  (separate Phase-4 pick).
- Streaming/chunked params — a single file is sufficient for the configured
  ceiling.
