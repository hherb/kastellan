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
- **At the end of every session**, update `HANDOVER.md` in this strict order
  (header fields first; prose last):

  1. **Bump the header fields at the top** — *before adding any prose*. The
     header is what the next session reads first and treats as authoritative,
     so it must be current even if you run out of time for the prose:

     - `Last updated:` → today's date
     - `Last commit on <branch>:` → the hash of the most recent shipped
       commit on whichever branch you're handing over from. Confirm with
       `git log --oneline -1`.
     - `Session-end verification:` → re-run `cargo test --workspace` and
       copy the **passed / failed / ignored / `[SKIP]`** counts into this line.
     - **Every test-count number embedded in the doc that changed this
       session** — search for the old count and replace with the new one.
       Stale numbers are silently misleading; a fresh agent will trust them.

  2. **Move the previous "Next TODO" into "Recently completed (this session)"**
     if it shipped — with enough detail (file paths, decisions, gotchas,
     test-count delta) that the next session can start cold.
  3. **Write a fresh "Next TODO"** for the next session.
  4. **Refresh "Working state"** — anything that became real, anything new
     under stubs.
  5. **Tick `[ ]` → `[x]` in [`../ROADMAP.md`](../ROADMAP.md)** with the
     commit hash for every item that shipped.
  6. **Commit `HANDOVER.md` + `ROADMAP.md` together** with a
     `docs(handover): ...` message.

  **Why header-first matters.** The prose is the easy part to write but
  the easy part to skip-update; if a session ends with stale header
  fields, the next session reads the wrong commit hash and the wrong
  test count, and silently drifts off-state. Updating the header first
  guarantees the load-bearing fields are current even if the session
  is cut short before the prose is fully written.
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
