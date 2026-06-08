//! End-to-end smoke for the production [`ToolHostStepDispatcher`] — the
//! `StepDispatcher` impl the scheduler's inner loop calls once per
//! `PlannedStep`.
//!
//! This is the regression pin for **Task 3.2.bis** (HANDOVER): until
//! this slice landed, the dispatcher was a `NOT_IMPLEMENTED`
//! placeholder, so the daemon could schedule tasks but never actually
//! invoke a worker. Every assertion below is something the placeholder
//! couldn't satisfy.
//!
//! ## What this test proves
//!
//!   1. **Happy path** — a `PlannedStep` naming an allowlisted argv
//!      results in `StepOutcome::Ok(value)` where `value["exit_code"]`
//!      is 0 and `value["stdout"]` carries the echoed text. Audit row
//!      with `actor = "tool:shell-exec"`, `action = "shell.exec"`,
//!      payload carrying `req`/`result`/`ms`.
//!   2. **Worker-policy denial** — a non-allowlisted argv yields
//!      `StepOutcome::Err { code: "POLICY_DENIED", detail }`. Audit row
//!      with the same actor/action, payload carrying `err` (not `result`).
//!   3. **Unknown-tool path** — a step naming a tool absent from the
//!      registry returns `StepOutcome::Err { code: "UNKNOWN_TOOL", detail }`
//!      and writes a single `actor="scheduler" action="step.unknown_tool"`
//!      audit row (the spawn never happens, so the `tool_host::dispatch`
//!      chokepoint is bypassed — the dispatcher itself is responsible
//!      for the audit insert). The detail names the missing tool.
//!   4. **Spawn-failure path** — a step naming a tool whose `ToolEntry`
//!      carries an invalid policy (relative path in `fs_read`, rejected
//!      up front by the sandbox backend) returns
//!      `StepOutcome::Err { code: "SPAWN_FAILED", detail }` and writes
//!      a single `actor="scheduler" action="step.spawn_failed"` audit
//!      row carrying the sandbox error string.
//!
//! ## How it differs from `audit_dispatch_e2e.rs`
//!
//! That test exercises `tool_host::dispatch` directly (chokepoint
//! correctness). This test exercises the layer one up:
//! `ToolHostStepDispatcher::dispatch_step` calling into `dispatch`,
//! plus the `StepOutcome` mapping and the registry lookup. Together
//! they pin the scheduler's tool path end-to-end.
//!
//! ## Skip behaviour
//!
//! Skips with `[SKIP]` lines on hosts missing Postgres, supervisor,
//! sandbox backend, or the worker binary. macOS hosts without a
//! Postgres install hit the skip cleanly. `cargo test -- --nocapture`
//! to see the skip lines.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::sync::Arc;

use hhagent_core::cassandra::types::{DataClass, PlannedStep};
use hhagent_core::handoff::{HandoffCache, HandoffRef, DEFAULT_RESULT_BYTE_CAP};
use hhagent_core::scheduler::inner_loop::{StepDispatcher, StepOutcome};
use hhagent_core::scheduler::{shell_exec_entry, ToolEntry, ToolHostStepDispatcher, ToolRegistry};
use hhagent_core::secrets::Vault;
use hhagent_sandbox::SandboxPolicy;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";
#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

#[cfg(target_os = "linux")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::linux_bwrap::LinuxBwrap;
    if let Err(e) = LinuxBwrap::probe() {
        eprintln!("\n[SKIP] bwrap probe failed: {e}\n");
        return true;
    }
    false
}

#[cfg(target_os = "macos")]
fn skip_if_sandbox_unavailable() -> bool {
    use hhagent_sandbox::macos_seatbelt::MacosSeatbelt;
    if let Err(e) = MacosSeatbelt::probe() {
        eprintln!("\n[SKIP] sandbox-exec probe failed: {e}\n");
        return true;
    }
    false
}

fn sandbox_bundle() -> Arc<hhagent_sandbox::SandboxBackends> {
    Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os())
}

fn worker_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    target.join("debug").join("hhagent-worker-shell-exec")
}

fn step(tool: &str, method: &str, params: serde_json::Value) -> PlannedStep {
    PlannedStep {
        tool: tool.into(),
        method: method.into(),
        parameters: params,
        returns: "stdout".into(),
        done_when: "exit_code == 0".into(),
        classification: DataClass::Public,
    }
}

#[test]
fn dispatcher_routes_ok_denied_and_unknown_tool_paths() {
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let service_name = format!("hhagent-supervisor-test-pg-stepdisp-{suffix}");
    let _cluster = bring_up_pg_cluster(&bin_dir, "step-d", "step-l", &service_name);
    let conn_spec = &_cluster.conn_spec;

    // `tool_host::dispatch` uses `block_in_place` around the synchronous
    // `Client::call`; mandatory multi-thread runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        // Probe applies migrations and writes the bring-up audit row.
        hhagent_db::probe::run(
            conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "scheduler-step-dispatch"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(conn_spec)
            .await
            .expect("connect runtime pool");

        // Registry: register shell-exec with ECHO_PATH allowlisted, plus
        // `broken-tool` whose policy carries a relative `fs_read` path —
        // both `LinuxBwrap::spawn_under_policy` and
        // `MacosSeatbelt::spawn_under_policy` reject this up-front with
        // `SandboxError::Backend`, so dispatching against `broken-tool`
        // gives us a deterministic SPAWN_FAILED trigger without depending
        // on a missing binary (which would race the worker's early exit
        // and surface as IO_ERROR/PROTOCOL_ERROR instead).
        let mut registry = ToolRegistry::new();
        registry.insert(
            "shell-exec",
            shell_exec_entry(worker.clone(), &[ECHO_PATH.to_string()]),
        );
        registry.insert(
            "broken-tool",
            ToolEntry {
                binary: worker.clone(),
                policy: SandboxPolicy {
                    // Relative path here is the rejection trigger; both
                    // sandbox backends validate absolute-path-ness before
                    // doing anything else.
                    fs_read: vec![PathBuf::from("relative/path/triggers/rejection")],
                    mem_mb: 32,
                    ..SandboxPolicy::default()
                },
                wall_clock_ms: Some(5_000),
                lifecycle: hhagent_core::worker_lifecycle::Lifecycle::SingleUse,
                sandbox_backend: None,
                container_image: None,
            },
        );
        let registry = Arc::new(registry);
        assert_eq!(registry.len(), 2);

        let sandboxes = sandbox_bundle();
        let lifecycle: Arc<dyn hhagent_core::worker_lifecycle::WorkerLifecycleManager> =
            Arc::new(hhagent_core::worker_lifecycle::SingleUseLifecycle::new(
                sandboxes,
            ));
        let dispatcher = ToolHostStepDispatcher::new(
            pool.clone(),
            Arc::new(Vault::new()),
            lifecycle,
            registry,
            std::sync::Arc::new(hhagent_core::handoff::HandoffCache::new()),
        );

        // ---------- (1) Happy path ----------
        let ok_step = step(
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "step-ok"]}),
        );
        let outcome = dispatcher.dispatch_step(0, &ok_step).await;
        let StepOutcome::Ok(value) = &outcome else {
            panic!("expected Ok, got {outcome:?}");
        };
        assert_eq!(value["exit_code"], 0);
        assert_eq!(
            value["stdout"].as_str().expect("stdout is string").trim_end(),
            "step-ok"
        );

        // ---------- (2) Worker-policy denial ----------
        let denied_step = step(
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": ["/bin/cat", "/etc/passwd"]}),
        );
        let outcome = dispatcher.dispatch_step(0, &denied_step).await;
        let StepOutcome::Err { code, detail } = &outcome else {
            panic!("expected Err, got {outcome:?}");
        };
        assert_eq!(code, "POLICY_DENIED",
                   "non-allowlisted argv must map to POLICY_DENIED, not {code}");
        assert!(
            !detail.is_empty(),
            "POLICY_DENIED detail must carry the worker's message"
        );

        // ---------- (3) Unknown tool ----------
        let unknown_step = step(
            "web-fetch",
            "fetch",
            serde_json::json!({"url": "https://example.com"}),
        );
        let outcome = dispatcher.dispatch_step(0, &unknown_step).await;
        let StepOutcome::Err { code, detail } = &outcome else {
            panic!("expected Err, got {outcome:?}");
        };
        assert_eq!(code, "UNKNOWN_TOOL");
        assert!(
            detail.contains("web-fetch"),
            "UNKNOWN_TOOL detail should name the missing tool, got: {detail}"
        );

        // ---------- (4) Spawn failure (registered tool, invalid policy) -
        // The `broken-tool` entry was registered with a relative path in
        // `fs_read`, which the sandbox backend rejects up front. The
        // dispatcher's spawn path returns `ToolHostError::Sandbox(_)` →
        // SPAWN_FAILED, and (post-slice) writes an audit row.
        let spawn_fail_step = step(
            "broken-tool",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "never-runs"]}),
        );
        let outcome = dispatcher.dispatch_step(0, &spawn_fail_step).await;
        let StepOutcome::Err { code, detail } = &outcome else {
            panic!("expected Err, got {outcome:?}");
        };
        assert_eq!(
            code, "SPAWN_FAILED",
            "relative fs_read must surface as SPAWN_FAILED, not {code}",
        );
        assert!(
            !detail.is_empty(),
            "SPAWN_FAILED detail must carry the sandbox's error message",
        );

        // ---------- audit_log assertions ----------
        // Five rows:
        //   - row 0 — bring-up (`core`/`startup`)
        //   - row 1 — happy-path dispatch (`tool:shell-exec`/`shell.exec`, with `result`)
        //   - row 2 — policy-denied dispatch (`tool:shell-exec`/`shell.exec`, with `err`)
        //   - row 3 — unknown-tool dispatch (`scheduler`/`step.unknown_tool`, no `err`)
        //   - row 4 — spawn-failed dispatch (`scheduler`/`step.spawn_failed`, with `err`)
        //
        // Rows 3 + 4 are the contract for this slice: paths that short-
        // circuit before `tool_host::dispatch` must still leave an audit
        // trail, otherwise an operator triaging "the planner asked for X"
        // or "X never started" has nothing to grep.
        let rows = sqlx::query_as::<_, (i64, String, String, serde_json::Value)>(
            "SELECT id, actor, action, payload FROM audit_log ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select audit_log");
        assert_eq!(
            rows.len(),
            5,
            "expected 5 rows (bring-up + ok + denied + unknown + spawn_fail); got {rows:?}",
        );

        // Row 0: bring-up.
        assert_eq!(rows[0].1, "core");
        assert_eq!(rows[0].2, "startup");

        // Row 1: happy path — result, no err.
        assert_eq!(rows[1].1, "tool:shell-exec");
        assert_eq!(rows[1].2, "shell.exec");
        let p1 = rows[1].3.as_object().expect("payload object");
        assert!(p1.contains_key("req"));
        assert!(p1.contains_key("result"));
        assert!(p1.contains_key("ms"));
        assert!(!p1.contains_key("err"));

        // Row 2: policy-denied — err, no result.
        assert_eq!(rows[2].1, "tool:shell-exec");
        assert_eq!(rows[2].2, "shell.exec");
        let p2 = rows[2].3.as_object().expect("payload object");
        assert!(p2.contains_key("req"));
        assert!(p2.contains_key("err"));
        assert!(p2.contains_key("ms"));
        assert!(!p2.contains_key("result"));

        // Row 3: unknown-tool — actor=scheduler, action=step.unknown_tool.
        // No `err` field (there is no underlying error; just a missing
        // registration). `tool`+`method`+`req`+`ms` mirror the chokepoint
        // shape so audit consumers don't need a separate parser.
        assert_eq!(rows[3].1, "scheduler");
        assert_eq!(rows[3].2, "step.unknown_tool");
        let p3 = rows[3].3.as_object().expect("payload object");
        assert_eq!(p3.get("tool").and_then(|v| v.as_str()), Some("web-fetch"));
        assert_eq!(p3.get("method").and_then(|v| v.as_str()), Some("fetch"));
        assert!(p3.contains_key("req"));
        assert!(p3.contains_key("ms"));
        assert!(!p3.contains_key("err"),
                "UNKNOWN_TOOL payload must not carry `err`; got {:#}", rows[3].3);

        // Row 4: spawn-failed — actor=scheduler, action=step.spawn_failed,
        // payload carries the sandbox error string under `err`.
        assert_eq!(rows[4].1, "scheduler");
        assert_eq!(rows[4].2, "step.spawn_failed");
        let p4 = rows[4].3.as_object().expect("payload object");
        assert_eq!(p4.get("tool").and_then(|v| v.as_str()), Some("broken-tool"));
        assert_eq!(p4.get("method").and_then(|v| v.as_str()), Some("shell.exec"));
        assert!(p4.contains_key("req"));
        assert!(p4.contains_key("ms"));
        let err_str = p4.get("err").and_then(|v| v.as_str())
            .expect("SPAWN_FAILED payload must carry `err`");
        assert!(
            !err_str.is_empty(),
            "spawn_failed err must be a non-empty sandbox error string",
        );

        pool.close().await;
    });
}

/// The handoff-cache stash branch, exercised through the *real* worker path
/// (ROADMAP:129; the dispatcher-side wiring #198 flagged as untested).
///
/// The cache primitives are unit-tested in `core/src/handoff.rs` and the
/// `fetch` intercept is pinned hermetically in `handoff_dispatch_e2e.rs`. What
/// neither covers is the dispatcher's *stash* block — the security-load-bearing
/// `Ok(v) if task_id > 0` gate, the placeholder substitution, and the
/// `policy/handoff.stashed` audit row. A scripted dispatcher can't reach it:
/// the stashed body is produced by `tool_host::dispatch` talking to a live
/// worker, so this needs the real PG + sandbox + worker scaffolding (and skips
/// with the same `[SKIP]` lines as its sibling above when they are absent).
///
/// Strategy: `echo` a payload whose serialized-JSON result exceeds the 64 KiB
/// cap, then assert (1) a positive `task_id` yields the placeholder and the
/// body is retrievable from the cache, (2) `purge_task` drops it, and (3) the
/// `task_id = 0` operator path passes the same oversized result through
/// verbatim — the gate that keeps the operator's human-facing output un-stashed.
#[test]
fn dispatcher_stashes_oversized_ok_result_only_for_positive_task_id() {
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };
    let worker = worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let service_name = format!("hhagent-supervisor-test-pg-stashdisp-{suffix}");
    let _cluster = bring_up_pg_cluster(&bin_dir, "stash-d", "stash-l", &service_name);
    let conn_spec = &_cluster.conn_spec;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        hhagent_db::probe::run(
            conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "scheduler-step-stash"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(conn_spec)
            .await
            .expect("connect runtime pool");

        let mut registry = ToolRegistry::new();
        registry.insert(
            "shell-exec",
            shell_exec_entry(worker.clone(), &[ECHO_PATH.to_string()]),
        );
        let registry = Arc::new(registry);

        let lifecycle: Arc<dyn hhagent_core::worker_lifecycle::WorkerLifecycleManager> =
            Arc::new(hhagent_core::worker_lifecycle::SingleUseLifecycle::new(
                sandbox_bundle(),
            ));

        // Hold a clone of the cache so we can inspect what the dispatcher
        // stashed (the dispatcher owns its copy behind an `Arc`).
        let handoff = Arc::new(HandoffCache::new());
        let dispatcher = ToolHostStepDispatcher::new(
            pool.clone(),
            Arc::new(Vault::new()),
            lifecycle,
            registry,
            Arc::clone(&handoff),
        );

        // A single echo arg ~80 KiB long → the result's serialized JSON
        // comfortably exceeds the 64 KiB cap. Well under ARG_MAX on both OSes.
        let big_arg = "h".repeat(80 * 1024);
        let big_step = step(
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, big_arg]}),
        );

        // ---------- (A) Positive task id → stashed + placeholder ----------
        const TASK_ID: i64 = 1;
        let outcome = dispatcher.dispatch_step(TASK_ID, &big_step).await;
        let StepOutcome::Ok(value) = &outcome else {
            panic!("expected Ok placeholder, got {outcome:?}");
        };
        // Placeholder shape — not the raw worker result.
        assert!(
            value.get("stdout").is_none(),
            "stashed result must be a placeholder, not the raw worker value: {value:#}"
        );
        let ref_str = value["handoff_ref"]
            .as_str()
            .expect("placeholder carries handoff_ref");
        assert!(ref_str.starts_with("sha256:"), "handoff_ref is content-addressed: {ref_str}");
        assert_eq!(value["truncated"], true);
        assert!(
            value["byte_len"].as_u64().expect("byte_len is a number") as usize
                > DEFAULT_RESULT_BYTE_CAP,
            "stashed body must exceed the cap"
        );
        assert!(
            value["summary_head"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "placeholder carries a non-empty readable head"
        );

        // The advertised ref resolves in the cache, under this task only.
        let parsed = HandoffRef::parse(ref_str).expect("placeholder ref parses");
        assert!(
            handoff.get_slice(TASK_ID, &parsed, 0, 16).is_some(),
            "stashed body must be retrievable for the owning task"
        );
        assert!(
            handoff.get_slice(TASK_ID + 1, &parsed, 0, 16).is_none(),
            "stashed body must not be visible to another task"
        );

        // ---------- (B) purge_task drops it (the lane-runner terminal hook) -
        dispatcher.purge_task(TASK_ID);
        assert!(
            handoff.get_slice(TASK_ID, &parsed, 0, 16).is_none(),
            "purge_task must drop the task's stashed bodies"
        );

        // ---------- (C) task_id = 0 → passthrough, never stashed ----------
        // This is the gate that keeps the operator `memory l3 run` output
        // (human-facing, no fetch loop) un-stashed. Same oversized result.
        let outcome = dispatcher.dispatch_step(0, &big_step).await;
        let StepOutcome::Ok(value) = &outcome else {
            panic!("expected Ok passthrough for task_id=0, got {outcome:?}");
        };
        assert!(
            value.get("handoff_ref").is_none(),
            "task_id=0 must pass the raw result through, not stash it: {value:#}"
        );
        assert_eq!(value["exit_code"], 0, "passthrough carries the real worker result");
        assert!(
            value["stdout"].as_str().map(|s| s.len() > DEFAULT_RESULT_BYTE_CAP).unwrap_or(false),
            "passthrough stdout is the full oversized body"
        );

        // ---------- audit: exactly one handoff.stashed row, from case (A) ---
        let stashed_rows = sqlx::query_as::<_, (String, serde_json::Value)>(
            "SELECT actor, payload FROM audit_log WHERE action = 'handoff.stashed' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select handoff.stashed rows");
        assert_eq!(
            stashed_rows.len(),
            1,
            "exactly one stash (case A); case C's task_id=0 must not stash: {stashed_rows:?}"
        );
        assert_eq!(stashed_rows[0].0, "policy");
        let sp = stashed_rows[0].1.as_object().expect("payload object");
        assert_eq!(sp.get("task_id").and_then(|v| v.as_i64()), Some(TASK_ID));
        assert_eq!(sp.get("tool").and_then(|v| v.as_str()), Some("shell-exec"));
        assert_eq!(sp.get("handoff_ref").and_then(|v| v.as_str()), Some(ref_str));

        pool.close().await;
    });
}
