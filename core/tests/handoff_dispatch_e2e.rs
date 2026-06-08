//! Hermetic dispatcher-level coverage for the `fetch_handoff` built-in
//! intercept in [`ToolHostStepDispatcher::dispatch_step`].
//!
//! No live Postgres, no sandbox, no worker process: the intercept fires before
//! the registry lookup / worker acquire and returns early, and its audit insert
//! is best-effort (a never-connecting pool just makes it log-and-continue). The
//! lifecycle manager is a `panic!`-on-acquire fake — reaching it would mean the
//! intercept failed to short-circuit, which is itself the bug this test guards.
//!
//! Review-fix for the handoff-cache feature (ROADMAP:129): the cache primitives
//! are unit-tested in `core/src/handoff.rs`; this pins the dispatcher wiring of
//! the reserved `handoff`/`fetch` built-in (intercept-before-lookup + the three
//! `FetchResult` -> `StepOutcome` arms).

use std::sync::Arc;
use std::time::Duration;

use hhagent_core::cassandra::types::{DataClass, PlannedStep};
use hhagent_core::handoff::{HandoffCache, HandoffRef, DEFAULT_RESULT_BYTE_CAP};
use hhagent_core::scheduler::inner_loop::{StepDispatcher, StepOutcome};
use hhagent_core::scheduler::{ToolEntry, ToolHostStepDispatcher, ToolRegistry};
use hhagent_core::secrets::Vault;
use hhagent_core::tool_host::ToolHostError;
use hhagent_core::worker_lifecycle::{WorkerHandle, WorkerLifecycleManager};
use sqlx::postgres::PgPoolOptions;

/// Lifecycle fake that must never be acquired — the fetch intercept returns
/// before the worker layer, so reaching this is the bug under test.
struct NeverAcquire;

#[async_trait::async_trait]
impl WorkerLifecycleManager for NeverAcquire {
    async fn acquire(&self, _tool: &str, _entry: &ToolEntry) -> Result<WorkerHandle, ToolHostError> {
        panic!("fetch_handoff intercept must not reach the worker-acquire path");
    }
}

/// A never-connecting pool with a short acquire timeout, so the intercept's
/// best-effort `handoff.fetched` audit insert fails fast (it is swallowed).
fn dead_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(250))
        .connect_lazy("postgres://127.0.0.1:1/handoff_dispatch_test")
        .expect("lazy pool parses the URL without connecting")
}

fn fetch_step(params: serde_json::Value) -> PlannedStep {
    PlannedStep {
        tool: "handoff".into(),
        method: "fetch".into(),
        parameters: params,
        returns: "slice".into(),
        done_when: "fetched".into(),
        classification: DataClass::Public,
    }
}

fn dispatcher_with(cache: Arc<HandoffCache>) -> ToolHostStepDispatcher {
    ToolHostStepDispatcher::new(
        dead_pool(),
        Arc::new(Vault::new()),
        Arc::new(NeverAcquire),
        Arc::new(ToolRegistry::new()), // empty: the intercept returns before lookup
        cache,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_handoff_intercept_returns_stashed_slice() {
    let cache = Arc::new(HandoffCache::new());
    // Stash an oversized body under task 7.
    let big = serde_json::json!({"text": "needle ".repeat(20_000)});
    let stash = cache
        .stash_if_oversized(7, &big, DEFAULT_RESULT_BYTE_CAP)
        .expect("body exceeds the cap, so it is stashed");
    let dispatcher = dispatcher_with(cache);

    let step = fetch_step(serde_json::json!({
        "handoff_ref": stash.handoff_ref.as_str(),
        "offset": 0,
        "len": 100,
    }));
    match dispatcher.dispatch_step(7, &step).await {
        StepOutcome::Ok(v) => {
            assert_eq!(v["handoff_ref"], stash.handoff_ref.as_str());
            assert_eq!(v["encoding"], "utf8");
            assert!(v["data"].as_str().unwrap().contains("needle"));
        }
        other => panic!("expected Ok slice, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_handoff_intercept_unknown_ref_is_not_found() {
    let cache = Arc::new(HandoffCache::new());
    let dispatcher = dispatcher_with(cache);
    let step = fetch_step(serde_json::json!({
        "handoff_ref": HandoffRef::of(b"never stashed").as_str(),
    }));
    match dispatcher.dispatch_step(7, &step).await {
        StepOutcome::Err { code, .. } => assert_eq!(code, "HANDOFF_NOT_FOUND"),
        other => panic!("expected HANDOFF_NOT_FOUND, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_handoff_intercept_malformed_params_is_invalid() {
    let cache = Arc::new(HandoffCache::new());
    let dispatcher = dispatcher_with(cache);
    let step = fetch_step(serde_json::json!({})); // no handoff_ref
    match dispatcher.dispatch_step(7, &step).await {
        StepOutcome::Err { code, .. } => assert_eq!(code, "INVALID_PARAMS"),
        other => panic!("expected INVALID_PARAMS, got {other:?}"),
    }
}
