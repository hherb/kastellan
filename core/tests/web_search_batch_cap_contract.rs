//! Cross-crate pins for the `web.search_batch` contract between `kastellan-core`
//! and the web-search worker. Because `web-common` is only a **dev-dependency**
//! of core, the core library can't import the shared consts to share one
//! definition — so each pair lives as separate literals. These tests (which
//! *can* see both, as integration tests) pin them equal, so a rename on either
//! side fails CI instead of silently breaking at runtime.

/// Core injects `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` into the web-search
/// jail; the worker reads the same name via `web-common`. A rename on one side
/// would silently disable the override (core injects the old name; the worker
/// reads the new one → the operator's cap is ignored and the worker falls back
/// to its default).
#[test]
fn web_search_batch_cap_env_matches_worker_contract() {
    assert_eq!(
        kastellan_core::workers::web_search::MAX_BATCH_QUERIES_ENV,
        kastellan_worker_web_common::WEB_SEARCH_MAX_BATCH_QUERIES_ENV,
        "core injects a different env-var name than the worker reads — the \
         web.search_batch size-cap override would silently stop working",
    );
}

/// Core advertises the batch method string in `tool_docs()` (and keys the
/// planner-summary cap on it); the planner emits it as the step `method`; the
/// worker dispatches on the same string via `web-common`. A rename on one side
/// would route every batch call to `METHOD_NOT_FOUND` — the planner would keep
/// asking for a method the worker no longer answers.
#[test]
fn web_search_batch_method_matches_worker_contract() {
    assert_eq!(
        kastellan_core::workers::web_search::WEB_SEARCH_BATCH_METHOD,
        kastellan_worker_web_common::WEB_SEARCH_BATCH_METHOD,
        "core advertises a different batch method than the worker dispatches on \
         — every web.search_batch call would return METHOD_NOT_FOUND",
    );
}
