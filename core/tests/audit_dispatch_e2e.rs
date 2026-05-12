//! End-to-end smoke for `tool_host::dispatch` — the chokepoint
//! every Phase 0+ tool call goes through.
//!
//! What this test proves:
//!   1. `dispatch` makes the JSON-RPC call against a sandboxed
//!      shell-exec worker and returns the result verbatim.
//!   2. The same dispatch call writes one row into `audit_log` with
//!      `actor = "tool:shell-exec"`, `action = "<method>"`, and a
//!      payload carrying `req`, `result`, and `ms` fields.
//!   3. A failing call (non-allowlisted argv → POLICY_DENIED) still
//!      lands an audit row, but with `err` instead of `result`.
//!
//! Bring-up scaffolding (per-test PG cluster + sandbox probe + binary
//! discovery + RAII cleanup) lives in `hhagent-tests-common` as of
//! issue #15.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres, a
//! reachable supervisor, the worker binary, or a working sandbox
//! backend. `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, policy_for_shell_exec,
    shell_exec_worker_binary, skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix,
};

#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";
#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

#[test]
fn dispatch_writes_audit_row_for_success_and_failure() {
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
    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "disp-d",
        "disp-l",
        &format!("hhagent-supervisor-test-pg-dispatch-{suffix}"),
    );

    // Dispatch uses `tokio::task::block_in_place` around the
    // synchronous `worker.call`; that requires a multi-thread runtime.
    // `current_thread` would panic at the first dispatch.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime");

    rt.block_on(async {
        // Probe applies migrations 0001 + 0002 + 0003 and writes the
        // bring-up audit row. The dispatch test inserts on top of that
        // baseline.
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"version": "test", "purpose": "audit-dispatch"}),
        )
        .await
        .expect("probe run");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("connect runtime pool");

        let policy = policy_for_shell_exec(&worker, &[ECHO_PATH]);
        let backend = backend();
        let worker_str = worker.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn shell-exec");

        // ---------- success path ----------
        let result = dispatch(
            &pool,
            &mut sworker,
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": [ECHO_PATH, "dispatch-ok"]}),
        )
        .await
        .expect("dispatch success");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"].as_str().unwrap().trim_end(), "dispatch-ok");

        // ---------- failure path ----------
        // Non-allowlisted argv → worker returns POLICY_DENIED. The
        // call returns an Err from `dispatch`, but the audit row must
        // still be written.
        let err = dispatch(
            &pool,
            &mut sworker,
            "shell-exec",
            "shell.exec",
            serde_json::json!({"argv": ["/bin/cat", "/etc/passwd"]}),
        )
        .await
        .expect_err("dispatch must propagate worker policy denial");
        assert!(
            err.to_string().contains("-32001"),
            "expected POLICY_DENIED (-32001) in error string: {err}"
        );

        // ---------- audit_log assertions ----------
        // Three rows total: bring-up + success dispatch + failure
        // dispatch. The assertions below pin the *shape* of each
        // dispatch row separately so a refactor that drops the `err`
        // field (or accidentally writes `result` for the failure
        // case) trips the test.
        let rows = sqlx::query_as::<_, (i64, String, String, serde_json::Value)>(
            "SELECT id, actor, action, payload \
             FROM audit_log ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("select audit_log");
        assert_eq!(rows.len(), 3, "expected 3 rows; got {rows:?}");

        // Row 0: bring-up.
        assert_eq!(rows[0].1, "core");
        assert_eq!(rows[0].2, "startup");

        // Row 1: success dispatch — has `result` but no `err`.
        assert_eq!(rows[1].1, "tool:shell-exec");
        assert_eq!(rows[1].2, "shell.exec");
        let p1 = rows[1].3.as_object().expect("payload object");
        assert!(p1.contains_key("req"), "missing req: {:?}", rows[1].3);
        assert!(p1.contains_key("result"), "missing result: {:?}", rows[1].3);
        assert!(p1.contains_key("ms"), "missing ms: {:?}", rows[1].3);
        assert!(!p1.contains_key("err"), "success row must not carry err");

        // Row 2: failure dispatch — has `err` but no `result`.
        assert_eq!(rows[2].1, "tool:shell-exec");
        assert_eq!(rows[2].2, "shell.exec");
        let p2 = rows[2].3.as_object().expect("payload object");
        assert!(p2.contains_key("req"));
        assert!(p2.contains_key("err"), "missing err on failure row: {:?}", rows[2].3);
        assert!(p2.contains_key("ms"));
        assert!(
            !p2.contains_key("result"),
            "failure row must not carry result"
        );
        let err_str = p2.get("err").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            err_str.contains("-32001") || err_str.contains("POLICY_DENIED"),
            "audit err field should mention POLICY_DENIED, got: {err_str}"
        );

        let _ = sworker.close();
        pool.close().await;
    });
}
