# python-exec runtime params — design

**Date:** 2026-06-14
**Status:** approved (brainstorm complete; ready for implementation plan)
**Roadmap:** Phase 4, the deferred "params" piece of the python-exec skill catalog
(`docs/devel/ROADMAP.md:263`). Builds on skill-catalog slice 1 (crystallise/approve,
PR #275) + slice 2 (invocation/surfacing, PR #276, `e478309`).

## Problem

A Python skill is agent-authored, verbatim source promoted through the L3 trust
lifecycle and stored SHA-256-bound (approve == execute). Today it is **param-less**:
`l3py_invoke` builds a single `python.exec` step carrying only `{ code }`, so a skill
can only ever run the exact bytes that were approved, with no runtime input. This
makes whole classes of useful skills impossible — "summarise *this* text", "transform
*these* records", "look up *this* id" — because the operand can't be supplied at call
time.

We want runtime params: named values supplied at invocation, available to the skill's
code, **without** weakening the SHA binding or the containment ceiling.

## Hard constraint that shapes everything

The code is verbatim and SHA-256-bound. We therefore **cannot** inject params by
wrapping, prepending, or otherwise mutating the source — that would break
approve==execute. Params must arrive on a **side channel** the skill author writes
code against. `stdin` is already consumed delivering the program (`python -`), so the
channel must be something else.

## Decisions (locked in brainstorm)

1. **Channel = environment variable.** A single env var `KASTELLAN_PYTHON_PARAMS`
   carries one JSON object. Chosen over argv (lands in `/proc/*/cmdline`, often
   world-readable — the same leak surface the code-over-stdin design deliberately
   avoids), a scratch file (needs writable scratch, absent on macOS slice-1), and a
   dedicated fd (non-idiomatic). Env survives the `-I` isolation flag (`-I` drops only
   `PYTHON*` names) and `/proc/*/environ` is readable by owner+root only.

2. **Size cap = 64 KiB serialized.** Linux `execve` enforces `MAX_ARG_STRLEN` = 128 KiB
   **per individual string** (argv *or* envp); a single env var over that → `E2BIG` →
   spawn fails. 64 KiB sits under the wall with headroom and mirrors the existing
   `MAX_CODE_BYTES` discipline. argv shares the same per-string limit, so it would buy
   nothing here. Genuinely-large payloads (>64 KiB) are a **deferred** scratch-file
   channel, not papered over.

3. **Value types = arbitrary JSON.** The payload is JSON end-to-end, so values may be
   ints/lists/dicts/bools, not just strings. The per-value guard inspects string-typed
   leaves only — allowing non-string values doesn't weaken screening.

4. **Secret refs allowed.** Params ride the step's `parameters` object, which already
   flows through `tool_host::dispatch` → `substitute_refs_in_params`, so a value of
   `secret://…` materialises automatically (identical to every other tool's input). No
   special code; containment is the worker's `Net::Deny` (a materialised secret has no
   egress) plus the Strict output-guard. This is distinct from — and does not weaken —
   the rule forbidding `secret://` in approved *code* (that rule protects operator
   auditability of stored bytes; runtime params are supplied fresh per-call).

5. **Params suppliable from both invocation paths** — operator CLI (`memory l3 run`)
   and the agent's autonomous `invoke_skill` — mirroring the templated path.

6. **Free-form passthrough, no declared schema.** The stored skill declares no params;
   `PythonSkillCandidate`, crystallise, approval, and the stored SHA are **untouched**.
   Invocation supplies a JSON object; we screen it (object-typed, snake_case top-level
   keys, 64 KiB cap, secret substitution) and hand it over. The author's code reads
   whatever keys it expects; a missing/typo'd key is a runtime `KeyError` → traceback →
   non-zero `exit_code` (the established "Python exceptions are exit_code+traceback, the
   planner iterates on its own code" philosophy). **Follow-up (recorded): battle-test
   for risk slip-throughs in test mode before relying on free-form**; a declared schema
   is the fallback if it proves too loose.

## Author-facing contract

The worker **always** sets `KASTELLAN_PYTHON_PARAMS`, defaulting to `{}` when no params
are supplied, so the read never `KeyError`s on the env lookup itself:

```python
import os, json
params = json.loads(os.environ["KASTELLAN_PYTHON_PARAMS"])
```

## Architecture

### Slice A — worker (`workers/python-exec`)

- `handler.rs`: `ExecParams { code: String, params: Option<serde_json::Value> }`.
  - `params` absent ⇒ `{}`.
  - `params` present but not a JSON **object** (array/scalar/null) ⇒ `INVALID_PARAMS`.
  - serialized params over `MAX_PARAMS_BYTES` ⇒ `INVALID_PARAMS`. The worker is the
    **authoritative** enforcer (a direct or malformed call must never reach `execve`
    with an oversize env var).
- `exec.rs`: new `pub const MAX_PARAMS_BYTES: usize = 64 * 1024;`. `run_code` gains a
  `params_json: &str` argument and adds `.env("KASTELLAN_PYTHON_PARAMS", params_json)`
  after `env_clear()` and the existing `TMPDIR`/`HOME`. Stdin code delivery, capture
  caps, and capping logic are unchanged.
- A small pure serialize/validate seam (object-check + size-check) is unit-testable
  without an interpreter; the real-interpreter integration test round-trips a param
  value through to stdout and asserts the empty-default (`{}`) when none is sent.

### Slice B — core (`core`)

- **Pure** (`core/src/memory/l3py_invoke/pure.rs`):
  - `validate_python_params(Value) -> Result<Value, PyParamError>` — object-typed,
    top-level keys snake_case (the param *names*; nested structure is opaque author
    data), serialized ≤ 64 KiB. Returns the validated object. Unit-tested, mirroring
    the templated path's arg guard but **without** the newline/control-char/`{{`/`}}`
    rejections (no template substitution; `serde_json` escapes control chars inside
    JSON strings, so newlines and long text pass freely — directly serving the
    long-text use case).
  - `python_exec_step(code, params)` builds `parameters: { code, params }`, **omitting
    `params` when empty** so existing-shape rows and no-param calls remain byte-identical
    (back-compat with slice-2 behaviour and its tests).
- **Agent** (`agent.rs`): `expand_python_for_agent(…, params)` threads the validated
  params into the single `PlannedStep`'s `parameters`, same omit-when-empty rule. The
  expanded step still flows through the unchanged CASSANDRA review → dispatch → audit
  pipeline.
- **Operator** (`operator.rs`): `prepare_python_steps` / `invoke_python_skill` accept
  params and forward them into the step. Gate-once semantics preserved.
- **Inner loop** (`scheduler/inner_loop.rs`): the `invoke_skill` python arm reads the
  directive's params object and passes it to `expand_python_for_agent`.
  **Scope discipline:** this nudges `inner_loop.rs` (629 LOC) further over cap; the big
  `invoke_skill`-expansion refactor (refactor bucket (b)) stays a **separate** tracked
  item and is explicitly out of scope here.
- **Daemon** (`scheduler/l3_run.rs`): the `kind=="python"` branch forwards operator
  params into `invoke_python_skill`.
- **Secret substitution is free**: because `params` is nested in the step's
  `parameters`, the recursive `substitute_refs_in_params` walker in `tool_host::dispatch`
  materialises `secret://` leaves with no new code (asserted by e2e).
- **CLI** (`bin/kastellan-cli/memory_l3/run.rs`): `memory l3 run <id>` gains
  `--params-json '<object>'` (full-fidelity JSON) and `--param k=v` (repeatable,
  string-valued sugar). Merge semantics: start from `--params-json` (or `{}`), then
  apply each `--param` as a string value (later wins). Result is `validate_python_params`'d
  before dispatch.

## Data flow (one operator invocation with a secret param)

```
operator: memory l3 run 42 --param query=hello --params-json '{"token":"secret://api/key"}'
  → CLI merges → {"query":"hello","token":"secret://api/key"}
  → validate_python_params (object, snake_case keys, ≤64 KiB)        [core, pure]
  → python_exec_step(code, params)  →  parameters:{code, params}     [core, pure]
  → daemon dispatch → substitute_refs_in_params materialises token   [tool_host chokepoint]
  → worker: ExecParams{code, params}; object+size check; serialize   [worker, authoritative]
  → run_code sets KASTELLAN_PYTHON_PARAMS=<json>; pipes code on stdin
  → CPython: json.loads(os.environ["KASTELLAN_PYTHON_PARAMS"])
  → {exit_code, stdout, stderr, *_truncated}   (Net::Deny: token cannot egress)
```

## Error handling

| Condition | Where | Result |
| --- | --- | --- |
| params not a JSON object | core (early) + worker (authoritative) | reject / `INVALID_PARAMS` |
| top-level key not snake_case | core | reject before dispatch |
| serialized params > 64 KiB | core (early) + worker (authoritative) | reject / `INVALID_PARAMS` |
| `secret://` ref in a param | tool_host chokepoint | materialised (allowed) |
| skill code reads a missing key | CPython | `KeyError` → traceback → non-zero `exit_code` (not an RPC error) |
| no params supplied | worker | env var set to `{}` |

## Security invariants (must all hold post-change)

1. **Containment unchanged** — no new syscalls, no net, no fs grants; `Net::Deny` +
   `WorkerStrict` + caps identical to slice 1.
2. **Approve == execute** — code SHA binding untouched; params are runtime-only and
   never enter the hash.
3. **No secret embedding in stored bytes** — the `secret://`-in-code ban is unchanged;
   secret *params* are per-call, vault-gated, and contained by `Net::Deny`.
4. **Pinned-only autonomy** — the agent path still resolves and runs only `pinned`
   Python skills; params do not relax the trust gate.
5. **Fail-closed daemon** — an unregistered/disabled python-exec still yields a
   tool-not-found step error, params or not.
6. **Code never surfaced** — `l3_surface` is untouched; params add no code-bearing
   surface.

## Testing

- **Worker unit:** params object/non-object/oversize validation; serialize seam;
  `run_code` env-injection shape.
- **Worker real-interpreter:** param value round-trips to stdout; empty-default `{}`
  when none sent; >64 KiB rejected.
- **Core unit:** `validate_python_params` (object, snake_case top-level keys, nested
  data opaque, size cap, arbitrary-JSON values, newline/long-text allowed); step
  builders carry/omit params; CLI merge semantics.
- **Core e2e (`cli_memory_l3py_run_daemon_e2e`, PG+sandbox gated):** an approved Python
  skill reads a param from `KASTELLAN_PYTHON_PARAMS` and echoes it (live jail); a
  `secret://` param materialises through the chokepoint; over-cap params rejected
  fail-closed.

## Deferred (explicitly out of scope)

- **Scratch-file param channel** for payloads >64 KiB — rides the macOS writable-scratch
  work (shared with browser-driver Phase 2); on demand.
- **Declared-param schema** — only if free-form battle-testing shows it is too loose.
- **`inner_loop.rs` `invoke_skill`-expansion split** — refactor bucket (b), separate
  change.

## Slicing summary

- **Slice A** (worker accepts `params`) lands first.
- **Slice B** (core threading + CLI + e2e) depends on A.
- Two PRs; each green (`cargo test --workspace`, clippy `-D warnings`) before merge.
