# L3 skill autonomous-door Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the agent autonomously invoke an operator-**pinned** L3 skill from inside its planning loop, expanding the directive into concrete tool steps that flow through the existing CASSANDRA-review → sandboxed-dispatch → audit pipeline.

**Architecture:** The agent emits an optional `Plan.invoke_skill` directive. In `run_to_terminal`, *before* the existing CASSANDRA review, a present directive is loaded (newest pinned skill by name), re-validated against the daemon's **live** `ToolRegistry` via the reused `prepare_invocation`, and expanded into `PlannedStep`s (`classification = plan.data_ceiling`) that **populate `plan.steps`**. The reviewer then sees concrete steps; dispatch/audit are unchanged. A refused directive is audited and fed back as a block so the agent replans. Autonomy is gated on a new `pinned` tier (new `pin` command + `is_autonomously_invocable`); re-crystallisation is suppressed for invoke-driven tasks.

**Tech Stack:** Rust (workspace crates `hhagent-core`, `hhagent-db`), `sqlx`/Postgres, `serde_json`, `async-trait`, `thiserror`. Tests: `cargo test`, live-PG e2e gated on `HHAGENT_PG_BIN_DIR` (skip-as-pass otherwise).

**Spec:** `docs/superpowers/specs/2026-06-04-l3-skill-autonomous-door-design.md`

**Build/test prelude (every task):**
```sh
source "$HOME/.cargo/env"
```
Live-PG e2e on this Mac uses Postgres.app v18 — set the session-local override before running PG-gated tests:
```sh
export HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
```

---

## Task 1: `InvokeDirective` type + `Plan.invoke_skill` field + `Plan::validate_invoke()`

**Files:**
- Modify: `core/src/cassandra/types.rs` (add type near `L3SkillCandidate` ~line 126; add field to `Plan` ~line 191; add method + error in `impl Plan` ~line 247)
- Modify: every `Plan { … }` struct literal in the tree (compiler will list them; add `invoke_skill: None`)

- [ ] **Step 1: Write failing tests** — append to the existing `#[cfg(test)] mod tests` in `core/src/cassandra/types.rs` (find it; if none in this file, add `#[cfg(test)] mod tests { use super::*; … }` at the end):

```rust
#[test]
fn invoke_directive_deserializes_from_plan_json() {
    let json = r#"{
        "context":"c","decision":"act","rationale":"r","steps":[],
        "data_ceiling":"Public",
        "invoke_skill":{"name":"summarise_repo_readme","args":{"repo_path":"/tmp/x"}}
    }"#;
    let plan: Plan = serde_json::from_str(json).unwrap();
    let dir = plan.validate_invoke().expect("well-formed invoke");
    assert_eq!(dir.name, "summarise_repo_readme");
    assert_eq!(dir.args.get("repo_path").map(String::as_str), Some("/tmp/x"));
}

#[test]
fn validate_invoke_rejects_invoke_with_nonempty_steps() {
    let plan = Plan {
        context: "c".into(), decision: "act".into(), rationale: "r".into(),
        steps: vec![PlannedStep {
            tool: "shell-exec".into(), method: "shell.exec".into(),
            parameters: serde_json::json!({}), returns: String::new(),
            done_when: String::new(), classification: DataClass::Public,
        }],
        result: None, data_ceiling: DataClass::Public, refused: None,
        floor_request: None, l1_insight: None, l3_skill: None,
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default() }),
    };
    assert!(matches!(plan.validate_invoke(), Err(MalformedInvoke::HasSteps)));
}

#[test]
fn validate_invoke_rejects_invoke_on_terminal_plan() {
    let plan = Plan {
        context: "c".into(), decision: "task_complete".into(), rationale: "r".into(),
        steps: vec![], result: Some(serde_json::json!({"body":"x"})),
        data_ceiling: DataClass::Public, refused: None, floor_request: None,
        l1_insight: None, l3_skill: None,
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default() }),
    };
    assert!(matches!(plan.validate_invoke(), Err(MalformedInvoke::Terminal)));
}

#[test]
fn validate_invoke_rejects_invoke_with_l3_skill() {
    let plan = Plan {
        context: "c".into(), decision: "act".into(), rationale: "r".into(),
        steps: vec![], result: None, data_ceiling: DataClass::Public, refused: None,
        floor_request: None, l1_insight: None,
        l3_skill: Some(L3SkillCandidate {
            name: "s".into(), description: "d".into(),
            parameters: vec![], steps: vec![],
        }),
        invoke_skill: Some(InvokeDirective { name: "s".into(), args: Default::default() }),
    };
    assert!(matches!(plan.validate_invoke(), Err(MalformedInvoke::HasL3Skill)));
}

#[test]
fn plan_without_invoke_skill_round_trips_without_the_key() {
    // skip_serializing_if keeps existing fixtures byte-stable.
    let plan = Plan {
        context: "c".into(), decision: "act".into(), rationale: "r".into(),
        steps: vec![], result: None, data_ceiling: DataClass::Public, refused: None,
        floor_request: None, l1_insight: None, l3_skill: None, invoke_skill: None,
    };
    let s = serde_json::to_string(&plan).unwrap();
    assert!(!s.contains("invoke_skill"), "absent directive must not serialize a key");
}
```

- [ ] **Step 2: Run tests to verify they fail (do not compile / type missing)**

Run: `cargo test -p hhagent-core --lib cassandra::types 2>&1 | head -30`
Expected: compile errors — `InvokeDirective`, `MalformedInvoke`, `validate_invoke`, and the `invoke_skill` field don't exist yet.

- [ ] **Step 3: Add `InvokeDirective` + `MalformedInvoke`** — insert after `L3SkillCandidate` (after line 126) in `core/src/cassandra/types.rs`. Ensure `use std::collections::BTreeMap;` is present at the top of the file (add it if missing):

```rust
/// Agent-emitted directive to autonomously invoke a pinned L3 skill.
/// Sibling to [`Plan::l3_skill`]: where `l3_skill` *crystallises* a new
/// skill on a terminal plan, `invoke_skill` *runs* an already-pinned one
/// on a non-terminal plan. The inner loop expands it into concrete
/// [`PlannedStep`]s before review; only `pinned` skills are invocable
/// (see `crate::memory::l3_invoke::is_autonomously_invocable`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InvokeDirective {
    /// snake_case skill name, exactly as surfaced in the `<skills>` block.
    pub name: String,
    /// Agent-supplied parameter values (param name → literal value). Must
    /// supply exactly the skill's declared parameters; values are guarded
    /// by `substitute_template` (no newline/control/`{{`/`}}`/over-cap).
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

/// Why a plan carrying an `invoke_skill` directive is structurally
/// malformed. A malformed directive is a refusal (the agent replans);
/// it is NEVER a silent fall-through to dispatching co-supplied steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedInvoke {
    /// `invoke_skill` present alongside non-empty `steps`.
    HasSteps,
    /// `invoke_skill` present on a terminal plan (`decision == "task_complete"`).
    Terminal,
    /// `invoke_skill` present alongside an `l3_skill` crystallisation candidate.
    HasL3Skill,
}

impl std::fmt::Display for MalformedInvoke {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            MalformedInvoke::HasSteps => "invoke_skill may not be combined with hand-written steps",
            MalformedInvoke::Terminal => "invoke_skill may not appear on a terminal (task_complete) plan",
            MalformedInvoke::HasL3Skill => "invoke_skill may not be combined with an l3_skill crystallisation",
        };
        f.write_str(s)
    }
}
```

- [ ] **Step 4: Add the `invoke_skill` field to `Plan`** — after the `l3_skill` field (after line 191) in `core/src/cassandra/types.rs`:

```rust
    /// Agent-emitted directive to autonomously invoke a pinned L3 skill
    /// (mutually exclusive with `steps` / `l3_skill` / terminal — see
    /// [`Plan::validate_invoke`]). The inner loop expands it into concrete
    /// `steps` before the CASSANDRA review. Round-trips with
    /// `skip_serializing_if = Option::is_none` so non-invoking plans stay
    /// byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoke_skill: Option<InvokeDirective>,
```

- [ ] **Step 5: Add `validate_invoke` to `impl Plan`** — inside `impl Plan` (after `completion_skill`, ~line 246):

```rust
    /// Validate a plan that carries an `invoke_skill` directive. Returns
    /// the directive when the mutual-exclusivity preconditions hold
    /// (`steps == []`, not terminal, no `l3_skill`); otherwise the
    /// specific [`MalformedInvoke`] reason. Callers branch on
    /// `self.invoke_skill.is_some()` FIRST — presence triggers the invoke
    /// path; this method never lets a malformed directive fall through to
    /// normal step dispatch.
    pub fn validate_invoke(&self) -> Result<&InvokeDirective, MalformedInvoke> {
        let dir = self
            .invoke_skill
            .as_ref()
            .expect("validate_invoke called with no invoke_skill");
        if !self.steps.is_empty() {
            return Err(MalformedInvoke::HasSteps);
        }
        if self.decision == DECISION_TERMINAL {
            return Err(MalformedInvoke::Terminal);
        }
        if self.l3_skill.is_some() {
            return Err(MalformedInvoke::HasL3Skill);
        }
        Ok(dir)
    }
```

- [ ] **Step 6: Add `invoke_skill: None` to every `Plan { … }` literal** — build the workspace; the compiler lists each missing-field site. Add `invoke_skill: None,` after the `l3_skill: None,` line in each (production + tests). Known sites include `core/tests/scheduler_inner_loop_e2e.rs` (`task_complete_plan`, `one_step_plan`), `core/src/scheduler/inner_loop_audit.rs` (test plans ~lines 301/347), and any others surfaced.

Run: `cargo build -p hhagent-core --tests 2>&1 | grep -A2 "missing field" | head -40`
Fix each listed site, then re-run until clean.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p hhagent-core --lib cassandra::types 2>&1 | tail -20`
Expected: the 5 new tests PASS.

- [ ] **Step 8: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/cassandra/types.rs core/tests/scheduler_inner_loop_e2e.rs core/src/scheduler/inner_loop_audit.rs
# add any other files the compiler flagged in Step 6
git commit -m "feat(l3): Plan.invoke_skill directive + validate_invoke (autonomous door)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `plan.formulate` audit payload carries `invoke_skill`; bump key-count pins

**Files:**
- Modify: `core/src/scheduler/inner_loop_audit.rs` (`build_plan_formulate_payload` ~line 79; key-count pin tests ~line 390 and ~434)

**Context:** The `plan.formulate` payload embeds compact plan fields as explicit keys (`l1_insight`, `l3_skill`) with explicit-null when absent so `WHERE payload ? 'key'` finds every row, and pins the key count (26 default / 27 cli_inferred). Add an `invoke_skill` compact key the same way.

- [ ] **Step 1: Update the key-count pin tests to expect one more key** — in `core/src/scheduler/inner_loop_audit.rs`:

Find `build_plan_formulate_payload_pins_twenty_six_keys_for_default_source` (~line 390). In its expected-keys list (~line 422, the array containing `"l1_insight", "l3_skill",`) add `"invoke_skill"`:

```rust
            "l1_insight", "l3_skill", "invoke_skill",
```
and change the asserted count from 26 to 27 (update the literal and, if present, the test name's textual count — rename the fn to `…pins_twenty_seven_keys_for_default_source` and update its doc comment).

In `build_plan_formulate_payload_cli_inferred_source_has_27_keys_with_signals` (~line 434) change 27 → 28 (and rename to `…has_28_keys_with_signals`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p hhagent-core --lib inner_loop_audit 2>&1 | tail -20`
Expected: the two key-count tests FAIL (payload still has the old count; `invoke_skill` key absent).

- [ ] **Step 3: Add the `invoke_skill` key to the payload** — in `build_plan_formulate_payload`, immediately after the block that inserts `"l3_skill"` (~line 147-150), add (compact form: name + arg_count, explicit null when absent, mirroring the `l3_skill` `{name, step_count}` compaction):

```rust
    // Compact `invoke_skill` projection — `{name, arg_count}` when the
    // agent emitted an invoke directive, explicit JSON null otherwise so
    // `WHERE payload ? 'invoke_skill'` finds every row (mirrors the
    // `l1_insight` / `l3_skill` precedent). The full directive (incl. arg
    // values) is NOT embedded here; arg names/values surface in the
    // separate `l3.invoked` envelope + per-step chokepoint rows.
    obj.insert(
        "invoke_skill".into(),
        match &plan.invoke_skill {
            Some(d) => serde_json::json!({ "name": d.name, "arg_count": d.args.len() }),
            None => serde_json::Value::Null,
        },
    );
```
(Use the same local accumulator variable name the function already uses — read the function to confirm it is `obj`; if the function builds a `serde_json::Map` under a different binding, match it.)

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p hhagent-core --lib inner_loop_audit 2>&1 | tail -20`
Expected: all `inner_loop_audit` tests PASS, including the renamed 27/28-key pins.

- [ ] **Step 5: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/scheduler/inner_loop_audit.rs
git commit -m "feat(l3): plan.formulate payload carries compact invoke_skill key (27/28 keys)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `l3_invoke` pure additions — autonomy predicate, classed step mapper, agent expansion

**Files:**
- Modify: `core/src/memory/l3_invoke.rs` (add after `is_runnable` ~line 236 and after `planned_step_from_l3` ~line 253)
- Modify: `core/src/memory/l3_invoke/tests.rs` (add unit tests)

- [ ] **Step 1: Write failing tests** — append to `core/src/memory/l3_invoke/tests.rs`:

```rust
#[test]
fn is_autonomously_invocable_only_pinned() {
    use crate::memory::l3_approval::SkillTrust;
    assert!(is_autonomously_invocable(SkillTrust::Pinned));
    assert!(!is_autonomously_invocable(SkillTrust::UserApproved));
    assert!(!is_autonomously_invocable(SkillTrust::Untrusted));
}

#[test]
fn autonomy_ladder_is_subset_of_runnable_and_surfaceable() {
    use crate::memory::l3_approval::SkillTrust;
    use crate::memory::l3_surface::is_surfaceable;
    for t in [SkillTrust::Untrusted, SkillTrust::UserApproved, SkillTrust::Pinned] {
        if is_autonomously_invocable(t) {
            assert!(is_runnable(t), "autonomous ⊆ runnable for {t:?}");
            assert!(is_surfaceable(t), "autonomous ⊆ surfaceable for {t:?}");
        }
    }
}

#[test]
fn planned_step_from_l3_with_class_sets_classification() {
    use crate::cassandra::types::{DataClass, L3TemplateStep};
    let step = L3TemplateStep {
        tool: "shell-exec".into(), method: "shell.exec".into(),
        parameters: serde_json::json!({"argv":["echo","hi"]}),
    };
    let ps = planned_step_from_l3_with_class(&step, DataClass::ClinicalConfidential);
    assert_eq!(ps.classification, DataClass::ClinicalConfidential);
    assert_eq!(ps.tool, "shell-exec");
    // back-compat: the no-class wrapper still pins Secret.
    assert_eq!(planned_step_from_l3(&step).classification, DataClass::Secret);
}

#[test]
fn expand_for_agent_happy_sets_data_ceiling_classification() {
    use std::collections::{BTreeMap, BTreeSet};
    use crate::cassandra::types::{DataClass, L3Param, L3SkillCandidate, L3TemplateStep};
    use crate::memory::l3_approval::SkillTrust;

    let template = L3SkillCandidate {
        name: "summarise_repo_readme".into(),
        description: "d".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "p".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(), method: "shell.exec".into(),
            parameters: serde_json::json!({"argv":["cat","{{repo_path}}/README.md"]}),
        }],
    };
    let mut args = BTreeMap::new();
    args.insert("repo_path".into(), "/tmp/x".into());
    let live: BTreeSet<String> = ["shell-exec".to_string()].into_iter().collect();

    let steps = expand_for_agent(&template, SkillTrust::Pinned, &args, &live, DataClass::Personal)
        .expect("pinned + tool present + valid args");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].classification, DataClass::Personal);
    assert_eq!(steps[0].parameters["argv"][1], "/tmp/x/README.md");
}

#[test]
fn expand_for_agent_refuses_non_pinned() {
    use std::collections::{BTreeMap, BTreeSet};
    use crate::cassandra::types::{DataClass, L3SkillCandidate};
    use crate::memory::l3_approval::SkillTrust;
    let template = L3SkillCandidate {
        name: "s".into(), description: "d".into(), parameters: vec![], steps: vec![],
    };
    let err = expand_for_agent(&template, SkillTrust::UserApproved, &BTreeMap::new(),
        &BTreeSet::new(), DataClass::Public).unwrap_err();
    assert!(err.reasons.iter().any(|r| r.contains("not autonomously invocable")));
}

#[test]
fn expand_for_agent_refuses_tool_absent_from_live_registry() {
    use std::collections::{BTreeMap, BTreeSet};
    use crate::cassandra::types::{DataClass, L3SkillCandidate, L3TemplateStep};
    use crate::memory::l3_approval::SkillTrust;
    let template = L3SkillCandidate {
        name: "s".into(), description: "d".into(), parameters: vec![],
        steps: vec![L3TemplateStep {
            tool: "web-fetch".into(), method: "fetch".into(),
            parameters: serde_json::json!({}),
        }],
    };
    // live registry has only shell-exec → web-fetch refused (TOCTOU close).
    let live: BTreeSet<String> = ["shell-exec".to_string()].into_iter().collect();
    let err = expand_for_agent(&template, SkillTrust::Pinned, &BTreeMap::new(),
        &live, DataClass::Public).unwrap_err();
    assert!(!err.reasons.is_empty());
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p hhagent-core --lib memory::l3_invoke 2>&1 | head -30`
Expected: compile errors — `is_autonomously_invocable`, `planned_step_from_l3_with_class`, `expand_for_agent` don't exist.

- [ ] **Step 3: Add the autonomy predicate** — in `core/src/memory/l3_invoke.rs`, after `is_runnable` (after line 236):

```rust
/// PURE stricter gate for AGENT-autonomous invocation: only `pinned`
/// skills may be invoked by the agent itself. A strict subset of
/// [`is_runnable`] (the operator-CLI gate, which also allows
/// `user_approved`) and of
/// [`crate::memory::l3_surface::is_surfaceable`]. Granting autonomy is a
/// distinct human action (`memory l3 pin`) gated on a prior `approve`;
/// pinned-in-sync by `autonomy_ladder_is_subset_of_runnable_and_surfaceable`.
pub fn is_autonomously_invocable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::Pinned)
}
```

- [ ] **Step 4: Refactor `planned_step_from_l3` to delegate to a classed mapper** — replace the existing `planned_step_from_l3` body (lines 244-253) with:

```rust
/// Synthesize a [`PlannedStep`] from a concrete template step, with an
/// explicit `classification`. `returns` / `done_when` are empty (unused
/// by `dispatch_step`). The agent path passes `plan.data_ceiling` so the
/// deterministic policy's I2/I3 invariants hold automatically.
pub fn planned_step_from_l3_with_class(step: &L3TemplateStep, class: DataClass) -> PlannedStep {
    PlannedStep {
        tool: step.tool.clone(),
        method: step.method.clone(),
        parameters: step.parameters.clone(),
        returns: String::new(),
        done_when: String::new(),
        classification: class,
    }
}

/// Operator-path mapper: `classification` is the most conservative class
/// (`Secret`) and is UNUSED on that path (`dispatch_step` reads only
/// `tool` / `method` / `parameters`). Delegates to
/// [`planned_step_from_l3_with_class`].
pub fn planned_step_from_l3(step: &L3TemplateStep) -> PlannedStep {
    planned_step_from_l3_with_class(step, DataClass::Secret)
}
```

- [ ] **Step 5: Add the agent expansion helper** — after the `prepare_invocation` function (after line 296) in `core/src/memory/l3_invoke.rs`:

```rust
/// PURE agent-path expansion: gate on the stricter
/// [`is_autonomously_invocable`] (pinned only), re-validate + substitute
/// via [`prepare_invocation`] against the daemon's live tool set, and
/// synthesize concrete [`PlannedStep`]s whose `classification` is the
/// invoking plan's `data_ceiling` (so deterministic-policy I2/I3 hold and
/// governance reduces to the I1 check on the plan the agent declared).
///
/// On any failure returns an [`InvokeRefusal`] collecting the reason(s) —
/// the inner loop audits it (`l3.invoke_rejected`) and feeds it back so
/// the agent replans.
pub fn expand_for_agent(
    template: &L3SkillCandidate,
    stored_trust: SkillTrust,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
    data_ceiling: DataClass,
) -> Result<Vec<PlannedStep>, InvokeRefusal> {
    if !is_autonomously_invocable(stored_trust) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill trust '{}' is not autonomously invocable (agent may invoke only pinned skills)",
                stored_trust.as_str()
            )],
        });
    }
    let concrete = prepare_invocation(template, stored_trust, args, live_tools)?;
    Ok(concrete
        .iter()
        .map(|s| planned_step_from_l3_with_class(s, data_ceiling))
        .collect())
}
```

- [ ] **Step 6: Run to verify they pass**

Run: `cargo test -p hhagent-core --lib memory::l3_invoke 2>&1 | tail -20`
Expected: the 6 new tests PASS; existing `l3_invoke` tests stay green.

- [ ] **Step 7: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/memory/l3_invoke.rs core/src/memory/l3_invoke/tests.rs
git commit -m "feat(l3): is_autonomously_invocable + expand_for_agent (pinned-only, data_ceiling class)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `load_pinned_skill_by_name` loader + `PinnedSkill` type

**Files:**
- Modify: `core/src/memory/l3_invoke.rs` (add the loader + `PinnedSkill`)

**Context:** The agent names a skill; the loader fetches the newest pinned row whose `template.name` matches, with a defensive trust re-check (mirrors `l3_surface::load_l3_skills_for_prompt`). Tested via the live-PG e2e in Task 8 (no pure unit test — it's a DB query).

- [ ] **Step 1: Add `PinnedSkill` + the loader** — in `core/src/memory/l3_invoke.rs`. First extend the imports at the top: change the `l3_approval` import to also bring in `from_metadata_str` usage (it's a `SkillTrust` assoc fn, already reachable) and add the db imports:

```rust
use hhagent_db::memories::{load_layer_by_trust, MemoryLayer};
```
(place beside the existing `use sqlx::PgPool;`). Then add near the loader region:

```rust
/// A pinned L3 skill resolved by name, ready for agent-path expansion.
#[derive(Debug, Clone)]
pub struct PinnedSkill {
    pub memory_id: i64,
    pub template: L3SkillCandidate,
    pub body_sha256: String,
}

/// Load the newest `pinned` L3 skill whose `template.name == name`.
///
/// Trust is filtered in SQL (`load_layer_by_trust(Skill, ["pinned"], …)`);
/// a defensive [`is_autonomously_invocable`] re-check runs over the result
/// so a future SQL/Rust divergence fails safe. Newest-wins resolves the
/// unlikely same-name case (matches surfacing's newest-first order).
/// `Ok(None)` when no pinned skill of that name exists — the inner loop
/// turns that into an "unknown or non-pinned skill" refusal.
pub async fn load_pinned_skill_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<PinnedSkill>, hhagent_db::DbError> {
    // Cap: a generous bound on how many pinned skills could share a name.
    // Newest-first, so the first name match is the newest.
    const SCAN_CAP: usize = 64;
    let rows = load_layer_by_trust(pool, MemoryLayer::Skill, &["pinned"], SCAN_CAP).await?;
    for row in rows {
        let trust = row
            .metadata
            .get("trust")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !is_autonomously_invocable(SkillTrust::from_metadata_str(trust)) {
            continue; // defense-in-depth; SQL already excluded these
        }
        let template: L3SkillCandidate = match row
            .metadata
            .get("template")
            .cloned()
            .and_then(|t| serde_json::from_value(t).ok())
        {
            Some(t) => t,
            None => continue, // unparseable template — skip fail-safe
        };
        if template.name != name {
            continue;
        }
        let body_sha256 = row
            .metadata
            .get("body_sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(Some(PinnedSkill { memory_id: row.id, template, body_sha256 }));
    }
    Ok(None)
}
```

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p hhagent-core 2>&1 | tail -5 && cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5`
Expected: clean build, exit 0.

- [ ] **Step 3: Commit**

```sh
git add core/src/memory/l3_invoke.rs
git commit -m "feat(l3): load_pinned_skill_by_name loader for agent-path invocation

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `StepDispatcher::known_tools()` + `ToolHostStepDispatcher` override

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (the `StepDispatcher` trait ~line 197)
- Modify: `core/src/scheduler/tool_dispatch.rs` (`ToolHostStepDispatcher` impl ~line 317; optional `ToolRegistry::tool_names` ~line 182)

**Context:** The loop needs the live registry tool-name set. A **default** trait method (returns empty) keeps every existing impl compiling unchanged; only the production dispatcher overrides it.

- [ ] **Step 1: Add the default trait method** — replace the `StepDispatcher` trait (lines 197-200) in `core/src/scheduler/inner_loop.rs`:

```rust
#[async_trait::async_trait]
pub trait StepDispatcher: Send + Sync {
    async fn dispatch_step(&self, step: &PlannedStep) -> StepOutcome;

    /// Live tool-name set this dispatcher can reach. Used by the agent
    /// L3-invoke path to re-validate a skill against the registry as it is
    /// *now* (the TOCTOU close). Default: empty — only the production
    /// [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`] holds a
    /// registry; non-loop / test doubles that never expand an invoke can
    /// keep the empty default.
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }
}
```

- [ ] **Step 2: Add `tool_names` to `ToolRegistry`** — in `core/src/scheduler/tool_dispatch.rs`, inside `impl ToolRegistry` (after `entries`, ~line 181):

```rust
    /// The set of registered tool names (deterministic, sorted). Used by
    /// the agent L3-invoke live re-validation.
    pub fn tool_names(&self) -> std::collections::BTreeSet<String> {
        self.entries.keys().cloned().collect()
    }
```

- [ ] **Step 3: Override `known_tools` on the production dispatcher** — in `core/src/scheduler/tool_dispatch.rs`, inside `impl StepDispatcher for ToolHostStepDispatcher` (after `dispatch_step`, ~line 317+), add:

```rust
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        self.registry.tool_names()
    }
```

- [ ] **Step 4: Build + verify all StepDispatcher impls still compile**

Run: `cargo build -p hhagent-core --tests 2>&1 | tail -10`
Expected: clean (the default method covers `ScriptedDispatcher`, the CLI's `DryRunNeverDispatches`, and any other doubles).

- [ ] **Step 5: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/scheduler/inner_loop.rs core/src/scheduler/tool_dispatch.rs
git commit -m "feat(l3): StepDispatcher::known_tools() for live-registry invoke re-validation

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Audit constants + payload builders (pin + agent-path invoke_rejected)

**Files:**
- Modify: `core/src/scheduler/audit.rs` (constants ~line 136; builders ~after line 576)

- [ ] **Step 1: Write failing tests** — in `core/src/scheduler/audit.rs`'s `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn build_l3_pinned_payload_shape() {
    let p = build_l3_pinned_payload(7, "summarise_repo_readme", "abc123");
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "summarise_repo_readme");
    assert_eq!(p["body_sha256"], "abc123");
}

#[test]
fn build_l3_pin_rejected_payload_shape() {
    let reasons = vec!["no registry snapshot".to_string()];
    let p = build_l3_pin_rejected_payload(7, Some("s"), &reasons);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "s");
    assert_eq!(p["reasons"][0], "no registry snapshot");
    // skill_name may be null when the template did not parse
    let p2 = build_l3_pin_rejected_payload(7, None, &reasons);
    assert!(p2["skill_name"].is_null());
}

#[test]
fn build_l3_invoke_rejected_agent_payload_allows_null_ids() {
    let reasons = vec!["unknown or non-pinned skill".to_string()];
    let p = build_l3_invoke_rejected_agent_payload("ghost", None, None, &reasons);
    assert_eq!(p["skill_name"], "ghost");
    assert!(p["memory_id"].is_null());
    assert!(p["body_sha256"].is_null());
    assert_eq!(p["reasons"][0], "unknown or non-pinned skill");
    // and the populated form
    let p2 = build_l3_invoke_rejected_agent_payload("s", Some(7), Some("sha"), &reasons);
    assert_eq!(p2["memory_id"], 7);
    assert_eq!(p2["body_sha256"], "sha");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p hhagent-core --lib scheduler::audit 2>&1 | head -20`
Expected: compile errors — the three builders don't exist.

- [ ] **Step 3: Add the constants** — in `core/src/scheduler/audit.rs`, after `ACTION_L3_INVOKE_REJECTED` (line 136):

```rust
/// Action verb for the operator `memory l3 pin` success row (trust
/// `user_approved` → `pinned`, granting agent-autonomous invocability).
pub const ACTION_L3_PINNED: &str = "l3.pinned";
/// Action verb for a refused `memory l3 pin` (not yet approved / gate
/// rejected / no registry snapshot). Trust unchanged; audited as a
/// security-relevant attempt.
pub const ACTION_L3_PIN_REJECTED: &str = "l3.pin_rejected";
```

- [ ] **Step 4: Add the payload builders** — after `build_l3_invoke_rejected_payload` (after line 576):

```rust
/// Payload for the `l3.pinned` row (operator pinned an approved skill).
pub fn build_l3_pinned_payload(memory_id: i64, skill_name: &str, body_sha256: &str) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
    })
}

/// Payload for the `l3.pin_rejected` row. `skill_name` is `None` when the
/// stored template did not parse (the only no-name pin-reject path).
pub fn build_l3_pin_rejected_payload(
    memory_id: i64,
    skill_name: Option<&str>,
    reasons: &[String],
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "reasons": reasons,
    })
}

/// Agent-path variant of [`build_l3_invoke_rejected_payload`]: `memory_id`
/// and `body_sha256` are `Option` because an unknown / non-pinned skill
/// name refusal happens before any row is loaded. `skill_name` (the
/// directive's requested name) is always known.
pub fn build_l3_invoke_rejected_agent_payload(
    skill_name: &str,
    memory_id: Option<i64>,
    body_sha256: Option<&str>,
    reasons: &[String],
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
        "reasons": reasons,
    })
}
```

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p hhagent-core --lib scheduler::audit 2>&1 | tail -20`
Expected: the 3 new tests PASS.

- [ ] **Step 6: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/scheduler/audit.rs
git commit -m "feat(l3): l3.pinned / l3.pin_rejected audit + agent-path invoke_rejected builder

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `cli_audit` pin composers

**Files:**
- Modify: `core/src/cli_audit.rs` (after `l3_revoke_and_audit` ~line 696; update the audit-const import list)

- [ ] **Step 1: Add the composers** — in `core/src/cli_audit.rs`, after `l3_revoke_and_audit`. First ensure the `use` that imports `ACTION_L3_APPROVED` etc. from `crate::scheduler::audit` also imports `ACTION_L3_PINNED, ACTION_L3_PIN_REJECTED, build_l3_pinned_payload, build_l3_pin_rejected_payload` (extend the existing import line). Then add:

```rust
/// Flip an already-`user_approved` L3 row to `pinned` and emit one
/// `actor='cli' action='l3.pinned'` row. The gate (must currently be
/// `user_approved` + pass `evaluate_approval`) is enforced by the caller
/// (`memory_l3_pin`); this helper only composes the trust flip with its
/// audit row. Returns the audit row id (0 on best-effort audit failure).
pub async fn l3_pin_and_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
) -> Result<i64, hhagent_db::DbError> {
    use crate::memory::l3_approval::SkillTrust;

    hhagent_db::memories::set_skill_trust(pool, memory_id, SkillTrust::Pinned.as_str()).await?;
    let payload = build_l3_pinned_payload(memory_id, skill_name, body_sha256);
    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_PINNED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.pinned audit insert failed (best-effort)");
            0
        }
    };
    Ok(audit_id)
}

/// Emit one `actor='cli' action='l3.pin_rejected'` row WITHOUT changing
/// trust (a refused pin leaves the row as-is). Best-effort audit.
pub async fn l3_pin_rejected_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: Option<&str>,
    reasons: &[String],
) -> i64 {
    let payload = build_l3_pin_rejected_payload(memory_id, skill_name, reasons);
    match hhagent_db::audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_L3_PIN_REJECTED, payload).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.pin_rejected audit insert failed (best-effort)");
            0
        }
    }
}
```

- [ ] **Step 2: Build + clippy**

Run: `cargo build -p hhagent-core 2>&1 | tail -5 && cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5`
Expected: clean, exit 0.

- [ ] **Step 3: Commit**

```sh
git add core/src/cli_audit.rs
git commit -m "feat(l3): cli_audit l3_pin_and_audit + l3_pin_rejected_audit composers

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Inner-loop expansion wiring + live-PG e2e

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (`run_to_terminal`, between the `plan.formulate` audit write ~line 290 and the CASSANDRA review ~line 292; the terminal capture ~line 399; add imports)
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (add `known_tools` to `ScriptedDispatcher`; new invoke scenarios + plan-factory helper)

**Context:** This is the core integration. Read `run_to_terminal` (lines 205-429) before editing. Add a loop-scoped `invoke_used` (suppression) and a per-iteration `current_invoke: Option<(i64, String)>` (memory_id, skill_name) used to write `l3.invoke_outcome` after dispatch.

- [ ] **Step 1: Write failing e2e scenarios** — first, give `ScriptedDispatcher` a real `known_tools` and a constructor that takes a tool set. In `core/tests/scheduler_inner_loop_e2e.rs` replace the `ScriptedDispatcher` struct + impl (lines 128-143) with:

```rust
struct ScriptedDispatcher {
    table: std::collections::HashMap<(String, String), StepOutcome>,
    tools: std::collections::BTreeSet<String>,
}

#[async_trait]
impl StepDispatcher for ScriptedDispatcher {
    async fn dispatch_step(&self, step: &PlannedStep) -> StepOutcome {
        self.table
            .get(&(step.tool.clone(), step.method.clone()))
            .cloned()
            .unwrap_or(StepOutcome::Err {
                code: "POLICY_DENIED".into(),
                detail: format!("no script for {}::{}", step.tool, step.method),
            })
    }
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        self.tools.clone()
    }
}
```
Update the existing `ScriptedDispatcher { table: Default::default() }` constructions in this file to `ScriptedDispatcher { table: Default::default(), tools: Default::default() }` (the compiler will flag each).

Add a helper to build + pin + insert a pinned skill row, and an invoke plan factory, near the other plan helpers (~line 200):

```rust
use hhagent_core::cassandra::types::{InvokeDirective, L3Param, L3SkillCandidate, L3TemplateStep};

/// Insert a `pinned` L3 skill row directly (bypassing crystallise/approve
/// for test focus). Returns its memory id.
async fn seed_pinned_skill(pool: &sqlx::PgPool, name: &str, tool: &str, method: &str) -> i64 {
    use hhagent_db::memories::{insert_memory_at_layer, set_skill_trust, MemoryLayer};
    let template = serde_json::json!({
        "name": name, "description": "d",
        "parameters": [{"name":"p","description":"d"}],
        "steps": [{"tool": tool, "method": method, "parameters": {"v":"{{p}}"}}]
    });
    let metadata = serde_json::json!({
        "template": template, "trust": "pinned", "body_sha256": "deadbeef"
    });
    let id = insert_memory_at_layer(pool, "skill body", metadata, MemoryLayer::Skill)
        .await.unwrap();
    // ensure trust marker is exactly "pinned" via the canonical writer too
    set_skill_trust(pool, id, "pinned").await.unwrap();
    id
}

fn invoke_plan(name: &str, arg_key: &str, arg_val: &str) -> Plan {
    let mut args = std::collections::BTreeMap::new();
    args.insert(arg_key.to_string(), arg_val.to_string());
    Plan {
        context: "c".into(), decision: "act".into(), rationale: "r".into(),
        steps: vec![], result: None, data_ceiling: DataClass::Public, refused: None,
        floor_request: None, l1_insight: None, l3_skill: None,
        invoke_skill: Some(InvokeDirective { name: name.into(), args }),
    }
}
```
(Confirm `insert_memory_at_layer`'s exact signature with `grep -n "pub async fn insert_memory_at_layer" db/src/memories/write.rs`; adapt the call if the parameter order differs.)

Then add the test scenarios at the end of the file:

```rust
/// Agent invokes a pinned skill: directive expands to the template's
/// steps, dispatches, and writes l3.invoked + l3.invoke_outcome (scheduler).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_invoke_pinned_skill_expands_and_dispatches() {
    let Some((pool, _cluster)) = bring_up_pg("inv").await else { return; };
    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({})).await.unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    seed_pinned_skill(&pool, "do_thing", "shell-exec", "shell.exec").await;

    // plan 1: invoke; plan 2: task_complete (after seeing the step result).
    let formulator = Arc::new(ScriptedFormulator::new(vec![
        invoke_plan("do_thing", "p", "value-1"),
        task_complete_plan("done"),
    ]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let mut table = std::collections::HashMap::new();
    table.insert(("shell-exec".into(), "shell.exec".into()), StepOutcome::Ok(serde_json::json!("ok")));
    let dispatcher = Arc::new(ScriptedDispatcher {
        table,
        tools: ["shell-exec".to_string()].into_iter().collect(),
    });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 4))
        .await.unwrap();
    assert!(matches!(result.outcome, Outcome::Completed(_)), "got {:?}", result.outcome);
    assert_eq!(result.dispatch_count, 1, "the expanded template's single step dispatched");

    let rows = hhagent_db::audit::fetch_since(&pool, 0, 500).await.unwrap();
    let has = |actor: &str, action: &str| rows.iter().any(|r| r.actor == actor && r.action == action);
    assert!(has("scheduler", "l3.invoked"), "l3.invoked row (scheduler) present");
    assert!(has("scheduler", "l3.invoke_outcome"), "l3.invoke_outcome row present");
}

/// Agent invokes a NON-pinned (here: absent) skill name → refused, audited,
/// fed back; the agent replans to task_complete on the next iteration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_invoke_unknown_skill_refuses_then_replans() {
    let Some((pool, _cluster)) = bring_up_pg("invref").await else { return; };
    let id = insert_pending(&pool, Lane::Fast, serde_json::json!({})).await.unwrap();
    let _ = tasks::claim_one(&pool, Lane::Fast, 60).await.unwrap().unwrap();

    let formulator = Arc::new(ScriptedFormulator::new(vec![
        invoke_plan("ghost", "p", "v"),       // no such pinned skill
        task_complete_plan("recovered"),
    ]));
    let review = Arc::new(ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]));
    let dispatcher = Arc::new(ScriptedDispatcher {
        table: Default::default(),
        tools: ["shell-exec".to_string()].into_iter().collect(),
    });

    let result = run_to_terminal(&pool, formulator, review, dispatcher, make_ctx(id, 4))
        .await.unwrap();
    match result.outcome {
        Outcome::Completed(v) => assert_eq!(v["body"], "recovered"),
        o => panic!("expected Completed after replan, got {:?}", o),
    }
    assert_eq!(result.dispatch_count, 0, "refused invoke dispatched nothing");
    let rows = hhagent_db::audit::fetch_since(&pool, 0, 500).await.unwrap();
    assert!(rows.iter().any(|r| r.actor == "scheduler" && r.action == "l3.invoke_rejected"),
        "refusal audited");
}
```

- [ ] **Step 2: Run the new e2e to verify it fails** (with PG configured)

Run: `cargo test -p hhagent-core --test scheduler_inner_loop_e2e agent_invoke 2>&1 | tail -30`
Expected: FAIL — the loop ignores `invoke_skill` today (the unknown-skill plan would proceed as a non-terminal empty-steps plan and loop; the pinned-invoke plan would dispatch nothing). Confirm the assertions fail (no `l3.invoked` rows).

- [ ] **Step 3: Wire the expansion into `run_to_terminal`** — in `core/src/scheduler/inner_loop.rs`:

First, add imports near the top (with the other `use crate::...` lines):
```rust
use crate::memory::l3_invoke::{expand_for_agent, load_pinned_skill_by_name};
use crate::memory::l3_approval::SkillTrust;
use crate::scheduler::audit::{
    build_l3_invoked_payload, build_l3_invoke_outcome_payload,
    build_l3_invoke_rejected_agent_payload, ACTION_L3_INVOKED, ACTION_L3_INVOKE_OUTCOME,
    ACTION_L3_INVOKE_REJECTED, SCHEDULER_AUDIT_ACTOR,
};
```

Add the suppression flag with `dispatch_count` (after line 217):
```rust
    // Set true once any iteration expands an `invoke_skill` directive.
    // ANDed into the terminal `l3_skill` capture so an invoke-driven task
    // never re-crystallises the skill it just ran (forecloses a
    // crystallise → pin → invoke → re-crystallise cycle).
    let mut invoke_used = false;
```

Inside the loop, **between** `write_audit_plan_formulate(...).await?;` (line 290) and the `// 2. CASSANDRA review` comment (line 292), insert the expansion block. Note `plan` is currently bound immutably at line 259 (`let (plan, meta) = …`); change that binding to `let (mut plan, meta) = …` so `plan.steps` can be populated:

```rust
        // 1b. L3 autonomous invoke expansion (before review, so the
        // reviewer governs the concrete steps). Presence of `invoke_skill`
        // triggers this branch; a malformed directive or a refused gate is
        // audited + fed back as a block so the agent replans — never a
        // silent fall-through to dispatching co-supplied steps.
        //
        // `plan` is bound `mut` (see the formulate line above); we resolve
        // the directive to OWNED data first so the immutable borrow from
        // `validate_invoke` ends before we assign `plan.steps`.
        let mut current_invoke: Option<(i64, String)> = None;
        if plan.invoke_skill.is_some() {
            // Helper: write one refusal row + push the reason(s) for replan.
            // Binds `$reasons` (a `Vec<String>`) ONCE so the expression is
            // not evaluated twice.
            macro_rules! refuse_invoke {
                ($name:expr, $mem:expr, $sha:expr, $reasons:expr) => {{
                    let reasons_v: Vec<String> = $reasons;
                    let payload = build_l3_invoke_rejected_agent_payload(
                        $name, $mem, $sha, &reasons_v,
                    );
                    if let Err(e) = hhagent_db::audit::insert(
                        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_INVOKE_REJECTED, payload,
                    ).await {
                        tracing::warn!(task_id = ctx.task_id, error = %e,
                            "l3.invoke_rejected audit insert failed (best-effort)");
                    }
                    for r in &reasons_v { ctx.blocks.push(format!("invoke_rejected: {r}")); }
                    continue; // replan; bounded by plan_count cap
                }};
            }

            // Owned (name, args) or the malformed reason — releases the
            // borrow on `plan` (type inferred; `MalformedInvoke: Display`).
            let validated = plan
                .validate_invoke()
                .map(|d| (d.name.clone(), d.args.clone()));

            match validated {
                Err(malformed) => {
                    let name = plan
                        .invoke_skill
                        .as_ref()
                        .map(|d| d.name.clone())
                        .unwrap_or_default();
                    refuse_invoke!(&name, None, None, vec![malformed.to_string()]);
                }
                Ok((name, args)) => match load_pinned_skill_by_name(pool, &name).await? {
                    None => {
                        refuse_invoke!(&name, None, None,
                            vec![format!("unknown or non-pinned skill: {name}")]);
                    }
                    Some(pinned) => {
                        let live_tools = dispatcher.known_tools();
                        match expand_for_agent(
                            &pinned.template,
                            SkillTrust::Pinned,
                            &args,
                            &live_tools,
                            plan.data_ceiling,
                        ) {
                            Err(refusal) => {
                                refuse_invoke!(
                                    &name,
                                    Some(pinned.memory_id),
                                    Some(pinned.body_sha256.as_str()),
                                    refusal.reasons
                                );
                            }
                            Ok(steps) => {
                                let arg_names: Vec<String> = args.keys().cloned().collect();
                                let payload = build_l3_invoked_payload(
                                    pinned.memory_id, &name, &pinned.body_sha256,
                                    &arg_names, steps.len(),
                                );
                                if let Err(e) = hhagent_db::audit::insert(
                                    pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_INVOKED, payload,
                                ).await {
                                    tracing::warn!(task_id = ctx.task_id, error = %e,
                                        "l3.invoked audit insert failed (best-effort)");
                                }
                                plan.steps = steps;
                                invoke_used = true;
                                current_invoke = Some((pinned.memory_id, name));
                            }
                        }
                    }
                },
            }
        }
```

- [ ] **Step 4: Write `l3.invoke_outcome` after the expanded steps dispatch** — after the existing `write_audit_plan_outcome(...).await?;` (line 426), add:

```rust
        if let Some((memory_id, skill_name)) = &current_invoke {
            let payload = build_l3_invoke_outcome_payload(
                *memory_id, skill_name, steps_executed, steps_total, any_err,
            );
            if let Err(e) = hhagent_db::audit::insert(
                pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_INVOKE_OUTCOME, payload,
            ).await {
                tracing::warn!(task_id = ctx.task_id, error = %e,
                    "l3.invoke_outcome audit insert failed (best-effort)");
            }
        }
```

- [ ] **Step 5: Suppress re-crystallisation** — in the terminal capture (lines 399-404), AND in `!invoke_used`:

```rust
            let captured_l3_skill: Option<crate::cassandra::types::L3SkillCandidate> =
                if dispatch_count >= 1 && !invoke_used {
                    plan.completion_skill().cloned()
                } else {
                    None
                };
```

- [ ] **Step 6: Run the e2e to verify it passes** (with PG configured)

Run: `cargo test -p hhagent-core --test scheduler_inner_loop_e2e 2>&1 | tail -30`
Expected: all scenarios PASS (the existing 4 + the 2 new invoke ones). Zero `[SKIP]` with PG configured.

- [ ] **Step 7: Full lib tests + clippy**

Run: `cargo test -p hhagent-core --lib 2>&1 | tail -8 && cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5`
Expected: green; clippy exit 0.

- [ ] **Step 8: Commit**

```sh
git add core/src/scheduler/inner_loop.rs core/tests/scheduler_inner_loop_e2e.rs
git commit -m "feat(l3): inner-loop expands invoke_skill before review (autonomous door core)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: `pin` CLI command

**Files:**
- Modify: `core/src/bin/hhagent-cli/memory_l3.rs` (dispatch table ~line 12; usage; new `memory_l3_pin` handler)
- Modify: `core/tests/cli_memory_l3_e2e.rs` (pin happy + reject scenarios)

- [ ] **Step 1: Write failing e2e** — read `core/tests/cli_memory_l3_e2e.rs` to learn its harness (how it spawns the CLI binary, seeds rows, asserts). Following the existing `approve`/`revoke` scenario pattern, add a `pin` scenario: seed an L3 skill, write a `registry.loaded` snapshot containing its tool, `approve` it, then `pin` it; assert exit 0, stdout mentions `pinned`, and the row's `metadata.trust == "pinned"` + an `l3.pinned` audit row exists. Add a reject scenario: `pin` a skill that is still `untrusted` (never approved) → non-zero exit, trust unchanged, `l3.pin_rejected` row. (Mirror the exact bring-up + binary-invocation helpers already in the file — do not invent new ones.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p hhagent-core --test cli_memory_l3_e2e pin 2>&1 | tail -20`
Expected: FAIL — `pin` is an unknown subcommand (exit 2).

- [ ] **Step 3: Add the dispatch entry + usage** — in `core/src/bin/hhagent-cli/memory_l3.rs`, update `run_memory_l3` (lines 12-28): add `"pin" => with_runtime("memory l3", memory_l3_pin(&args[1..])),` and update both usage strings to `<list|approve|pin|revoke|remove|run>` and the unknown-action message to include `pin`.

- [ ] **Step 4: Add the `memory_l3_pin` handler** — add after `memory_l3_revoke`. It mirrors `memory_l3_approve` (load + layer-guard + parse template + snapshot gate) but additionally requires current trust `user_approved` and flips to `pinned`:

```rust
async fn memory_l3_pin(args: &[String]) -> ExitCode {
    use hhagent_core::cassandra::types::L3SkillCandidate;
    use hhagent_core::cli_audit::{l3_pin_and_audit, l3_pin_rejected_audit};
    use hhagent_core::memory::l3_approval::{evaluate_approval, ApprovalDecision, RejectReason, SkillTrust};
    use hhagent_db::memories::{fetch_by_ids, MemoryLayer};
    use hhagent_db::pool::connect_runtime_pool;

    let id_str = match args {
        [s] => s,
        _ => {
            eprintln!("usage: hhagent-cli memory l3 pin <id>");
            return ExitCode::from(2);
        }
    };
    let id: i64 = match id_str.parse() {
        Ok(n) => n,
        Err(e) => { eprintln!("memory l3 pin: invalid id '{id_str}': {e}"); return ExitCode::from(2); }
    };

    let spec = match resolve_connect_spec() {
        Ok(s) => s, Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p, Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let row = match fetch_by_ids(&pool, &[id]).await {
        Ok(mut v) => v.pop(), Err(e) => { eprintln!("memory l3 pin: {e}"); return ExitCode::from(1); }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => { eprintln!("memory l3 pin: no layer-3 skill with id={id}"); return ExitCode::from(1); }
    };
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());

    // Ladder: a skill must be `user_approved` before it can be pinned.
    let current = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    if current != SkillTrust::UserApproved {
        let reasons = vec![format!(
            "skill must be user_approved before pinning (current: {})", current.as_str()
        )];
        let _ = l3_pin_rejected_audit(&pool, id, None, &reasons).await;
        eprintln!("memory l3 pin: id={id} is '{}', not user_approved; approve it first", current.as_str());
        return ExitCode::from(1);
    }

    let template: L3SkillCandidate = match row
        .metadata.get("template").cloned().and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            let reasons = vec!["stored L3 row has no parseable 'template'".to_string()];
            let _ = l3_pin_rejected_audit(&pool, id, None, &reasons).await;
            eprintln!("memory l3 pin: id={id} has no parseable template; not pinned");
            return ExitCode::from(1);
        }
    };
    let skill_name = template.name.clone();

    // Defense-in-depth: re-run the approval gate against the snapshot
    // before granting autonomy (the strongest privilege).
    let decision = match latest_registry_tools(&pool).await {
        Ok(Some(known)) => evaluate_approval(&template, &known),
        Ok(None) => ApprovalDecision::Reject { reasons: vec![RejectReason::NoRegistrySnapshot] },
        Err(e) => { eprintln!("memory l3 pin: {e}"); return ExitCode::from(1); }
    };

    match decision {
        ApprovalDecision::Approve => {
            let sha = body_sha256.unwrap_or("");
            if let Err(e) = l3_pin_and_audit(&pool, id, &skill_name, sha).await {
                eprintln!("memory l3 pin: {e}");
                return ExitCode::from(1);
            }
            println!("pinned skill '{skill_name}' (#{id}) → trust=pinned (agent-autonomously invocable)");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_pin_rejected_audit(&pool, id, Some(&skill_name), &rendered).await;
            eprintln!("pin REJECTED for skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}
```

- [ ] **Step 5: Run to verify it passes** (with PG configured)

Run: `cargo test -p hhagent-core --test cli_memory_l3_e2e 2>&1 | tail -20`
Expected: pin scenarios PASS; existing list/approve/revoke/remove/run scenarios stay green.

- [ ] **Step 6: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/bin/hhagent-cli/memory_l3.rs core/tests/cli_memory_l3_e2e.rs
git commit -m "feat(l3): memory l3 pin command (user_approved -> pinned, gated re-validation)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Recall surfacing — `[invocable]` marker on pinned skills

**Files:**
- Modify: `core/src/memory/l3_surface.rs` (`SurfacedSkill` ~line 36; `parse_surfaced_skill` ~line 49; `render_skill_entry` ~line 103; the loader ~line 191)
- Modify: `core/src/memory/l3_surface/tests.rs` (if tests are lifted to a sibling) OR the inline tests

**Context:** `parse_surfaced_skill` currently takes only `metadata` and drops trust. To tag pinned skills, thread the parsed trust into a new `invocable: bool` on `SurfacedSkill`.

- [ ] **Step 1: Write failing tests** — in the l3_surface test module add:

```rust
#[test]
fn render_skill_entry_tags_invocable_pinned_skill() {
    let skill = SurfacedSkill {
        name: "do_thing".into(), description: "d".into(),
        params: vec![], invocable: true,
    };
    let out = render_skill_entry(&skill);
    assert!(out.contains("[invocable]"), "pinned skill is tagged: {out}");
}

#[test]
fn render_skill_entry_no_tag_for_reference_only() {
    let skill = SurfacedSkill {
        name: "ref_only".into(), description: "d".into(),
        params: vec![], invocable: false,
    };
    assert!(!render_skill_entry(&skill).contains("[invocable]"));
}

#[test]
fn parse_surfaced_skill_marks_invocable_from_pinned_trust() {
    let md = serde_json::json!({
        "trust": "pinned",
        "template": {"name":"s","description":"d","parameters":[],"steps":[]}
    });
    assert!(parse_surfaced_skill(&md).unwrap().invocable);
    let md2 = serde_json::json!({
        "trust": "user_approved",
        "template": {"name":"s","description":"d","parameters":[],"steps":[]}
    });
    assert!(!parse_surfaced_skill(&md2).unwrap().invocable);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p hhagent-core --lib memory::l3_surface 2>&1 | head -20`
Expected: compile errors — `SurfacedSkill` has no `invocable` field.

- [ ] **Step 3: Add the field** — in `core/src/memory/l3_surface.rs`, `SurfacedSkill` (line 36):

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfacedSkill {
    pub name: String,
    pub description: String,
    pub params: Vec<L3Param>,
    /// True iff this skill is `pinned` (agent-autonomously invocable). The
    /// planner may emit `invoke_skill` ONLY for invocable skills; the rest
    /// are reference-only.
    pub invocable: bool,
}
```

- [ ] **Step 4: Thread trust into `parse_surfaced_skill`** — replace the function body (lines 49-57) so it reads `metadata.trust` and sets `invocable`:

```rust
pub fn parse_surfaced_skill(metadata: &serde_json::Value) -> Option<SurfacedSkill> {
    let template = metadata.get("template")?;
    let cand: L3SkillCandidate = serde_json::from_value(template.clone()).ok()?;
    let trust = metadata.get("trust").and_then(|v| v.as_str()).unwrap_or("");
    let invocable = is_autonomously_invocable(SkillTrust::from_metadata_str(trust));
    Some(SurfacedSkill {
        name: cand.name,
        description: cand.description,
        params: cand.parameters,
        invocable,
    })
}
```
Add the import at the top of `l3_surface.rs` if not present:
```rust
use crate::memory::l3_invoke::is_autonomously_invocable;
```
(`SkillTrust` + `from_metadata_str` are already in scope in this module — confirm; the loader already uses `SkillTrust::from_metadata_str`.)

- [ ] **Step 5: Tag invocable skills in `render_skill_entry`** — in the function (lines 103-121), change the name line to append the tag:

```rust
    out.push_str("- ");
    out.push_str(&skill.name);
    if skill.invocable {
        out.push_str(" [invocable]");
    }
    out.push_str(": ");
    out.push_str(&skill.description);
    out.push('\n');
```

- [ ] **Step 6: Fix the loader** — `load_l3_skills_for_prompt` calls `parse_surfaced_skill(&row.metadata)` which now carries trust through, so no signature change is needed there. Confirm it still compiles. If any other `SurfacedSkill { … }` literal exists (e.g. in tests or `cap_surfaced` tests), add `invocable: false` (or the appropriate value). Build to find them:

Run: `cargo build -p hhagent-core --tests 2>&1 | grep -A2 "missing field" | head -20`
Fix each.

- [ ] **Step 7: Run to verify they pass**

Run: `cargo test -p hhagent-core --lib memory::l3_surface 2>&1 | tail -20`
Expected: the 3 new tests PASS; existing l3_surface tests green.

- [ ] **Step 8: Clippy + commit**

```sh
cargo clippy -p hhagent-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/memory/l3_surface.rs
# add the l3_surface/tests.rs path if tests live in a sibling
git commit -m "feat(l3): surface [invocable] tag on pinned skills (SurfacedSkill.invocable)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: Planner prompt — the invoke contract

**Files:**
- Modify: `prompts/agent_planner.md` (the `<skills>` block ~lines 29-46; the Plan schema field list ~lines 70-78; the field docs after ~line 118)

**Context:** Replace the "no skill-invocation field … the runner will ignore it" paragraph with the invoke contract, add `invoke_skill` to the schema block, and document it.

- [ ] **Step 1: Replace the `<skills>` reference-only paragraph** — replace lines 29-46 of `prompts/agent_planner.md` with:

````markdown
## The `<skills>` block

A `<skills>` block may precede these instructions. It lists skills you
previously crystallised that an operator has reviewed, each with its
name, a one-line description, and its parameters — for example:

```
<skills>
- summarise_repo_readme [invocable]: Read a repo's README and return a short summary.
  params: repo_path (absolute path to the repo)
- archive_old_logs: Move logs older than N days to cold storage.
  params: days (age threshold)
</skills>
```

A skill tagged **`[invocable]`** has been *pinned* by the operator: you
MAY invoke it directly (see `invoke_skill` below). A skill **without** the
tag is approved for reference only — you may reproduce its approach with
ordinary `steps`, but an `invoke_skill` of a non-pinned skill will be
**refused** by the runner and you will be asked to replan.
````

- [ ] **Step 2: Add `invoke_skill` to the Plan schema block** — in the JSON schema block (lines 70-77), add the field after `"l3_skill": null,`:

```json
    "l3_skill":       null,
    "invoke_skill":   null,
    "floor_request":  null,
```

- [ ] **Step 3: Document the `invoke_skill` field** — add a new subsection after the `l3_skill` documentation (after line 118):

````markdown
**Optional: `invoke_skill` (run a pinned skill).** On a NON-terminal plan,
instead of hand-writing `steps`, you MAY invoke an `[invocable]` skill
from the `<skills>` block by emitting an `invoke_skill` object. The runner
expands it into the skill's concrete steps and runs them through the same
review + sandbox + audit path as ordinary steps.

Rules:

- `invoke_skill` requires `steps: []`, a non-`task_complete` `decision`,
  and no `l3_skill` on the same plan (these are mutually exclusive — a plan
  carrying both is refused).
- Supply `args` for **exactly** the skill's declared parameters; values
  must be single-line, free of control characters and `{{`/`}}`, and under
  1 KiB each.
- Only `[invocable]` (pinned) skills may be invoked. Invoking a
  reference-only skill is refused.

Shape:

```json
"invoke_skill": {
  "name": "summarise_repo_readme",
  "args": { "repo_path": "/srv/project" }
}
```

After the invoked steps run, you will see their results on the next
iteration and can continue planning (e.g. emit `task_complete`).
````

- [ ] **Step 4: Verify no prompt-shape test breaks** — some tests assert prompt content or the assembled-prompt sha. Run the prompt-adjacent suites:

Run: `cargo test -p hhagent-core --lib prompt_assembly 2>&1 | tail -10 && cargo test -p hhagent-core --test prompt_assembly_e2e 2>&1 | tail -10`
Expected: green (the prompt file is loaded at runtime, not hashed into a pinned constant; if any test pins `agent_planner` content, update its expectation to match).

- [ ] **Step 5: Commit**

```sh
git add prompts/agent_planner.md
git commit -m "docs(l3): planner prompt teaches invoke_skill + [invocable] contract

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (after all tasks)

- [ ] **Full workspace test + clippy + doc-links** (with PG configured for the live e2e):

```sh
export HHAGENT_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin"
cargo test --workspace 2>&1 | tail -15
cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -5
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" cargo doc -p hhagent-core --no-deps --document-private-items 2>&1 | grep -c "unresolved" || true
```
Expected: all tests pass (baseline 1293 + new); clippy exit 0; doc-links count == main's 21.

- [ ] **Live-PG L3 regression sweep** (zero `[SKIP]`):

```sh
cargo test -p hhagent-core --test cli_memory_l3_e2e --test cli_memory_l3_run_e2e \
  --test memory_l3_crystallise_e2e --test l3_surface_e2e \
  --test scheduler_inner_loop_e2e --test prompt_assembly_e2e 2>&1 | tail -20
```
Expected: all green.

---

## File-size watch

After Tasks 3 + 4, `core/src/memory/l3_invoke.rs` gains ~80 production lines
(`is_autonomously_invocable`, `planned_step_from_l3_with_class`,
`expand_for_agent`, `PinnedSkill`, `load_pinned_skill_by_name`) on top of its
~467 prod LOC — likely crossing the 500-LOC soft cap. Its tests already live in
the `l3_invoke/tests.rs` sibling, so a further test-lift won't help. Per the
established convention (cf. `tool_host.rs`, the L3 crystallise/invoke slices),
**do not block this feature on it** — finish the slice, then flag a follow-up
in HANDOVER's Next-TODO (a real production split of `l3_invoke.rs` along the
operator-path / agent-path / pure-substitution seam, mirroring the
`memories.rs` write/search split). Note the exact LOC in the session-end
verification so the next session can size the lift.

## Self-review notes (spec coverage)

- Spec §4 (Plan schema + mutual exclusivity) → Task 1.
- Spec §5.1 (`is_autonomously_invocable`) → Task 3; §5.2 (by-name loader) → Task 4.
- Spec §6 (inner-loop expansion, refusal→block→replan, live_tools) → Tasks 5 + 8.
- Spec §7 (no recursion is structural — `L3TemplateStep` has no invoke; re-crystallisation suppression) → Task 8 Step 5.
- Spec §8 (`pin` command, ladder + gate) → Tasks 6/7/9.
- Spec §9 (surfacing `[invocable]` + prompt) → Tasks 10 + 11.
- Spec §10 (security) → enforced across Tasks 3 (pinned-only), 5/8 (live re-validation + CASSANDRA sees expanded steps via expand-before-review), 8 (audited refusals).
- Spec §11 (audit contract incl. actor `scheduler`, optional ids) → Tasks 2/6/8.
- Spec §12 (testing) → unit (Tasks 1/3/6/10) + live-PG e2e (Tasks 8/9) + regression (Final).
