# Design — Inject a trusted `<now>` block into the planner context

**Date:** 2026-07-12
**Branch:** `feat/planner-now-context`
**Status:** design (approved, pre-implementation)
**Related:** planner-feedback arc #337→#340 (already shipped); this is a *separate*
root cause. Sibling follow-up (own spec): batch web-search.

## Problem

The agent planner has **no idea what the current date/time is.** For any
date-relative question ("what happened in Germany yesterday", "latest news",
"today's …") the planner web-searches to *discover* the date, gets inconsistent
snippet dates back from SearxNG, and loops until it either exhausts the
5-iteration plan cap (`plan_iteration_cap_exceeded` → `task.failed`) or guesses a
date and answers on a false premise.

### Evidence (live DGX, 2026-07-12, audit_log)

| Task | Question | Outcome |
| ---- | -------- | ------- |
| 88 | "news in Germany yesterday" | **FAILED** at the 5-plan cap — every plan re-searched trying to pin the date |
| 89 | same | "completed" but **wrong** — the planner guessed *"current date is June 24"* (actual: July 12) and answered from that false date |
| 90 | "gold medals at Tokyo 2020" (no date needed) | **clean success**, correct answer on a short plan |

Task 88, plan 5, the planner's own `context` field:

> "I need to determine the current date to identify what happened in Germany
> 'yesterday'. Previous attempts to find the current date via search yielded
> inconsistent results (June 24, July 10, July 12, 2026)."

This proves two things: (1) the #338 feed-tool-output-to-planner mechanism
**works** — the planner is summarising prior search output; and (2) the loop is
driven by **missing ground-truth "now"**, not by a feedback gap. Task 90 confirms
non-date questions converge normally.

### Confirmed in code

The prompt-assembly path (`core/src/prompt_assembly/`) injects **no** date/time
anywhere; `agent_planner.md` never mentions the date; `chrono`/`jiff` are not
dependencies. The agent is date-blind by construction.

## Goal

Give the planner an **authoritative, system-generated** current date/time it can
trust, so it uses it directly instead of searching for it. Scope is deliberately
narrow: *inject the fact + tell the planner to use it.* No change to dispatch,
allowlists, the reviewer, or the feedback mechanism.

## Non-goals

- Batch web-search (its own spec/PR — the sibling performance feature).
- Any change to how tool output is fed back (#338 already correct).
- A user-facing timezone-management feature (future travel work; this only needs
  a single configured "home" timezone with correct DST).

## Design

### 1. Pure renderer — `render_now_block`

New pure function in `core/src/prompt_assembly/` (its own small module,
`now.rs`, to keep `assemble.rs` focused):

```rust
/// Render the trusted current-date/time grounding block for the planner
/// system prompt. Pure: the caller supplies the instant, so this is fully
/// deterministic and unit-testable. Minute resolution (no seconds) keeps the
/// assembled system prompt — and its `system_prompt_sha256` — stable within a
/// plan iteration so the local model's KV-cache prefix is not churned each
/// second.
fn render_now_block(now: &jiff::Zoned) -> String
```

Output (verbatim, **not** escaped — system-generated, not adversary-influenced):

```
<now>
Current date and time: Sunday, 12 July 2026, 14:05 (AEST, UTC+10:00).
</now>
```

Formatting via `jiff::fmt::strtime` (`%A, %-d %B %Y, %H:%M`, plus the zone
abbreviation `%Z` and offset `%:z`). Weekday and explicit offset are included so
"yesterday"/"this week" reasoning is unambiguous and the block is
self-describing.

### 2. Timezone resolution — `jiff`

Add `jiff` (Apache-2.0 OR MIT — AGPL-compatible; safe in multithreaded
processes, unlike `time::now_local()`; bundles IANA tz handling with correct
DST) to `kastellan-core`.

Pure resolver:

```rust
/// Resolve the operator's configured timezone. `KASTELLAN_TIMEZONE` is an IANA
/// name (e.g. "Australia/Sydney"); unset → the host system tz; unresolvable →
/// UTC (fail-safe: the block still renders, just in UTC). Returns the zone plus
/// a source label for the audit/log.
fn resolve_timezone(configured: Option<&str>) -> (jiff::tz::TimeZone, TzSource)
```

`TzSource` ∈ { `Configured`, `System`, `UtcFallback` } — logged once at startup
so a misconfigured `KASTELLAN_TIMEZONE` is visible rather than silent.

DST is automatic: `jiff::Timestamp::now().to_zoned(tz)` yields the correct local
wall-clock and abbreviation for the instant, across DST boundaries, for a
long-running daemon — no restart needed. (This is why we chose `jiff` over a
zero-dep fixed offset: travel/timezone management is on Kastellan's roadmap.)

### 3. Assembly integration

Add a 7th parameter to `assemble_system_prompt`, **appended last** — following
the codebase's own convention (the `<tools>` param is last precisely so param
order is decoupled from render position; see the assemble module doc):

```rust
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    skills: &[SurfacedSkill],
    recalled: &RecalledContext,
    base: &str,
    tools: &[ToolDoc],
    now: Option<&str>,     // NEW (appended last) — rendered <now> block, or None
) -> String
```

Render position ≠ param position: the `<now>` block is emitted **first** (before
`<l0_meta_rules>`) — a grounding fact belongs at the top — and only when
`Some(non-empty)`. When `None`, the **output is byte-identical** to today; the
existing direct-call sites (the builder + assembly tests) simply **append a
`None` argument** (a trivial edit; rendered output unchanged). The higher-level
integration tests go through `PgSystemPromptBuilder::new(pool)`, which defaults
`timezone: None` → `now: None` → those call sites are **untouched**. Update the
module-doc order line to `now → L0 → L1 → skills → recalled → tools → handoff →
base`.

### 4. Builder wiring — opt-in, mirrors `with_tool_docs`

`PgSystemPromptBuilder` gains a `timezone: Option<jiff::tz::TimeZone>` field and
a defaulting setter:

```rust
pub fn with_timezone(mut self, tz: jiff::tz::TimeZone) -> Self
```

- `PgSystemPromptBuilder::new(pool)` leaves `timezone: None` → `build()` passes
  `now: None` → **all existing tests byte-identical** (same guarantee the
  `<tools>` work preserved by keeping `new(pool)`).
- In `build()`, when `timezone` is `Some(tz)`: capture the instant *at build
  time* (`jiff::Timestamp::now().to_zoned(tz)`), render via `render_now_block`,
  pass `Some(&block)`. The zone is fixed on the builder; the instant is fresh
  each formulation, so `<now>` is always current.

The daemon opts in in `main.rs` where the builder is constructed alongside the
tool registry: `.with_timezone(resolve_timezone(env)…)`.

### 5. Planner guidance — `agent_planner.md`

Add a short, explicit rule (near the "Answer directly when you can" section):

> **The current date and time is given in the `<now>` block of your system
> prompt.** Use it directly for all date/time reasoning — "today", "yesterday",
> "this week", "recent", "latest", "how long ago". **Never** use web search to
> determine the current date or time; you already have it, and search snippets
> report inconsistent dates.

## Component boundaries

| Unit | Purpose | Depends on | Tested by |
| ---- | ------- | ---------- | --------- |
| `render_now_block(&Zoned) -> String` | format the block | `jiff` | pure unit tests (format, weekday, offset sign, minute rounding, UTC) |
| `resolve_timezone(Option<&str>) -> (TimeZone, TzSource)` | pick the zone | `jiff` | pure unit tests (IANA / unset→system / bad→UTC) |
| `assemble_system_prompt(now, …)` | frame the prompt | — | assembly tests (first when Some, omitted when None, order preserved) |
| `PgSystemPromptBuilder::{with_timezone, build}` | wire instant per build | above | builder test (block present & shaped when tz set; absent by default) |

## Testing (TDD)

Unit gate is pure-Rust → **no DGX needed to merge**:

- `render_now_block`: exact string for a fixed `Zoned`; weekday correctness;
  positive/negative offset rendering; minute (not second) resolution; a UTC
  instant renders `UTC, UTC+00:00`.
- `resolve_timezone`: `Some("Australia/Sydney")` → Configured; `None` → System;
  `Some("Not/AZone")` → UtcFallback.
- `assemble_system_prompt`: `<now>` present and first when `Some`; omitted and
  byte-identical to prior output when `None`; the existing block order after it
  is unchanged.
- `PgSystemPromptBuilder`: default `new(pool)` → no `<now>`; `.with_timezone(tz)`
  → `<now>` present and well-formed (presence/shape, not exact wall-clock).

**Live acceptance (DGX, post-merge):** re-run task 88's question
(`kastellan-cli ask "what were the main news stories in Germany yesterday?"
--fast`) and confirm the planner answers from `<now>` without a date-resolution
search loop, and that "yesterday" resolves against the true current date.

## License / constraints

`jiff` = Apache-2.0 OR MIT ✓ (AGPL-compatible). Pure functions preferred ✓.
Cross-platform ✓ (`jiff` reads the tz DB on both Linux and macOS). No new
worker, no sandbox/seccomp surface, no migration. Files stay well under the
500-LOC cap (`now.rs` is small; `assemble.rs` grows by one guarded block).

## Rollout

Operator sets `KASTELLAN_TIMEZONE=Australia/Sydney` in `kastellan.env` (added to
the documented env alongside the others); unset falls back to the DGX system tz.
Deploy via `scripts/build-release.sh` (the `--features live-matrix` caveat) and
restart; re-add force-routing to the unit per the standing deploy runbook.
