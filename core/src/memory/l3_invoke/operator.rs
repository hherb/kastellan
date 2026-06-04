//! Operator-path orchestration of an approved L3 skill (the invocation
//! "DOOR"). Drives the existing
//! [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`] through the
//! pure decision in [`super::pure::prepare_invocation`].
//!
//! Only `user_approved` / `pinned` skills run; dry-run is the default (the
//! CLI passes `execute = false`). There is NO agent-autonomous invocation
//! here (see [`super::agent`]) and NO CASSANDRA review on the operator path
//! — the reviewer polices agent-formulated plans; an operator running their
//! own approved skill with explicit args is an authorised action.
//!
//! See `docs/superpowers/specs/2026-06-02-l3-skill-invocation-design.md`.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::PgPool;

use crate::cassandra::types::{L3SkillCandidate, L3TemplateStep};
use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::memory::l3_approval::SkillTrust;
use crate::scheduler::audit::{
    build_l3_invoke_outcome_payload, build_l3_invoke_rejected_payload, build_l3_invoked_payload,
    ACTION_L3_INVOKED, ACTION_L3_INVOKE_OUTCOME, ACTION_L3_INVOKE_REJECTED,
};
use crate::scheduler::inner_loop::{StepDispatcher, StepOutcome};

use super::pure::{planned_step_from_l3, prepare_invocation, InvokeRefusal};

/// Dispatch each concrete step through the injected [`StepDispatcher`],
/// collecting outcomes and stopping at the first [`StepOutcome::Err`]
/// (mirrors `inner_loop::run_to_terminal`). No audit / DB here — the
/// per-step chokepoint rows are written inside `dispatch_step`; the
/// envelope rows are the caller's job.
pub async fn run_steps(
    dispatcher: &dyn StepDispatcher,
    steps: &[L3TemplateStep],
) -> Vec<StepOutcome> {
    let mut outcomes = Vec::with_capacity(steps.len());
    for step in steps {
        let ps = planned_step_from_l3(step);
        let outcome = dispatcher.dispatch_step(&ps).await;
        let is_err = outcome.is_err();
        outcomes.push(outcome);
        if is_err {
            break;
        }
    }
    outcomes
}

/// Result of an [`invoke_l3`] call.
#[derive(Debug)]
pub enum InvokeReport {
    /// Trust gate or live re-validation refused; nothing dispatched.
    Refused { reasons: Vec<String> },
    /// Dry-run (default): the concrete steps that WOULD dispatch.
    DryRun { steps: Vec<L3TemplateStep> },
    /// `--execute`: the per-step outcomes (stops at first error).
    Executed { outcomes: Vec<StepOutcome>, steps_total: usize },
}

/// Orchestrate operator-triggered invocation of an approved skill.
///
/// `memory_id` / `template` / `stored_trust` / `body_sha256` come from the
/// stored L3 row (`memory_id` is threaded into the audit-row payloads);
/// `live_tools` from the freshly-rebuilt registry's tool
/// names; `args` from `parse_args`. `execute == false` ⇒ dry-run: no
/// dispatch and no `l3.invoked` / `l3.invoke_outcome` rows — but a refusal
/// is **always** audited (`l3.invoke_rejected`) regardless of `execute`,
/// because a refused run attempt is a security event worth a trail. Audit
/// writes are best-effort (warn-on-failure), matching the chokepoint posture.
#[allow(clippy::too_many_arguments)]
pub async fn invoke_l3(
    pool: &PgPool,
    memory_id: i64,
    dispatcher: &dyn StepDispatcher,
    template: &L3SkillCandidate,
    stored_trust: SkillTrust,
    body_sha256: &str,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
    execute: bool,
) -> InvokeReport {
    let steps = match prepare_invocation(template, stored_trust, args, live_tools) {
        Ok(steps) => steps,
        Err(InvokeRefusal { reasons }) => {
            let payload =
                build_l3_invoke_rejected_payload(memory_id, &template.name, body_sha256, &reasons);
            best_effort_audit(pool, ACTION_L3_INVOKE_REJECTED, payload).await;
            return InvokeReport::Refused { reasons };
        }
    };

    if !execute {
        return InvokeReport::DryRun { steps };
    }

    let arg_names: Vec<String> = args.keys().cloned().collect();
    let invoked =
        build_l3_invoked_payload(memory_id, &template.name, body_sha256, &arg_names, steps.len());
    best_effort_audit(pool, ACTION_L3_INVOKED, invoked).await;

    let steps_total = steps.len();
    let outcomes = run_steps(dispatcher, &steps).await;
    let any_err = outcomes.iter().any(|o| o.is_err());
    let outcome_payload = build_l3_invoke_outcome_payload(
        memory_id, &template.name, outcomes.len(), steps_total, any_err,
    );
    best_effort_audit(pool, ACTION_L3_INVOKE_OUTCOME, outcome_payload).await;

    InvokeReport::Executed { outcomes, steps_total }
}

async fn best_effort_audit(pool: &PgPool, action: &str, payload: serde_json::Value) {
    if let Err(e) = hhagent_db::audit::insert(pool, CLI_AUDIT_ACTOR, action, payload).await {
        tracing::warn!(error = %e, action, "l3 invoke audit insert failed (best-effort)");
    }
}
