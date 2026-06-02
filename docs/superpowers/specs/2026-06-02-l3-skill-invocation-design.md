# L3 skill invocation — operator-triggered execution (design)

**Date:** 2026-06-02
**Status:** approved (brainstorm)
**Slice:** Next-TODO item 10(b-next), "the DOOR" — scoped to **operator-triggered execution only**.
**Predecessors (all merged):**
crystallisation writer (PR #173) → `l3_crystallise.rs` test-lift (PR #175) →
trust enum + approval gate (PR #176) → recall surfacing `<skills>` block (PR #177).

---

## 1. Goal

Let an operator execute an already-approved L3 skill with concrete arguments,
through the **same** sandboxed, audited path the agent uses for any ordinary
tool step. This proves the execution machinery — substitute parameters → live
re-validation → sandboxed dispatch → audit — under explicit operator control,
*before* any agent autonomy is wired.

This is the security-first decomposition the whole L3 arc has followed: ship
the *control* before the *capability* (writer before invocation; gate before
door; here: operator-driven execution before agent-autonomous invocation).

**Out of scope (explicitly deferred to the autonomous slice):**

- Agent-autonomous invocation (planner emitting an invoke directive). The
  planner prompt's "no skill-invocation field … the runner will ignore it"
  contract in `prompts/agent_planner.md` is **untouched** this slice.
- The `pin` command. `Pinned` stays defined-but-command-less. `l3 run` accepts
  both `user_approved` and `pinned` as runnable so it is forward-compatible,
  but no command produces `Pinned` yet — that distinction (e.g. "agent may
  auto-invoke pinned skills without per-call confirmation") only bites in the
  autonomous slice.
- Re-running the CASSANDRA reviewer chain on the operator path (see §6).

---

## 2. Background — the machine as it stands

An approved L3 skill is an `L3SkillCandidate`
(`core/src/cassandra/types.rs`) stored at `layer = 3` with
`metadata.trust ∈ {user_approved, pinned}` and a `metadata.template` holding:

- `name` (snake_case), `description`,
- `parameters: Vec<L3Param>` — each `{name (snake_case), description}`,
- `steps: Vec<L3TemplateStep>` — each `{tool, method, parameters: JSON}`, where
  `parameters` may embed `{{param_name}}` placeholders in string leaves.

The writer's validator (`memory::l3_crystallise::validate_l3_skill`) already
enforces a **closed-world** placeholder invariant: the set of declared
parameters equals the set of `{{…}}` placeholders referenced across all steps.

The execution seam already exists:
`StepDispatcher::dispatch_step(&PlannedStep)` →
`ToolHostStepDispatcher` looks up `step.tool` in a `ToolRegistry` → acquires a
sandboxed worker via the `WorkerLifecycleManager` → calls the audited
`tool_host::dispatch` chokepoint (one `audit_log` row per call) → maps the
result into a `StepOutcome`. An `L3TemplateStep` is structurally a
`PlannedStep` minus `returns` / `done_when` / `classification`.

The approval gate (`memory::l3_approval::evaluate_approval`) is a **pure**
function: structural re-validation + baked-in `secret://` scan + every step's
tool ∈ an injected `known_tools` set. At approval time `known_tools` comes from
the latest `registry.loaded` audit **snapshot** (the CLI cannot rebuild the
daemon's registry); the gate spec explicitly defers the **live** registry
re-check to invocation — which is this slice.

---

## 3. Execution-engine choice (the one real architectural decision)

**Chosen: Approach A — reuse `ToolHostStepDispatcher` via synthesized
`PlannedStep`s.** Substitute params → build one `PlannedStep` per template step
→ call the *same* `dispatch_step` the daemon uses. Gets registry lookup,
lifecycle/sandbox, the audited `tool_host::dispatch` chokepoint, and
result-mapping for free. The only wart: `PlannedStep`'s `returns` / `done_when`
/ `classification` fields are unused on this path and carry documented
placeholders.

Rejected alternatives:

- **B — new lower-level executor calling `tool_host::dispatch` directly.**
  Avoids the placeholder fields but re-implements registry lookup + lifecycle
  acquire + the unknown-tool / spawn-failed audit rows — duplicating the
  dispatcher and inviting drift. Not worth it.
- **C — wrap the steps in a synthetic `Plan` and run a stripped inner loop.**
  Overkill: the inner loop does replanning, CASSANDRA review, and floor logic,
  none of which apply to a deterministic operator run of a fixed template.

---

## 4. Execution locus — in-process registry rebuild

There is no daemon command channel (the daemon blocks on signals; building an
IPC command socket is its own slice). So the CLI **rebuilds the registry
in-process from current config and runs the engine in the CLI process** — the
same way `cli_ask` already constructs scheduler machinery in-process for its
e2e.

This is genuinely "live re-validation": the registry is rebuilt from the
current `HHAGENT_SHELL_EXEC_BIN` env + the `tool_allowlists` DB table + the
gliner-relex env *at invocation time*, which is strictly stronger than the
approval-time audit snapshot. Spawning sandboxed workers from the CLI process
is the same threat model as from the daemon — same OS user, same sandbox, same
chokepoint (see `docs/threat-model.md`).

**Refactor required:** `build_tool_registry` currently lives in the daemon
binary (`core/src/main.rs`) and emits a `registry.loaded` audit row as a side
effect. Split it:

- A library function in `hhagent_core` builds the registry from `(pool,
  gliner_relex_entry)` and **does not** write any audit row.
- `main.rs` calls the library function, then writes the `registry.loaded` row
  separately (its current behaviour, preserved byte-for-byte).
- The CLI calls the library function and **never** writes `registry.loaded` —
  rebuilding the registry must not corrupt the snapshot the approval gate
  reads.

`build_gliner_relex_entry` (env resolution → `Option<ToolEntry>`) moves to the
lib alongside it so the CLI builds an identical registry; when gliner env is
unset it returns `None` and that tool is simply unregistered (matching a daemon
with gliner disabled).

---

## 5. New pure module `core/src/memory/l3_invoke.rs`

All pure, no I/O, fully unit-testable:

### 5.1 `parse_args`

```
parse_args(tokens: &[String]) -> Result<BTreeMap<String, String>, InvokeError>
```

Each token is a `name=value` pair (the CLI strips the `--arg` flag). Splits on
the **first** `=` so values may themselves contain `=`. A token without `=`,
a duplicate name, or a name that is not snake_case → `InvokeError`.

### 5.2 `substitute_template`

```
substitute_template(
    template: &L3SkillCandidate,
    args: &BTreeMap<String, String>,
) -> Result<Vec<L3TemplateStep>, InvokeError>
```

- **Closed-world arity check:** the set of supplied arg names must *exactly
  equal* the template's declared parameter names. Missing → error listing the
  missing names; unknown → error listing the extras. (The writer already
  guarantees declared == referenced, so this also guarantees every `{{…}}` has
  a value and no value is wasted.)
- **Per-value guards:** each value must be free of newlines and ASCII control
  characters (`b < 0x20`) and within a byte cap (`L3_ARG_MAX_VALUE_BYTES`,
  proposed 1024). A supplied value is just a tool argument — shell-exec does no
  shell interpretation and `argv[0]` stays operator-allowlisted — but keeping
  values clean and bounded mirrors the template guards and avoids a value
  smuggling a newline into an argument vector.
- **Substitution:** walk each step's `parameters` JSON; in every **string
  leaf**, replace each `{{name}}` occurrence with `args[name]`
  (string-interpolation, so embedded forms like `"{{repo_path}}/README.md"`
  work). Reuse / mirror the writer's `scan_placeholders` byte scanner so the
  two stay consistent.
- **Post-condition:** assert no `{{…}}` remains in any produced step (defence
  against a scanner/substituter divergence) → `InvokeError` if any does.

Returns the concrete (placeholder-free) steps.

### 5.3 Re-validation reuses the gate against the live registry

Before any dispatch the orchestration runs:

1. **Trust check** — the stored row's `metadata.trust` parsed via the fail-safe
   `SkillTrust::from_metadata_str` must be `UserApproved` or `Pinned`. Anything
   else (`Untrusted` / corrupt / absent) → refuse. (Reuse `is_surfaceable`
   from `l3_surface`, or a sibling `is_runnable` predicate with identical
   membership — single vocabulary source; pinned in sync by a test.)
2. **`evaluate_approval(stored_template, live_tools)`** — the *exact same gate*
   as approval, fed the freshly-rebuilt registry's tool-name set instead of an
   audit snapshot. This is the TOCTOU close: structural re-validation +
   `secret://` re-scan + every step's tool must exist in the registry **as it
   is now**. A skill approved against an old snapshot whose tool was since
   removed is refused here.

Method existence stays dispatch-time: the registry has no method index, and an
unknown method surfaces as `METHOD_NOT_FOUND` from the worker, already mapped by
`map_dispatch_result`.

### 5.4 Orchestration `invoke_l3`

```
invoke_l3(
    pool: &PgPool,
    dispatcher: &dyn StepDispatcher,
    candidate: &L3SkillCandidate,   // the stored template (from metadata)
    stored_trust: SkillTrust,
    body_sha256: &str,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
    execute: bool,
) -> Result<InvokeReport, InvokeError>
```

Flow:

1. Trust check (§5.3.1) → on failure write one `l3.invoke_rejected` audit row
   (a refused run attempt is a security event worth a trail, mirroring
   `l3.approve_rejected`) and return a refusal report.
2. `substitute_template` (§5.2).
3. `evaluate_approval` against `live_tools` (§5.3.2) → on `Reject` write one
   `l3.invoke_rejected` audit row and return a refusal report.
4. **Dry-run (`execute == false`, the default):** return an `InvokeReport`
   carrying the concrete substituted steps for the CLI to print. **Write
   nothing, spawn nothing.**
5. **Execute (`execute == true`):**
   - Write one `l3.invoked` envelope row.
   - For each substituted step: synthesize a `PlannedStep` (a small helper
     `planned_step_from_l3` — `returns`/`done_when` empty, `classification` a
     documented placeholder, unused on this path) and call
     `dispatcher.dispatch_step(&step)`. Each call writes its own
     `tool:<name>/<method>` chokepoint row. Collect `StepOutcome`s; **stop at
     the first `StepOutcome::Err`** (mirrors `inner_loop::run_to_terminal`).
   - Write one `l3.invoke_outcome` row.
   - Return an `InvokeReport` with the per-step outcomes.

`InvokeReport` is an enum/struct distinguishing `Refused { reasons }`,
`DryRun { steps }`, and `Executed { outcomes, any_err }` so the CLI renders
each clearly and picks an exit code.

`InvokeError` (thiserror) covers arg-parse / substitution / trust / db failures.

---

## 6. Security posture (stated explicitly)

- **No CASSANDRA review on the operator path.** The reviewer chain polices
  *agent-formulated* plans; an operator running their own approved skill with
  explicit arguments is an authorised operator action. The reviewer re-enters
  in the future autonomous slice (where the agent chooses what to run). This is
  a deliberate scoping decision, not an oversight.
- **Trust gate is load-bearing and fail-safe.** Only `user_approved` / `pinned`
  run; `untrusted` / corrupt / absent trust → refuse, via the same fail-safe
  `from_metadata_str` used everywhere in the L3 arc.
- **Live re-validation closes TOCTOU.** Approval may have happened against a
  stale snapshot; run re-checks against the registry as rebuilt at invocation.
- **No new sandbox surface.** Execution goes through the unchanged
  `tool_host::dispatch` chokepoint and the unchanged sandbox backends; worst-case
  worker compromise still reaches at most the agent's own OS user.
- **Dry-run is read-only.** It substitutes + re-validates and prints; it spawns
  no worker and writes no audit row.
- **Secret refs in args are not a special feature.** Operator arg values are
  literal strings. If a value happens to be a well-formed `secret://<8hex>`
  ref, the existing `tool_host::dispatch` substitution would redeem it (existing
  behaviour, audited with the ref left opaque) — this slice neither encourages
  nor blocks it, and the value guards (§5.2) still apply.

---

## 7. Audit contract

| Actor | Action | Payload | When |
|---|---|---|---|
| `cli` | `l3.invoked` | `{memory_id, skill_name, body_sha256, arg_names:[…], step_count}` | start of `--execute` |
| `cli` | `l3.invoke_outcome` | `{memory_id, skill_name, steps_executed, steps_total, any_err}` | end of `--execute` |
| `cli` | `l3.invoke_rejected` | `{memory_id, skill_name?, reasons:[…]}` | re-validation refusal, before any dispatch |
| `tool:<name>` | `<method>` | existing `{req, result\|err, ms}` | one per executed step (the chokepoint) |

Notes:

- The envelope carries **arg *names* only**, not values. Substituted values
  land in the per-step chokepoint rows, where the existing redaction keeps any
  `secret://` ref opaque. This mirrors the `l3.approved` payload's
  redaction-aware shape.
- **Dry-run writes no audit rows** (pure preview).
- New audit-action constants live beside the existing `l3.*` constants;
  payload builders are pure functions (unit-tested for shape), following the
  approval slice's `build_l3_*_payload` precedent.

---

## 8. CLI surface

```
hhagent-cli memory l3 run <id> [--arg name=value]… [--execute]
```

- Default (no `--execute`): **dry-run** — load + layer-guard the row, rebuild
  the registry, substitute + re-validate, and print the concrete steps that
  *would* dispatch (tool/method/parameters). Spawns nothing, writes nothing.
- `--execute`: run the steps (§5.4 step 5).
- The CLI builds: `pool` + an empty `Vault::new()` + `SandboxBackends::
  default_for_current_os()` + `CompositeLifecycle::new(sandboxes)` + the
  rebuilt registry → a `ToolHostStepDispatcher`. It loads the row via
  `fetch_by_ids`, layer-guards it to `MemoryLayer::Skill`, parses
  `metadata.template` + `metadata.trust` + `metadata.body_sha256`, derives
  `live_tools` from the rebuilt registry's `entries()`, and calls `invoke_l3`.
- Clear, specific errors: an unparseable stored template, a missing/wrong-layer
  id, or a tool the rebuilt registry lacks (e.g. `HHAGENT_SHELL_EXEC_BIN`
  unset → "shell-exec not registered; set HHAGENT_SHELL_EXEC_BIN") each produce
  a distinct message and a non-zero exit.
- Exit codes: `0` on dry-run print and on all-steps-ok execute; `1` on refusal
  / step error / db error; `2` on usage error.

`run_memory_l3`'s dispatch table and usage string gain `run`.

---

## 9. Testing

**Pure unit (`l3_invoke.rs`):**

- `parse_args`: happy multi-arg; value containing `=`; missing `=`; duplicate
  name; non-snake_case name.
- `substitute_template`: happy; embedded placeholder (`{{x}}/sub`); missing
  arg; unknown arg; leftover-placeholder post-condition; newline/control-char
  value rejected; over-cap value rejected; zero-param skill with empty args.
- trust/runnable predicate: `user_approved`/`pinned` run, `untrusted`/unknown
  refuse; in-sync-with-`is_surfaceable` pin.
- payload builders: `l3.invoked` / `l3.invoke_outcome` / `l3.invoke_rejected`
  shapes.

**Live-PG e2e (`cli_memory_l3_e2e` extension or a sibling):**

- Dry-run preview of an approved `shell-exec` skill prints the concrete steps,
  spawns nothing, writes no audit rows.
- `--execute` of an approved `shell-exec` skill round-trips through the real
  sandbox; assert the `l3.invoked` + per-step `tool:shell-exec/...` +
  `l3.invoke_outcome` rows.
- An `untrusted` row refuses (no dispatch, `l3.invoke_rejected` row).
- A skill whose tool is not in the rebuilt registry refuses (live re-validation).
- Stop-at-first-error: a two-step skill whose first step errors executes
  exactly one step.

Regression: existing `cli_memory_l3_e2e` (list/approve/revoke/remove) and
`memory_l3_crystallise_e2e` stay green.

---

## 10. File-size / structure

- `core/src/memory/l3_invoke.rs` — pure fns + `invoke_l3` orchestration; target
  under the 500-LOC cap. If tests push it over, lift them to a sibling
  `l3_invoke/tests.rs` (the established pattern).
- Registry-build refactor: a new small lib module (e.g.
  `core/src/registry_build.rs`) holding the no-audit registry builder +
  gliner-entry resolver, re-exported; `main.rs` shrinks accordingly.
- CLI: extend `core/src/bin/hhagent-cli/memory_l3.rs` (watch its size; it is
  ~255 LOC today — adding `run` may approach the cap, in which case split the
  `run` handler into a sibling).
- Audit constants + payload builders beside the existing `l3.*` ones
  (`core/src/scheduler/audit*` / `core/src/cli_audit.rs`, matching the approval
  slice's placement).

---

## 11. What this unblocks

The autonomous-invocation slice (the agent emitting an invoke directive that
the inner loop expands) reuses `substitute_template` + the live-registry
`evaluate_approval` re-check verbatim, adds the CASSANDRA review back on the
agent path, introduces the `pin` command with a real `Pinned`-vs-`UserApproved`
behavioural distinction, and changes the planner prompt + `Plan` schema. None of
that is in this slice.
