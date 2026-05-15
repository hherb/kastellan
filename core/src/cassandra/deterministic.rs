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
//!   operator-pinned via `hhagent-cli ask --classification-floor`.
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
/// on a clean plan.
///
/// The three checks form a total enforcement: every plan that round-
/// trips the helper as `None` satisfies all three invariants
/// simultaneously. Conversely, a violating plan always surfaces the
/// *single most fundamental* violation per the declared order —
/// the caller never needs to interpret a list of co-occurring
/// violations.
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
    // I3: every step.classification <= plan.data_ceiling (lowest violating index wins).
    //
    // MUST be a separate loop from I2: I2 runs all steps before I3 starts, so an
    // I2 violation at a higher step index still wins over an I3 violation at a
    // lower index. Fusing the two loops with `if/else` would silently break the
    // declared-order precedence pinned by `i2_wins_over_i3_when_both_could_fire`.
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
        // ceiling=Personal, floor=Personal, step 0 at ClinicalConfidential
        // (above ceiling -> I3), step 1 at Public (below floor -> I2).
        // I2 runs all-steps before I3 starts, so step 1's I2 violation
        // wins even though step 0 has a lower index than step 1.
        let p = plan(
            DataClass::Personal,
            vec![
                DataClass::ClinicalConfidential,
                DataClass::Public,
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
}
