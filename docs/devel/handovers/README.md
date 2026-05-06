# Session Handovers

This directory holds the **rolling handover document** that lets a fresh
Claude Code session pick up exactly where the previous one left off. The
user just says "read the handover" and the next session has full context.

## Convention

- **One active document**: [`HANDOVER.md`](HANDOVER.md). Always the current
  state-of-the-world.
- **At the start of every session**, read `HANDOVER.md` first. It tells you:
  what's done, what's working, what the next TODO is, and the context you
  need to start.
- **At the end of every session**, update `HANDOVER.md`:
  - Move the previous "Next TODO" into "Recently completed" if it shipped
  - Write a fresh "Next TODO" with enough context (file paths, design
    decisions, gotchas) for the next session to start cold
  - Update the "Working state" snapshot (what's green, what's stubbed)
  - Update [`../ROADMAP.md`](../ROADMAP.md) — tick `[ ]` → `[x]` for any
    items that shipped this session, with the commit hash
- **Archive** older handovers under `archive/<YYYY-MM-DD-slug>.md` only if
  the new session-end content is markedly different and the previous
  context is worth preserving as history. Most updates should overwrite
  in place.

## Why this exists

Sessions on this project tend to span weeks. Without a deliberate handover,
context drifts: stubs get re-stubbed, decisions get re-litigated, and
threat-model details get forgotten. The handover doc is the cheapest
mechanism that fixes that.
