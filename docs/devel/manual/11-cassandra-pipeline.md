# 11 — CASSANDRA review pipeline

CASSANDRA is the semantic-oversight layer. It runs inside
`core::tool_host::dispatch()` so every worker call passes through it —
there is no path that skips review. This chapter walks through what each
stage does, where the code lives, and how to add a new rule.

> **Source of truth.** `docs/cassandra_design_plan.md` is the full design
> document. This chapter is the developer-facing tour.

---

## Two screens around the worker call

```
                ┌────────── tool_host::dispatch ──────────┐
                │                                          │
   PlannedStep ─┤  pre-spawn (plan review):                │
                │    constitutional → deterministic → …    │
                │                                          │
                │  if Block: short-circuit, audit, return  │
                │                                          │
                │  spawn worker under SandboxPolicy        │
                │  worker.call() → result                  │
                │                                          │
                │  post-call (output screen):              │
                │    injection_guard::screen(result)       │
                │                                          │
                │  if Block: replace result with redacted  │
                │            placeholder + audit row       │
                │  if Allow: return result to scheduler    │
                └──────────────────────────────────────────┘
```

Both screens are inside the chokepoint. There is no "skip review" flag.

---

## Module layout

```
core/src/cassandra/
  mod.rs              Public re-exports
  types.rs            Plan, PlannedStep, Verdict, DataClass, Severity, …
  review.rs           ReviewStage trait, ChainReviewStage runner,
                      ConstitutionalGuard and DeterministicPolicy stubs,
                      NoopReviewStage
  constitutional.rs   Five hard-coded constitutional constraints (Stage -1)
  deterministic.rs    Data-classification invariants (Stage 0)
  injection_guard.rs  Worker-output prompt-injection screen
```

Public surface (re-exported through `mod.rs`):

- `ReviewStage` trait, `ChainReviewStage`, `ConstitutionalGuard`,
  `DeterministicPolicy`, `NoopReviewStage`, `ReviewStageContext`
- `Plan`, `PlannedStep`, `Verdict`, `Severity`, `DataClass`,
  `DECISION_TERMINAL`
- `extract_scannable_text`, `screen`, `InjectionDecision`,
  `InjectionVerdict`, `BLOCK_THRESHOLD`, `SCAN_BYTE_CAP`

---

## Pre-spawn review — the `ReviewStage` trait

```rust
pub trait ReviewStage: Send + Sync {
    fn review(&self, plan: &Plan, ctx: &ReviewStageContext) -> Verdict;
}
```

`ChainReviewStage` holds an ordered list of `Box<dyn ReviewStage>` and
runs them in order. The first `Verdict::Block` wins and the chain
short-circuits.

Today the chain is wired with two stages:

- **Stage -1 — `ConstitutionalGuard`** (`constitutional.rs`). Five
  hard-coded constraints, currently stubbed to always `Approve` so the
  baseline agent loop is measurable. The eventual real implementation
  replaces the stub in place; the trait, types, and chain runner are
  stable.

- **Stage 0 — `DeterministicPolicy`** (`deterministic.rs`).
  Data-classification invariants (e.g. "PII cannot be sent to a frontier
  LLM"). Also stubbed in Phase 0, real logic follows the same
  replace-in-place pattern.

The `#[non_exhaustive]` `Verdict` enum lets future stages introduce a
`Review` tier without breaking callers.

---

## Post-call screen — `injection_guard`

`injection_guard.rs` is a pure-function catalogue scan that runs over the
worker's result body **after** `worker.call()` returns Ok and **before**
the scheduler sees the result. It exists because the parent-side sandbox
can stop a worker from making network calls, but it cannot stop a
worker's *output* from carrying a prompt-injection payload that the
agent's LLM might obediently follow on the next turn.

### How the scan works

1. `normalize` lowercases and strips zero-width code points.
2. The catalogue (22 entries in Slice 1) is matched as substrings.
   Each entry has:
   - a class code (`instruction_override`, `role_hijack`,
     `secret_exfiltration`, `unsafe_tool_coercion`)
   - a weight in `[0.0, 1.0]`
3. Per-rule weights for matches are summed (capped at 1.0).
4. If the score ≥ `BLOCK_THRESHOLD` (0.70) → `InjectionDecision::Block`.
   Otherwise `Allow`.

### What happens on Block

`tool_host::dispatch`:

- Replaces the worker result with a redacted placeholder JSON.
- Writes an audit row with `actor='policy' action='injection.blocked'`
  carrying only the SHA-256 of the scanned body, its length, the score,
  and the class codes hit.
- **Never** persists the raw scanned text. The privacy invariant is
  pinned by an integration test
  (`policy_audit_row_contains_no_substring_of_blocked_body`).

### Known limitations (Slice 1)

`injection_guard.rs` documents these in its module comment, but worth
knowing:

- Substring matching after normalisation. **Trivially evadable** by an
  attacker who knows the catalogue (narrow visible whitespace, leetspeak,
  non-English equivalents).
- Two 0.40 patterns sum to 0.80 ≥ threshold, so a careful attacker can
  stay just under any single pattern's score.

The guard is a cheap first line of defence, not a complete one. A Slice 2
candidate (whitespace fold, leetspeak fold, combining-character
permutations) is on the roadmap.

---

## Adding a new rule

| Rule type | Where to add it |
|-----------|------------------|
| Constitutional constraint (Stage -1) | New method or branch inside `ConstitutionalGuard::review` in `constitutional.rs`. Add a test that pins both the block path and the audit row payload. |
| Deterministic classification rule (Stage 0) | New branch in `DeterministicPolicy::review` in `deterministic.rs`. Add a test using a `Plan` fixture that should and should not trigger it. |
| New pre-spawn stage | New struct implementing `ReviewStage`; register in the `ChainReviewStage` built by `tool_host::dispatch`. |
| Injection-guard pattern | One catalogue entry in `injection_guard.rs` — weight, pattern, class code. Keep the catalogue greppable: one entry per line, no clever helpers. Add an `accepts`/`rejects` pair in the existing test table. |

Reviewers will reject a new rule that:

- Reads or writes any state outside the `Plan` / result it was handed
  (rules must be pure functions).
- Logs the raw scanned text into the audit row (the SHA-256 + length +
  score + class codes pattern is mandatory).
- Adds a "bypass" flag or "trusted worker" exemption (there isn't one,
  by design).

---

## Testing the pipeline

- `core/src/cassandra/*.rs` — unit tests inline in each module's
  `#[cfg(test)] mod tests`.
- `core/tests/` — integration tests that exercise dispatch end-to-end,
  including the privacy invariant for the injection guard.

When you change the catalogue or a stage's branch logic, run:

```sh
cargo test -p kastellan-core cassandra
cargo test -p kastellan-core injection_guard
```

`-- --nocapture` will show the audit-row payloads if you need to debug
why a test pinned a particular shape.
