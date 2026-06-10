# Automatic classification-floor inference — design

**Status:** draft, awaiting user review.
**Author:** session 2026-05-16.
**Pre-reqs:** PR #68 (first real `DeterministicPolicy` rule), PR #61 (Slice A — `agent/plan.formulate` payload bump carries `classification_floor`).

## Why this slice now

The current `DeterministicPolicy` rule fires three invariants against
`(task.classification_floor, plan.data_ceiling, plan.steps[].classification)`.
The floor is producer-set: today the only producer path that ever sets it is
`kastellan-cli ask --classification-floor <DataClass>`. Without that flag the
floor defaults to `Public`, so the I1 invariant (`data_ceiling >= floor`) and
the I2 invariant (every `step.classification >= floor`) are trivially satisfied
on Public-defaulted tasks — only the I3 invariant (`step <= ceiling`) does any
real work.

The user is a senior emergency physician whose workload mix is mostly clinical
with some coding and admin sprinkled in. Forgetting `--classification-floor`
on a real clinical task is one keystroke away from a leak that DP cannot catch.
This slice closes that gap by inferring the floor from the instruction text
automatically, with a producer-trusted CLI-side keyword classifier as the
primary signal and an agent-side raise-only channel as defence in depth.

## Design choices (from brainstorming pass)

1. **Trust direction: hybrid (CLI floor + agent can raise).** The CLI runs a
   pure keyword classifier before submission; the producer-set floor is a
   commitment. The planner may emit `floor_request: Option<DataClass>` in
   the plan body; the inner loop enforces
   `effective = max(producer_floor, agent_request)` so the agent can only
   raise, never lower.
2. **Default posture: tiered scan with per-class patterns.** Each non-Public
   class has its own signal catalogue. The classifier returns the highest
   matching class (tiebreak ordering: Secret > Clinical > Personal > Public).
3. **Provenance: source tag + matched signals.** Task payload gains
   `classification_floor_source` and `classification_floor_signals`. These
   propagate into the `agent/plan.formulate` audit-row payload as two new
   keys (15 keys when `cli_inferred`, 14 keys otherwise — pure-additive).

## What ships

### 1. CLI-side keyword classifier — new pure module

Module location: `core/src/cli_audit/classification_inference.rs` (new file,
~200 LOC including tests).

Public surface:

```rust
/// Result of running the keyword classifier against an instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferredFloor {
    /// The highest matching class. `Public` when no signals matched.
    pub class: DataClass,
    /// Snake_case tags of the pattern phrases that triggered the match.
    /// Empty when `class == Public`.
    pub signals: Vec<&'static str>,
}

/// Run the tiered keyword scan against `instruction` and return the
/// inferred floor + matched signal tags. Pure function; no I/O.
pub fn infer_floor(instruction: &str) -> InferredFloor;
```

**Matching style** (matches the `ConstitutionalGuard` post-review precedent
from commit `5d48e3e`):

- Case-insensitive `contains_word` (whole-word substring via
  `match_indices` + ASCII alphanumeric byte boundaries).
- No regex, no NLP — catalogues stay small enough to read in one sitting.
- Patterns are bare phrases, not regex; multi-word phrases use `contains`
  (substring) since they have no whole-word collision risk.

**Tiered scan order** (check classes in order from highest to lowest; the
first class with ≥1 matched signal becomes the result, and ALL matched
signals from that winning class are collected — lower-class patterns are
not consulted once a winning class is found):

```
1. Check Secret patterns        — if any match: result = Secret + matched tags; STOP
2. Check Clinical patterns      — if any match: result = ClinicalConfidential + matched tags; STOP
3. Check Personal patterns      — if any match: result = Personal + matched tags; STOP
4. Default                      — result = Public + empty signals
```

Why "winning class only" instead of "every class that matched": the
provenance signal is meant to explain WHY the floor was set to its final
value. A prompt containing both `password` and `patient` warrants a Secret
floor; the operator doesn't need to know Clinical also matched — the
elevation reason is `password`. This keeps the signal list short and
operator-readable.

**Initial pattern catalogues** (this is the seed; future slices may extend):

| Class | Pattern phrases | Signal tags |
| ----- | --------------- | ----------- |
| **Secret** | `password`, `secret`, `credential`, `credentials`, `api key`, `private key`, `bearer token`, `access token`, `certificate` | `password` / `secret` / `credential` / `api_key` / `private_key` / `bearer_token` / `access_token` / `certificate` |
| **ClinicalConfidential** | `patient`, `diagnosis`, `pathology`, `radiology`, `histology`, `biopsy`, `mri`, `ct scan`, `x-ray`, `xray`, `ecg`, `ekg`, `medication`, `prescription`, `dosage`, `discharge summary`, `medical record`, `clinical`, `hl7`, `dicom`, `icd-10`, `snomed` | `patient` / `diagnosis` / `pathology` / `radiology` / `histology` / `biopsy` / `mri` / `ct_scan` / `xray` / `ecg` / `medication` / `prescription` / `dosage` / `discharge_summary` / `medical_record` / `clinical` / `hl7` / `dicom` / `icd_10` / `snomed` |
| **Personal** | `my email`, `my address`, `my phone`, `my calendar`, `family member`, `personal calendar`, `private contact` | `my_email` / `my_address` / `my_phone` / `my_calendar` / `family_member` / `personal_calendar` / `private_contact` |
| **Public** | (none — default) | (none) |

Signal tags share a name across `ecg` / `ekg` (alias), `xray` / `x-ray` (alias)
— both rendered to the same canonical tag so operators querying audit logs
don't have to enumerate variants.

False-positive defence (mirrored from CG): each catalogue uses `contains_word`
so passive forms (`silenced`, `disabled`) and substring collisions
(`I'm patient` adjective) are guarded. Phrase patterns (`ct scan`,
`discharge summary`) use `contains` directly since they have no whole-word
collision risk.

### 2. CLI wiring — `kastellan-cli ask`

Existing helper `parse_classification_floor` stays unchanged.

New flow in `run_ask`:

1. If `--classification-floor X` was passed: floor = `X`, source = `"operator"`,
   signals = `None`. If `infer_floor(instruction).class > X`, emit a
   `tracing::warn!` line carrying the suppressed inferred class and signals
   (operator-visible breadcrumb that the explicit flag silenced inference).
2. Otherwise: run `infer_floor(instruction)`. If matched class is `Public`
   and signals is empty: floor = `Public`, source = `"default"`,
   signals = `None`. Else: floor = matched class, source = `"cli_inferred"`,
   signals = matched tags.

The `tasks.payload` JSONB column gains two new optional keys when the source
is not `"operator"` / not `"default"`:

```json
{
  "instruction": "...",
  "classification_floor": "ClinicalConfidential",
  "classification_floor_source": "cli_inferred",
  "classification_floor_signals": ["patient", "pathology"]
}
```

When source is `"operator"` or `"default"`, the `classification_floor_signals`
key is omitted; `classification_floor_source` is always present.

### 3. Agent-side floor-raise channel — new `Plan.floor_request` field

`core/src/cassandra/types.rs`:

```rust
pub struct Plan {
    // ...existing fields...
    /// Agent-side request to raise the producer-set floor for the rest of
    /// the task. `None` (the default) leaves the floor unchanged. A
    /// `Some(class)` lower than or equal to the current floor is honoured
    /// as a no-op (never lowers). Round-trips through serde with
    /// `skip_serializing_if = Option::is_none` so existing fixtures stay
    /// byte-stable when the agent doesn't emit a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_request: Option<DataClass>,
}
```

**Planner-prompt update** (`prompts/agent_planner.md`): add one new paragraph
explaining the field; add `"floor_request": null,` to the JSON-schema example
between `data_ceiling` and `refused`. The `agent_prompts` SHA-256 ledger
(migration 0006 + 0011) picks up the new hash on next daemon start
automatically.

### 4. Inner-loop wiring — `core::scheduler::inner_loop::run_to_terminal`

Sequence per plan iteration (changes in bold):

1. `agent.formulate_plan(ctx)` → `Plan` (existing).
2. **Floor-raise check** (new): if `plan.floor_request.is_some()` AND
   `floor_request.rank() > ctx.classification_floor.rank()`:
   - Update `ctx.classification_floor = floor_request`.
   - Update `ctx.classification_floor_source = AgentRaised`.
   - Clear `ctx.classification_floor_signals` (signals are CLI-inference-only;
     once the agent raises, the CLI signals no longer explain the current
     floor).
3. `write_audit_plan_formulate(...)` with the (possibly elevated) floor +
   the (possibly updated) source. NOTE: when the raise happens on the
   first plan, the audit row for that first plan carries `AgentRaised`
   (not the original CLI source) — the source field is a single
   discriminator that always describes the CURRENT floor. Operators who
   need the pre-raise source can query the original `tasks.payload` row.
4. `review_chain.review(plan, ctx)` (existing). DP now sees the elevated
   floor for I1 + I2 checks.
5. Existing terminal / refused / step-dispatch logic.

**Trust direction reminder**: the agent CANNOT lower the floor. A
`floor_request` whose rank is below the current floor is a no-op — pinned
by a unit test (`agent_floor_request_lower_than_producer_is_ignored`).

### 5. Audit-row payload — `build_plan_formulate_payload`

Existing 13-key shape (post-Slice-A): `task_id`, `lane`, `plan_index`,
`decision_kind`, `plan_step_count`, `plan_terminal`, `plan_refused_principle`,
`plan_refused_reason`, `prompt_hash`, `prompt_name`, `latency_ms`,
`plan` (full Plan JSON), `classification_floor`.

This slice adds:

- `classification_floor_source: String` — always present, one of
  `"operator"`, `"cli_inferred"`, `"agent_raised"`, `"default"`. 14 keys
  when source is anything other than `"cli_inferred"`.
- `classification_floor_signals: Vec<&'static str>` — present only when
  source is `"cli_inferred"`. 15 keys total in that case.

Pure-additive: existing JSONB consumers (replay harness, observation
capture) keep working unchanged.

Signature change to `build_plan_formulate_payload`: add two new
parameters carrying the source string and an `Option<&[&'static str]>` for
the signals. The caller (`write_audit_plan_formulate` and its sole prod
call site in `run_to_terminal`) extracts these from a new pair of fields
on `TaskContext`:

```rust
pub struct TaskContext {
    // ...existing...
    pub classification_floor_source: ClassificationFloorSource,
    pub classification_floor_signals: Vec<String>,  // empty unless source == CliInferred
}

pub enum ClassificationFloorSource {
    Operator,
    CliInferred,
    AgentRaised,
    Default,
}
```

`runner.rs::run_inner_loop_for_task` reads
`task.payload["classification_floor_source"]` (defaulting to `"default"` if
absent) and `task.payload["classification_floor_signals"]` (defaulting to
empty if absent) and seeds `TaskContext` accordingly. Unrecognised
`classification_floor_source` value is a hard error (`failed_result(...)`)
parallel to the existing handling of `classification_floor`.

### 6. Test coverage

**Unit tests** (per CLAUDE.md rule #2; estimated +18-25 tests):

- `classification_inference::tests` (~12):
  - Per-class single-signal match (one per pattern catalogue): clinical
    phrases → Clinical, password phrases → Secret, family-member phrases
    → Personal, benign Python question → Public.
  - Multi-class match priority: prompt containing both `patient` and
    `password` → Secret (highest wins), signals carry both.
  - Whole-word matching rejects passive collisions: `"I'm patient"`
    adjective use → Public (since `patient` matches whole-word; this test
    pins the design intent — adjustment if false-positive surfaces).
  - Multi-word phrase: `ct scan` matches; `ct` alone does not.
  - Aliases collapse to canonical tag: `ekg` and `ecg` both → `ecg` tag.
  - Empty / whitespace-only / case variants.
- `kastellan-cli::tests` (~3): operator-explicit suppresses inference (no
  signals collected); `tracing::warn!` fires on suppression-with-elevation
  (verified by an enabled-logger test seam if practical, else pinned by
  source-tag-equals-Operator);  `--classification-floor` + matching
  prompt produces source=Operator, no signals.
- `cassandra::types::tests` (~2): `Plan.floor_request` round-trips; absent
  field is preserved as `None` after serde round-trip.
- `scheduler::inner_loop::tests` (~3):
  - `agent_floor_request_higher_than_producer_elevates_ctx`.
  - `agent_floor_request_lower_than_producer_is_ignored`.
  - `agent_floor_request_equal_to_producer_is_no_op`.
- `scheduler::inner_loop::build_plan_formulate_payload tests` (~3):
  - 14-key shape pin when source is `"default"`.
  - 15-key shape pin when source is `"cli_inferred"` (includes signals).
  - `"agent_raised"` source overrides whatever source was passed in.

**Integration tests** (~2):

- `core/tests/scheduler_inner_loop_e2e.rs::agent_raise_chain_blocks_low_step`:
  Producer submits task with floor=Public; agent emits plan with
  `floor_request: ClinicalConfidential` + a single step classified as Public;
  DP's I2 fires and the chain produces `Verdict::Block(...)`.
- `core/tests/cli_ask_e2e.rs` happy path multiset bump: assert the new
  `classification_floor_source` key (and `classification_floor_signals`
  when inference matched) is present on `agent/plan.formulate` rows.

**Test count delta estimate:** 557 → ~575-582 (+18 to +25). Zero failures,
zero warnings, zero `[SKIP]` lines on Linux.

## Non-goals (explicitly out of scope)

- **No ML/LLM classifier.** Deterministic keyword-only, per the existing
  "no NLP" posture for reviewer rules.
- **No multilingual support.** English-only — matches the user (anglophone
  EM physician).
- **No declassifier/anonymiser path.** A plan that legitimately downgrades
  a Clinical-input → Public-output (e.g. anonymised text) is still blocked
  by I2 at the elevated floor. Phase 2+ work.
- **No pattern learning from observation captures.** The catalogue is
  hand-edited.
- **No retroactive re-classification of existing audit rows.** Audit rows
  are point-in-time; new behaviour applies to future submissions.
- **No CLI override flag for the inference logic.** No `--no-infer-floor`.
  The operator can always pin explicitly with `--classification-floor`.
- **No agent-side floor LOWER request.** A `floor_request` below the
  current floor is silently a no-op (pinned by unit test).
- **No expansion of Personal-class signals beyond a tiny seed.** Personal
  patterns are fuzzy (`my email` is a strong signal but `my email is
  patient` is ambiguous). Grow the catalogue only when real workloads
  surface needs.
- **No daemon-side re-inference if the operator submits without
  `classification_floor_source`.** The CLI is the canonical inference site;
  daemon trusts what the producer wrote. Future channel-bus adapters
  (Phase 2+) must run their own inference before submitting.

## File touch list

- NEW `core/src/cli_audit/classification_inference.rs` (~200 LOC incl. tests).
- `core/src/cli_audit.rs` — make `classification_inference` a public
  submodule via `pub mod classification_inference;`.
- `core/src/bin/kastellan-cli.rs` — wire `infer_floor` into `run_ask` /
  `ask_async`; thread the inferred class + source + signals into the
  submitted payload; emit `tracing::warn!` on operator-explicit
  suppression-with-elevation; +3 unit tests.
- `core/src/cassandra/types.rs` — add `Plan.floor_request` field; +2 unit
  tests.
- `core/src/scheduler/inner_loop.rs` — add `TaskContext` fields for source
  + signals; add the floor-raise check; widen
  `build_plan_formulate_payload` signature; +6 unit tests.
- `core/src/scheduler/runner.rs` — read source + signals from
  `task.payload`; thread into `TaskContext`; fail-closed on bad source
  string.
- `core/tests/scheduler_inner_loop_e2e.rs` — +1 integration test.
- `core/tests/cli_ask_e2e.rs` — payload-shape assertions extended.
- `prompts/agent_planner.md` — one new paragraph + JSON-schema example
  update.
- NEW spec: this file.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` —
  end-of-session update.

## Migration shape

No schema migration. `task.payload` is JSONB; new keys are pure-additive.

## Files-size compliance (CLAUDE.md rule #4)

- `classification_inference.rs` — target <300 LOC including tests.
- `core/src/bin/kastellan-cli.rs` — already 1089 LOC; soft-cap breach is
  pre-existing (flagged in HANDOVER). This slice adds ~30 LOC for the
  inference wiring + 3 unit tests; modest growth, no split warranted.
- `core/src/scheduler/inner_loop.rs` — already ~700 LOC; soft-cap breach is
  pre-existing. This slice adds ~50 LOC for the floor-raise check + audit
  payload widening + 6 unit tests; modest growth, no split warranted.

## Why each design choice (one-line each)

- **Hybrid trust** — producer commits to a floor, agent can raise: catches
  both "operator forgot the flag" and "operator under-classified out of
  habit" failure modes.
- **Tiered per-class catalogues** — symmetric coverage; future expansion
  is per-class, not "elevate from Public on more signals."
- **Source + signals provenance** — operators can replay any blocked plan
  back to the elevating phrase; the rule-iteration harness can show why a
  plan's verdict differs from baseline.
- **Operator-explicit wins over inference** — preserves the producer-trust
  posture; `--classification-floor` is a deliberate commitment.
- **`contains_word` whole-word matching** — mirrors the ConstitutionalGuard
  post-review precedent so passive-form false positives stay out of the
  default catalogue.

## Open questions parked for follow-up

1. **Should `floor_request` round-trip into the plan's `data_ceiling` field
   if the agent forgot to bump that too?** Today they're independent; a
   future refactor could derive `effective_ceiling = max(data_ceiling,
   floor_request)` to keep the I3 invariant honest when the agent raises
   the floor without bumping the ceiling. Filed for a future slice if
   real workloads surface the case.
2. **Should the rule-iteration harness's replay path know about the new
   provenance keys?** The harness reads `classification_floor` from
   captures; once captures recapture with this slice live, the new keys
   land in captures automatically. Harness logic doesn't need to change
   today — the keys are documentation, not behaviour.
3. **Pattern catalogue lifecycle.** Once observation-phase captures show
   under-detection cases (clinical work that didn't elevate), add the
   missing pattern. Track in a follow-up: `pattern_misses.md` or similar.
