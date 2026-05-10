//! Data types for plan review.

use serde::{Deserialize, Serialize};

/// Classification of data flowing through a plan step.
///
/// Outbound policy attaches to each level (see
/// `docs/cassandra_design_plan.md` §7).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DataClass {
    Public,
    Personal,
    ClinicalConfidential,
    Secret,
}

impl DataClass {
    /// Total ordering: higher is more sensitive.
    pub fn rank(self) -> u8 {
        match self {
            DataClass::Public => 0,
            DataClass::Personal => 1,
            DataClass::ClinicalConfidential => 2,
            DataClass::Secret => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

/// One step within a plan. Each maps 1:1 to a `tool_host::dispatch`
/// invocation when the plan executes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlannedStep {
    pub tool: String,
    pub method: String,
    pub parameters: serde_json::Value,
    pub returns: String,
    pub done_when: String,
    pub classification: DataClass,
}

/// One agent-formulated plan, reviewed as a unit.
///
/// The terminal signal: `decision == "task_complete"` AND
/// `steps.is_empty()` AND `result.is_some()`. The reviewer trivially
/// approves these (no actions to evaluate); the inner loop returns
/// `Outcome::Completed(result)`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub context: String,
    pub decision: String,
    pub rationale: String,
    pub steps: Vec<PlannedStep>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    pub data_ceiling: DataClass,
}

// Invariant (enforced by future Stage 0 — not by stubs in this work's scope):
//   plan.data_ceiling >= task.classification_floor
// i.e. outputs cannot be classified *below* the producer-pinned floor without
// passage through an anonymiser/declassifier (anonymiser is out of scope here).
// The floor is a producer-set minimum on outputs; the ceiling is the inferred
// maximum classification of any input the plan touches.

impl Plan {
    pub fn is_terminal(&self) -> bool {
        self.decision == "task_complete"
            && self.steps.is_empty()
            && self.result.is_some()
    }
}

/// Reviewer verdict on one plan. The four-tier model from
/// `docs/cassandra_design_plan.md` §4.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    Approve,
    Advisory(String),
    Escalate(String, Severity),
    Block(String),
    /// Absolute, non-overridable. Numeric principle index 1..=5.
    ConstitutionalBlock { principle: u8, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_class_total_order_is_consistent() {
        assert!(DataClass::Public.rank() < DataClass::Personal.rank());
        assert!(DataClass::Personal.rank() < DataClass::ClinicalConfidential.rank());
        assert!(DataClass::ClinicalConfidential.rank() < DataClass::Secret.rank());
    }

    #[test]
    fn plan_is_terminal_requires_all_three_conditions() {
        let mut p = Plan {
            context: "c".into(),
            decision: "task_complete".into(),
            rationale: "r".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
            data_ceiling: DataClass::Public,
        };
        assert!(p.is_terminal(), "all three present");

        p.result = None;
        assert!(!p.is_terminal(), "missing result");

        p.result = Some(serde_json::json!("ok"));
        p.steps = vec![PlannedStep {
            tool: "x".into(),
            method: "y".into(),
            parameters: serde_json::json!({}),
            returns: "".into(),
            done_when: "".into(),
            classification: DataClass::Public,
        }];
        assert!(!p.is_terminal(), "non-empty steps");

        p.steps = vec![];
        p.decision = "act".into();
        assert!(!p.is_terminal(), "wrong decision string");
    }

    #[test]
    fn plan_serialises_skipping_none_result() {
        let p = Plan {
            context: "c".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![],
            result: None,
            data_ceiling: DataClass::Public,
        };
        let s = serde_json::to_string(&p).unwrap();
        let p2: Plan = serde_json::from_str(&s).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn verdict_serialises_all_variants() {
        for v in [
            Verdict::Approve,
            Verdict::Advisory("x".into()),
            Verdict::Escalate("y".into(), Severity::Medium),
            Verdict::Block("z".into()),
            Verdict::ConstitutionalBlock { principle: 1, reason: "harm".into() },
        ] {
            let s = serde_json::to_string(&v).unwrap();
            let v2: Verdict = serde_json::from_str(&s).unwrap();
            assert_eq!(v, v2);
        }
    }
}
