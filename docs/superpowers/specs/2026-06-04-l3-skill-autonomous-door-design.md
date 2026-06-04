# L3 skill invocation — the AUTONOMOUS door (design)

**Date:** 2026-06-04
**Status:** approved (brainstorm)
**Slice:** the final L3 invocation slice — agent-autonomous skill invocation.
**Predecessors (all merged to `main`):**
crystallisation writer (PR #173) → `l3_crystallise.rs` test-lift (PR #175) →
trust enum + approval gate (PR #176) → recall surfacing `<skills>` block
(PR #177) → operator-triggered invocation "the operator DOOR" (PR #178) →
`run` registry-divergence diagnostic, interim #179 (PR #180).

---

## 1. Goal

Let the **agent itself** invoke a previously-crystallised, operator-**pinned**
L3 skill from inside its planning loop — the first time the agent (driven by an
LLM that could be steered by prompt injection in tool output) can trigger
execution of a stored skill template. The agent emits an *invoke directive*; the
inner loop expands it into concrete tool steps that flow through the **unchanged**
CASSANDRA-review → sandboxed-dispatch → audit pipeline.

This completes the security-first L3 ratchet:

```
crystallise  → stored trust:"untrusted", non-executable, surfaced to no one
approve      → trust:"user_approved": operator-CLI-runnable + surfaced to the
               planner for REFERENCE ONLY
pin          → trust:"pinned": the agent MAY autonomously invoke it
```

Each rung is a separate human decision. Autonomy (the agent running a skill on
its own) requires the **strongest** rung, `pinned` — a gate that did not exist
behaviourally before this slice (`Pinned` was defined-but-command-less since
PR #176).

**In scope:** Plan-schema invoke directive; inner-loop expansion; the
already-present agent-path CASSANDRA review now governing expanded steps; the
`pin` command; the planner-prompt change; recall-surfacing invocability marker;
re-crystallisation suppression; audit; unit + mock + live-PG tests.

**Out of scope (explicitly deferred):**

- **Rerouting the operator `run` CLI to a daemon IPC trigger** (issue #179's
  structural Opt-3 remainder). The agent loop already runs *inside the daemon*
  with the live `ToolRegistry`, so agent-autonomous invocation is daemon-side for
  free and #179's env-divergence problem never arises on this path. Rerouting the
  *operator CLI* needs a brand-new daemon command channel (an IPC socket) — its
  own substantial slice. **#179 stays OPEN**; its interim diagnostic (PR #180)
  continues to cover the operator CLI.

---

## 2. Background — the machine as it stands

A crystallised L3 skill is an `L3SkillCandidate` (`core/src/cassandra/types.rs`)
stored at `layer = 3` with `metadata.template` (the candidate JSON),
`metadata.trust ∈ {untrusted, user_approved, pinned}`, and
`metadata.body_sha256`. The template holds:

- `name` (snake_case), `description`,
- `parameters: Vec<L3Param>` — each `{name (snake_case), description}`,
- `steps: Vec<L3TemplateStep>` — each `{tool, method, parameters: JSON}`, where
  string leaves of `parameters` may embed `{{param_name}}` placeholders.

The writer's validator (`memory::l3_crystallise::validate_l3_skill`) enforces a
**closed-world** placeholder invariant (declared params == referenced
placeholders) and reserved-tag / control-char guards on names, descriptions, and
param descriptions.

The reusable execution kit already exists from the operator slice
(`core/src/memory/l3_invoke.rs`), all pure:

- `substitute_template(template, args) -> Result<Vec<L3TemplateStep>, InvokeError>`
  — closed-world arity check + per-value guards (no newline/control chars,
  no `{{`/`}}` sequences, ≤ `L3_ARG_MAX_VALUE_BYTES` = 1024) + interpolation +
  no-leftover-placeholder post-condition.
- `prepare_invocation(template, stored_trust, args, live_tools) ->
  Result<Vec<L3TemplateStep>, InvokeRefusal>` — the **pure decision**: trust gate
  (`is_runnable`) → `evaluate_approval(template, live_tools)` (structural
  re-validation + baked-in `secret://` scan + every step's tool ∈ `live_tools`) →
  `substitute_template`.
- `planned_step_from_l3(step) -> PlannedStep` — synthesizes a `PlannedStep`
  (`returns`/`done_when` empty, `classification` a placeholder).
- `is_runnable(trust) = matches!(UserApproved | Pinned)` — the **operator** gate.

The agent path's execution pipeline (`core/src/scheduler/inner_loop.rs ::
run_to_terminal`) already, every iteration: formulates a `Plan`, applies any
floor-raise, writes the `plan.formulate` audit row, runs the **CASSANDRA review
chain** (`ChainReviewStage` → constitutional + deterministic), and on a
non-terminal plan dispatches each `plan.steps[i]` through
`StepDispatcher::dispatch_step` (the audited `tool_host::dispatch` chokepoint).

The deterministic reviewer
(`cassandra::deterministic::screen_plan_for_classification_violations`) enforces
three invariants over a plan: **I1** `plan.data_ceiling ≥ floor`; **I2** every
`step.classification ≥ floor`; **I3** every `step.classification ≤
plan.data_ceiling`.

Recall surfacing (`core/src/memory/l3_surface.rs`) already injects a `<skills>`
block listing `user_approved`/`pinned` skills (name + description + params) into
the planner prompt for **reference only**; the prompt explicitly forbids
invocation today.

---

## 3. Architectural decision: expand the invoke directive into steps *before* review

**Chosen — Approach A.** The agent expresses invocation as an optional
`Plan.invoke_skill` directive (sibling to `l3_skill`). The inner loop, right
after `formulate_plan` and **before** the existing CASSANDRA review, expands a
present directive into concrete `PlannedStep`s and **populates `plan.steps`**;
the plan then flows through the unchanged review → dispatch → audit pipeline. The
reviewer therefore sees **real, concrete steps**, and every existing governance
layer (constitutional, deterministic-classification, floor, the sandboxed
chokepoint, per-step audit) applies for free.

**Rejected — B (invoke as a synthetic step / `tool:"l3-skill"` the dispatcher
expands):** the dispatcher would have to recurse, and CASSANDRA would review an
opaque directive rather than the concrete steps — weakening the very governance
this slice exists to apply.

**Rejected — C (a separate invoke pipeline in the loop with its own review and
dispatch):** duplicates review/dispatch/audit logic and invites drift (the same
reason the operator slice rejected its B/C).

---

## 4. Plan schema — the invoke directive

`core/src/cassandra/types.rs` — `Plan` gains one optional field:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub invoke_skill: Option<InvokeDirective>,

pub struct InvokeDirective {
    pub name: String,                       // snake_case skill name (as surfaced)
    pub args: BTreeMap<String, String>,     // agent-supplied param values
}
```

**Mutual exclusivity (a structural guard against a confused or steered LLM):**
an `invoke_skill` plan must have `steps == []`, `l3_skill == None`, and
`decision != "task_complete"`. A plan carrying both `invoke_skill` and non-empty
`steps` (or `invoke_skill` on a terminal plan) is a **malformed directive**.

**The presence of `invoke_skill` is what triggers the invoke branch** — it is
*never* a silent fall-through to dispatching whatever `steps` were also supplied.
The loop branches whenever `plan.invoke_skill.is_some()`; the mutual-exclusivity
preconditions are validated **inside** that branch and a violation is a refusal +
replan (§6), not a normal step dispatch. The agent does not get to smuggle extra
hand-written steps alongside a templated invoke. This is captured by a single
pure, tested method:

```rust
// Err carries the precedence-violation reason for the refusal audit row.
pub fn validate_invoke(&self) -> Result<&InvokeDirective, MalformedInvoke>
// caller pattern: `if plan.invoke_skill.is_some() { match plan.validate_invoke() {…} }`
```

`args` values are agent-supplied (from the LLM). They pass the **same**
`substitute_template` value guards as the operator path — no newline/control
chars, no `{{`/`}}` sequences, ≤ 1024 bytes each.

---

## 5. New trust predicate + by-name loader

### 5.1 `is_autonomously_invocable`

In `l3_invoke.rs`:

```rust
pub fn is_autonomously_invocable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::Pinned)
}
```

A **new, stricter** predicate distinct from the operator CLI's `is_runnable`
(`UserApproved | Pinned`). The membership chain is now a strict ladder, pinned by
a single test:

```
autonomously_invocable ⊆ runnable ⊆ surfaceable
   {Pinned}            ⊆ {UA,Pin} ⊆  {UA,Pin}
```

`from_metadata_str` stays the fail-safe total parse: `untrusted` /
`user_approved` / corrupt / absent all read as not-autonomously-invocable.

### 5.2 `load_pinned_skill_by_name`

The agent references a skill by **name** (that is what `<skills>` surfaces — names,
not memory ids). A loader (placed beside the existing L3 loaders; reuses
`load_layer_by_trust(MemoryLayer::Skill, &["pinned"], cap)`):

```rust
async fn load_pinned_skill_by_name(pool, name)
    -> Result<Option<PinnedSkill>, DbError>
// PinnedSkill { memory_id: i64, template: L3SkillCandidate, body_sha256: String }
```

returns the **newest** pinned row whose `template.name == name` (newest-wins
resolves the unlikely same-name case and matches surfacing's newest-first
ordering). `None` when no pinned skill of that name exists → an
`UnknownSkill`-class refusal (§6).

---

## 6. Inner-loop expansion (the core change)

`run_to_terminal`, per iteration, after `formulate_plan` and **before** the
CASSANDRA review:

```
if plan.invoke_skill.is_some() {                       // presence triggers the branch
  match plan.validate_invoke() {                        // §4 mutual-exclusivity
    Err(malformed) => REFUSE(malformed)                 // never falls through to dispatch
    Ok(dir) => match load_pinned_skill_by_name(pool, &dir.name).await? {
        None => REFUSE("unknown or non-pinned skill: <name>")
        Some(pinned) => {
            // trust is `Pinned` by construction of the loader, but re-assert
            // via is_autonomously_invocable for a single vocabulary source.
            match prepare_invocation(&pinned.template, Pinned, &dir.args, live_tools) {
                Err(refusal) => REFUSE(refusal.reasons)         // tool gone / bad args / secret-ref / structural
                Ok(concrete_steps) => {
                    write l3.invoked (actor=scheduler)
                    plan.steps = concrete_steps
                        .map(planned_step_from_l3)
                        .map(|s| s.classification = plan.data_ceiling)
                    mark invoke_used = true                      // §7 suppression
                    // fall through to the UNCHANGED pipeline below
                }
            }
        }
    }
  }
}
// existing pipeline: CASSANDRA review(&plan) → floor → dispatch each plan.steps[i]
//                    → (after dispatch) write l3.invoke_outcome if invoke_used this iter
```

- **`live_tools`** is the daemon's live `ToolRegistry` tool-name set
  (`registry.entries()` → `BTreeSet<String>`). **No rebuild** — the loop runs in
  the daemon, which holds the real registry. This is the cleanest possible TOCTOU
  close and is exactly why #179 does not arise here.
- **`classification = plan.data_ceiling`** on every expanded step makes
  deterministic-policy **I2** (`≥ floor`) and **I3** (`≤ ceiling`) hold
  automatically, reducing classification governance for an invoke to the single
  **I1** check (`data_ceiling ≥ floor`) on the plan the agent itself declared and
  CASSANDRA enforces. The agent must therefore declare a `data_ceiling` honest
  enough to cover the skill's data, or be Blocked.
- **REFUSE** path: write one `l3.invoke_rejected` row (actor `scheduler`), push
  the refusal reason into the agent's advisory feedback channel (the same channel
  CASSANDRA `Advisory`/`Block` already feeds back), and `continue` the loop so the
  agent **replans** — with ordinary steps or a corrected directive. Bounded by the
  existing plan-count cap; every attempt is audited. No dispatch on refusal.
- After a successful invoke's steps dispatch, write one `l3.invoke_outcome` row
  (steps_executed / steps_total / any_err), mirroring the operator path.

The expansion logic is a pure helper in `l3_invoke.rs` returning either the
concrete `PlannedStep`s (classification applied) or a refusal-reasons list, so the
loop wiring stays thin and the decision is unit-tested without a database.

---

## 7. Recursion & crystallisation hygiene

- **No recursion by construction.** `L3TemplateStep` is `{tool, method,
  parameters}` — a plain tool step. A template can never contain an
  `invoke_skill`, so a skill cannot invoke a skill. The invoke replaces exactly
  one iteration's `steps`; the loop then continues normally.
- **Re-crystallisation suppression.** If any iteration of a task used
  `invoke_skill`, the terminal `l3_skill` capture is suppressed for that task
  (one `invoke_used` bool on the loop state, AND-ed into the existing
  `terminal_l3_skill` capture gate). Rationale: the invoked skill already exists;
  re-crystallising a near-duplicate of work that was itself a skill invocation
  would create churn and risks a crystallise → pin → invoke → re-crystallise
  cycle with rapid performance degradation. The dedup SHA prevents exact
  duplicates, but suppression forecloses the cycle outright.

---

## 8. The `pin` command

`hhagent-cli memory l3 pin <id>`:

- Loads the row, layer-guards to `MemoryLayer::Skill`, parses
  `metadata.{template, trust, body_sha256}`.
- **Requires current trust == `user_approved`** (enforces the ladder: a skill
  must be approved before it can be pinned; `untrusted` or already-`pinned` →
  usage refusal, no trust change).
- Re-runs `evaluate_approval(template, snapshot_tools)` against the latest
  `registry.loaded` snapshot — the **same** defense-in-depth check `approve`
  uses, justified because pinning grants the strongest privilege (autonomy).
  Fails closed (`NoRegistrySnapshot`) when no snapshot exists.
- On pass → `set_skill_trust(id, Pinned)` (the existing layer-guarded
  `jsonb_set` helper) + audit `l3.pinned`.
- On fail → audit `l3.pin_rejected`, trust unchanged.

`revoke` already downgrades **any** trust → `untrusted`, so it covers un-pinning;
no separate `unpin` is needed. `run_memory_l3`'s dispatch table and usage string
gain `pin`.

---

## 9. Recall surfacing & planner prompt

- `SurfacedSkill` gains the trust marker (or a derived `invocable: bool`) so
  `render_skill_entry` tags **pinned** entries — e.g. a trailing `[invocable]` on
  the name line. `<skills>` continues to surface **both** `user_approved` and
  `pinned` (existing behaviour preserved); only pinned carry the tag. The
  surfaceable trust set is unchanged.
- `prompts/agent_planner.md`: replace the current "There is **no
  skill-invocation field** … the runner will ignore it" paragraph with the
  invoke contract:
  - A skill tagged `[invocable]` may be invoked by emitting an `invoke_skill`
    object `{name, args}` with `steps: []` and a non-terminal `decision`.
  - `args` must supply exactly the skill's declared parameters.
  - Skills **not** tagged `[invocable]` are reference-only; you may reproduce
    their approach with ordinary `steps`, but an `invoke_skill` of a non-pinned
    skill will be **refused** by the runner (and you will be asked to replan).
  - The `invoke_skill` field is documented in the Plan schema block alongside
    `l3_skill` (both default `null`).

---

## 10. Security posture (stated explicitly)

- **Pinned-only autonomy.** Only `pinned` skills are autonomously invocable, via
  the fail-safe `from_metadata_str` + the new `is_autonomously_invocable`
  predicate. `untrusted` / `user_approved` / corrupt / absent → refused. Granting
  autonomy is a distinct human action (`pin`) gated on prior `approve`.
- **Live re-validation closes TOCTOU.** `prepare_invocation` re-runs the full
  approval gate against the daemon's **live** registry; a tool removed since
  approval/pin is refused before any dispatch.
- **CASSANDRA governs the expanded concrete steps** — unlike the operator path,
  which is an authorised operator action. The constitutional guard screens the
  task instruction; the deterministic guard enforces I1/I2/I3 over the expanded
  steps (classification = `data_ceiling`).
- **Agent arg values clear the same guards** as operator args; `argv[0]` stays
  operator-allowlisted; execution goes through the unchanged `tool_host::dispatch`
  chokepoint and unchanged sandbox backends — worst-case worker compromise still
  reaches at most the agent's own OS user (threat-model invariant intact).
- **No recursion** (§7); **no re-crystallisation cycle** (§7).
- **Refusals always audited**; injection-driven retries are bounded by the plan
  cap and every attempt leaves an `l3.invoke_rejected` trail.
- **Secret refs:** template-baked `secret://` refs are rejected at the gate
  (`evaluate_approval`'s scan). An agent-supplied *arg value* that happens to be a
  well-formed `secret://<8hex>` ref behaves exactly as on the operator path —
  redeemed at the chokepoint, audited with the ref left opaque — neither
  encouraged nor specially blocked; the §4 value guards still apply.

---

## 11. Audit contract

| Actor | Action | Payload | When |
|---|---|---|---|
| `scheduler` | `l3.invoked` | `{memory_id, skill_name, body_sha256, arg_names:[…], step_count}` | a directive accepted, before dispatch |
| `scheduler` | `l3.invoke_outcome` | `{memory_id, skill_name, steps_executed, steps_total, any_err}` | after the expanded steps dispatch |
| `scheduler` | `l3.invoke_rejected` | `{memory_id?, skill_name, body_sha256?, reasons:[…]}` | any refusal (unknown/non-pinned name, gate reject, malformed directive), before any dispatch |
| `tool:<name>` | `<method>` | existing `{req, result\|err, ms}` | one per executed step (the chokepoint) |
| `cli` | `l3.pinned` | `{memory_id, skill_name, body_sha256}` | `pin` succeeds |
| `cli` | `l3.pin_rejected` | `{memory_id, skill_name?, reasons:[…]}` | `pin` fails the gate / ladder |

Notes:

- The agent-path invoke rows reuse the existing `build_l3_invoke*_payload`
  builders verbatim; only the **actor** differs (`scheduler` vs the operator
  path's `cli`). `SCHEDULER_AUDIT_ACTOR` is the canonical constant.
- `l3.invoke_rejected` on the agent path may carry `memory_id`/`body_sha256` as
  `null` when the refusal is *unknown/non-pinned name* (no row was loaded) — so on
  this path those two fields are **optional** (the operator path, which always
  holds a loaded template before calling `invoke_l3`, keeps them required). The
  `skill_name` (the directive's requested name) is always present.
- The envelope carries arg **names** only; substituted values land in the
  per-step chokepoint rows, where existing redaction keeps any `secret://` ref
  opaque.
- New `l3.pinned` / `l3.pin_rejected` constants + pure payload builders live
  beside the existing `l3.*` ones, following the approval slice's
  `build_l3_*_payload` precedent.

---

## 12. Testing

**Pure unit:**

- `Plan` deserialization: `invoke_skill` present & well-formed; mutual-exclusivity
  rejections (`invoke_skill` + non-empty `steps`; `invoke_skill` on terminal;
  `invoke_skill` + `l3_skill`); `Plan::invoke_directive()` precedence.
- `is_autonomously_invocable`: only `Pinned` true; the
  `autonomously_invocable ⊆ runnable ⊆ surfaceable` ladder pin.
- expansion helper: maps template steps → `PlannedStep`s with `classification ==
  data_ceiling`; surfaces refusal reasons for tool-gone / bad-args / secret-ref;
  arg-value guards (newline/control/`{{}}`/over-cap) bubble up.
- `pin` gating: only `user_approved` pins; `untrusted`/`pinned`/wrong-layer refuse.
- payload builders: `l3.pinned` / `l3.pin_rejected` shapes; `l3.invoke_rejected`
  with null memory_id/sha (unknown-name case).

**Loop mock e2e** (`router_agent_mock_e2e` style — hand-rolled formulator/
dispatcher doubles, no PG):

- queued plan emits `invoke_skill` → loop loads a pinned double → expands →
  dispatches → asserts the `l3.invoked` + step + `l3.invoke_outcome` sequence and
  that `plan.steps` were the expanded template steps.
- refusal-then-replan: a `user_approved`-not-pinned (or unknown) name → one
  `l3.invoke_rejected` + advisory fed back + a second plan iteration runs.
- CASSANDRA blocks an invoke whose `data_ceiling < floor` (I1) → no dispatch.
- re-crystallisation suppression: an invoke-driven task that reaches terminal
  with an `l3_skill` field emits **no** crystallisation row.

**Live-PG e2e** (`cli_memory_l3_e2e` extension / sibling + an agent-path e2e):

- `pin` a `user_approved` `shell-exec` skill (happy) and a pin-reject (no
  snapshot / not approved).
- agent-path: seed a pinned skill + registry snapshot, drive a task whose first
  plan invokes it, assert `l3.invoked` (actor `scheduler`) + per-step
  `tool:shell-exec/...` + `l3.invoke_outcome`.
- agent-path refusal: a non-pinned name is refused and the task replans.

Regression: existing `cli_memory_l3_e2e`, `cli_memory_l3_run_e2e`,
`memory_l3_crystallise_e2e`, `l3_surface_e2e`, `prompt_assembly_e2e`,
`router_agent_mock_e2e` stay green.

---

## 13. File-size / structure

- `core/src/memory/l3_invoke.rs` — adds `is_autonomously_invocable` + the pure
  expansion helper; reuses `prepare_invocation`/`substitute_template`/
  `planned_step_from_l3`. Tests already live in the `l3_invoke/tests.rs` sibling;
  keep new unit tests there. Watch the parent's LOC; if the helper pushes it over
  the 500-cap, the test sibling already absorbs the test growth.
- `core/src/scheduler/inner_loop.rs` — the expansion wiring (load → helper →
  populate steps → audit; refusal → advisory + continue) and the `invoke_used`
  suppression bool. Tests in the existing `inner_loop/tests.rs` sibling. Watch the
  438-LOC parent.
- by-name loader in `db` / `memory` beside the existing L3 loaders.
- `core/src/cassandra/types.rs` — `InvokeDirective` + `Plan.invoke_skill` +
  `Plan::invoke_directive()`.
- `core/src/bin/hhagent-cli/memory_l3.rs` — `pin` handler + dispatch/usage entry.
  Watch its size (it has grown across the L3 CLI slices); split the `pin` handler
  to a sibling if it approaches the cap.
- `core/src/memory/l3_surface.rs` — trust/invocable marker on `SurfacedSkill` +
  `render_skill_entry` tag.
- `prompts/agent_planner.md` — the invoke contract + schema field.
- Audit constants + payload builders beside the existing `l3.*`
  (`core/src/scheduler/audit*` / `core/src/cli_audit.rs`).

---

## 14. What this completes

This is the final slice of the L3 invocation arc. After it, the full lifecycle is
on `main`: the agent crystallises a reusable skill → the operator reviews,
approves, and pins it → the agent autonomously invokes it under full CASSANDRA
governance and the sandboxed audited chokepoint. The only L3-adjacent remainder is
issue #179's operator-CLI-to-daemon-IPC reroute (a separate command-channel
slice), which this design deliberately leaves open.
