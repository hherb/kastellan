//! `entities reembed` — backfill `entities.embedding` for every entity whose
//! embedding is NULL, through the real `RouterEmbedder` (same config as the
//! daemon). Prints `scanned=/embedded=/skipped=`; exits non-zero when a batch
//! found rows but embedded none (e.g. an unreachable embed endpoint) so a
//! scripted `reembed && next-step` chain does not proceed. Takes no args.
//!
//! Lifted from `entities.rs` per the 500-LOC soft cap; mirrors the
//! [`crate::entities_kinds`] delegation precedent.

use std::process::ExitCode;

use crate::common::{resolve_connect_spec, with_runtime};

/// Sync entry-point for `entities reembed`.
///
/// Per [Issue #97](https://github.com/hherb/kastellan/issues/97) posture,
/// arg validation happens *before* `with_runtime` so no tokio worker threads
/// are spawned on a bad invocation.
pub(crate) fn run(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("usage: kastellan-cli entities reembed");
        return ExitCode::from(2);
    }
    with_runtime("entities reembed", reembed(args))
}

async fn reembed(_args: &[String]) -> ExitCode {
    use std::sync::Arc;

    use kastellan_core::memory::{
        format_reembed_report, reembed_batch_failed, reembed_entities_null, RouterEmbedder,
    };
    use kastellan_db::pool::connect_runtime_pool;

    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // Build the Router-backed embedder. `from_env` reads the host's
    // KASTELLAN_LLM_* config — run this with the same env the daemon uses so
    // backfilled vectors match on-insert ones.
    let router_cfg = match kastellan_llm_router::RouterConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("entities reembed: RouterConfig::from_env: {e}");
            return ExitCode::from(1);
        }
    };
    let router = match kastellan_llm_router::Router::new(router_cfg) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("entities reembed: Router::new: {e}");
            return ExitCode::from(1);
        }
    };
    let embedder = RouterEmbedder::new(pool.clone(), router);

    match reembed_entities_null(&pool, &embedder).await {
        Ok(report) => {
            println!("{}", format_reembed_report(&report));
            // A batch that found rows but embedded none exits non-zero; the
            // idempotent no-op (scanned==0) exits 0.
            if reembed_batch_failed(&report) {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(e) => {
            eprintln!("entities reembed: {e}");
            ExitCode::from(1)
        }
    }
}
