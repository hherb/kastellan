//! Cross-crate pin for the `web.search_batch` size-cap env var.
//!
//! Core injects `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` into the web-search
//! jail; the worker reads the same name via `web-common`. Because `web-common`
//! is only a **dev-dependency** of core, the core library can't import the
//! shared const to share one definition — so the two live as separate literals.
//! This test (which *can* see both, as an integration test) pins them equal, so
//! a rename on either side fails CI instead of silently disabling the override
//! (core would inject the old name; the worker would read the new one → the
//! operator's cap is ignored and the worker falls back to its default).

#[test]
fn web_search_batch_cap_env_matches_worker_contract() {
    assert_eq!(
        kastellan_core::workers::web_search::MAX_BATCH_QUERIES_ENV,
        kastellan_worker_web_common::WEB_SEARCH_MAX_BATCH_QUERIES_ENV,
        "core injects a different env-var name than the worker reads — the \
         web.search_batch size-cap override would silently stop working",
    );
}
