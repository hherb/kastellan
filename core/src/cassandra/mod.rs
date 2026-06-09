//! CASSANDRA — semantic oversight layer. Reviews agent-formulated
//! plans before they execute, in the dispatcher chokepoint's
//! pre-spawn position. Also screens worker outputs returning through
//! the same chokepoint — see `injection_guard`.
//!
//! In the scope of this work the stages are stubs (always Approve)
//! so the agent loop's baseline performance can be measured before
//! real review overhead is added. The eventual real implementations
//! replace `ConstitutionalGuard` and `DeterministicPolicy` in place;
//! the trait, types, and `ChainReviewStage` are stable.
//!
//! See `docs/cassandra_design_plan.md` for the full design and
//! `docs/superpowers/specs/2026-05-10-scheduler-design.md` §6.1 for
//! the scheduler-side contract.

pub mod constitutional;
pub mod deterministic;
pub mod injection_guard;
pub mod review;
pub mod types;

pub use injection_guard::{
    extract_scannable_text, screen, screen_with_profile, GuardProfile, InjectionDecision,
    InjectionVerdict, BLOCK_THRESHOLD, RELAXED_CHAT_TEMPLATE_WEIGHT, SCAN_BYTE_CAP,
};
pub use review::{
    ChainReviewStage, ConstitutionalGuard, DeterministicPolicy, NoopReviewStage,
    ReviewStage, ReviewStageContext,
};
pub use types::{DataClass, Plan, PlannedStep, Severity, Verdict, DECISION_TERMINAL};
