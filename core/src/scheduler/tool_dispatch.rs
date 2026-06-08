//! Production `StepDispatcher` that maps each [`PlannedStep`] to a real
//! sandboxed worker call via [`tool_host::dispatch`].
//!
//! ## Where this fits
//!
//! The inner loop calls `dispatcher.dispatch_step(&step)` once per
//! [`PlannedStep`] on an approved plan. In production, that dispatcher
//! is a [`ToolHostStepDispatcher`]; in tests, it's a scripted stub
//! (`scheduler_inner_loop_e2e` and friends construct closures).
//!
//! For each call, this dispatcher:
//!
//!   1. Looks up the `step.tool` name in the [`ToolRegistry`] — a
//!      pre-configured map of `tool name → (binary path, sandbox
//!      policy, wall-clock budget)`. Unknown tools surface as
//!      `StepOutcome::Err { code: "UNKNOWN_TOOL", ... }`.
//!   2. Spawns a fresh worker under the configured `SandboxBackend`,
//!      using the entry's policy + binary. Spawn-per-step matches the
//!      existing "spawn-per-call" mode in `tool_host`; long-lived
//!      workers are a Phase-1+ revisit (see HANDOVER §"Open questions").
//!   3. Calls [`tool_host::dispatch`] which is the chokepoint — it
//!      writes one `audit_log` row per call regardless of success or
//!      failure, then returns the worker's result.
//!   4. Drops the worker (closes stdio, cancels watchdog, reaps).
//!   5. Translates [`Result<Value, ToolHostError>`] into a
//!      [`StepOutcome`] so the inner loop can decide whether to keep
//!      executing remaining steps.
//!
//! ## Why a registry, not hardcoded tool resolution
//!
//! There is only one tool today (`shell-exec`). But the dispatcher is
//! the natural seam where future tools (`web-fetch`, `python-exec`,
//! the embedding worker) plug in — each needs its own binary path,
//! sandbox policy shape, and budget. Threading those through a
//! constructor keeps `dispatch_step` short and the daemon's startup
//! responsible for *which* tools are available. Tests build a
//! registry from scratch with whatever fixtures they need.
//!
//! ## Audit-log rows from this slice
//!
//! Three actor/action shapes can come out of one [`dispatch_step`]
//! call, and an operator triaging the audit log relies on the
//! distinction:
//!
//!   * **`tool:<name>` / `<method>`** — the worker was reached and
//!     `tool_host::dispatch` wrote the row (one per call, success
//!     or failure). The shape is `{req, result|err, ms}`.
//!   * **`scheduler` / `step.unknown_tool`** — the planner asked for
//!     a tool not in the registry. No spawn happened, the chokepoint
//!     was not reached, and this dispatcher writes the row itself.
//!     Payload: `{tool, method, req, ms}` (no `err` field — the
//!     failure is a registration gap, not an error).
//!   * **`scheduler` / `step.spawn_failed`** — the registry hit but
//!     [`spawn_worker`] returned [`ToolHostError`](crate::tool_host::ToolHostError) (sandbox rejection,
//!     stdio setup failure, etc.). The chokepoint was not reached, so
//!     this dispatcher writes the row itself. Payload:
//!     `{tool, method, req, err, ms}` — `err` carries the
//!     `ToolHostError::Display` string so operators can triage from
//!     the audit log alone.
//!
//! The audit insert is **best-effort**: if Postgres is unavailable or
//! the pool is exhausted, the dispatcher logs via [`tracing::error`]
//! and still returns the original `StepOutcome::Err` to the caller.
//! Masking the spawn/lookup failure because we couldn't log it would
//! be a strictly worse failure mode. This matches the chokepoint's
//! own best-effort posture; see [`crate::tool_host::dispatch`].
//!
//! The inner loop's separate `scheduler/plan.outcome` audit row
//! aggregates step counts; `agent/plan.formulate` and
//! `cassandra:chain/verdict` rows are emitted elsewhere in the loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::secrets::Vault;

use hhagent_sandbox::SandboxPolicy;
use sqlx::PgPool;

use crate::cassandra::types::PlannedStep;
use crate::tool_host::dispatch;

use super::inner_loop::{StepDispatcher, StepOutcome};

/// One entry in the tool registry.
///
/// Construct via [`shell_exec_entry`] (canonical for the only shipping
/// tool today) or build by hand for tests. `policy` is cloned per
/// dispatch call so the same entry can serve many concurrent steps
/// without cross-talk.
#[derive(Clone, Debug)]
pub struct ToolEntry {
    /// Absolute path to the worker binary on the host. Bound into the
    /// jail by `policy.fs_read` (or via the worker prelude's Landlock
    /// allowlist — see `derive_lockdown_env`).
    pub binary: PathBuf,
    /// Base sandbox policy. Cloned per call. Per-step overrides (e.g.
    /// a per-step scratch dir) would mutate the clone before passing
    /// to `spawn_worker`.
    pub policy: SandboxPolicy,
    /// Wall-clock budget for the entire worker process lifetime, in
    /// milliseconds. `None` disables the watchdog. See
    /// [`WorkerSpec::wall_clock_ms`] for the semantics.
    pub wall_clock_ms: Option<u64>,
    /// Lifecycle policy. Defaults to [`Lifecycle::SingleUse`] (current
    /// behaviour); inference workers in slice 2+ will declare
    /// [`Lifecycle::IdleTimeout`]. See
    /// `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
    pub lifecycle: crate::worker_lifecycle::Lifecycle,
    /// Per-worker sandbox-backend opt-in. `None` (current default for
    /// every shipping tool) uses the per-OS default backend (Seatbelt
    /// on darwin, Bwrap on linux). `Some(K)` requests a specific
    /// backend, validated at compile time by the cfg-gated enum.
    ///
    /// Slice 2.5 will set `Some(SandboxBackendKind::Container)` on
    /// the `gliner-relex` manifest to opt that worker into macOS
    /// memory enforcement (Seatbelt has no memory primitive). All
    /// other workers stay on `None` until they have a concrete
    /// reason to diverge. See
    /// `docs/superpowers/specs/2026-05-21-macos-container-slice-2-design.md`.
    pub sandbox_backend: Option<hhagent_sandbox::SandboxBackendKind>,
    /// Container image tag for the `MacosContainer` backend. Only
    /// meaningful when `sandbox_backend == Some(Container)`; ignored
    /// otherwise. Type is `Option<String>` rather than enum-coupled so
    /// future container-based backends on other platforms (e.g. a
    /// hypothetical Linux Firecracker backend) could reuse the same
    /// shape without enum widening.
    ///
    /// * `None` with `sandbox_backend == Some(Container)` →
    ///   `MacosContainer`'s `DEFAULT_IMAGE` (`alpine:3.20`). Useful for
    ///   Slice 1-style smoke tests.
    /// * `Some(tag)` → per-call
    ///   `Arc::new(MacosContainer::with_image(tag))` via
    ///   `SandboxBackends::resolve`. Production workers (gliner-relex,
    ///   future python-exec) populate this with their per-worker image.
    pub container_image: Option<String>,
}

/// Look-up table from logical tool name (as it appears in
/// `PlannedStep::tool`) to the recipe for spawning that tool.
///
/// The dispatcher resolves `step.tool` here on every call; a miss
/// produces `StepOutcome::Err { code: "UNKNOWN_TOOL", ... }` so the
/// inner loop records the failure and (typically) breaks out of the
/// remaining steps.
#[derive(Default, Debug)]
pub struct ToolRegistry {
    entries: HashMap<String, ToolEntry>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, entry: ToolEntry) {
        self.entries.insert(name.into(), entry);
    }

    pub fn lookup(&self, name: &str) -> Option<&ToolEntry> {
        self.entries.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Borrowed iterator over `(tool_name, entry)` pairs. Stable item type
    /// so callers (e.g. the daemon-startup container-image health check
    /// in [`crate::sandbox_health`]) don't depend on `HashMap`'s internal
    /// iterator type. Iteration order matches `HashMap` (i.e. unordered;
    /// callers that need a deterministic order must sort).
    pub fn entries(&self) -> impl Iterator<Item = (&str, &ToolEntry)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The set of registered tool names (deterministic, sorted). Used by
    /// the agent L3-invoke live re-validation.
    pub fn tool_names(&self) -> std::collections::BTreeSet<String> {
        self.entries.keys().cloned().collect()
    }
}

// `shell_exec_entry` now lives in `crate::workers::shell_exec` (the worker
// owns its own manifest + constructor). Re-exported here so the existing
// `scheduler::tool_dispatch::shell_exec_entry` / `scheduler::shell_exec_entry`
// paths are unchanged for callers.
pub use crate::workers::shell_exec::shell_exec_entry;

// Pure result-mapping helpers (`rpc_code_name`, `map_dispatch_result`)
// live in the `result_mapping` sibling so this file stays under the
// 500-LOC soft cap. Re-exported here so their public paths
// (`scheduler::tool_dispatch::{rpc_code_name, map_dispatch_result}`)
// and this module's own `dispatch_step` call to `map_dispatch_result`
// resolve byte-for-byte unchanged.
mod result_mapping;
pub use result_mapping::{map_dispatch_result, rpc_code_name};

// Re-export of the canonical actor string for scheduler-emitted audit
// rows. The dispatcher's short-circuit rows (`step.unknown_tool`,
// `step.spawn_failed`) and the lane runner's lifecycle rows must agree
// on this string; sourcing both from `super::audit` means a future
// rename touches exactly one file. See the docstring on the const
// itself in `super::audit` for the full contract.
use super::audit::SCHEDULER_AUDIT_ACTOR;

/// `action` value for an `audit_log` row written when
/// [`ToolRegistry::lookup`] missed: the planner named a tool that
/// isn't in the daemon's registry.
const ACTION_STEP_UNKNOWN_TOOL: &str = "step.unknown_tool";

/// `action` value for an `audit_log` row written when [`spawn_worker`]
/// returned an error: a registered tool whose sandbox spawn was
/// rejected (bad policy, OS error, etc.).
const ACTION_STEP_SPAWN_FAILED: &str = "step.spawn_failed";

/// Build the JSON payload for a `scheduler/step.<kind>` audit row.
///
/// Pure helper — no I/O, no clock, no global state — so the wire shape
/// is unit-testable without spinning up a real database. The chokepoint
/// in [`crate::tool_host::dispatch`] uses `{req, result|err, ms}`; this
/// payload adds `tool` and `method` so audit consumers can filter
/// without a join: when `actor = "scheduler"`, the worker name doesn't
/// appear in the action.
///
/// * `err = None`  → suitable for `step.unknown_tool` (no underlying
///   error string; the failure is a missing registration).
/// * `err = Some`  → suitable for `step.spawn_failed` (`Display`
///   string of the sandbox/IO error).
fn build_scheduler_step_failure_payload(
    tool: &str,
    method: &str,
    req: serde_json::Value,
    err: Option<&str>,
    ms: u64,
) -> serde_json::Value {
    let mut payload = serde_json::Map::with_capacity(5);
    payload.insert("tool".into(), serde_json::Value::String(tool.into()));
    payload.insert("method".into(), serde_json::Value::String(method.into()));
    payload.insert("req".into(), req);
    if let Some(e) = err {
        payload.insert("err".into(), serde_json::Value::String(e.into()));
    }
    payload.insert("ms".into(), serde_json::Value::Number(ms.into()));
    serde_json::Value::Object(payload)
}

/// Production [`StepDispatcher`]: looks up `step.tool` in a
/// [`ToolRegistry`], asks the [`crate::worker_lifecycle::WorkerLifecycleManager`]
/// for a [`crate::worker_lifecycle::WorkerHandle`], calls
/// [`tool_host::dispatch`], and maps the result into a [`StepOutcome`].
///
/// The spawn path lives behind
/// [`crate::worker_lifecycle::WorkerLifecycleManager::acquire`]; see
/// `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` for the
/// lifecycle contract.
///
/// Cheap to clone (all fields are `Arc`/`PgPool`); the daemon's
/// scheduler holds a single instance and the inner loop calls
/// `dispatch_step` directly on it.
pub struct ToolHostStepDispatcher {
    pool: PgPool,
    vault: Arc<Vault>,                    // NEW — Item 31
    lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
    registry: Arc<ToolRegistry>,
}

impl ToolHostStepDispatcher {
    pub fn new(
        pool: PgPool,
        vault: Arc<Vault>,               // NEW — Item 31 (insert after `pool`)
        lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        Self { pool, vault, lifecycle, registry }
    }
}

#[async_trait::async_trait]
impl StepDispatcher for ToolHostStepDispatcher {
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        self.registry.tool_names()
    }

    async fn dispatch_step(&self, task_id: i64, step: &PlannedStep) -> StepOutcome {
        let _ = task_id; // used by Task 5/6; discarded here to keep -D warnings clean
        // Measured from dispatcher entry, not from worker spawn — so
        // `ms` on a `step.unknown_tool` row is essentially zero (just
        // the registry lookup) and `ms` on `step.spawn_failed`
        // captures the time the failed spawn cost.
        let started = Instant::now();

        let Some(entry) = self.registry.lookup(&step.tool) else {
            // Tool not in registry — surfaced loudly so the operator
            // sees which tool name the planner asked for. The inner
            // loop will mark the plan as `err` and replanning kicks
            // in on the next iteration (bounded by `max_plans`).
            tracing::warn!(
                tool = %step.tool, method = %step.method,
                "ToolHostStepDispatcher: unknown tool — not in registry"
            );

            // Audit row is best-effort: a transient DB error is logged
            // but the lookup-miss is still surfaced to the caller. See
            // the module-level "Audit-log rows from this slice" doc for
            // why this matches the chokepoint's own best-effort posture.
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let payload = build_scheduler_step_failure_payload(
                &step.tool,
                &step.method,
                step.parameters.clone(),
                None,
                elapsed_ms,
            );
            if let Err(audit_err) = hhagent_db::audit::insert(
                &self.pool,
                SCHEDULER_AUDIT_ACTOR,
                ACTION_STEP_UNKNOWN_TOOL,
                payload,
            )
            .await
            {
                tracing::error!(
                    tool = %step.tool, method = %step.method, error = %audit_err,
                    "step.unknown_tool audit_log INSERT failed; outcome still propagated"
                );
            }

            return StepOutcome::Err {
                code: "UNKNOWN_TOOL".into(),
                detail: format!("tool '{}' not registered", step.tool),
            };
        };

        // The manager owns the spawn/warm-cache decision. `acquire` returns
        // `Err(ToolHostError)` only on real spawn failures (warm-cache hits never
        // reach the `Err` arm); the `SPAWN_FAILED` audit row below treats both
        // lifecycle policies uniformly. Pass `&step.tool` (the logical registry key)
        // so the idle-timeout warm-cache keys by tool identity, not binary basename.
        let mut handle = match self.lifecycle.acquire(&step.tool, entry).await {
            Ok(h) => h,
            Err(e) => {
                // Spawn failure short-circuits before
                // `tool_host::dispatch`, so the chokepoint never sees
                // it. Closing that audit-trail gap is the contract of
                // this branch: write a `scheduler/step.spawn_failed`
                // row carrying the sandbox/IO error string, then
                // surface `SPAWN_FAILED` upstream.
                let err_string = e.to_string();
                tracing::error!(
                    tool = %step.tool, method = %step.method, error = %err_string,
                    "ToolHostStepDispatcher: lifecycle.acquire failed"
                );

                let elapsed_ms = started.elapsed().as_millis() as u64;
                let payload = build_scheduler_step_failure_payload(
                    &step.tool,
                    &step.method,
                    step.parameters.clone(),
                    Some(&err_string),
                    elapsed_ms,
                );
                if let Err(audit_err) = hhagent_db::audit::insert(
                    &self.pool,
                    SCHEDULER_AUDIT_ACTOR,
                    ACTION_STEP_SPAWN_FAILED,
                    payload,
                )
                .await
                {
                    tracing::error!(
                        tool = %step.tool, method = %step.method, error = %audit_err,
                        "step.spawn_failed audit_log INSERT failed; outcome still propagated"
                    );
                }

                return StepOutcome::Err {
                    code: "SPAWN_FAILED".into(),
                    detail: err_string,
                };
            }
        };

        let result = dispatch(
            &self.pool,
            &self.vault,             // NEW — Item 31
            handle.worker_mut(),
            &step.tool,
            &step.method,
            step.parameters.clone(),
        )
        .await;

        // Signal worker death to the manager. No-op for single-use; for idle-timeout
        // this suppresses the worker-return path and bumps the restart-backoff counter.
        // See `dispatch_indicates_worker_dead` for the variant→liveness mapping.
        if crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead(&result) {
            handle.report_crash();
        }

        // Drop runs the lifecycle-appropriate teardown: terminate (single-use) or
        // return-to-slot + schedule idle-teardown (idle-timeout).
        drop(handle);

        map_dispatch_result(result)
    }
}

#[cfg(test)]
mod tests;
