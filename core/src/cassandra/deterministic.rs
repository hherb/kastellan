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
