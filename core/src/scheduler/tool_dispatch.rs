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
//!     [`spawn_worker`] returned [`ToolHostError`] (sandbox rejection,
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

use hhagent_protocol::{client::ClientError, codes};
use hhagent_sandbox::{Net, Profile, SandboxPolicy};
use sqlx::PgPool;

use crate::cassandra::types::PlannedStep;
use crate::tool_host::{dispatch, ToolHostError};

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
}

/// Canonical [`ToolEntry`] for the `shell-exec` worker.
///
/// `allowlist` is the JSON-encoded list of permitted argv\[0\] values
/// the worker will accept (delivered via the `HHAGENT_SHELL_ALLOWLIST`
/// env var; see `workers/shell-exec/src/main.rs`). The daemon
/// administrator controls this list; the LLM-supplied
/// `step.parameters` cannot widen it.
///
/// Defaults baked in here:
///   * `net = Net::Deny` — shell-exec has no business reaching the
///     network; if a future variant needs it, build a separate entry.
///   * `profile = WorkerStrict` — no `socket(2)` allowed (the seccomp
///     filter kills the syscall).
///   * `cpu_ms = 5_000` / `mem_mb = 256` / `wall_clock_ms = Some(30_000)`
///     — small, defensible defaults that match the integration-test
///     fixture in `audit_dispatch_e2e.rs`. Tunable per-tool when a
///     concrete workload demands more.
pub fn shell_exec_entry(binary: PathBuf, allowlist: &[String]) -> ToolEntry {
    // serde_json on a `&[String]` is infallible — the only ways it
    // could fail (non-string keys, NaN floats) are absent here.
    let allow_json = serde_json::to_string(allowlist)
        .expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![binary.clone()],
        fs_write: vec![],
        net: Net::Deny,
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerStrict,
        env: vec![("HHAGENT_SHELL_ALLOWLIST".to_string(), allow_json)],
        cpu_quota_pct: None,
        tasks_max: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
    }
}

/// Map a JSON-RPC numeric error code to its mnemonic. The mnemonics
/// match the constants in [`hhagent_protocol::codes`]; an unknown code
/// surfaces as `"RPC_ERROR"` so the inner loop sees *something*
/// usable without a magic number.
///
/// This is the only place where the wire-level integer is rendered
/// back to a string consumers (the audit log, the inner loop's plan
/// reflection summary) will see, so the names are intentionally
/// short, ALL_CAPS, and identical to the protocol module's constant
/// names.
pub fn rpc_code_name(code: i32) -> &'static str {
    match code {
        codes::PARSE_ERROR => "PARSE_ERROR",
        codes::INVALID_REQUEST => "INVALID_REQUEST",
        codes::METHOD_NOT_FOUND => "METHOD_NOT_FOUND",
        codes::INVALID_PARAMS => "INVALID_PARAMS",
        codes::INTERNAL_ERROR => "INTERNAL_ERROR",
        codes::POLICY_DENIED => "POLICY_DENIED",
        codes::OPERATION_FAILED => "OPERATION_FAILED",
        _ => "RPC_ERROR",
    }
}

/// Translate a `tool_host::dispatch` result into the inner-loop's
/// [`StepOutcome`]. Pure — extracted so the wire-level error mapping
/// is unit-testable without spawning a worker.
///
/// The mapping is:
///
/// | dispatch outcome                                     | StepOutcome                                                |
/// | ---------------------------------------------------- | ---------------------------------------------------------- |
/// | `Ok(value)`                                          | `Ok(value)`                                                |
/// | `Err(Sandbox(_))`                                    | `Err { code: "SPAWN_FAILED", detail }`                     |
/// | `Err(Io(_))`                                         | `Err { code: "IO_ERROR",     detail }`                     |
/// | `Err(Protocol(ClientError::Rpc { code: c, msg, .. }))`| `Err { code: rpc_code_name(c), detail: msg }`              |
/// | `Err(Protocol(_other))`                              | `Err { code: "PROTOCOL_ERROR", detail }`                   |
///
/// The first three buckets are pre-RPC failures the dispatcher itself
/// is responsible for. The fourth is the worker's structured rejection
/// (`POLICY_DENIED`, `OPERATION_FAILED`, etc.) and is the most common
/// failure mode in production. The fifth is decode / I/O at the
/// stdio-pipe layer.
pub fn map_dispatch_result(
    result: Result<serde_json::Value, ToolHostError>,
) -> StepOutcome {
    match result {
        Ok(v) => StepOutcome::Ok(v),
        Err(ToolHostError::Sandbox(e)) => StepOutcome::Err {
            code: "SPAWN_FAILED".into(),
            detail: e.to_string(),
        },
        Err(ToolHostError::Io(e)) => StepOutcome::Err {
            code: "IO_ERROR".into(),
            detail: e.to_string(),
        },
        Err(ToolHostError::Protocol(ClientError::Rpc(rpc))) => StepOutcome::Err {
            code: rpc_code_name(rpc.code).into(),
            detail: rpc.message,
        },
        Err(ToolHostError::Protocol(other)) => StepOutcome::Err {
            code: "PROTOCOL_ERROR".into(),
            detail: other.to_string(),
        },
    }
}

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
/// **Slice-1 architecture note:** the previous version held an
/// `Arc<dyn SandboxBackend>` and called `spawn_worker` inline. That
/// spawn path now lives behind the
/// [`crate::worker_lifecycle::WorkerLifecycleManager::acquire`] seam so
/// slice 2 can swap `SingleUseLifecycle` for an idle-timeout pool
/// without touching this struct. See
/// `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
///
/// Cheap to clone (all fields are `Arc`/`PgPool`); the daemon's
/// scheduler holds a single instance and the inner loop calls
/// `dispatch_step` directly on it.
pub struct ToolHostStepDispatcher {
    pool: PgPool,
    lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
    registry: Arc<ToolRegistry>,
}

impl ToolHostStepDispatcher {
    pub fn new(
        pool: PgPool,
        lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        Self { pool, lifecycle, registry }
    }
}

#[async_trait::async_trait]
impl StepDispatcher for ToolHostStepDispatcher {
    async fn dispatch_step(&self, step: &PlannedStep) -> StepOutcome {
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

        // Slice-1 lifecycle seam: `SingleUseLifecycle::acquire` does the same
        // `spawn_worker(self.sandbox.as_ref(), &spec)` call inline as the old code
        // did. The `ToolHostError` it returns is byte-equivalent to the previous
        // direct-call shape, so the `SPAWN_FAILED` audit path below is unchanged.
        // Slice 2's `IdleTimeoutLifecycle` will instead return an `Err(ToolHostError)`
        // only on real spawn failures; warm-cache hits never reach this `match` arm
        // at all.
        let mut handle = match self.lifecycle.acquire(entry).await {
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
            handle.worker_mut(),
            &step.tool,
            &step.method,
            step.parameters.clone(),
        )
        .await;

        // Slice 2: signal to the lifecycle manager whether the worker survived. For
        // single-use this is a no-op; for idle-timeout it suppresses the worker-return
        // path so the dead worker isn't put back into the warm slot, and bumps the
        // restart-backoff counter. Classified using the protocol-error variant —
        // transport-level failures (`Io`, `Decode`, `EarlyExit`, `IdMismatch`) indicate
        // the worker died; `Rpc(_)` errors mean the worker rejected the call but is
        // alive.
        if crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead(&result) {
            handle.report_crash();
        }

        // Drop closes stdio + cancels the watchdog. For `SingleUseLifecycle`, dropping
        // the handle drops the inner `SupervisedWorker`; for `IdleTimeoutLifecycle`,
        // Drop hands the worker back to the warm slot (or terminates it if
        // `report_crash` was called, the request cap fired, or the worker aged out)
        // and schedules an idle-teardown task.
        drop(handle);

        map_dispatch_result(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_protocol::RpcError;
    use std::io;
    use std::path::PathBuf;

    // ----- rpc_code_name -----

    #[test]
    fn rpc_code_name_maps_known_codes() {
        // Each branch is pinned individually so a future rename in
        // `hhagent_protocol::codes` (e.g. renaming POLICY_DENIED) trips
        // a single specific assertion instead of a coalesced diff.
        assert_eq!(rpc_code_name(codes::PARSE_ERROR), "PARSE_ERROR");
        assert_eq!(rpc_code_name(codes::INVALID_REQUEST), "INVALID_REQUEST");
        assert_eq!(rpc_code_name(codes::METHOD_NOT_FOUND), "METHOD_NOT_FOUND");
        assert_eq!(rpc_code_name(codes::INVALID_PARAMS), "INVALID_PARAMS");
        assert_eq!(rpc_code_name(codes::INTERNAL_ERROR), "INTERNAL_ERROR");
        assert_eq!(rpc_code_name(codes::POLICY_DENIED), "POLICY_DENIED");
        assert_eq!(rpc_code_name(codes::OPERATION_FAILED), "OPERATION_FAILED");
    }

    #[test]
    fn rpc_code_name_unknown_falls_back_to_generic() {
        // An app-level code the dispatcher hasn't been taught about
        // must surface as RPC_ERROR rather than an empty / panicking
        // mapping. The detail string still carries the worker's
        // original message.
        assert_eq!(rpc_code_name(-32099), "RPC_ERROR");
        assert_eq!(rpc_code_name(0), "RPC_ERROR");
        assert_eq!(rpc_code_name(i32::MAX), "RPC_ERROR");
    }

    // ----- map_dispatch_result -----

    #[test]
    fn map_dispatch_result_ok_preserves_value() {
        let v = serde_json::json!({"exit_code": 0, "stdout": "hi"});
        let out = map_dispatch_result(Ok(v.clone()));
        match out {
            StepOutcome::Ok(got) => assert_eq!(got, v),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn map_dispatch_result_protocol_rpc_uses_named_code() {
        // The worker rejected the call with POLICY_DENIED (-32001). The
        // dispatcher must surface the *name* not the integer, and
        // must preserve the worker's `message` verbatim so the audit
        // trail captures the underlying reason.
        let rpc = RpcError::new(codes::POLICY_DENIED, "argv not allowlisted");
        let err = ToolHostError::Protocol(ClientError::Rpc(rpc));
        let out = map_dispatch_result(Err(err));
        match out {
            StepOutcome::Err { code, detail } => {
                assert_eq!(code, "POLICY_DENIED");
                assert_eq!(detail, "argv not allowlisted");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn map_dispatch_result_protocol_rpc_unknown_code_falls_back() {
        let rpc = RpcError::new(-32099, "custom worker error");
        let err = ToolHostError::Protocol(ClientError::Rpc(rpc));
        match map_dispatch_result(Err(err)) {
            StepOutcome::Err { code, detail } => {
                assert_eq!(code, "RPC_ERROR");
                assert_eq!(detail, "custom worker error");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn map_dispatch_result_protocol_non_rpc_uses_protocol_error_code() {
        // ClientError::EarlyExit (worker exited before responding) is
        // a non-Rpc protocol failure — distinct from a structured
        // RPC error.
        let err = ToolHostError::Protocol(ClientError::EarlyExit);
        match map_dispatch_result(Err(err)) {
            StepOutcome::Err { code, detail } => {
                assert_eq!(code, "PROTOCOL_ERROR");
                // The Display string must contain *something*
                // operator-readable; pin the substring rather than
                // the exact form so a thiserror message tweak
                // doesn't churn this test.
                assert!(detail.contains("exited"), "detail: {detail:?}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn map_dispatch_result_io_error_is_distinct_from_protocol() {
        // A raw stdio I/O failure (e.g. broken pipe) is bucketed
        // as IO_ERROR, not PROTOCOL_ERROR. Operators triaging audit
        // logs can split host-side I/O issues from JSON-RPC issues.
        let io = io::Error::new(io::ErrorKind::BrokenPipe, "pipe down");
        let err = ToolHostError::Io(io);
        match map_dispatch_result(Err(err)) {
            StepOutcome::Err { code, detail } => {
                assert_eq!(code, "IO_ERROR");
                assert!(detail.contains("pipe"), "detail: {detail:?}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    // ----- ToolRegistry -----

    fn fake_entry() -> ToolEntry {
        ToolEntry {
            binary: PathBuf::from("/usr/local/bin/fake"),
            policy: SandboxPolicy {
                mem_mb: 32,
                ..SandboxPolicy::default()
            },
            wall_clock_ms: Some(5_000),
            lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        }
    }

    #[test]
    fn tool_registry_starts_empty() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.lookup("anything").is_none());
    }

    #[test]
    fn tool_registry_insert_then_lookup_round_trip() {
        let mut reg = ToolRegistry::new();
        reg.insert("shell-exec", fake_entry());
        assert!(!reg.is_empty());
        assert_eq!(reg.len(), 1);
        let got = reg.lookup("shell-exec").expect("entry present");
        assert_eq!(got.binary, PathBuf::from("/usr/local/bin/fake"));
        assert!(reg.lookup("nope").is_none());
    }

    #[test]
    fn tool_registry_insert_replaces_existing_entry() {
        // Re-inserting under the same name swaps the entry (HashMap
        // semantics). Documented here so a future split into a
        // multi-entry registry tripwires this expectation.
        let mut reg = ToolRegistry::new();
        reg.insert("shell-exec", fake_entry());
        let mut second = fake_entry();
        second.binary = PathBuf::from("/opt/hhagent/shell-exec");
        reg.insert("shell-exec", second);
        assert_eq!(reg.len(), 1);
        assert_eq!(
            reg.lookup("shell-exec").unwrap().binary,
            PathBuf::from("/opt/hhagent/shell-exec")
        );
    }

    // ----- shell_exec_entry -----

    #[test]
    fn shell_exec_entry_carries_allowlist_in_env() {
        // The allowlist round-trips into the policy's env vec as
        // HHAGENT_SHELL_ALLOWLIST = JSON array. The worker reads it
        // at startup; changing the env-var name or the encoding here
        // requires a coordinated change in `workers/shell-exec/src`.
        let binary = PathBuf::from("/usr/local/bin/hhagent-worker-shell-exec");
        let allowlist = vec![
            "/usr/bin/echo".to_string(),
            "/bin/echo".to_string(),
        ];
        let entry = shell_exec_entry(binary.clone(), &allowlist);

        assert_eq!(entry.binary, binary);
        assert_eq!(entry.wall_clock_ms, Some(30_000));

        // Policy invariants the threat-model relies on.
        assert!(matches!(entry.policy.net, Net::Deny),
                "shell-exec must default to network-denied");
        assert!(matches!(entry.policy.profile, Profile::WorkerStrict),
                "shell-exec must run under WorkerStrict (no socket() syscalls)");
        assert_eq!(entry.policy.fs_write, Vec::<PathBuf>::new(),
                   "shell-exec entry should not pre-allocate writable scratch");
        assert!(entry.policy.fs_read.contains(&binary),
                "binary must be in fs_read so bwrap can mount it");

        // The allowlist env entry.
        let allow_env = entry.policy.env.iter()
            .find(|(k, _)| k == "HHAGENT_SHELL_ALLOWLIST")
            .expect("allowlist env entry must be present");
        let parsed: Vec<String> = serde_json::from_str(&allow_env.1)
            .expect("allowlist value must be JSON-decodable");
        assert_eq!(parsed, allowlist);
    }

    #[test]
    fn shell_exec_entry_empty_allowlist_is_valid_deny_all() {
        // An empty allowlist is the safest default — the worker
        // accepts no argv. The daemon admin opts programs in
        // explicitly. Worker-side handling (shell-exec/src) must
        // already reject "no allowlist" or "empty allowlist" with
        // POLICY_DENIED.
        let entry = shell_exec_entry(PathBuf::from("/x"), &[]);
        let allow_env = entry.policy.env.iter()
            .find(|(k, _)| k == "HHAGENT_SHELL_ALLOWLIST")
            .expect("allowlist env entry must be present");
        assert_eq!(allow_env.1, "[]");
    }

    #[test]
    fn shell_exec_entry_declares_single_use_lifecycle() {
        // Shell-exec must remain single-use forever — per-request isolation IS its
        // security model. If a future change to `shell_exec_entry` accidentally swaps
        // this for `IdleTimeout`, this test trips so the regression is caught at PR
        // time rather than in production. See
        // `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md` §"The
        // two policies" for why shell-exec stays in the `single_use` category.
        let entry = shell_exec_entry(PathBuf::from("/x"), &[]);
        assert!(matches!(
            entry.lifecycle,
            crate::worker_lifecycle::Lifecycle::SingleUse
        ));
    }

    // The unknown-tool branch of `ToolHostStepDispatcher::dispatch_step`
    // is covered end-to-end in `core/tests/scheduler_step_dispatch_e2e.rs`
    // (the dispatcher needs a real `PgPool` to construct, so a pure unit
    // test would be tautological). `tool_registry_starts_empty` above
    // pins the underlying registry-miss contract.

    // ----- build_scheduler_step_failure_payload -----

    #[test]
    fn build_payload_unknown_tool_shape_has_no_err_field() {
        // UNKNOWN_TOOL is a registry-miss; there is no underlying error
        // string to attach. The audit consumer's filter on
        // `payload ? 'err'` distinguishes this row from `step.spawn_failed`
        // by structure alone.
        let req = serde_json::json!({"url": "https://example.com"});
        let payload = build_scheduler_step_failure_payload(
            "web-fetch", "fetch", req.clone(), None, 0,
        );
        let obj = payload.as_object().expect("payload must be a JSON object");
        assert_eq!(obj.get("tool").and_then(|v| v.as_str()), Some("web-fetch"));
        assert_eq!(obj.get("method").and_then(|v| v.as_str()), Some("fetch"));
        assert_eq!(obj.get("req"), Some(&req));
        assert_eq!(obj.get("ms").and_then(|v| v.as_u64()), Some(0));
        assert!(
            !obj.contains_key("err"),
            "UNKNOWN_TOOL payload must omit `err`; got {payload:#}",
        );
        // Exactly the keys we expect — no accidental extras (which would
        // shift the audit-shape contract in a future refactor).
        let keys: std::collections::BTreeSet<&str> =
            obj.keys().map(|s| s.as_str()).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["tool", "method", "req", "ms"].iter().copied().collect();
        assert_eq!(keys, expected, "unexpected keys in payload");
    }

    #[test]
    fn build_payload_spawn_failed_shape_includes_err_string() {
        // SPAWN_FAILED carries the sandbox/IO error's `to_string()` so
        // operators can triage from the audit log alone.
        let req = serde_json::json!({"argv": ["/bin/echo", "hi"]});
        let payload = build_scheduler_step_failure_payload(
            "shell-exec",
            "shell.exec",
            req.clone(),
            Some("sandbox: policy paths must be absolute"),
            7,
        );
        let obj = payload.as_object().expect("payload must be a JSON object");
        assert_eq!(obj.get("tool").and_then(|v| v.as_str()), Some("shell-exec"));
        assert_eq!(obj.get("method").and_then(|v| v.as_str()), Some("shell.exec"));
        assert_eq!(obj.get("req"), Some(&req));
        assert_eq!(
            obj.get("err").and_then(|v| v.as_str()),
            Some("sandbox: policy paths must be absolute"),
        );
        assert_eq!(obj.get("ms").and_then(|v| v.as_u64()), Some(7));
        // No accidental extras here either.
        let keys: std::collections::BTreeSet<&str> =
            obj.keys().map(|s| s.as_str()).collect();
        let expected: std::collections::BTreeSet<&str> =
            ["tool", "method", "req", "err", "ms"].iter().copied().collect();
        assert_eq!(keys, expected, "unexpected keys in payload");
    }
}
