# Teach the planner to call `fetch_handoff` — design

**Date:** 2026-06-09
**Status:** approved (brainstorming) → ready for implementation plan
**ROADMAP:** 129 (direct follow-up to the large-tool-result handoff cache, PR #199)

## Problem

PR #199 shipped the large-tool-result handoff cache: a tool result whose
serialized JSON exceeds `DEFAULT_RESULT_BYTE_CAP` (64 KiB) is stashed in an
in-memory per-task cache and replaced — in the planner's step history — with a
small placeholder:

```json
{ "handoff_ref": "sha256:<64-hex>", "byte_len": 123456,
  "summary_head": "<first ~1 KiB of readable text>", "truncated": true }
```

The planner can pull the full body back in slices through a reserved built-in
(`tool="handoff" method="fetch"`), intercepted in
`scheduler::tool_dispatch::ToolHostStepDispatcher::dispatch_step` before any
registry lookup or worker spawn.

**The mechanism exists and is tested, but is inert.** Nothing tells the planner
that the placeholder can be expanded or how to form the `fetch` step. A stashed
result is currently a dead end: the planner sees `summary_head` and an opaque
`handoff_ref` with no instruction for what to do with it.

## Goal

Surface the `fetch_handoff` protocol in the assembled system prompt so the
planner knows — when it sees a handoff placeholder — that the full body was
stashed and how to retrieve more of it on demand. Compiled-in alongside the
mechanism so the instruction and the code cannot drift apart.

## Non-goals (deferred, per HANDOVER.md)

- No change to the base prompt file `prompts/agent_planner.md` (programmatic
  block chosen over static markdown for testability + drift-resistance).
- No e2e assertion in `cli_ask_e2e` — unit coverage on the pure assembler is the
  right level for a prompt-text change.
- On-disk Workspace-backed store and the per-tool `result_byte_cap` override
  stay deferred as in the handover.

## Approach

A new **unconditional** `<handoff>` block emitted by `assemble_system_prompt` in
[`core/src/prompt_assembly/assemble.rs`](../../../core/src/prompt_assembly/assemble.rs).

### Placement

Order becomes: `L0 → L1 → skills → recalled → handoff → base`.

The block sits **between `<recalled>` and `<base>`**. `<base>` stays the terminal
block — several existing tests structurally assume base is last, and the
handoff instruction is a compiled-in, maximally-trusted capability note that
belongs adjacent to the base operating instructions (not among the
lower-trust memory layers).

### Always present

The built-in is always available, so the instruction always applies. Unlike the
`<skills>` / `<recalled>` blocks (which omit when their slice is empty), the
`<handoff>` block has no empty state — it is constant text. This is a deliberate
contract change from the current "empty everything → `<base>` alone" behaviour.

### Drift-proofing

The block is produced by a small pure helper:

```rust
fn render_handoff_block() -> String
```

that interpolates the **source-of-truth constants** rather than re-typing the
literals:

- `crate::scheduler::tool_dispatch::HANDOFF_TOOL` (`"handoff"`)
- `crate::scheduler::tool_dispatch::HANDOFF_METHOD_FETCH` (`"fetch"`)

A unit test cross-checks that the rendered block also names every field the
mechanism actually uses, so a future change to those shapes fails the test:

- placeholder fields from a real `build_handoff_placeholder(...)` output:
  `handoff_ref`, `byte_len`, `summary_head`, `truncated`
- fetch params: `offset`, `len`

### Block content (the protocol)

Roughly eight lines, conveying:

- When a tool result is the placeholder
  `{handoff_ref, byte_len, summary_head, truncated: true}`, the full output was
  too large for context and was stashed.
- `summary_head` is the readable head (~1 KiB) — often enough to proceed with no
  fetch at all.
- To read more, emit a step:
  `tool="handoff" method="fetch" parameters={handoff_ref, offset?, len?}`.
  `offset` defaults to 0; `len` defaults to and is clamped at 256 KiB.
- The fetch returns `{handoff_ref, offset, len, data, encoding:"utf8", eof}`
  where `len` is the bytes actually returned.
- To read the whole body, repeat with `offset += len` until `eof: true`.

The exact prose is an implementation detail; the tests pin the load-bearing
tokens (tool/method names, param + field names), not the wording.

## Testing (TDD)

Write failing tests first, then the helper.

New tests:

1. **Present** — the `<handoff>` block appears even when L0/L1/skills/recalled
   are all empty.
2. **Position** — `</recalled>` (when present) precedes `<handoff>`, and
   `<handoff>` precedes `<base>`; base remains terminal.
3. **Drift cross-check** — the block contains the real `HANDOFF_TOOL` /
   `HANDOFF_METHOD_FETCH` values, every placeholder field name from a real
   `build_handoff_placeholder(...)` output, and the `offset` / `len` fetch
   params.
4. **Determinism** — same inputs still yield identical bytes.

Deliberately updated existing byte-exact pins (they assert "empty everything →
`<base>` alone", which the always-present block intentionally changes):

- `empty_l0_l1_recalled_emits_base_block_only`
- `skills_block_absent_when_empty_is_byte_identical`
- `both_layers_assembled_in_order_with_blank_separators`
- `base_trailing_newlines_are_normalized_to_exactly_one`

The module docstring's framing diagram + rules also gain `<handoff>` in the
order.

## File-size handling

`assemble.rs` is 419 LOC. Production additions are small (~25 lines), but the
new + updated tests will likely push the file past the 500-LOC cap. If so, lift
the `#[cfg(test)] mod tests` block to a sibling `assemble/tests.rs` as part of
this work — the established move in this repo (`recall.rs`, `macos_seatbelt.rs`,
etc.). Production code stays byte-identical across the lift.

## Verification

- `cargo test -p kastellan-core --lib prompt_assembly` green.
- `cargo clippy -p kastellan-core --all-targets --locked -- -D warnings` exit 0.
- `cargo build --workspace` clean.

No PG / sandbox / worker needed — this is a pure-function change.

## Files touched

- `core/src/prompt_assembly/assemble.rs` — new `render_handoff_block` helper,
  unconditional `<handoff>` block in `assemble_system_prompt`, docstring update,
  test updates/additions (lifted to `assemble/tests.rs` if over cap).
