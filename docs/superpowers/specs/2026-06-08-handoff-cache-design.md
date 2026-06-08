# Large-tool-result handoff cache — design (ROADMAP:129)

**Date:** 2026-06-08
**Status:** approved, ready for implementation plan
**Roadmap item:** Phase 1 cont. — "Large-tool-result handoff cache" (ROADMAP:129)
**Branch:** `feat/handoff-cache`

## Problem

`tool_host::dispatch` returns the worker's full result verbatim to the
scheduler, which feeds it back into the planner's context on the next
iteration. Now that the `web-fetch` worker (PR #197) has landed, a single tool
result can be large — web-fetch caps its extracted text at **100 KiB**
(`MAX_TEXT_BYTES` in `workers/web-fetch/src/extract.rs`), and future
`web-search` / `browser-driver` workers will be worse. A single oversized
result can dwarf the planner's remaining context budget.

openhuman solves this with an in-memory `ResultHandoffCache`
(`docs/agent-subagent-tool-flow.md`): oversized tool results are replaced by a
placeholder and stashed; the agent fetches slices on demand. This design adapts
that pattern to hhagent, keeping the existing dispatcher chokepoint and audit
invariants intact.

## Goals / non-goals

**Goals**
- Cap what a single tool result injects into the planner's context.
- Stash the full body so it isn't lost; let the planner pull slices on demand.
- Zero behaviour change for everything shipping today (results under the cap
  pass through byte-identical).
- Preserve the audit invariant: the `tool:<name>` row still records (a
  SHA-envelope of) the full body; only what the *planner* sees becomes a
  placeholder.
- Preserve the security invariant: an injection-blocked output is **never**
  retrievable through the cache.

**Non-goals (YAGNI, explicitly cut)**
- No on-disk persistence. (ROADMAP:129 named the per-task `Workspace` scratch
  dir, but `Workspace` is implemented/tested yet **never constructed in the
  live scheduler flow** — only named in an `inner_loop.rs` doc comment. Wiring
  Workspace into the flow is its own slice. In-memory matches openhuman's
  actual pattern and is self-contained.)
- No cross-task dedup, compression, or streaming.
- No new migration — `fetch`/`stash` audit rows ride the existing `audit_log`.
- A disk-backed store can drop in later behind the same `HandoffCache` surface
  if memory pressure ever demands it; not now.

## Decision summary (the resolved forks)

1. **Storage:** in-memory, per-task, content-addressed cache. Not disk.
2. **Retrieval:** a `fetch_handoff` built-in, shipped this session.
3. **Placement:** the cap/stash/intercept logic lives in the **dispatcher
   layer** (`ToolHostStepDispatcher::dispatch_step`), *after* `dispatch`
   returns — **not** inside the sealed `dispatch` chokepoint. Rationale:
   `dispatch`/`dispatch_with_sink` stay byte-for-byte unchanged (the issue-#16
   seal and its many test callers are untouched); `task_id` is already in scope
   at the call site; the cache lives naturally next to the `ToolRegistry`.
   Injection-blocked outputs arrive from `dispatch` as the *tiny*
   `injection_blocked` placeholder, so they are already under the cap and are
   never stashed — the "blocked content is never retrievable" property falls
   out for free.
4. **Default cap:** 64 KiB (see Tunables).

## Architecture

### New module `core/src/handoff.rs`

A pure, in-memory, content-addressed cache. No clock, no I/O, unit-testable in
isolation.

```rust
/// "sha256:<64-lowercase-hex>". Parse-validated; the only way to name a
/// stashed body. Opaque to the planner.
pub struct HandoffRef(String);

impl HandoffRef {
    pub fn parse(s: &str) -> Option<HandoffRef>;   // validates the shape
    pub fn as_str(&self) -> &str;
}

pub struct HandoffCache { /* Mutex<HashMap<(i64, HandoffRef), Vec<u8>>> + bookkeeping */ }

impl HandoffCache {
    pub fn new() -> Self;

    /// Stash `body` for `task_id`. Returns its content-addressed ref.
    /// Storing an identical body again returns the same ref (cheap idempotence
    /// within a task). Evicts the oldest entries for the task if the per-task
    /// byte budget would be exceeded.
    pub fn put(&self, task_id: i64, body: &[u8]) -> HandoffRef;

    /// Return up to `len` bytes of the stashed body starting at `offset`.
    /// `None` if the (task_id, ref) is unknown or was evicted. The returned
    /// `Slice` carries the bytes plus an `eof` flag (offset+len reached the
    /// end). Callers clamp `len` to MAX_FETCH_BYTES before calling.
    pub fn get_slice(&self, task_id: i64, r: &HandoffRef, offset: usize, len: usize) -> Option<Slice>;

    /// Drop every entry for `task_id`. Called at task terminal.
    pub fn purge_task(&self, task_id: i64);
}

pub struct Slice { pub bytes: Vec<u8>, pub eof: bool }
```

**Boundedness.** Two layers so memory can't grow without bound:
- Per-task byte budget (`PER_TASK_BYTE_BUDGET`, 64 MiB). On `put`, if adding the
  body would exceed it, evict that task's oldest entries until it fits. A body
  larger than the whole budget is still stored (it just evicts everything else
  for the task) — never refused, because the alternative is losing the result.
- A global backstop: a bound on total tasks tracked, evicting the
  least-recently-used *task* wholesale, in case a `purge_task` is missed (e.g.
  a daemon crash between dispatch and terminal). Normal operation never hits
  this — `purge_task` runs at every terminal.

Eviction is strictly an availability concern: an evicted ref resolves to
`None`, surfaced to the planner as an explicit `HANDOFF_NOT_FOUND` error (it can
re-run the tool or replan), never a panic.

### Cap + reserved name (`scheduler/tool_dispatch.rs`)

- The cap is the global const `DEFAULT_RESULT_BYTE_CAP` (64 KiB) applied
  uniformly to every tool result this slice. **Deferred (YAGNI):** a per-tool
  `ToolEntry.result_byte_cap: Option<usize>` override — no worker needs a
  divergent cap today, and adding the struct field would touch ~14 `ToolEntry`
  construction sites for a value that would be `None` at every one. Add it when
  the first worker genuinely needs a different cap (likely `web-search` /
  `browser-driver`).
- The reserved built-in tool name `"handoff"` must not be shadowable by a
  worker manifest. `registry_build::assemble_registry` **skips** any manifest
  claiming that name (does not register it) and emits a loud `tracing::warn!`,
  so the `dispatch_step` intercept can never be bypassed by a registered worker
  of the same name. (No manifest claims it today; this is a forward guard.)

### Stash + placeholder (`ToolHostStepDispatcher::dispatch_step`)

After `dispatch(...)` returns `Ok(v)` for a *real* worker call:

1. `let body = serde_json::to_vec(&v)` — measure `body.len()`.
2. If `body.len() <= cap` → pass `v` through unchanged (byte-identical; the
   overwhelming common case today).
3. If `body.len() > cap`:
   - `let r = cache.put(task_id, &body);`
   - Build the placeholder and return *that* to the inner loop instead of `v`:

```json
{
  "handoff_ref": "sha256:abcd…",
  "byte_len": 123456,
  "summary_head": "first ~1 KiB of human-readable text from the result…",
  "truncated": true
}
```

`summary_head` is produced by reusing
`cassandra::injection_guard::extract_scannable_text(&v, SUMMARY_HEAD_BYTES)`,
so it's the readable text the planner cares about (and is char-boundary safe).
In practice the head plus `byte_len` is often enough that the planner never
issues a fetch.

A best-effort `policy / handoff.stashed` audit row is written
(`{tool, method, handoff_ref, byte_len, ms}`); a transient insert failure is
logged via `tracing`, not propagated (same posture as every other audit write
in the dispatcher).

The `tool:<name>` audit row written *inside* `dispatch` is unaffected — it
already records the full result subject to the existing 4 KiB SHA-envelope
truncation in `hhagent_db::audit::insert`, so forensics keep (a hash of) the
full body.

### `fetch_handoff` built-in (`ToolHostStepDispatcher::dispatch_step`)

Intercepted at the **top of `dispatch_step`, before `registry.lookup`**, so no
worker spawns and the chokepoint's spawn path is never entered:

- Recognised when `step.tool == "handoff" && step.method == "fetch"`.
- `step.parameters`: `{ "handoff_ref": "sha256:…", "offset": <u64, default 0>,
  "len": <u64, default MAX_FETCH_BYTES> }`.
- `len` is clamped to `MAX_FETCH_BYTES` (256 KiB) so a single fetch cannot blow
  the planner's context either.
- On hit → `StepOutcome::Ok` carrying:

```json
{ "handoff_ref": "sha256:…", "offset": 0, "len": 65536, "data": "…", "eof": false }
```

  `data` is the slice as a UTF-8 string when the slice is valid UTF-8 on its
  boundaries; otherwise base64 with `"encoding": "base64"`. (Stashed
  web/text results are UTF-8; base64 is the safe fallback for binary.)
- On miss/eviction → `StepOutcome::Err { code: "HANDOFF_NOT_FOUND", detail }`.
- On malformed params (bad ref shape, non-numeric offset) →
  `StepOutcome::Err { code: "INVALID_PARAMS", detail }`.
- Every arm writes a best-effort `policy / handoff.fetched` audit row
  (`{handoff_ref, offset, len, outcome, ms}`).

Because the intercept is before the registry lookup and `"handoff"` is a
reserved name, `fetch_handoff` is always core-side and never reaches a sandbox.

### Lifecycle threading (`scheduler/inner_loop.rs`)

- `StepDispatcher::dispatch_step` gains `task_id: i64`:
  `async fn dispatch_step(&self, task_id: i64, step: &PlannedStep) -> StepOutcome`.
  The inner loop passes `ctx.task_id`. Test doubles update trivially.
- `StepDispatcher::purge_task(&self, task_id: i64)` — new trait method,
  **default no-op**. `ToolHostStepDispatcher` overrides it to call
  `cache.purge_task(task_id)`. The inner loop calls `dispatcher.purge_task(ctx.task_id)`
  at every terminal return (one call site if placed in the lane runner after
  `run_to_terminal`, or via a guard; the plan will pick the single chokepoint).

### Wiring (`main.rs` / dispatcher construction)

`HandoffCache` is created once at daemon startup and shared (`Arc`) into the
`ToolHostStepDispatcher` alongside the existing `pool`/`vault`/`registry`. No
new config surface; the tunables are consts.

## Data flow (oversized web-fetch result)

```
planner → PlannedStep{tool:"web-fetch", …}
  inner_loop.run_to_terminal → dispatcher.dispatch_step(task_id, step)
    registry.lookup("web-fetch") → entry (cap = DEFAULT_RESULT_BYTE_CAP)
    lifecycle.acquire → worker
    tool_host::dispatch(...) → Ok(big Value)              ← full body, audited here
    serde_json::to_vec(big).len() > cap
      cache.put(task_id, body) → "sha256:…"
      audit policy/handoff.stashed
      return placeholder{handoff_ref, byte_len, summary_head}   ← what the planner sees
  …next iteration, planner decides it needs more…
  planner → PlannedStep{tool:"handoff", method:"fetch", params:{handoff_ref, offset, len}}
    dispatch_step intercept (before registry.lookup)
      cache.get_slice(task_id, ref, offset, len.min(MAX_FETCH_BYTES))
      audit policy/handoff.fetched
      return {data, eof, …}
  …task terminal…
  dispatcher.purge_task(task_id)   ← cache entries for the task dropped
```

## Tunables (consts, no config surface)

| Const | Value | Why |
| ----- | ----- | --- |
| `DEFAULT_RESULT_BYTE_CAP` | 64 KiB | ~16k tokens — generous for one document; below web-fetch's 100 KiB `MAX_TEXT_BYTES` so genuinely large results stash while small/medium pass through. |
| `SUMMARY_HEAD_BYTES` | 1 KiB | Enough readable head that the planner often needs no fetch. |
| `MAX_FETCH_BYTES` | 256 KiB | Per-fetch ceiling so one `fetch_handoff` can't blow the context. |
| `PER_TASK_BYTE_BUDGET` | 64 MiB | Bounds per-task cache memory; oldest-evict past it. |

## Error handling

- `cache.put` never fails (evicts instead of refusing) — an oversized result is
  always either passed through (≤ cap) or stashed (> cap).
- `fetch_handoff` miss/eviction → planner-visible `HANDOFF_NOT_FOUND`, never a
  panic; the planner can re-run the tool or replan.
- All audit writes are best-effort (logged, not propagated), matching the
  dispatcher's existing posture.
- Malformed `fetch` params → `INVALID_PARAMS`, no cache mutation.

## Security notes

- **Blocked content is never retrievable.** `dispatch` replaces an
  injection-blocked result with the small `injection_blocked` placeholder
  *before* `dispatch_step` sees it; that placeholder is under the cap and is
  never stashed. There is no path from a blocked output to a `handoff_ref`.
- **No cross-task leakage.** The cache is keyed by `(task_id, ref)`;
  `get_slice` for task A can never read task B's body even with a guessed ref
  (and sha256 refs aren't guessable anyway).
- **Reserved name.** `"handoff"` cannot be claimed by a worker manifest, so the
  built-in intercept can't be shadowed into a sandbox round-trip.
- **Audit fidelity preserved.** The full body is still hashed into the
  `tool:<name>` row; the cache changes only what the planner sees.

## Testing (TDD)

**Unit (`handoff.rs`):**
- `HandoffRef::parse` accepts `sha256:<64hex>`, rejects junk/wrong-length.
- `put` then `get_slice` round-trips; identical body → identical ref.
- `get_slice` honours offset/len and sets `eof` correctly at the tail.
- per-task budget eviction drops oldest; evicted ref → `None`.
- `purge_task` removes only that task's entries.

**Dispatcher (`scheduler/tool_dispatch/tests.rs`):**
- result just **under** cap → passthrough, byte-identical, cache empty.
- result just **over** cap → placeholder shape correct, cache populated,
  `handoff.stashed` audit row.
- injection-blocked result → small placeholder, **not** stashed.
- `fetch_handoff` happy path returns the right slice + `eof`.
- `fetch_handoff` `len` clamped to `MAX_FETCH_BYTES`.
- `fetch_handoff` unknown/evicted ref → `HANDOFF_NOT_FOUND`.
- a worker manifest claiming `"handoff"` is refused by `assemble_registry`.
- `dispatch_step` signature/`task_id` plumbed; `purge_task` clears the task.

PG-required e2e through the full chokepoint is optional and may be deferred to
keep the macOS skip-as-pass suite green; the dispatcher-level tests above cover
the behaviour with scripted doubles.

## Files touched

- **new** `core/src/handoff.rs` (+ `core/src/handoff/tests.rs` if it grows past
  the 500-LOC soft cap).
- `core/src/scheduler/tool_dispatch.rs` — the cap/stash/placeholder path (using
  the global `DEFAULT_RESULT_BYTE_CAP`); the `fetch_handoff` intercept;
  `purge_task` override; cache field on `ToolHostStepDispatcher`.
- `core/src/scheduler/inner_loop.rs` — `StepDispatcher` trait sig (`task_id`) +
  `purge_task` default; pass `ctx.task_id`; terminal purge call.
- `core/src/registry_build.rs` — reserve/refuse the `"handoff"` name.
- `core/src/main.rs` — construct + share the `HandoffCache`.
- `core/src/lib.rs` — `pub mod handoff;`.
- Test doubles implementing `StepDispatcher` across the scheduler test suites —
  mechanical signature update.

## Verification

`cargo build --workspace`; `cargo test -p hhagent-core` green (macOS
skip-as-pass); `cargo clippy -p hhagent-core --all-targets --locked -- -D
warnings` exit 0. New `handoff.rs` under the 500-LOC soft cap.
