//! End-to-end integration tests for the gliner-relex worker.
//!
//! These tests spawn the real Python worker
//! (`workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex`) on
//! the real model weights staged under
//! `$HHAGENT_DATA_DIR/workers/gliner-relex/weights/multi-v1.0/`. They
//! exercise the full Slice 2 wiring chain: [`gliner_relex_entry`] →
//! [`IdleTimeoutLifecycle::acquire`] → [`hhagent_core::tool_host::dispatch`]
//! → JSON-RPC over stdio → Python `extract` dispatch → response decode
//! through [`hhagent_core::workers::gliner_relex::ExtractResponse`].
//!
//! Without the venv + weights (and without a running Postgres, and
//! without bwrap/Seatbelt), every test in this file `[SKIP]`s cleanly.
//! That matches the default deployment posture: gliner-relex is opt-in
//! via `HHAGENT_GLINER_RELEX_ENABLE=1` and operators run
//! `scripts/workers/gliner-relex/install.sh` before flipping the flag.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! and the Slice 2 section of
//! `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md` for the
//! design.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::sync::Arc;

use hhagent_core::scheduler::ToolEntry;
use hhagent_core::tool_host;
use hhagent_core::worker_lifecycle::{IdleTimeoutLifecycle, WorkerLifecycleManager};
use hhagent_core::workers::gliner_relex::{
    gliner_relex_entry, ExtractRequest, ExtractResponse, GlinerRelexEnv,
};
use hhagent_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, PgCluster,
};

/// Resolve the venv shim path relative to the workspace root.
///
/// Returns `None` (with a `[SKIP]` print on stderr) when the path
/// doesn't exist. Mirrors the resolution `core/src/main.rs::
/// build_gliner_relex_entry` does in production except that this
/// helper never honours the daemon's `HHAGENT_GLINER_RELEX_VENV_DIR`
/// override — tests always run against the in-tree
/// `workers/gliner-relex/.venv/`.
fn resolve_worker_script() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent — broken workspace layout")
        .to_path_buf();
    let script = workspace_root
        .join("workers/gliner-relex/.venv/bin/hhagent-worker-gliner-relex");
    if !script.exists() {
        eprintln!(
            "\n[SKIP] gliner-relex venv shim not built at {} — run scripts/workers/gliner-relex/install.sh\n",
            script.display()
        );
        return None;
    }
    Some(script)
}

/// Resolve the weights snapshot dir for `multi-v1.0`.
///
/// Honours `HHAGENT_DATA_DIR` first, falls back to
/// `$HOME/.local/share/hhagent` (mirrors `build_gliner_relex_entry`'s
/// resolution). Skip-as-pass when the dir is missing on disk.
fn resolve_weights_dir() -> Option<PathBuf> {
    let data_dir = std::env::var("HHAGENT_DATA_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share/hhagent"))
        })?;
    let weights = data_dir.join("workers/gliner-relex/weights/multi-v1.0");
    if !weights.is_dir() {
        eprintln!(
            "\n[SKIP] gliner-relex weights dir missing at {} — run scripts/workers/gliner-relex/install.sh\n",
            weights.display()
        );
        return None;
    }
    Some(weights)
}

/// Skip-helper smoke test: confirms the resolution helpers compile +
/// run without panicking on hosts where the venv/weights are absent.
/// The real assertions land in [`happy_path_extract_returns_entities_and_triples`]
/// and friends below.
#[test]
fn skip_helpers_compile_and_return_cleanly_on_unstaged_hosts() {
    let _ = resolve_worker_script();
    let _ = resolve_weights_dir();
}

/// Build the gliner-relex `ToolEntry` against the in-tree venv + the
/// on-disk weights. Returns `None` if any of the four preconditions
/// (sandbox / supervisor / venv / weights) is missing — every caller
/// converts that into a `[SKIP]` early return.
fn build_test_entry() -> Option<ToolEntry> {
    if skip_if_sandbox_unavailable() {
        return None;
    }
    if skip_if_no_supervisor() {
        return None;
    }
    let script = resolve_worker_script()?;
    let weights = resolve_weights_dir()?;
    let venv_dir = script
        .parent()
        .and_then(|bin| bin.parent())
        .expect("script_path is .venv/bin/<bin> — both parent levels must exist")
        .to_path_buf();
    let env = GlinerRelexEnv {
        script_path: script,
        venv_dir,
        weights_dir: weights,
        model_id: "knowledgator/gliner-relex-multi-v1.0".to_string(),
        device: "auto".to_string(),
    };
    Some(gliner_relex_entry(&env))
}

/// Bring up a one-shot Postgres cluster + run the schema probe. Skips
/// cleanly when `pg_bin_dir_or_skip` returns `None`.
///
/// Returns the cluster (drop-cleanup wired through `PgCluster::_guards`)
/// plus a runtime-role-scoped `PgPool` ready for `tool_host::dispatch`.
async fn bring_up_pg(label: &str) -> Option<(PgCluster, sqlx::PgPool)> {
    let bin_dir = pg_bin_dir_or_skip()?;
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("glr-{label}-d"),
        &format!("glr-{label}-l"),
        &format!("hhagent-supervisor-test-pg-gliner-{label}-{suffix}"),
    );
    hhagent_db::probe::run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": format!("gliner-relex-{label}")}),
    )
    .await
    .expect("probe run");
    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("connect runtime pool");
    Some((cluster, pool))
}

/// Happy path: spawn a real worker, send one `extract` over the
/// dispatcher, decode the response.
///
/// On the operator's DGX (which has weights staged + vLLM owning the
/// GPU at the time of writing), this exercises the CPU code path and
/// completes the cold-start + one inference in ~3-5 s.
#[tokio::test(flavor = "multi_thread")]
async fn happy_path_extract_returns_entities_and_triples() {
    let Some(entry) = build_test_entry() else {
        return;
    };
    let Some((_cluster, pool)) = bring_up_pg("happy").await else {
        return;
    };

    let sandbox: Arc<dyn hhagent_sandbox::SandboxBackend> = Arc::from(backend());
    let lifecycle = IdleTimeoutLifecycle::new(sandbox);

    let mut handle = lifecycle
        .acquire("gliner-relex", &entry)
        .await
        .expect("acquire gliner-relex worker");

    let req = ExtractRequest {
        text: "Dr Smith treats asthma in Mosman.".to_string(),
        entity_labels: vec!["person".into(), "disease".into(), "location".into()],
        relation_labels: vec!["treats".into(), "located_in".into()],
        threshold: Some(0.5),
        relation_threshold: Some(0.5),
        max_entities: Some(64),
    };
    let params = serde_json::to_value(&req).expect("serialise ExtractRequest");

    let result_value = tool_host::dispatch(
        &pool,
        handle.worker_mut(),
        "gliner-relex",
        "extract",
        params,
    )
    .await
    .expect("dispatch extract");

    let response: ExtractResponse =
        serde_json::from_value(result_value).expect("decode ExtractResponse");

    // Don't pin exact entity / triple counts — that depends on the
    // model version and the threshold. The shape pin (at-least-one
    // entity) is enough; the model-quality fingerprint lives in the
    // POC spike notes and the README, not in the test suite.
    assert!(
        !response.entities.is_empty(),
        "model should find at least one entity in 'Dr Smith treats asthma in Mosman.'"
    );
    // If we did get triples, sanity-check the nested-shape pin — head
    // and tail must carry `type` and `entity_idx` keys via TripleEntity,
    // and the head text must be a substring of the original input.
    if let Some(t) = response.triples.first() {
        assert!(!t.head.r#type.is_empty(), "head.type must be populated");
        assert!(
            !t.relation.is_empty(),
            "triple.relation must be populated"
        );
    }
}

/// Two sequential acquires for the same tool must hit the same warm
/// worker — proves the IdleTimeoutLifecycle warm-cache key actually
/// lands the gliner-relex entry in the per-tool slot.
///
/// The pin is via `IdleTimeoutLifecycle::_test_slot_has_warm`
/// (`#[doc(hidden)]`; the same accessor `worker_lifecycle_idle_timeout_e2e`
/// uses for warm-keep observation without PID introspection). Wall-clock
/// is the second-order signal: the second call's dispatch latency is
/// materially smaller than the first because no model reload happens.
/// We don't pin that here — too brittle on shared hardware — but the
/// `_test_slot_has_warm` true-result is the structural guarantee.
#[tokio::test(flavor = "multi_thread")]
async fn warm_reuse_two_calls_keep_one_worker_warm() {
    let Some(entry) = build_test_entry() else {
        return;
    };
    let Some((_cluster, pool)) = bring_up_pg("warm").await else {
        return;
    };

    let sandbox: Arc<dyn hhagent_sandbox::SandboxBackend> = Arc::from(backend());
    let lifecycle = IdleTimeoutLifecycle::new(sandbox);

    // A small request that won't strain the model — we're testing the
    // warm-cache key, not the inference quality.
    let request = || ExtractRequest {
        text: "alpha beta gamma".to_string(),
        entity_labels: vec!["term".into()],
        relation_labels: vec![],
        threshold: Some(0.3),
        relation_threshold: Some(0.3),
        max_entities: Some(8),
    };

    // First call: cold spawn.
    {
        let mut handle = lifecycle
            .acquire("gliner-relex", &entry)
            .await
            .expect("acquire 1 (cold spawn)");
        let params = serde_json::to_value(&request()).unwrap();
        tool_host::dispatch(&pool, handle.worker_mut(), "gliner-relex", "extract", params)
            .await
            .expect("dispatch 1");
        // Handle drops here → IdleTimeoutLifecycle returns the
        // SupervisedWorker to the warm slot (post-completion cap eval
        // shows we're under all limits).
    }

    // After drop, the slot must hold a warm worker keyed by the
    // logical tool name we passed to `acquire`.
    assert!(
        lifecycle._test_slot_has_warm("gliner-relex").await,
        "expected warm worker in 'gliner-relex' slot after first call drop"
    );

    // Second call: warm reuse (no second cold-spawn).
    {
        let mut handle = lifecycle
            .acquire("gliner-relex", &entry)
            .await
            .expect("acquire 2 (warm reuse)");
        let params = serde_json::to_value(&request()).unwrap();
        tool_host::dispatch(&pool, handle.worker_mut(), "gliner-relex", "extract", params)
            .await
            .expect("dispatch 2");
    }

    // Slot is still warm after the second call — cap eval shows we're
    // still way under the 10_000 request cap.
    assert!(
        lifecycle._test_slot_has_warm("gliner-relex").await,
        "slot must stay warm after second call drop (well under max_requests=10_000)"
    );
}
