# First real `DeterministicPolicy` rule — data-classification invariant + CLI `--classification-floor` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the first real Stage 0 reviewer rule (data-classification invariant check) plus a `kastellan-cli ask --classification-floor` flag so operators can pin the task-level floor at submission.

**Architecture:** Two pieces in one branch. (1) New pure module `core/src/cassandra/deterministic.rs` exporting `ClassificationViolation` enum + `screen_plan_for_classification_violations(plan, floor)` over the typed `DataClass` fields already on `Plan`/`PlannedStep`/`ReviewStageContext`. Checks three invariants in declared order, first hit wins. (2) `DeterministicPolicy::review` wired to the helper; `Verdict::Block(violation.format_reason())` on a hit. (3) `--classification-floor <DataClass>` flag added to `kastellan-cli ask`'s arg loop; serialises into `tasks.payload.classification_floor` as PascalCase (already read by `runner.rs:283-296` via serde).

**Tech Stack:** Rust 2021, sync code (the pure module is non-async; the trait method is async per `ReviewStage`), `serde` for case-insensitive parsing, no new deps. Existing test harness (`#[tokio::test]` for the async trait, plain `#[test]` for the pure helpers).

**Spec:** [docs/superpowers/specs/2026-05-15-deterministic-policy-classification-floor-design.md](../specs/2026-05-15-deterministic-policy-classification-floor-design.md)

**Branch:** `feat/deterministic-policy-classification` (already created; carries `181fb05` HANDOVER/ROADMAP sync + `5fdb62b` spec commit at the start).

---

## File Structure

- **Create:** `core/src/cassandra/deterministic.rs` — pure invariant-check module; ~250 LOC (~120 production + ~130 tests). Public surface: `ClassificationViolation` enum + `screen_plan_for_classification_violations` + helper methods on the enum.
- **Modify:** `core/src/cassandra/mod.rs` — add `pub mod deterministic;` declaration (1 line).
- **Modify:** `core/src/cassandra/review.rs` — fill in `DeterministicPolicy::review` body; module-level doc updated; replace `deterministic_policy_is_still_a_stub` test with two new tests pinning the real behaviour.
- **Modify:** `core/src/bin/kastellan-cli.rs` — new `parse_classification_floor` pure helper; new `--classification-floor` branch in `run_ask`'s arg loop; pass floor into `ask_async` (signature widening); add helper-tests module; update help text.
- **Modify:** `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — session-end update at the end.

**File-size watch:** `core/src/cassandra/deterministic.rs` will land at ~250 LOC, well under the 500-LOC soft cap. `core/src/bin/kastellan-cli.rs` was 797 LOC before Slice B (already over the cap, flagged for future split into one-file-per-subcommand) — this slice adds ~80 LOC; not warranting a split today but the natural future shape (`run_ask` → `core/src/bin/kastellan-cli/ask.rs`) gets one step closer.

---

## Background — reading list for the engineer

Before starting, skim these in order so the surrounding contract is in context:

1. **Spec:** [docs/superpowers/specs/2026-05-15-deterministic-policy-classification-floor-design.md](../specs/2026-05-15-deterministic-policy-classification-floor-design.md) — the why and the rule-by-rule shape.
2. **Type surface:** [core/src/cassandra/types.rs](../../../core/src/cassandra/types.rs) — read `DataClass` (lines 21-40), `PlannedStep.classification` (line 59), `Plan.data_ceiling` (line 90), the invariant comment at lines 105-110, and `Verdict::Block(String)` (line 136).
3. **Mirror module — `ConstitutionalGuard`:** [core/src/cassandra/constitutional.rs](../../../core/src/cassandra/constitutional.rs) — note the module-doc structure (4 sections: why a separate module / scope / in-scope / out-of-scope), the pure helper signature pattern, the `mod tests` layout with verbatim fixture prompts as `const &str`. The new `deterministic.rs` should mirror this shape.
4. **Mirror trait wiring — `ConstitutionalGuard::review`:** [core/src/cassandra/review.rs:87-100](../../../core/src/cassandra/review.rs#L87-L100) — three-line match body. `DeterministicPolicy::review` should be the same shape.
5. **CLI arg-loop pattern:** [core/src/bin/kastellan-cli.rs:222-247](../../../core/src/bin/kastellan-cli.rs#L222-L247) (the `run_ask` body) plus [core/src/bin/kastellan-cli.rs:114-140](../../../core/src/bin/kastellan-cli.rs#L114-L140) (`run_audit_tail` as a flag-with-value pattern reference).
6. **Floor read path:** [core/src/scheduler/runner.rs:283-296](../../../core/src/scheduler/runner.rs#L283-L296) — the existing `task.payload.classification_floor` reader uses serde directly on the PascalCase string. The CLI flag just needs to write the same shape.

**Build/test:**
```sh
source "$HOME/.cargo/env"
cargo build --workspace
cargo test --workspace                            # 519 expected at start
cargo test -p kastellan-core cassandra::            # subset for fast iteration
cargo test -p kastellan-core --test scheduler_inner_loop_e2e
```

**Branch already created; no extra setup needed.**

---

### Task 1: Scaffold `deterministic.rs` with the `ClassificationViolation` enum

**Files:**
- Create: `core/src/cassandra/deterministic.rs`
- Modify: `core/src/cassandra/mod.rs`

This task lands ONLY the enum and its two helper methods (`reason_tag`, `format_reason`). The `screen_plan_for_classification_violations` body comes in Task 2. We split this way so the enum's shape is locked first; that lets `screen_*`'s tests assert against a stable enum.

- [ ] **Step 1: Create the file scaffold + module declaration**

Create `core/src/cassandra/deterministic.rs` with this exact content (the body comes in Task 2 — for now just types + their tests):

```rust
//! Plan-level data-classification invariant check used by
//! [`DeterministicPolicy`](super::review::DeterministicPolicy).
//!
//! ## Why a separate module
//!
//! The screen is a pure function over (`Plan`, `DataClass`) so it lives
//! apart from the trait wiring in `review.rs`. This keeps the
//! invariant catalogue readable in one sitting and lets the helper be
//! exercised without the async trait machinery.
//!
//! ## Scope (the three invariants)
//!
//! This is the **first real** Stage 0 rule. It enforces three
//! invariants over the typed [`DataClass`] fields already on
//! [`Plan`](super::types::Plan),
//! [`PlannedStep`](super::types::PlannedStep), and
//! [`ReviewStageContext`](super::review::ReviewStageContext):
//!
//! - **I1: `plan.data_ceiling >= ctx.classification_floor`** — the
//!   spec invariant from [`types.rs:105-110`](super::types). Catches
//!   the "upgrading without justification" shape where the agent
//!   claims high-class outputs from low-class inputs.
//! - **I2: every `step.classification >= ctx.classification_floor`** —
//!   the leak/downgrade catch. If the floor is `ClinicalConfidential`,
//!   every step must touch at-least-clinical data; a `Public`-labelled
//!   step in a clinical task signals implicit declassification.
//! - **I3: every `step.classification <= plan.data_ceiling`** —
//!   plan-internal consistency: a step labelled at a class higher than
//!   the plan's declared ceiling is the agent self-contradicting.
//!
//! Invariants checked in declared order (I1, then I2, then I3); first
//! hit wins. Within per-step invariants, lowest `step_index` wins.
//! Same precedence shape as [`super::constitutional`]'s
//! "first principle wins".
//!
//! ## Out of scope (filed as follow-ups)
//!
//! - **Automatic floor inference from prompt keywords.** The floor is
//!   operator-pinned via `kastellan-cli ask --classification-floor`.
//!   Inferring it from instruction text is a separate slice.
//! - **Anonymiser / declassifier mechanism.** A step that legitimately
//!   downgrades classification (e.g. a "summarise without identifiers"
//!   step) would today be blocked by I2. The anonymiser path is a
//!   Phase 2 feature; until it lands, "downgrade is never legitimate"
//!   is the safe default.
//! - **`Verdict::Escalate` severity-split.** Today every violation
//!   surfaces as `Verdict::Block`. Splitting by severity is a future
//!   slice.

use super::types::{DataClass, Plan};

/// One invariant violation found by the deterministic-policy screen.
///
/// Carries enough structured detail for both the audit log (via
/// [`Self::reason_tag`]) and the human-readable verdict reason (via
/// [`Self::format_reason`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClassificationViolation {
    /// I1 — `plan.data_ceiling < ctx.classification_floor`.
    CeilingBelowFloor {
        ceiling: DataClass,
        floor:   DataClass,
    },
    /// I2 — `plan.steps[step_index].classification < ctx.classification_floor`.
    StepClassificationBelowFloor {
        step_index: usize,
        step_class: DataClass,
        floor:      DataClass,
    },
    /// I3 — `plan.steps[step_index].classification > plan.data_ceiling`.
    StepClassificationAboveCeiling {
        step_index: usize,
        step_class: DataClass,
        ceiling:    DataClass,
    },
}

impl ClassificationViolation {
    /// Stable `snake_case` identifier for grep-ability in audit-log
    /// reason strings. Renaming is a contract break.
    pub fn reason_tag(&self) -> &'static str {
        match self {
            Self::CeilingBelowFloor { .. } => "ceiling_below_floor",
            Self::StepClassificationBelowFloor { .. } => "step_classification_below_floor",
            Self::StepClassificationAboveCeiling { .. } => "step_classification_above_ceiling",
        }
    }

    /// Human-readable verdict reason, prefixed with the structured
    /// `"data-classification: <tag>"` so operators can both eyeball
    /// the violation and grep for it.
    pub fn format_reason(&self) -> String {
        match self {
            Self::CeilingBelowFloor { ceiling, floor } => format!(
                "data-classification: {} — plan.data_ceiling={:?} is below task.classification_floor={:?}",
                self.reason_tag(),
                ceiling,
                floor,
            ),
            Self::StepClassificationBelowFloor { step_index, step_class, floor } => format!(
                "data-classification: {} — step {} classified as {:?} but task.classification_floor={:?}",
                self.reason_tag(),
                step_index,
                step_class,
                floor,
            ),
            Self::StepClassificationAboveCeiling { step_index, step_class, ceiling } => format!(
                "data-classification: {} — step {} classified as {:?} but plan.data_ceiling={:?}",
                self.reason_tag(),
                step_index,
                step_class,
                ceiling,
            ),
        }
    }
}

/// Screen a plan against the three classification invariants.
///
/// Returns `Some(violation)` on the first hit (declared order: I1, I2,
/// I3; within per-step invariants, lowest `step_index` wins); `None`
/// on a clean plan. The body lands in Task 2 — this scaffold compiles
/// but always returns `None` so Task 1's tests can exercise the enum
/// shape independently.
pub fn screen_plan_for_classification_violations(
    _plan: &Plan,
    _floor: DataClass,
) -> Option<ClassificationViolation> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_tag_is_stable_for_each_variant() {
        let v1 = ClassificationViolation::CeilingBelowFloor {
            ceiling: DataClass::Public,
            floor:   DataClass::ClinicalConfidential,
        };
        let v2 = ClassificationViolation::StepClassificationBelowFloor {
            step_index: 2,
            step_class: DataClass::Public,
            floor:      DataClass::ClinicalConfidential,
        };
        let v3 = ClassificationViolation::StepClassificationAboveCeiling {
            step_index: 0,
            step_class: DataClass::ClinicalConfidential,
            ceiling:    DataClass::Public,
        };
        assert_eq!(v1.reason_tag(), "ceiling_below_floor");
        assert_eq!(v2.reason_tag(), "step_classification_below_floor");
        assert_eq!(v3.reason_tag(), "step_classification_above_ceiling");
    }

    #[test]
    fn format_reason_includes_tag_prefix_and_struct_values_i1() {
        let v = ClassificationViolation::CeilingBelowFloor {
            ceiling: DataClass::Public,
            floor:   DataClass::ClinicalConfidential,
        };
        let s = v.format_reason();
        assert!(s.starts_with("data-classification: ceiling_below_floor"), "got: {s}");
        assert!(s.contains("Public"), "got: {s}");
        assert!(s.contains("ClinicalConfidential"), "got: {s}");
    }

    #[test]
    fn format_reason_includes_step_index_for_per_step_variants() {
        let v2 = ClassificationViolation::StepClassificationBelowFloor {
            step_index: 7,
            step_class: DataClass::Personal,
            floor:      DataClass::ClinicalConfidential,
        };
        let s2 = v2.format_reason();
        assert!(s2.contains("step 7"), "got: {s2}");
        assert!(s2.contains("Personal"), "got: {s2}");
        assert!(s2.contains("ClinicalConfidential"), "got: {s2}");

        let v3 = ClassificationViolation::StepClassificationAboveCeiling {
            step_index: 0,
            step_class: DataClass::Secret,
            ceiling:    DataClass::ClinicalConfidential,
        };
        let s3 = v3.format_reason();
        assert!(s3.contains("step 0"), "got: {s3}");
        assert!(s3.contains("Secret"), "got: {s3}");
        assert!(s3.contains("ClinicalConfidential"), "got: {s3}");
    }

    #[test]
    fn scaffold_screen_returns_none_today() {
        // Task 2 fills the body. For now the scaffold returns None
        // unconditionally so we can land the enum shape first.
        let plan = Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![],
            result: None,
            data_ceiling: DataClass::Public,
            refused: None,
        };
        assert_eq!(
            screen_plan_for_classification_violations(&plan, DataClass::Public),
            None,
        );
    }
}
```

Modify `core/src/cassandra/mod.rs`. After the existing `pub mod constitutional;` line, add `pub mod deterministic;`:

```rust
pub mod constitutional;
pub mod deterministic;
pub mod review;
pub mod types;
```

- [ ] **Step 2: Run the new tests — expect them to GREEN immediately**

Run: `cargo test -p kastellan-core --lib cassandra::deterministic`

Expected: 4 passed (`reason_tag_is_stable_for_each_variant`, `format_reason_includes_tag_prefix_and_struct_values_i1`, `format_reason_includes_step_index_for_per_step_variants`, `scaffold_screen_returns_none_today`).

This task is shape-first, not strictly RED-then-GREEN — the enum + helpers are pure data with deterministic outputs, so tests pass on first compile. Task 2 will be the first real RED→GREEN cycle (against the still-stub `screen_*` body).

- [ ] **Step 3: Run the workspace tests — confirm no regressions**

Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED'`

Expected: every line says `ok`; no `FAILED`. Total passed should be **523** (519 baseline + 4 new tests in this task).

- [ ] **Step 4: Commit**

```bash
git add core/src/cassandra/deterministic.rs core/src/cassandra/mod.rs
git commit -m "$(cat <<'EOF'
feat(cassandra): scaffold ClassificationViolation enum + reason helpers

First commit of the Stage 0 reviewer rule. Lands only the type shape
+ reason-tag/format-reason helpers; the screen function returns None
unconditionally for now so the body can land separately in the next
commit with its own RED tests.

The enum carries enough structured detail for both the audit log
(via reason_tag snake_case identifier — stable, renaming is a
contract break) and the human-readable Verdict::Block payload (via
format_reason with a "data-classification: <tag> — ..." prefix).

+4 tests pinning enum-shape contract; workspace 519 -> 523.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Implement `screen_plan_for_classification_violations`

**Files:**
- Modify: `core/src/cassandra/deterministic.rs` (replace the scaffold body + add the real RED tests; delete `scaffold_screen_returns_none_today` because it's no longer the right pin)

This is the first real RED→GREEN cycle. Write tests first against the (currently-returning-`None`) body; expect FAIL; then fill the body; expect PASS.

- [ ] **Step 1: Write the failing tests — replace the test module's contents**

Open `core/src/cassandra/deterministic.rs`. Find the `#[cfg(test)] mod tests` block (it currently has 4 tests). Delete `scaffold_screen_returns_none_today` (it's no longer the right shape). After the existing 3 tests (`reason_tag_*` and `format_reason_*`), add these:

```rust
    // ---- screen_plan_for_classification_violations ----

    fn step(class: DataClass) -> super::super::types::PlannedStep {
        super::super::types::PlannedStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({}),
            returns: "".into(),
            done_when: "".into(),
            classification: class,
        }
    }

    fn plan(ceiling: DataClass, steps: Vec<DataClass>) -> Plan {
        Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: steps.into_iter().map(step).collect(),
            result: None,
            data_ceiling: ceiling,
            refused: None,
        }
    }

    #[test]
    fn approves_clean_plan_with_default_public_floor() {
        // All Public — the existing default shape. No invariant fires.
        let p = plan(DataClass::Public, vec![DataClass::Public, DataClass::Public]);
        assert_eq!(
            screen_plan_for_classification_violations(&p, DataClass::Public),
            None,
        );
    }

    #[test]
    fn approves_well_formed_clinical_plan() {
        // floor=ClinicalConfidential, ceiling=ClinicalConfidential,
        // every step at ClinicalConfidential. No invariant fires.
        let p = plan(
            DataClass::ClinicalConfidential,
            vec![DataClass::ClinicalConfidential, DataClass::ClinicalConfidential],
        );
        assert_eq!(
            screen_plan_for_classification_violations(&p, DataClass::ClinicalConfidential),
            None,
        );
    }

    #[test]
    fn i1_fires_when_ceiling_below_floor() {
        // floor=ClinicalConfidential, ceiling=Public. I1 violated.
        let p = plan(DataClass::Public, vec![DataClass::Public]);
        let got = screen_plan_for_classification_violations(&p, DataClass::ClinicalConfidential);
        assert_eq!(
            got,
            Some(ClassificationViolation::CeilingBelowFloor {
                ceiling: DataClass::Public,
                floor:   DataClass::ClinicalConfidential,
            }),
        );
    }

    #[test]
    fn i1_does_not_fire_when_ceiling_equal_to_floor() {
        let p = plan(DataClass::Personal, vec![DataClass::Personal]);
        assert_eq!(
            screen_plan_for_classification_violations(&p, DataClass::Personal),
            None,
        );
    }

    #[test]
    fn i2_fires_on_step_below_floor() {
        // ceiling satisfies I1 (>= floor), but step 1 is Public while
        // floor is ClinicalConfidential. I2 violated at step_index=1.
        let p = plan(
            DataClass::ClinicalConfidential,
            vec![DataClass::ClinicalConfidential, DataClass::Public],
        );
        let got = screen_plan_for_classification_violations(&p, DataClass::ClinicalConfidential);
        assert_eq!(
            got,
            Some(ClassificationViolation::StepClassificationBelowFloor {
                step_index: 1,
                step_class: DataClass::Public,
                floor:      DataClass::ClinicalConfidential,
            }),
        );
    }

    #[test]
    fn i2_picks_lowest_step_index_when_multiple_violate() {
        // Both step 0 and step 2 violate I2. Lowest index (0) wins.
        let p = plan(
            DataClass::ClinicalConfidential,
            vec![
                DataClass::Public,
                DataClass::ClinicalConfidential,
                DataClass::Personal,
            ],
        );
        let got = screen_plan_for_classification_violations(&p, DataClass::ClinicalConfidential);
        assert_eq!(
            got,
            Some(ClassificationViolation::StepClassificationBelowFloor {
                step_index: 0,
                step_class: DataClass::Public,
                floor:      DataClass::ClinicalConfidential,
            }),
        );
    }

    #[test]
    fn i3_fires_on_step_above_ceiling() {
        // ceiling=Public, step 0 at Personal. I1 holds (Public >= Public
        // floor); I2 holds (Personal >= Public floor); I3 violated.
        let p = plan(DataClass::Public, vec![DataClass::Personal]);
        let got = screen_plan_for_classification_violations(&p, DataClass::Public);
        assert_eq!(
            got,
            Some(ClassificationViolation::StepClassificationAboveCeiling {
                step_index: 0,
                step_class: DataClass::Personal,
                ceiling:    DataClass::Public,
            }),
        );
    }

    #[test]
    fn i3_picks_lowest_step_index_when_multiple_violate() {
        // Both step 1 and step 2 violate I3 (ceiling=Public). Lowest
        // index (1) wins.
        let p = plan(
            DataClass::Public,
            vec![
                DataClass::Public,
                DataClass::Personal,
                DataClass::ClinicalConfidential,
            ],
        );
        let got = screen_plan_for_classification_violations(&p, DataClass::Public);
        assert_eq!(
            got,
            Some(ClassificationViolation::StepClassificationAboveCeiling {
                step_index: 1,
                step_class: DataClass::Personal,
                ceiling:    DataClass::Public,
            }),
        );
    }

    #[test]
    fn i1_wins_over_i2_when_both_could_fire() {
        // ceiling=Public, floor=ClinicalConfidential, step at Public.
        // BOTH I1 (Public < ClinicalConfidential) AND I2 (step Public
        // < ClinicalConfidential) fire. Declared-order precedence
        // says I1 wins.
        let p = plan(DataClass::Public, vec![DataClass::Public]);
        let got = screen_plan_for_classification_violations(&p, DataClass::ClinicalConfidential);
        assert_eq!(
            got,
            Some(ClassificationViolation::CeilingBelowFloor {
                ceiling: DataClass::Public,
                floor:   DataClass::ClinicalConfidential,
            }),
        );
    }

    #[test]
    fn i2_wins_over_i3_when_both_could_fire() {
        // ceiling=Personal, floor=ClinicalConfidential, step=Secret.
        // I1 fires (Personal < ClinicalConfidential) — wait, that's
        // I1. Recompose: ceiling=Secret (so I1 holds), floor=
        // ClinicalConfidential, step at Personal. I2 fires (Personal
        // < ClinicalConfidential); I3 doesn't fire (Personal <= Secret).
        // So I2 is the unambiguous winner — no I3-vs-I2 contention
        // possible here because step.classification can't simultaneously
        // be below the floor AND above the ceiling unless ceiling<floor
        // (which I1 already catches first).
        //
        // The test we actually want pins: a step that satisfies I1+I2
        // both but violates I3, vs. a step that violates I2 only —
        // i.e., the *earliest violating step* across I2/I3 isn't quite
        // the question; the precedence is I2 ALL-STEPS before I3
        // ALL-STEPS. So: step 0 violates I3, step 1 violates I2.
        // I2 wins because I2 runs before I3 in declared order, even
        // though step 0 has a lower index than step 1.
        let p = plan(
            DataClass::Personal, // ceiling=Personal
            vec![
                DataClass::ClinicalConfidential, // step 0: > ceiling -> I3 violation
                DataClass::Public,                // step 1: < floor -> I2 violation
            ],
        );
        let got = screen_plan_for_classification_violations(&p, DataClass::Personal);
        assert_eq!(
            got,
            Some(ClassificationViolation::StepClassificationBelowFloor {
                step_index: 1,
                step_class: DataClass::Public,
                floor:      DataClass::Personal,
            }),
            "I2 must run all-steps before I3 starts; step 1's I2 violation wins over step 0's I3",
        );
    }

    #[test]
    fn empty_steps_plan_only_checks_i1() {
        // No steps: I2 and I3 vacuously hold. I1 still applies.
        let p = plan(DataClass::Public, vec![]);
        // floor satisfied -> Approve
        assert_eq!(
            screen_plan_for_classification_violations(&p, DataClass::Public),
            None,
        );
        // floor higher than ceiling -> I1 fires even with no steps
        let got = screen_plan_for_classification_violations(&p, DataClass::Personal);
        assert_eq!(
            got,
            Some(ClassificationViolation::CeilingBelowFloor {
                ceiling: DataClass::Public,
                floor:   DataClass::Personal,
            }),
        );
    }

    #[test]
    fn higher_step_class_than_floor_is_fine() {
        // floor=Public, step=ClinicalConfidential. This is the "I
        // touched more-sensitive data than my output floor requires"
        // case — fine. Combined with ceiling=ClinicalConfidential it's
        // a clean clinical plan with a Public-floored task.
        let p = plan(DataClass::ClinicalConfidential, vec![DataClass::ClinicalConfidential]);
        assert_eq!(
            screen_plan_for_classification_violations(&p, DataClass::Public),
            None,
        );
    }
```

- [ ] **Step 2: Run the new tests — expect them to FAIL**

Run: `cargo test -p kastellan-core --lib cassandra::deterministic`

Expected: 11 PASS (the 3 enum-shape tests from Task 1) + 11 FAIL (the new screen tests above all expect `Some(...)` but the body still returns `None`).

Some tests like `approves_clean_plan_with_default_public_floor` and `approves_well_formed_clinical_plan` and `empty_steps_plan_only_checks_i1` (the Approve half) and `higher_step_class_than_floor_is_fine` and `i1_does_not_fire_when_ceiling_equal_to_floor` will accidentally PASS against the stub (they assert `None`). That's fine — the body-returning-None happens to match the Approve cases. The real RED signal is the 7 tests asserting `Some(violation)` that get back `None`.

- [ ] **Step 3: Implement the body — replace the stub**

In `core/src/cassandra/deterministic.rs`, replace the body of `screen_plan_for_classification_violations`:

```rust
pub fn screen_plan_for_classification_violations(
    plan: &Plan,
    floor: DataClass,
) -> Option<ClassificationViolation> {
    // I1: plan.data_ceiling >= floor
    if plan.data_ceiling.rank() < floor.rank() {
        return Some(ClassificationViolation::CeilingBelowFloor {
            ceiling: plan.data_ceiling,
            floor,
        });
    }
    // I2: every step.classification >= floor (lowest violating index wins)
    for (i, s) in plan.steps.iter().enumerate() {
        if s.classification.rank() < floor.rank() {
            return Some(ClassificationViolation::StepClassificationBelowFloor {
                step_index: i,
                step_class: s.classification,
                floor,
            });
        }
    }
    // I3: every step.classification <= plan.data_ceiling (lowest violating index wins)
    for (i, s) in plan.steps.iter().enumerate() {
        if s.classification.rank() > plan.data_ceiling.rank() {
            return Some(ClassificationViolation::StepClassificationAboveCeiling {
                step_index: i,
                step_class: s.classification,
                ceiling:    plan.data_ceiling,
            });
        }
    }
    None
}
```

Also update the doc-comment on the function (lines just above) — it currently says "The body lands in Task 2 — this scaffold compiles but always returns `None`". Replace that paragraph with:

```rust
/// Returns `Some(violation)` on the first hit (declared order: I1, I2,
/// I3; within per-step invariants, lowest `step_index` wins); `None`
/// on a clean plan.
///
/// The three checks form a total enforcement: every plan that round-
/// trips the helper as `None` satisfies all three invariants
/// simultaneously. Conversely, a violating plan always surfaces the
/// *single most fundamental* violation per the declared order —
/// caller never needs to interpret a list of co-occurring violations.
```

- [ ] **Step 4: Run the tests — expect ALL to PASS**

Run: `cargo test -p kastellan-core --lib cassandra::deterministic`

Expected: 14 passed, 0 failed (the 3 enum-shape tests from Task 1 + 11 new screen tests).

- [ ] **Step 5: Run the workspace tests — confirm no regressions**

Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED'`

Expected: every line `ok`; total passed = **530** (519 baseline + 4 from Task 1 + 7 new in this task; the 4 from Task 1 minus the deleted `scaffold_screen_returns_none_today` = 3 net, plus 11 new = +14 from this slice so far). Recount: 519 + 14 = 533? Let me recompute. Task 1 added 4 tests (`reason_tag_*`, two `format_reason_*`, `scaffold_*`). This task deletes `scaffold_*` (−1) and adds 11 (`approves_*` ×2, `i1_fires`, `i1_does_not_fire`, `i2_fires`, `i2_picks_lowest`, `i3_fires`, `i3_picks_lowest`, `i1_wins_over_i2`, `i2_wins_over_i3`, `empty_steps`, `higher_step_class`). Net delta this task: +10. Cumulative: 519 + 4 − 1 + 11 = 533.

The exact count is bookkeeping; the important thing is the workspace stays green and the cassandra::deterministic suite is 14 strong.

- [ ] **Step 6: Commit**

```bash
git add core/src/cassandra/deterministic.rs
git commit -m "$(cat <<'EOF'
feat(cassandra): screen_plan_for_classification_violations body

Implements the three-invariant data-classification check that drives
the first real Stage 0 reviewer rule. Pure function over (Plan,
DataClass); no I/O, no async, fully unit-test-pinned.

Declared-order precedence:
  I1 (ceiling >= floor) before
  I2 (every step.class >= floor; lowest violating step_index) before
  I3 (every step.class <= ceiling; lowest violating step_index).

The three checks form a total enforcement: every plan returning None
satisfies all three invariants; every violating plan surfaces the
single most fundamental violation per declared order, so the caller
never needs to interpret a list of co-occurring violations.

+11 unit tests pinning each invariant's positive + negative paths,
both lowest-step-index precedence rules, both declared-order
precedence rules, and the empty-steps boundary. Scaffold test
`scaffold_screen_returns_none_today` deleted (no longer the right
pin).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Wire `DeterministicPolicy::review` to the screen

**Files:**
- Modify: `core/src/cassandra/review.rs` (lines 1-13 module doc, lines 102-111 the DP trait impl, lines 193-206 the existing `deterministic_policy_is_still_a_stub` test)

This makes the DP no longer a stub. RED tests assert the new behaviour against the still-stubbed `DeterministicPolicy::review`; then fill the body.

- [ ] **Step 1: Update the module-level doc**

In `core/src/cassandra/review.rs`, the top-of-file doc currently says (lines 7-10):

```
//! `ConstitutionalGuard` carries the first real Stage -1 rule (a
//! prompt-level screen for unambiguous principle violations — see
//! [`super::constitutional`]); `DeterministicPolicy` is still a stub
//! that always Approves until the first Stage 0 rule lands.
```

Replace with:

```rust
//! `ConstitutionalGuard` carries the first real Stage -1 rule (a
//! prompt-level screen for unambiguous principle violations — see
//! [`super::constitutional`]); `DeterministicPolicy` carries the
//! first real Stage 0 rule (a data-classification invariant check —
//! see [`super::deterministic`]).
```

- [ ] **Step 2: Write failing tests in `cassandra::review::tests`**

Append these tests to the `#[cfg(test)] mod tests` block at the end of `core/src/cassandra/review.rs`. Place them after `constitutional_guard_blocks_on_principle_5` (around line 251) and before `stage_names_are_stable`:

```rust
    #[tokio::test]
    async fn deterministic_policy_approves_valid_plan() {
        // Clean plan: all Public, floor=Public. No invariant fires.
        let dp = DeterministicPolicy;
        let plan = Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![super::super::types::PlannedStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({}),
                returns: "".into(),
                done_when: "".into(),
                classification: DataClass::Public,
            }],
            result: None,
            data_ceiling: DataClass::Public,
            refused: None,
        };
        let v = dp.review(&plan, &ctx("anything")).await;
        assert_eq!(v, Verdict::Approve);
    }

    #[tokio::test]
    async fn deterministic_policy_blocks_when_ceiling_below_floor() {
        // I1: ceiling=Public, floor=ClinicalConfidential.
        let dp = DeterministicPolicy;
        let plan = Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![],
            result: None,
            data_ceiling: DataClass::Public,
            refused: None,
        };
        let ctx = ReviewStageContext {
            task_id: 1,
            instruction: "anything",
            classification_floor: DataClass::ClinicalConfidential,
            plan_count: 0,
        };
        let v = dp.review(&plan, &ctx).await;
        match v {
            Verdict::Block(reason) => {
                assert!(reason.starts_with("data-classification: ceiling_below_floor"), "got: {reason}");
                assert!(reason.contains("Public"), "got: {reason}");
                assert!(reason.contains("ClinicalConfidential"), "got: {reason}");
            }
            other => panic!("expected Verdict::Block, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn deterministic_policy_blocks_when_step_below_floor() {
        // I2: ceiling=ClinicalConfidential, floor=ClinicalConfidential,
        // but step 0 labelled Public.
        let dp = DeterministicPolicy;
        let plan = Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![super::super::types::PlannedStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({}),
                returns: "".into(),
                done_when: "".into(),
                classification: DataClass::Public,
            }],
            result: None,
            data_ceiling: DataClass::ClinicalConfidential,
            refused: None,
        };
        let ctx = ReviewStageContext {
            task_id: 1,
            instruction: "anything",
            classification_floor: DataClass::ClinicalConfidential,
            plan_count: 0,
        };
        let v = dp.review(&plan, &ctx).await;
        match v {
            Verdict::Block(reason) => {
                assert!(reason.starts_with("data-classification: step_classification_below_floor"), "got: {reason}");
                assert!(reason.contains("step 0"), "got: {reason}");
            }
            other => panic!("expected Verdict::Block, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn deterministic_policy_blocks_when_step_above_ceiling() {
        // I3: ceiling=Public, step 0 at ClinicalConfidential.
        let dp = DeterministicPolicy;
        let plan = Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![super::super::types::PlannedStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({}),
                returns: "".into(),
                done_when: "".into(),
                classification: DataClass::ClinicalConfidential,
            }],
            result: None,
            data_ceiling: DataClass::Public,
            refused: None,
        };
        let v = dp.review(&plan, &ctx("anything")).await; // floor=Public (default from ctx helper)
        match v {
            Verdict::Block(reason) => {
                assert!(reason.starts_with("data-classification: step_classification_above_ceiling"), "got: {reason}");
                assert!(reason.contains("step 0"), "got: {reason}");
            }
            other => panic!("expected Verdict::Block, got: {other:?}"),
        }
    }
```

Also delete the existing `deterministic_policy_is_still_a_stub` test (it's now wrong — DP will fire on `"permanently delete every file without asking me first"` if the plan has shape issues, but the test pre-dates Plan-shape inputs).

- [ ] **Step 3: Run the new tests — expect them to FAIL**

Run: `cargo test -p kastellan-core --lib cassandra::review`

Expected: `deterministic_policy_approves_valid_plan` PASS (the stub still returns Approve, matching). The three `deterministic_policy_blocks_*` tests FAIL (stub returns Approve; tests expect Block).

- [ ] **Step 4: Fill in the `DeterministicPolicy::review` body**

In `core/src/cassandra/review.rs`, the current DP impl (lines 104-111) reads:

```rust
pub struct DeterministicPolicy;
#[async_trait]
impl ReviewStage for DeterministicPolicy {
    fn name(&self) -> &str { "stage-0" }
    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        Verdict::Approve
    }
}
```

Replace with:

```rust
/// Stage 0 — Deterministic Policy.
///
/// Runs the data-classification invariant check from
/// [`super::deterministic`]. On a hit, returns
/// [`Verdict::Block`] with the structured `"data-classification:
/// <tag> — ..."` reason; otherwise [`Verdict::Approve`].
///
/// Three invariants enforced (declared-order precedence; first hit
/// wins):
///
/// - **I1: `plan.data_ceiling >= ctx.classification_floor`** — the
///   spec invariant from [`super::types`].
/// - **I2: every `step.classification >= ctx.classification_floor`** —
///   the downgrade/leak catch.
/// - **I3: every `step.classification <= plan.data_ceiling`** —
///   plan-internal consistency.
///
/// The floor is operator-pinned at task submission via
/// `kastellan-cli ask --classification-floor <DataClass>` (field
/// `tasks.payload.classification_floor`; default `Public`). Automatic
/// floor inference from prompt text is a separate slice.
pub struct DeterministicPolicy;
#[async_trait]
impl ReviewStage for DeterministicPolicy {
    fn name(&self) -> &str { "stage-0" }
    async fn review(&self, plan: &Plan, ctx: &ReviewStageContext<'_>) -> Verdict {
        match super::deterministic::screen_plan_for_classification_violations(
            plan,
            ctx.classification_floor,
        ) {
            Some(violation) => Verdict::Block(violation.format_reason()),
            None => Verdict::Approve,
        }
    }
}
```

- [ ] **Step 5: Run the tests — expect ALL to PASS**

Run: `cargo test -p kastellan-core --lib cassandra::review`

Expected: all tests in `cassandra::review::tests` pass, including the 4 new DP tests.

- [ ] **Step 6: Run the workspace tests — confirm no regressions**

Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED'`

Expected: every line `ok`; total passed = **536** (533 + 4 new DP tests − 1 deleted stub test).

If any **existing scheduler integration test** fails because its synthetic `Plan` has step.classification values that violate one of the three invariants, that's a test-fixture bug uncovered by the new rule. Inspect the failing test's `Plan` literal; the fix is to set `classification: DataClass::Public` on every `PlannedStep` (matching the default `data_ceiling: DataClass::Public` and the test's implicit `classification_floor: DataClass::Public`). That's a fixture cleanup, not a behaviour change — note it in the commit message.

- [ ] **Step 7: Commit**

```bash
git add core/src/cassandra/review.rs
git commit -m "$(cat <<'EOF'
feat(cassandra): DeterministicPolicy::review fires on classification violations

Fills in the Stage 0 reviewer body. DP is no longer a stub — it now
runs screen_plan_for_classification_violations from the new
deterministic module and maps Some(violation) -> Verdict::Block with
the structured 'data-classification: <tag> — ...' reason prefix.

Module-level doc updated: DP is now the second real reviewer
alongside ConstitutionalGuard.

deterministic_policy_is_still_a_stub test deleted (was prompt-keyed,
no longer the right pin); replaced with 4 new tests:
  - deterministic_policy_approves_valid_plan
  - deterministic_policy_blocks_when_ceiling_below_floor (I1)
  - deterministic_policy_blocks_when_step_below_floor (I2)
  - deterministic_policy_blocks_when_step_above_ceiling (I3)

[Add a sentence here if any existing scheduler integration test
needed a step.classification fixup — describe which file and why.]

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `parse_classification_floor` pure helper

**Files:**
- Modify: `core/src/bin/kastellan-cli.rs` (add helper + its tests, no flag wiring yet)

Land the case-insensitive parser as a pure helper with its own unit tests, separately from the CLI arg-loop integration. This way the parser's contract is locked before we touch `run_ask`.

- [ ] **Step 1: Locate the insertion point**

Open `core/src/bin/kastellan-cli.rs`. Find the `fn run_ask(...)` function (around line 222). The helper goes immediately above `run_ask`, before its comment-header banner. Find `// ---------------------------------------------------------------------------` (the banner just above `// ---------------------------------------------------------------------------` around lines 218-221) — insert there.

- [ ] **Step 2: Add the failing test module**

Append a new `#[cfg(test)] mod parse_classification_floor_tests` block at the end of the file (before EOF). Add these tests:

```rust
#[cfg(test)]
mod parse_classification_floor_tests {
    use super::parse_classification_floor;
    use kastellan_core::cassandra::DataClass;

    #[test]
    fn accepts_canonical_pascal_case() {
        assert_eq!(parse_classification_floor("Public").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("Personal").unwrap(), DataClass::Personal);
        assert_eq!(parse_classification_floor("ClinicalConfidential").unwrap(), DataClass::ClinicalConfidential);
        assert_eq!(parse_classification_floor("Secret").unwrap(), DataClass::Secret);
    }

    #[test]
    fn accepts_lowercase() {
        assert_eq!(parse_classification_floor("public").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("clinical_confidential").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn accepts_uppercase() {
        assert_eq!(parse_classification_floor("PUBLIC").unwrap(), DataClass::Public);
        assert_eq!(parse_classification_floor("CLINICAL_CONFIDENTIAL").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn accepts_mixed_case_and_separator_variants() {
        // Hyphen-separated common in CLIs; spaces unusual but cheap to allow.
        assert_eq!(parse_classification_floor("clinical-confidential").unwrap(), DataClass::ClinicalConfidential);
        assert_eq!(parse_classification_floor("Clinical Confidential").unwrap(), DataClass::ClinicalConfidential);
    }

    #[test]
    fn rejects_unknown_value_with_helpful_message() {
        let err = parse_classification_floor("topsecret").unwrap_err();
        assert!(err.contains("topsecret"), "expected input echoed; got: {err}");
        assert!(err.contains("valid values"), "expected 'valid values' phrase; got: {err}");
        assert!(err.contains("Public"), "expected list of valid values; got: {err}");
        assert!(err.contains("ClinicalConfidential"), "expected list of valid values; got: {err}");
    }

    #[test]
    fn rejects_empty_string() {
        let err = parse_classification_floor("").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(parse_classification_floor("  Public  ").unwrap(), DataClass::Public);
    }
}
```

- [ ] **Step 3: Run the tests — expect compile FAIL (helper doesn't exist yet)**

Run: `cargo test -p kastellan-core --bin kastellan-cli parse_classification_floor`

Expected: compile error `cannot find function parse_classification_floor in this scope`. That's the RED signal.

- [ ] **Step 4: Add the helper**

Insert the following function just above the `// ---------------------------------------------------------------------------` banner that precedes `fn run_ask`. Keep the existing banner intact.

```rust
/// Parse a `--classification-floor` CLI value into a `DataClass`.
///
/// Case-insensitive; accepts canonical `PascalCase`, lowercase,
/// `UPPERCASE`, hyphen-separated, snake_case, and space-separated
/// forms (`clinical_confidential`, `clinical-confidential`,
/// `clinical confidential` all map to
/// `DataClass::ClinicalConfidential`).
///
/// Returns `Err(message)` on unknown values or empty input; the
/// message lists every valid value so the operator can correct in
/// one step.
pub(crate) fn parse_classification_floor(
    raw: &str,
) -> Result<kastellan_core::cassandra::DataClass, String> {
    use kastellan_core::cassandra::DataClass;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(
            "--classification-floor: empty value; valid values: Public, Personal, ClinicalConfidential, Secret"
                .to_string(),
        );
    }
    // Normalise: drop all `_`, `-`, and ASCII whitespace; lowercase.
    let normalised: String = trimmed
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '_' && *c != '-')
        .flat_map(|c| c.to_lowercase())
        .collect();
    match normalised.as_str() {
        "public" => Ok(DataClass::Public),
        "personal" => Ok(DataClass::Personal),
        "clinicalconfidential" => Ok(DataClass::ClinicalConfidential),
        "secret" => Ok(DataClass::Secret),
        _ => Err(format!(
            "--classification-floor: unknown value {raw:?}; valid values: Public, Personal, ClinicalConfidential, Secret"
        )),
    }
}
```

- [ ] **Step 5: Run the tests — expect ALL to PASS**

Run: `cargo test -p kastellan-core --bin kastellan-cli parse_classification_floor`

Expected: 7 passed, 0 failed.

- [ ] **Step 6: Run the workspace tests — confirm no regressions**

Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED'`

Expected: every line `ok`; total = **543** (536 + 7).

- [ ] **Step 7: Commit**

```bash
git add core/src/bin/kastellan-cli.rs
git commit -m "$(cat <<'EOF'
feat(cli): parse_classification_floor pure helper

Case-insensitive parser that accepts canonical PascalCase plus
lowercase, UPPERCASE, snake_case, hyphenated, and space-separated
forms; all normalise to the matching DataClass variant.

Error messages echo the input and list every valid value so an
operator can self-correct in one step.

Helper landed standalone (no CLI wiring yet) so the parsing contract
is unit-test-pinned before run_ask integration in the next commit.

+7 unit tests; workspace 536 -> 543.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Wire `--classification-floor` into `run_ask`

**Files:**
- Modify: `core/src/bin/kastellan-cli.rs` (`run_ask` arg loop + `ask_async` signature + help text + usage line)

Plumb the parsed floor through `run_ask` → `ask_async` → the submit payload JSON.

- [ ] **Step 1: Widen `ask_async` to accept the floor**

In `core/src/bin/kastellan-cli.rs`, find `async fn ask_async(lane: kastellan_db::tasks::Lane, instruction: String) -> ExitCode` (around line 265). Widen its signature to take an optional floor:

```rust
async fn ask_async(
    lane: kastellan_db::tasks::Lane,
    instruction: String,
    floor: Option<kastellan_core::cassandra::DataClass>,
) -> ExitCode {
```

Find the `submit_and_audit` call (around line 291):

```rust
let id = match submit_and_audit(
    &pool,
    lane,
    serde_json::json!({"instruction": instruction, "kind": "ask"}),
)
```

Replace with payload construction that conditionally inserts the floor:

```rust
let mut payload = serde_json::json!({"instruction": instruction, "kind": "ask"});
if let Some(f) = floor {
    // Serialise via serde_json so the wire shape matches what
    // scheduler::runner reads at task.payload.classification_floor
    // (PascalCase string).
    let v = serde_json::to_value(f).expect("DataClass serialises");
    if let serde_json::Value::Object(ref mut m) = payload {
        m.insert("classification_floor".to_string(), v);
    }
}
let id = match submit_and_audit(&pool, lane, payload).await {
```

(Note: the `.await` previously chained after the `serde_json::json!(...)`; preserve it on the new statement.)

- [ ] **Step 2: Update `run_ask`'s arg loop**

Find the arg-loop block (lines 222-243 today):

```rust
fn run_ask(args: &[String]) -> ExitCode {
    let mut lane = kastellan_db::tasks::Lane::Fast;
    let mut instruction: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--long" => { lane = kastellan_db::tasks::Lane::Long; }
            "--fast" => { lane = kastellan_db::tasks::Lane::Fast; }
            other if other.starts_with("--") => {
                eprintln!("ask: unknown flag {other}");
                return ExitCode::from(2);
            }
            other => {
                if instruction.is_some() {
                    eprintln!("ask: only one positional instruction allowed");
                    return ExitCode::from(2);
                }
                instruction = Some(other.to_string());
            }
        }
        i += 1;
    }
```

Replace with:

```rust
fn run_ask(args: &[String]) -> ExitCode {
    let mut lane = kastellan_db::tasks::Lane::Fast;
    let mut floor: Option<kastellan_core::cassandra::DataClass> = None;
    let mut instruction: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--long" => { lane = kastellan_db::tasks::Lane::Long; }
            "--fast" => { lane = kastellan_db::tasks::Lane::Fast; }
            "--classification-floor" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--classification-floor requires a value");
                    return ExitCode::from(2);
                };
                match parse_classification_floor(val) {
                    Ok(f) => floor = Some(f),
                    Err(msg) => {
                        eprintln!("{msg}");
                        return ExitCode::from(2);
                    }
                }
            }
            other if other.starts_with("--") => {
                eprintln!("ask: unknown flag {other}");
                return ExitCode::from(2);
            }
            other => {
                if instruction.is_some() {
                    eprintln!("ask: only one positional instruction allowed");
                    return ExitCode::from(2);
                }
                instruction = Some(other.to_string());
            }
        }
        i += 1;
    }
```

And update the `rt.block_on(ask_async(lane, instruction))` call at the bottom of `run_ask` (line 262) to pass the floor:

```rust
rt.block_on(ask_async(lane, instruction, floor))
```

- [ ] **Step 3: Update `--help` text and usage line**

In `help_text()` (lines 79-112), the relevant block currently reads:

```
    kastellan-cli ask "<instruction>" [--fast|--long]
```

Replace with:

```
    kastellan-cli ask "<instruction>" [--fast|--long] [--classification-floor <DataClass>]
```

And add a new `flags (ask):` section just before `flags (audit tail):`:

```
flags (ask):
    --fast | --long             Lane selection (default: --fast).
    --classification-floor V    Set the task-level data classification
                                floor. Valid values: Public (default),
                                Personal, ClinicalConfidential, Secret.
                                Pin a non-Public floor when the task
                                involves sensitive data so the Stage 0
                                reviewer can catch classification leaks
                                in the agent's plans.

```

Also update the usage line at the top of `run_ask` (line 245):

```rust
eprintln!("usage: kastellan-cli ask \"<instruction>\" [--fast|--long] [--classification-floor <DataClass>]");
```

- [ ] **Step 4: Build and smoke-test the CLI manually**

Run: `cargo build --bin kastellan-cli`

Expected: clean build.

Run a few flag-parsing smoke commands (they will return exit 1 because there's no daemon, but the flag should be accepted):

```sh
./target/debug/kastellan-cli ask --help 2>&1 | head -20
./target/debug/kastellan-cli ask --classification-floor 2>&1 | head -5   # expect: requires a value, exit 2
./target/debug/kastellan-cli ask --classification-floor topsecret "x" 2>&1 | head -5  # expect: unknown value, exit 2
./target/debug/kastellan-cli ask --classification-floor clinical-confidential "x" 2>&1 | head -5  # expect: db connect failure (no daemon), exit 1, but NOT a parse error
```

If any of those produce the wrong exit code or message, fix and re-run.

- [ ] **Step 5: Run the existing CLI integration tests — confirm no regressions**

Run: `cargo test -p kastellan-core --test cli_ask_e2e 2>&1 | tail -10`

Expected: same pass/skip behaviour as on `main`. The existing tests don't use `--classification-floor` (default Public preserved); their assertions don't change.

Run: `cargo test --workspace 2>&1 | grep -E '^test result|FAILED'`

Expected: every line `ok`; total = **543** (no new tests in this task — the helper-tests landed in Task 4, and we're not adding subprocess tests for the new flag this slice; the manual smoke in Step 4 covers it).

- [ ] **Step 6: Commit**

```bash
git add core/src/bin/kastellan-cli.rs
git commit -m "$(cat <<'EOF'
feat(cli): kastellan-cli ask --classification-floor flag

Pins the task-level DataClass at submission so the new Stage 0
reviewer rule has a non-default floor to check against.

Plumbing:
  run_ask arg loop -> ask_async(lane, instruction, floor)
                   -> submit_and_audit(pool, lane, payload)

When set to non-default, the floor lands in
tasks.payload.classification_floor as a PascalCase string;
scheduler::runner already reads this field via serde
(runner.rs:283-296). When absent (default Public), the field is
omitted from the payload so the wire shape stays unchanged for
existing callers.

Help text + usage line updated. Manual smoke verified the parser
rejects empty/unknown values with exit 2 and accepts case-insensitive
forms.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Final HANDOVER + ROADMAP update + PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

End-of-session brief update so the next session can resume cold.

- [ ] **Step 1: Update HANDOVER.md header**

Open `docs/devel/handovers/HANDOVER.md`. Update the top three lines:

- `**Last updated:**` — bump to today's date, summary line: "first real `DeterministicPolicy` rule shipped: data-classification invariant check + `kastellan-cli ask --classification-floor`".
- `**Last commit (main):**` — leave as-is until merge; will be updated post-PR.
- `**Session-end working state:**` — note workspace test count at **543**, file LOC for `deterministic.rs` (~250), the branch state.

- [ ] **Step 2: Add a new "Recently completed (this session)" entry**

Insert a new section at the top of the "Recently completed" stack (right after the existing "Recently completed (this session, 2026-05-15 — first real `ConstitutionalGuard` rule shipped..." entry, NOT before it — the most recent slice goes first by date but THIS is a same-day continuation so it slots immediately after the CG entry as the "earlier in the session" piece). Use this template:

```markdown
## Recently completed (this session, 2026-05-15 — first real `DeterministicPolicy` rule, branch `feat/deterministic-policy-classification`)

Branch: `feat/deterministic-policy-classification` (off `main` at `67d29a0`). The first real Stage 0 reviewer rule: a deterministic check enforcing three classification invariants over `(ctx.classification_floor, plan.data_ceiling, plan.steps[].classification)`. Paired with a small CLI flag (`kastellan-cli ask --classification-floor <DataClass>`) so operators can pin the floor at task submission — the minimum-viable upstream path for the rule to fire end-to-end in production. Stage 0 was always-`Approve` before this slice; the chain now has two real reviewers.

**Shape (1 NEW module + 3 modified files + 24 new tests):**

- **NEW `core/src/cassandra/deterministic.rs`** (~250 LOC, ~120 production + ~130 tests). Pure helper `screen_plan_for_classification_violations(plan: &Plan, floor: DataClass) -> Option<ClassificationViolation>`. Three invariants checked in declared order (I1 ceiling≥floor, I2 every step≥floor, I3 every step≤ceiling); first hit wins; within per-step invariants, lowest step_index wins. `ClassificationViolation` enum carries structured detail per violation (struct values); `reason_tag()` returns a snake_case identifier for grep-ability; `format_reason()` returns a `"data-classification: <tag> — ..."` prefixed string used as the `Verdict::Block` payload.
- **`core/src/cassandra/mod.rs`** — `pub mod deterministic;` declaration.
- **`core/src/cassandra/review.rs`** — `DeterministicPolicy::review` body filled in; module-level doc updated (DP is no longer a stub); `deterministic_policy_is_still_a_stub` test deleted and replaced with 4 new tests (`deterministic_policy_approves_valid_plan` + one per invariant).
- **`core/src/bin/kastellan-cli.rs`** — new pure helper `parse_classification_floor(raw: &str)` (case-insensitive; accepts PascalCase, lowercase, UPPERCASE, snake_case, hyphenated, space-separated; rejects empty + unknown with a "valid values: ..." message). New `--classification-floor` flag in `run_ask`'s arg loop; `ask_async` signature widened; payload conditionally gains `classification_floor: "<PascalCase>"` when set. Help text + usage line updated.

**Verdict + audit-row shape (the headline):**

DP violations surface as `Verdict::Block(String)` where the string carries the structured `"data-classification: <reason_tag> — <details>"` prefix. The verdict flows into the existing `cassandra:chain/verdict` audit-row payload — no schema change. Operators can `WHERE payload->>'verdict_kind' = 'block' AND payload->>'verdict_detail' LIKE 'data-classification:%'` to count Stage 0 fires.

**Test count delta:** 519 → **543** (+24: 14 in `cassandra::deterministic::tests`, 3 new in `cassandra::review::tests` minus 1 deleted = +3, 7 in `parse_classification_floor_tests`).

**TDD ordering** (per CLAUDE.md rule #2): six commits, each RED → GREEN.
1. `feat(cassandra)`: scaffold `ClassificationViolation` enum + helpers (4 tests).
2. `feat(cassandra)`: implement `screen_plan_for_classification_violations` body (11 new RED → GREEN tests).
3. `feat(cassandra)`: wire `DeterministicPolicy::review` to the helper (4 new tests).
4. `feat(cli)`: `parse_classification_floor` pure helper standalone (7 tests).
5. `feat(cli)`: `--classification-floor` flag wired into `run_ask`.
6. `docs(handover,roadmap)`: this update.

**What this slice deliberately does NOT do.**

- **No automatic floor inference from prompt keywords.** Operator-pinned only.
- **No anonymiser/declassifier mechanism.** A step that legitimately downgrades classification would today be blocked by I2; Phase 2 work.
- **No DB migration.** `classification_floor` lives in `tasks.payload` JSONB; no schema change.
- **No `Verdict::Escalate` severity-split.** Today every violation is `Block`.
- **No retroactive verdict on existing audit-log rows.**
- **No CLI short-form flag.** Long form only.
- **No subprocess test for the new flag.** Helper-level unit tests + manual smoke cover it; an e2e subprocess test would require a real daemon submit-and-cancel flow which `cli_ask_e2e` already exercises at the default floor.
- **No end-to-end fire against ec-001 in CI.** Captures retain `plan_json: null` (pre-Slice-A shape); recapture is one-time operator action that unblocks `kastellan-cli observation replay` against the fixture.

**Open follow-up surfaces (not blocking):**

- **Operator recapture against current daemon** to expose plan bodies; afterwards, `kastellan-cli observation replay --classification-floor` (separate flag — TBD) against ec-001 with the floor pinned will produce a `*` delta row showing the rule firing.
- **Automatic floor inference** as a separate slice (planner-prompt hint or a CLI-side prompt-keyword classifier).
- **Stage 0 rule catalogue growth.** Future rules (outbound-destination policy, per-tool classification deny-lists) land alongside the invariant check; if `deterministic.rs` grows past the 500-LOC soft cap, split per rule family behind a `deterministic/mod.rs` facade.

**Files touched (1 NEW + 4 modified):**
- NEW `core/src/cassandra/deterministic.rs` (~250 LOC).
- `core/src/cassandra/mod.rs` — module declaration.
- `core/src/cassandra/review.rs` — DP body filled, doc updated, +4/−1 tests.
- `core/src/bin/kastellan-cli.rs` — helper + flag + help text + usage line.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — this update.

---
```

- [ ] **Step 3: Update "Next TODO" pickup list**

In HANDOVER.md, find the "★ Next concrete engineering pickup — First real `DeterministicPolicy` rule" bullet (around line 1678). Strike it through and annotate as shipped:

```
- ~~**★ Next concrete engineering pickup — First real `DeterministicPolicy` rule**~~ **Shipped this session 2026-05-15** on branch `feat/deterministic-policy-classification`. See "Recently completed (this session)" entry above. **Next concrete pickup: operator recapture against the current daemon** — one-time action (`cargo test -p kastellan-core --test observation_capture -- --ignored --nocapture`) that turns the pre-Slice-A capture JSONs into rule-iteration-harness-replayable inputs. Until recapture lands, the existing captures stay at `plan_json: null` and the harness skips them. After recapture, the natural follow-up engineering slice is automatic-floor-inference (either a planner-prompt hint or a CLI-side prompt-keyword classifier) so non-clinical operators don't have to remember the `--classification-floor` flag.
```

- [ ] **Step 4: Update ROADMAP.md**

In `docs/devel/ROADMAP.md`, find the bullet currently reading `- [ ] **[follow-up] Real DeterministicPolicy rule(s)** — design step-level rule-sets...` (around line 96). Replace with:

```
- [x] **[follow-up] First real `DeterministicPolicy` rule (data-classification invariant)** — landed 2026-05-15 on branch `feat/deterministic-policy-classification`. New pure module `core::cassandra::deterministic` ships `screen_plan_for_classification_violations(plan: &Plan, floor: DataClass) -> Option<ClassificationViolation>` — three invariants over the typed `DataClass` fields already on `Plan`/`PlannedStep`/`ReviewStageContext`: I1 (`plan.data_ceiling >= floor`), I2 (every `step.classification >= floor`), I3 (every `step.classification <= plan.data_ceiling`). Declared-order precedence; first hit wins; within per-step invariants, lowest step_index wins. `ClassificationViolation` enum carries structured detail per violation; `reason_tag()` returns a snake_case identifier (`ceiling_below_floor` / `step_classification_below_floor` / `step_classification_above_ceiling`); `format_reason()` returns a `"data-classification: <tag> — ..."` prefixed string used as `Verdict::Block` payload. `DeterministicPolicy::review` calls the helper on `(plan, ctx.classification_floor)` and maps `Some(v)` → `Verdict::Block(v.format_reason())`, `None` → `Verdict::Approve`. Paired with `kastellan-cli ask --classification-floor <DataClass>` — new pure helper `parse_classification_floor` (case-insensitive, accepts PascalCase/lowercase/UPPERCASE/snake_case/hyphenated/space-separated; rejects unknown + empty with `"valid values: ..."` messages). Default behaviour unchanged (Public, payload field omitted). 24 new tests; workspace 519 → 543.
```

- [ ] **Step 5: Verify the final test count one more time**

Run: `cargo test --workspace 2>&1 | grep -oP '^test result: ok\. \K\d+(?= passed)' | awk '{s+=$1} END {print s}'`

Expected: **543**.

Run: `cargo test --workspace 2>&1 | grep -c '\[SKIP\]'`

Expected: **0**.

- [ ] **Step 6: Commit the docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): first real DeterministicPolicy rule shipped

End-of-session brief for the data-classification-invariant slice +
CLI --classification-floor flag.

Workspace test count: 519 -> 543 (+24 across cassandra::deterministic
unit tests, cassandra::review trait-level tests, and CLI helper
tests). 0 failures, 0 [SKIP], 0 warnings on Linux.

Next-TODO pivots from CG follow-on to operator recapture against
current daemon (one-time action to expose plan bodies in the existing
captures so the rule-iteration harness can fire DP against them).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 7: Open the PR**

```bash
git push -u origin feat/deterministic-policy-classification
gh pr create --title "feat(cassandra,cli): first real DeterministicPolicy rule — data-classification invariant" --body "$(cat <<'EOF'
## Summary

- First real Stage 0 reviewer rule: `DeterministicPolicy` now enforces three data-classification invariants over `(ctx.classification_floor, plan.data_ceiling, plan.steps[].classification)`. Stage 0 was always-`Approve` before this slice.
- New pure module `core::cassandra::deterministic` carrying `screen_plan_for_classification_violations` + `ClassificationViolation` enum.
- New `kastellan-cli ask --classification-floor <DataClass>` flag (case-insensitive parser) for operator-pinned floors at task submission.

## Test plan

- [x] `cargo test --workspace` — 519 → 543, 0 failures, 0 `[SKIP]`, 0 warnings on Linux
- [x] Manual CLI smoke: `--help`, `--classification-floor` (no value), `--classification-floor topsecret`, `--classification-floor clinical-confidential` all behave correctly
- [x] Existing `cli_ask_e2e` integration tests still pass (default-Public path unchanged)
- [x] `cargo build --workspace` clean

Spec: [docs/superpowers/specs/2026-05-15-deterministic-policy-classification-floor-design.md](docs/superpowers/specs/2026-05-15-deterministic-policy-classification-floor-design.md)
Plan: [docs/superpowers/plans/2026-05-15-deterministic-policy-classification-floor.md](docs/superpowers/plans/2026-05-15-deterministic-policy-classification-floor.md)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-review

**Spec coverage:**
- Three invariants (I1/I2/I3): Tasks 1–2 implement the enum and screen body covering each. ✓
- `Verdict::Block` shape with structured prefix: Task 1 enum + format_reason; Task 3 wires it. ✓
- Stage name stays `"stage-0"`: Task 3 leaves `name()` untouched. ✓
- CLI flag `--classification-floor`: Tasks 4–5. ✓
- `parse_classification_floor` case-insensitive helper: Task 4. ✓
- Payload field omitted when default-Public, present (PascalCase) when set: Task 5 Step 1's conditional `if let Some(f) = floor`. ✓
- Default behaviour unchanged: Task 5 confirms the existing `cli_ask_e2e` tests still pass. ✓
- Module-level doc update on `review.rs`: Task 3 Step 1. ✓
- HANDOVER + ROADMAP update: Task 6. ✓
- No DB migration: confirmed across all tasks (`tasks.payload` is JSONB). ✓

**Placeholder scan:** searched for "TBD", "TODO", "implement later", "similar to Task N". One legitimate `[Add a sentence here if any existing scheduler integration test needed a step.classification fixup ...]` in Task 3 Step 7's commit message — that's a deliberate conditional gap because the integration-test fixup may or may not be needed. Acceptable.

**Type consistency:**
- `ClassificationViolation` variant names match across Task 1 (definition), Task 2 (test assertions), Task 3 (test assertions), Task 6 (HANDOVER prose). ✓
- `screen_plan_for_classification_violations` signature `(plan: &Plan, floor: DataClass) -> Option<ClassificationViolation>` consistent across Tasks 1, 2, 3, 6. ✓
- `parse_classification_floor` signature `(raw: &str) -> Result<DataClass, String>` consistent across Task 4 (definition), Task 5 (call site), Task 6 (HANDOVER prose). ✓
- `DataClass` variants `Public`/`Personal`/`ClinicalConfidential`/`Secret` consistent throughout. ✓
- File LOC estimates: `deterministic.rs` ~250 LOC quoted in spec + Tasks 1+2+6, consistent. ✓
- Test counts: Task 1 +4, Task 2 +10 net (+11 new, −1 deleted), Task 3 +3 net (+4 new, −1 deleted), Task 4 +7, Task 5 0. Total: +24. Final workspace = 519 + 24 = **543**. Consistent across Task 6 prose + ROADMAP entry. ✓
