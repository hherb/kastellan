//! Autonomous `invoke_skill` expansion for the inner loop.
//!
//! Pulled out of [`super::run_to_terminal`] so the loop body stays under the
//! file-size cap. The single entry point [`expand_invoke_skill`] resolves a
//! plan's `invoke_skill` directive into either concrete executable steps or a
//! refusal, and writes every audit row for the invoke lifecycle
//! (`l3.invoke.rejected` / `l3.invoked`) itself. The caller only acts on the
//! returned [`InvokeExpansion`] — it installs the steps or records the
//! refusal on loop-local state the helper has no business owning (`ctx.blocks`,
//! the `continue` back to replanning).
//!
//! Behaviour is byte-identical to the previous inline block; this is a pure
//! mechanical extraction, covered end-to-end by the PG-gated invoke e2e
//! suites (`cli_memory_l3_e2e`, `cli_memory_l3py_run_daemon_e2e`).

use sqlx::PgPool;

use crate::cassandra::types::{Plan, PlannedStep};
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::{expand_for_agent, load_pinned_skill_by_name};
use crate::memory::l3py_invoke::{
    expand_python_for_agent, load_pinned_python_skill_by_name, with_python_kind,
};
use crate::scheduler::audit::{
    build_l3_invoke_rejected_agent_payload, build_l3_invoked_payload, ACTION_L3_INVOKED,
    ACTION_L3_INVOKE_REJECTED, SCHEDULER_AUDIT_ACTOR,
};

use super::{InnerLoopError, StepDispatcher};

/// Result of resolving a plan's `invoke_skill` directive.
///
/// The audit row for whichever case occurred is already written by
/// [`expand_invoke_skill`] before it returns; the loop only has to react to
/// the variant.
pub(super) enum InvokeExpansion {
    /// The directive was malformed, named no pinned skill, or the trust gate
    /// refused. `reasons` are already audited via `l3.invoke.rejected`; the
    /// caller pushes them onto `ctx.blocks` (as `invoke_rejected: …`) and
    /// loops back so the agent replans.
    Refused(Vec<String>),
    /// The directive resolved to executable steps, already audited via
    /// `l3.invoked`. The caller installs `steps` into the plan, flips
    /// `invoke_used`, and remembers `(memory_id, name)` for the later
    /// `l3.invoke.outcome` row.
    Expanded {
        steps: Vec<PlannedStep>,
        memory_id: i64,
        name: String,
    },
}

/// Resolve `plan.invoke_skill` into concrete steps or a refusal.
///
/// Precondition: `plan.invoke_skill.is_some()` — the caller guards this so a
/// no-invoke plan never pays for the resolution. Resolution order matches the
/// agent surface: a pinned **templated** skill of that name wins; failing
/// that, a pinned **Python** skill; failing that, a refusal. A malformed
/// directive (`validate_invoke` rejects co-supplied steps / terminal plans /
/// an `l3_skill`) is also a refusal — never a silent fall-through to the
/// agent's own co-supplied steps.
pub(super) async fn expand_invoke_skill(
    pool: &PgPool,
    dispatcher: &dyn StepDispatcher,
    plan: &Plan,
) -> Result<InvokeExpansion, InnerLoopError> {
    // Mirror of the old inline `refuse_invoke!`, but it yields an
    // `InvokeExpansion::Refused` value instead of `continue`-ing the loop —
    // the loop-control now lives at the single call site.
    macro_rules! refuse {
        ($name:expr, $mem:expr, $sha:expr, $reasons:expr) => {{
            let reasons_v: Vec<String> = $reasons;
            let payload =
                build_l3_invoke_rejected_agent_payload($name, $mem, $sha, &reasons_v);
            kastellan_db::audit::insert(
                pool,
                SCHEDULER_AUDIT_ACTOR,
                ACTION_L3_INVOKE_REJECTED,
                payload,
            )
            .await?;
            InvokeExpansion::Refused(reasons_v)
        }};
    }

    // Resolve the directive to OWNED data first so the borrow from
    // `validate_invoke` ends before the caller assigns `plan.steps`.
    let validated = plan
        .validate_invoke()
        .map(|d| (d.name.clone(), d.args.clone(), d.params.clone()));

    let expansion = match validated {
        Err(malformed) => {
            let name = plan
                .invoke_skill
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_default();
            refuse!(&name, None, None, vec![malformed.to_string()])
        }
        Ok((name, args, params)) => match load_pinned_skill_by_name(pool, &name).await? {
            Some(pinned) => {
                let live_tools = dispatcher.known_tools();
                match expand_for_agent(
                    &pinned.template,
                    SkillTrust::Pinned,
                    &args,
                    &live_tools,
                    plan.data_ceiling,
                ) {
                    Err(refusal) => refuse!(
                        &name,
                        Some(pinned.memory_id),
                        Some(pinned.body_sha256.as_str()),
                        refusal.reasons
                    ),
                    Ok(steps) => {
                        let arg_names: Vec<String> = args.keys().cloned().collect();
                        let payload = build_l3_invoked_payload(
                            pinned.memory_id,
                            &name,
                            &pinned.body_sha256,
                            &arg_names,
                            steps.len(),
                        );
                        kastellan_db::audit::insert(
                            pool,
                            SCHEDULER_AUDIT_ACTOR,
                            ACTION_L3_INVOKED,
                            payload,
                        )
                        .await?;
                        InvokeExpansion::Expanded {
                            steps,
                            memory_id: pinned.memory_id,
                            name,
                        }
                    }
                }
            }
            // No pinned *templated* skill of that name — try a pinned
            // *Python* skill before refusing.
            None => match load_pinned_python_skill_by_name(pool, &name).await? {
                None => refuse!(
                    &name,
                    None,
                    None,
                    vec![format!("unknown or non-pinned skill: {name}")]
                ),
                Some(py) => match expand_python_for_agent(
                    &py.candidate,
                    SkillTrust::Pinned,
                    &py.body_sha256,
                    plan.data_ceiling,
                    &params,
                ) {
                    Err(refusal) => refuse!(
                        &name,
                        Some(py.memory_id),
                        Some(py.body_sha256.as_str()),
                        refusal.reasons
                    ),
                    Ok(steps) => {
                        // Python skills take no args. Tag the audit row with
                        // kind:"python" for a coherent lifecycle stream (shares
                        // `with_python_kind` with the operator path — one source
                        // of truth for the tag).
                        let payload = with_python_kind(build_l3_invoked_payload(
                            py.memory_id,
                            &name,
                            &py.body_sha256,
                            &[],
                            steps.len(),
                        ));
                        kastellan_db::audit::insert(
                            pool,
                            SCHEDULER_AUDIT_ACTOR,
                            ACTION_L3_INVOKED,
                            payload,
                        )
                        .await?;
                        InvokeExpansion::Expanded {
                            steps,
                            memory_id: py.memory_id,
                            name,
                        }
                    }
                },
            },
        },
    };

    Ok(expansion)
}
