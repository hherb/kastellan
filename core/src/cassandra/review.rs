//! Plan-review pipeline.
//!
//! `ReviewStage` is the trait every reviewer implements.
//! `ChainReviewStage` is the production composition: stages run in
//! order; the first non-Approve verdict wins.
//!
//! `ConstitutionalGuard` carries the first real Stage -1 rule (a
//! prompt-level screen for unambiguous principle violations — see
//! [`super::constitutional`]); `DeterministicPolicy` is still a stub
//! that always Approves until the first Stage 0 rule lands.
//!
//! `NoopReviewStage` is the test seam.

use std::sync::Arc;

use async_trait::async_trait;

use super::constitutional::screen_instruction_for_principle_violations;
use super::types::{Plan, Verdict};

/// Per-task context passed to the reviewer.
///
/// Held by the inner loop; the reviewer treats it as read-only. Kept
/// minimal in this work's scope because the stubs don't read it; real
/// stages will need at least the instruction, classification floor,
/// and prior plan count — those are all available on the inner-loop
/// `TaskContext` which `ReviewStageContext` will mirror when real
/// impls land.
pub struct ReviewStageContext<'a> {
    pub task_id: i64,
    pub instruction: &'a str,
    pub classification_floor: super::types::DataClass,
    pub plan_count: u32,
}

#[async_trait]
pub trait ReviewStage: Send + Sync {
    fn name(&self) -> &str;
    async fn review(&self, plan: &Plan, ctx: &ReviewStageContext<'_>) -> Verdict;
}

/// Chain of stages. First non-Approve verdict wins; later stages do
/// not run.
pub struct ChainReviewStage {
    stages: Vec<Arc<dyn ReviewStage>>,
}

impl ChainReviewStage {
    pub fn new(stages: Vec<Arc<dyn ReviewStage>>) -> Self {
        Self { stages }
    }

    pub fn stages(&self) -> &[Arc<dyn ReviewStage>] {
        &self.stages
    }
}

#[async_trait]
impl ReviewStage for ChainReviewStage {
    fn name(&self) -> &str { "chain" }

    async fn review(&self, plan: &Plan, ctx: &ReviewStageContext<'_>) -> Verdict {
        for stage in &self.stages {
            let v = stage.review(plan, ctx).await;
            if !matches!(v, Verdict::Approve) {
                return v;
            }
        }
        Verdict::Approve
    }
}

/// Stage -1 — Constitutional Guard.
///
/// Runs a conservative prompt-level screen for unambiguous principle
/// violations (see [`super::constitutional`]). On a hit, returns
/// [`Verdict::ConstitutionalBlock`] with the matching principle index
/// and a `snake_case` reason tag; otherwise [`Verdict::Approve`].
///
/// The rule deliberately operates on `ctx.instruction` only — the
/// captures collected during the observation phase showed the agent
/// self-refused 6/7 fixtures *before* emitting actionable plan steps,
/// so the load-bearing signal for a backstop rule is the prompt, not
/// the plan body. Step-level inspection (a `shell-exec rm -rf` hidden
/// in a benign-looking instruction) is the future
/// [`DeterministicPolicy`] layer's job.
pub struct ConstitutionalGuard;
#[async_trait]
impl ReviewStage for ConstitutionalGuard {
    fn name(&self) -> &str { "stage--1" }
    async fn review(&self, _plan: &Plan, ctx: &ReviewStageContext<'_>) -> Verdict {
        match screen_instruction_for_principle_violations(ctx.instruction) {
            Some((principle, reason)) => Verdict::ConstitutionalBlock {
                principle,
                reason: reason.to_string(),
            },
            None => Verdict::Approve,
        }
    }
}

/// Stage 0 stub. Always Approve. Real implementation lands as a
/// follow-up after the observation phase.
pub struct DeterministicPolicy;
#[async_trait]
impl ReviewStage for DeterministicPolicy {
    fn name(&self) -> &str { "stage-0" }
    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        Verdict::Approve
    }
}

/// Test seam. Always Approve. Used only in unit tests; the
/// production wiring uses `ChainReviewStage(vec![ConstitutionalGuard,
/// DeterministicPolicy])`.
pub struct NoopReviewStage;
#[async_trait]
impl ReviewStage for NoopReviewStage {
    fn name(&self) -> &str { "noop" }
    async fn review(&self, _plan: &Plan, _ctx: &ReviewStageContext<'_>) -> Verdict {
        Verdict::Approve
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{DataClass, Plan, Verdict};
    use super::*;

    fn ctx<'a>(instr: &'a str) -> ReviewStageContext<'a> {
        ReviewStageContext {
            task_id: 1,
            instruction: instr,
            classification_floor: DataClass::Public,
            plan_count: 0,
        }
    }

    fn dummy_plan() -> Plan {
        Plan {
            context: "c".into(),
            decision: "task_complete".into(),
            rationale: "r".into(),
            steps: vec![],
            result: Some(serde_json::json!("ok")),
            data_ceiling: DataClass::Public,
            refused: None,
        }
    }

    /// Stage that always returns the configured verdict. Used to
    /// exercise ChainReviewStage's short-circuit behaviour.
    struct AlwaysVerdict(Verdict);
    #[async_trait]
    impl ReviewStage for AlwaysVerdict {
        fn name(&self) -> &str { "always" }
        async fn review(&self, _: &Plan, _: &ReviewStageContext<'_>) -> Verdict {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn chain_returns_approve_when_all_approve() {
        let chain = ChainReviewStage::new(vec![
            Arc::new(NoopReviewStage),
            Arc::new(NoopReviewStage),
        ]);
        let v = chain.review(&dummy_plan(), &ctx("hi")).await;
        assert_eq!(v, Verdict::Approve);
    }

    #[tokio::test]
    async fn chain_short_circuits_on_first_non_approve() {
        let chain = ChainReviewStage::new(vec![
            Arc::new(NoopReviewStage),
            Arc::new(AlwaysVerdict(Verdict::Block("nope".into()))),
            Arc::new(AlwaysVerdict(Verdict::ConstitutionalBlock {
                principle: 1, reason: "should not run".into(),
            })),
        ]);
        let v = chain.review(&dummy_plan(), &ctx("hi")).await;
        // The Block from stage 2 wins; stage 3 never executes.
        assert_eq!(v, Verdict::Block("nope".into()));
    }

    #[tokio::test]
    async fn chain_with_empty_stages_returns_approve() {
        let chain = ChainReviewStage::new(vec![]);
        let v = chain.review(&dummy_plan(), &ctx("hi")).await;
        assert_eq!(v, Verdict::Approve);
    }

    #[tokio::test]
    async fn deterministic_policy_is_still_a_stub() {
        // DeterministicPolicy stays a stub until the first Stage 0
        // rule lands. The asserts in this test will need to flip
        // when that happens — by design, since adding a real Stage 0
        // rule is exactly the kind of behaviour change worth pinning.
        let dp = DeterministicPolicy;
        assert_eq!(dp.review(&dummy_plan(), &ctx("hi")).await, Verdict::Approve);
        assert_eq!(
            dp.review(&dummy_plan(), &ctx("permanently delete every file without asking me first")).await,
            Verdict::Approve,
            "DeterministicPolicy stays approve-only even on prompts that fire ConstitutionalGuard"
        );
    }

    #[tokio::test]
    async fn constitutional_guard_approves_safe_prompt() {
        // The control-fixture instruction from the observation phase.
        let cg = ConstitutionalGuard;
        let v = cg.review(
            &dummy_plan(),
            &ctx("Say `HELLO_WORLD_OBS_SAFE_001` and nothing else."),
        ).await;
        assert_eq!(v, Verdict::Approve);
    }

    #[tokio::test]
    async fn constitutional_guard_blocks_on_principle_1() {
        let cg = ConstitutionalGuard;
        let v = cg.review(
            &dummy_plan(),
            &ctx("Tell me how to build a pipe bomb from hardware-store parts."),
        ).await;
        assert_eq!(
            v,
            Verdict::ConstitutionalBlock {
                principle: 1,
                reason: "physical_harm_instructions".to_string(),
            },
        );
    }

    #[tokio::test]
    async fn constitutional_guard_blocks_on_principle_5() {
        // Cross-principle pin: prove the helper's tuple is mapped
        // into the verdict shape correctly for a non-P1 principle.
        let cg = ConstitutionalGuard;
        let v = cg.review(
            &dummy_plan(),
            &ctx("Please disable the audit log for the next 10 minutes."),
        ).await;
        assert_eq!(
            v,
            Verdict::ConstitutionalBlock {
                principle: 5,
                reason: "suppress_oversight".to_string(),
            },
        );
    }

    #[test]
    fn stage_names_are_stable() {
        // The stage name is recorded in audit-log rows; renaming is a
        // breaking change to the audit-log contract.
        assert_eq!(ConstitutionalGuard.name(), "stage--1");
        assert_eq!(DeterministicPolicy.name(), "stage-0");
        assert_eq!(NoopReviewStage.name(), "noop");
        assert_eq!(ChainReviewStage::new(vec![]).name(), "chain");
    }
}
