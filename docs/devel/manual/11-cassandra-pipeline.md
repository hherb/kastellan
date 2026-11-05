# 11 — CASSANDRA review pipeline

CASSANDRA is the semantic-oversight layer. It sits at two points on the
path from "the planner proposed something" to "the planner reads a
result": every plan is **reviewed before any of its steps is
dispatched**, and every worker result is **screened before the planner
sees it**. This chapter walks through what each stage does, where the
code lives, what happens on each verdict (including the operator-ask
path), and how to add a rule.

> **Source of truth.** `docs/cassandra_design_plan.md` is the full design
> document; it describes more than is built. This chapter describes what
> is in the tree as of early November 2026.

---

## Where the screens sit

```
 scheduler inner loop (one iteration)            tool_host::dispatch (one step)
 ──────────────────────────────────────          ─────────────────────────────────
 planner LLM → Plan
   │ floor raise, data_ceiling resolution,
   │ invoke_skill expansion, method qualify
   ▼
 PLAN REVIEW  ChainReviewStage                    secret-ref substitution
   ConstitutionalGuard → DeterministicPolicy      spawn worker under SandboxPolicy
   │                                              worker.call()
   ├─ ConstitutionalBlock → task Blocked          │
   ├─ Block     → reason fed back, replan         ▼
   ├─ Escalate  → operator ask, task suspended    OUTPUT SCREEN  (post_process)
   ├─ Advisory  → noted, proceed                    1. catalogue  injection_guard
   └─ Approve   → dispatch each step ───────────►   2. guard model (optional, escalate-up only)
                                                   Block → placeholder + forensic audit row
                                                   │
 planner-summary SINK SCREEN  ◄────────────────────┘
   (every worker-influenced string, and the
    planner's own decision/call text, before
    it is rendered into the next prompt)
```

Plan review runs in `core/src/scheduler/inner_loop/execute.rs`; the output
screen runs in `core/src/tool_host/post_process.rs`; the sink screen in
`core/src/scheduler/inner_loop/summary/sink.rs`. None of the three has a
"skip review" flag, and the review chain is the same for the classic
planner loop and the task-notebook mode (see [chapter 15](./15-planner.md)).

The one dispatch path with no plan to review is an operator running an
already-approved L3 skill (`kastellan-cli memory l3 run <id>`): the
operator is the human in the loop there, and the skill is re-validated
against the live registry first (see
[chapter 12](./12-memory-and-recall.md#l3--skills)). It still goes through
the `tool_host` chokepoint and its output screen.

---

## Module layout

```
core/src/cassandra/
  mod.rs              Public re-exports
  types.rs / types/   Plan, PlannedStep, Verdict, DataClass, Severity, …
  review.rs           ReviewStage trait, ChainReviewStage, ConstitutionalGuard,
                      DeterministicPolicy, NoopReviewStage
  constitutional.rs   Constitutional-principle screen (Stage -1)
  deterministic.rs    Data-classification invariants (Stage 0)
  data_ceiling.rs     Resolving an omitted plan data_ceiling against the floor
  plan_digest.rs      What an operator approval binds to
  injection_guard.rs  Worker-output catalogue screen (tier 1)
  injection_guard/    Per-tool GuardProfile tests + screened_head (the
                      handoff placeholder's head, drawn only from screened text)
  guard_model/        Model-based adjudication tier (tier 2): client,
                      decide (τ), policy prompt, boot probe + timeout,
                      context and weights pins, boot report

core/src/scheduler/asks/      The operator-ask path (pure rules, DB
                              lifecycle, channel delivery)
core/src/guard_calibration/   Offline guard corpus + calibration report
core/src/observation/         Plan capture + offline replay harness
```

Public surface (re-exported through `cassandra/mod.rs`):

- `ReviewStage`, `ChainReviewStage`, `ConstitutionalGuard`,
  `DeterministicPolicy`, `NoopReviewStage`, `ReviewStageContext`
- `Plan`, `PlannedStep`, `Verdict`, `Severity`, `DataClass`,
  `DECISION_TERMINAL`
- `extract_scannable_text`, `screen`, `screen_with_profile`,
  `GuardProfile`, `InjectionDecision`, `InjectionVerdict`,
  `BLOCK_THRESHOLD`, `RELAXED_CHAT_TEMPLATE_WEIGHT`, `SCAN_BYTE_CAP`
- `GuardClient`, `GuardAdjudication` (the guard tier's own types live
  under `cassandra::guard_model`)

---

## Plan review — the `ReviewStage` trait

```rust
#[async_trait]
pub trait ReviewStage: Send + Sync {
    fn name(&self) -> &str;
    async fn review(&self, plan: &Plan, ctx: &ReviewStageContext<'_>) -> Verdict;
}
```

`ChainReviewStage` holds an ordered list of stages and runs them in order;
the first non-`Approve` verdict wins and the chain short-circuits. The
daemon builds the production chain in `core/src/main.rs`:

- **Stage -1 — `ConstitutionalGuard`** (`constitutional.rs`). Screens the
  task instruction against the five constitutional principles
  (`prompts/agent_planner.md`) with a small, English-only, case-insensitive
  substring catalogue. Each principle needs two signals (a topic phrase and
  a context phrase) except P1, where one canonical weapon-manufacturing
  phrase suffices. A hit returns `Verdict::ConstitutionalBlock { principle,
  reason }`; the task ends `Blocked` and the reviewer's verdict beats any
  planner self-refusal.

- **Stage 0 — `DeterministicPolicy`** (`deterministic.rs`). Enforces the
  data-classification invariants over the plan's typed `DataClass` fields:
  I0 the ceiling was resolved at all, I1 `ceiling ≥ floor`, I2 every
  `step ≥ floor`, I3 every `step ≤ ceiling`. First hit wins →
  `Verdict::Block`. The floor comes from upstream (the CLI keyword
  classifier in `classification_inference.rs`, the agent's raise-only
  `floor_request`, and a channel conversation's inherited class); an
  omitted `data_ceiling` is resolved to the floor itself by
  `data_ceiling.rs`, never to a constant (any constant is either fail-open
  or blocks legitimate plans).

Both stages are pure functions of the plan and context — no I/O.

### What each verdict does

`Verdict` has five variants. The inner loop's handling
(`inner_loop/execute.rs`) is:

| Verdict | Effect |
|---|---|
| `Approve` | Proceed: terminal plan completes, otherwise its steps dispatch. |
| `Advisory(concern)` | Recorded in the run's advisories; proceed. |
| `Block(reason)` | Reason is fed back into the next planning turn; the loop replans, bounded by the plan cap. |
| `Escalate(concern, severity)` | Raise an **operator ask** and suspend the task (below). |
| `ConstitutionalBlock { .. }` | Terminal: the task ends `Blocked`. |

Every verdict is written to the audit log (`actor='cassandra:chain'
action='verdict'`) before it is acted on. A plan the agent itself refused stays refused whatever a
non-constitutional verdict says.

**Honest status:** today the two shipped stages return only `Approve`,
`Block` or `ConstitutionalBlock` (a test pins this for
`DeterministicPolicy`). `Advisory` and `Escalate` are fully handled and
tested end to end, and are there for the next stage to use — a
model-based plan reviewer, or the declared-risk-class policy gate on the
roadmap.

### The operator-ask path (`Escalate`)

`scheduler/asks/` turns "a human must decide" into a durable record
rather than a degraded `Block`:

1. The inner loop computes a **plan digest** (`plan_digest.rs`) — a hash
   of everything in the plan except four narration fields the planner
   rewrites on every call (`context`, `rationale`, per-step `returns` and
   `done_when`). It is written as an *exclusion* list on purpose, so a new
   `Plan` field is counted by default.
2. `asks::raise_and_suspend` writes an `asks` row (kind, body, closed
   option set, deadline), parks the task in `awaiting_operator` with its
   run history, and audits `ask.raised`.
3. `asks::delivery` renders the ask and queues it to the channel the task
   came from (Matrix today; see
   [chapter 16](./16-channels.md#operator-asks-escalation)). Delivery
   never fails the ask: the row is already committed, and an
   `ask.delivered` / `ask.undelivered` / `ask.delivery_failed` row records
   what happened.
4. The operator answers with `/approve TOKEN` or `/deny TOKEN` in that
   conversation, or from the CLI with `kastellan-cli inbox list|show|resolve
   <ask-id> approve|deny`. The answer is a closed choice, never text that
   reaches a plan. First responder wins (a guarded `UPDATE … WHERE
   state='pending'`).
5. On resume the agent **replans from scratch** and the new plan is
   reviewed again. If it escalates with the same digest as an approved
   ask, the approval applies (`ask.approval_applied` is audited); a
   different plan escalates afresh. Approval never becomes a bypass.

An unanswered ask expires after `KASTELLAN_ASK_DEADLINE_S` (default 24 h)
and its task fails closed. If the ask cannot be raised at all, the task
fails — it does not quietly degrade to `Block`.

---

## Output screen, tier 1 — the catalogue (`injection_guard`)

The sandbox can stop a worker from reaching the network; it cannot stop a
worker's *output* from carrying a prompt-injection payload that the
planner might follow next turn. `injection_guard.rs` is a pure catalogue
scan over the result, run in `tool_host::post_process::screen_result`
after `worker.call()` returns and before anything else sees the result.

1. `extract_scannable_text` walks the JSON value's string leaves (keys,
   numbers and booleans are dropped, so the catalogue cannot fire on JSON
   shape), up to `SCAN_BYTE_CAP` (64 KiB).
2. `normalize` lowercases and strips zero-width code points.
3. The catalogue (22 rules, each `weight` + `pattern` + `class`, classes
   `instruction_override`, `role_hijack`, `secret_exfiltration`,
   `unsafe_tool_coercion`) is matched as substrings and the weights of
   the hits are summed (cap 1.0).
4. Score ≥ `BLOCK_THRESHOLD` (0.70) → `Block`.

**Per-tool profiles.** `GuardProfile::for_tool` picks `Relaxed` for the
tools whose legitimate output often *discusses* injection (`web-fetch`,
`web-search`, `web-research`, `browser-driver`) and `Strict` for
everything else. Under `Relaxed` the chat-template rules share one capped
contribution below the threshold (a `const` assertion keeps it below), so
a security blog post is not blocked but a corroborated attack still is.

### What happens on Block

- The result is replaced by a placeholder carrying `injection_blocked:
  true` and `WITHHELD_NOTE`, so the planner sees an intelligible "withheld,
  do not re-run" rather than a silent gap. Score and reason codes stay in
  the audit row only — they would tell a compromised planner which
  defence fired.
- A forensic `actor='policy' action='injection.blocked'` row carries only
  the SHA-256 of the scanned body, its length, the score, the class codes
  and the `tier` (`catalogue`, `guard_model` or `sink`). **Never** the raw
  text — pinned by `policy_audit_row_contains_no_substring_of_blocked_body`
  in `core/tests/injection_guard_e2e.rs`.
- A worker result that itself carries the `injection_blocked` key at its
  top level is withheld too (only the core may say that).

### Known limitations

Substring matching after normalisation is **trivially evadable** by
someone who knows the catalogue: narrow visible whitespace, leetspeak,
non-English phrasing, or stacking 0.40-weight patterns one at a time. The
guard corpus has cases for each. The catalogue is a cheap first line, not
a complete one — which is why tier 2 exists, and why the sandbox and the
egress allowlist remain the actual boundary.

---

## Output screen, tier 2 — the guard model (optional)

`cassandra/guard_model/` adds a local safety classifier behind the
catalogue. After a study of IBM Granite Guardian and Mistral's
Shieldstral, the shipped pick is **Shieldstral 1.0 3B** (GGUF, served by
`llama-server`). It is read from the distribution of its first output
token: the router asks for logprobs (`ChatRequest::with_logprobs`),
`llm_router::logprob_score` renormalises yes/no into a probability `p`,
and `decide` blocks when `p ≥ τ`.

Properties that bind anyone touching it:

- **Advisory defence-in-depth, not a gate.** Measured recall is about
  two-thirds at zero false positives, with misses concentrated on
  narrative framing. Nothing downstream may relax because the tier is on.
- **Escalate-up only.** It runs only on results the catalogue *allowed*,
  and can only turn Allow into Block. A catalogue Block short-circuits.
- **Fails open at runtime** on a router error or timeout (an attacker who
  can serve the agent a page must not get a denial-of-service oracle),
  but **fails closed at boot**: a half-configured tier, a τ outside
  (0, 1], a zero timeout, an unreachable backend, or a backend context
  too small for a worst-case document all stop the daemon, after writing
  a `guard_tier.boot_refused` audit row. `KASTELLAN_REQUIRE_GUARD=1`
  also makes an *unconfigured* tier fatal.
- **The policy prompt is a measured artefact** (`guard_model/policy.rs`,
  drift-pinned by `POLICY_DIGEST`). Do not reword it for readability; a
  reword moves every score and needs a re-calibration.

Configuration (all unset ⇒ tier off): `KASTELLAN_LLM_GUARD_URL`,
`KASTELLAN_LLM_GUARD_MODEL`, and `KASTELLAN_LLM_GUARD_TAU` — a **required**
operator input with no default (the current calibrated value is in
`docs/deploy/operator-env.md`). `KASTELLAN_LLM_GUARD_TIMEOUT_MS` pins the
per-call budget; otherwise a boot-time throughput probe derives it. Set
`KASTELLAN_GUARD_SERVER_BIN` and `KASTELLAN_GUARD_SERVER_MODEL` as well and
the installer adds a supervised `kastellan-guard` unit to
`kastellan.target`; core waits up to about 60 s at boot for it to load.
The guard endpoint is its own router config (`RouterConfig::for_guard`),
not the planner's — see [chapter 13](./13-llm-router.md).

### Calibration corpus

The labelled corpus lives in two places:

- `tests/guard/corpus/` — committed hand-written and catalogue-derived
  cases (`cat-*`, `inj-*`, `safe-*`), one JSON file per case.
- `tests/guard/manifest/` — metadata-only references (`cap-*`) to
  captured third-party documents; **no third-party text is committed**.
  `kastellan-cli guard capture --manifest tests/guard/manifest --out <dir>
  [--record]` materialises them by driving the real sandboxed `web-fetch`
  worker through the chokepoint, so the text scored is the text
  production sees, and verifies each case's pinned hash.

`kastellan-cli guard calibrate [--corpus DIR] [--tau F] [--per-case]
[--weights-unpinned]` scores a corpus through the shipping adjudicator and
prints a confusion matrix per provenance stratum. It checks the model
weights against the SHA-256 pinned in `guard_model/weights_pin/` (a τ
fitted on other weights does not transfer); `--weights-unpinned` is for
evaluating a candidate model. Both commands are offline tooling and need
the guard URL/model set.

---

## Planner-side screening

Two later changes moved part of the injection guard's old job:

- **The planner reads labelled JSON, not flattened text.**
  `scheduler/inner_loop/result_view.rs` prunes a successful result to
  budget while keeping identifier-shaped keys, numbers and booleans
  (`extract_scannable_text` is now the *screen's* view only). Keys never
  pass the guard model, so non-identifier keys are dropped, and an
  identifier (id, URL, path) is shown whole or not at all.
- **The sink screen is the single mandatory screen in front of the
  prompt.** Every worker-influenced string the planner summary renders
  goes through `summary/sink.rs` with the source tool's profile, and the
  planner's *own* `decision` text and step calls are screened there too
  (under `Strict`) before they are shown back to it. A block renders
  `[withheld: failed injection screen]` and writes an
  `injection.blocked` row with `tier: "sink"` and a `part` field
  (`decision`, `call` or `outcome`).

Two more screen points cover content that bypasses the first window: an
oversized result is stashed in the handoff cache with a head drawn only
from screened text (`injection_guard/screened_head.rs`), and every
`fetch_handoff` slice is re-screened under `Strict`
(`scheduler/tool_dispatch/fetch_screen.rs`) because it may lie past the
64 KiB the chokepoint read.

---

## Checks at dispatch

The design plan's "runtime verification" — checking that what executes
matches what was approved — is realised today as a set of mechanical
re-checks rather than a separate stage:

- Only the reviewed plan's steps are dispatched, after method names were
  qualified *before* review, so CASSANDRA reviewed the method that goes on
  the wire (`scheduler/tool_dispatch/method_qualify.rs`).
- `ToolHostStepDispatcher` refuses a tool not in the registry
  (`step.unknown_tool`), and every call goes through `tool_host::dispatch`
  — secret-ref redemption (fail-closed), leak-scanner provisioning for
  net workers, the sandbox, per-tool allowlists, and the egress proxy.
- An L3 skill invoked by the agent or operator is re-validated against
  the live registry, re-scanned for `secret://` refs and SHA-re-hashed
  before dispatch (the TOCTOU close in `memory/l3_invoke/pure.rs`).

A dedicated deviation detector (plan vs. actual call sequence) is not
built.

---

## Offline rule iteration — capture and replay

`core/src/observation/` lets you design rules against real planner output
instead of guesses:

- `core/tests/observation_capture.rs` (`#[ignore]`, needs a live LLM)
  runs the fixtures under `tests/observation/fixtures/` through the live
  agent and writes one capture per (fixture, date, model) under
  `tests/observation/captures/`, never overwriting.
- `kastellan-cli observation replay [--captures-dir PATH] [--model SLUG]`
  re-runs every captured plan through the production review chain — pure,
  no DB, no LLM — and prints which fixtures' verdicts changed against the
  recorded baseline.

---

## Adding a new rule

| Rule type | Where to add it |
|-----------|------------------|
| Constitutional constraint (Stage -1) | A phrase set or branch in `constitutional.rs`. Test both the block path and the `(principle, reason)` tag. |
| Deterministic rule (Stage 0) | A new invariant in `deterministic.rs` (keep the declared order; first hit wins). Test a `Plan` fixture that should and one that should not trigger it. |
| New plan-review stage | A struct implementing `ReviewStage`; register it in the chain built in `core/src/main.rs`. If it can emit `Escalate`, the ask path is ready for it — add a test through `scheduler_asks_e2e`. |
| Injection-catalogue pattern | One `Rule { weight, pattern, class, chat_template }` line in `CATALOGUE` in `injection_guard.rs`. Keep it greppable: one entry per line, no helpers. Add accept/reject cases to the test table, and a corpus case under `tests/guard/corpus/`. |

Before and after changing a plan-review rule, run `observation replay` and
read the verdict deltas.

Reviewers will reject a rule that:

- Reads or writes state outside the `Plan` / result it was handed (stages
  and screens are pure functions).
- Logs raw scanned text into any audit row (SHA-256 + length + score +
  class codes is the pattern).
- Adds a "bypass" flag or a "trusted worker" exemption.
- Makes the guard tier fail closed at runtime, or makes a plan reviewer
  fail open. The asymmetry is deliberate: the guard judges *fetched
  content* (failing closed hands attackers a DoS), a plan reviewer judges
  *the agent's own proposed action*.

---

## Testing the pipeline

- Unit tests sit beside each module (`#[cfg(test)]`, often lifted to a
  sibling `tests.rs` — see [chapter 14](./14-test-lift-refactors.md)).
- Integration tests in `core/tests/`: `injection_guard_e2e.rs`,
  `scheduler_asks_e2e.rs`, `observation_replay_e2e.rs`, and the
  `guard_*_e2e.rs` family (boot refusal, boot row, tier wiring, calibrate
  and capture CLIs, weights pin).

```sh
cargo test -p kastellan-core cassandra
cargo test -p kastellan-core --test injection_guard_e2e
cargo test -p kastellan-core --test scheduler_asks_e2e
```

The guard-tier e2es need Postgres, a working sandbox and the worker
binaries, and **skip-as-pass** without them. To make a run count as
evidence, use the gate profile, which sets the PG, sandbox and guard
REQUIRE knobs together and checks the counts:

```sh
bash scripts/run-e2e-gate.sh guard-tier
```

Run by hand, `[SKIP]` / `[E2E]` lines go to stderr — add
`-- --nocapture`, and remember that `KASTELLAN_GUARD_REQUIRE_E2E` alone
does not demand Postgres or the sandbox (see
[chapter 5](./05-build-test-run.md)).
