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
- **Pruning**: keep HANDOVER.md focused on what the next session needs to
  act on (current state + last 2–3 sessions in detail + next TODO). Older
  session entries get compressed into an "Earlier history" summary or
  dropped once they're no longer load-bearing. Before pruning, snapshot
  the current HANDOVER.md to [`archive/handover_<YYYYMMDD>[_<slug>].md`](archive/)
  — the archive is the audit trail and is never edited after the fact.
  See the "How to update this document at session end" section in HANDOVER.md
  for the full pruning checklist.

## Why this exists

Sessions on this project tend to span weeks. Without a deliberate handover,
context drifts: stubs get re-stubbed, decisions get re-litigated, and
threat-model details get forgotten. The handover doc is the cheapest
mechanism that fixes that.
