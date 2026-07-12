# Batch web-search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `web.search_batch{queries:[…]}` JSON-RPC method so the planner can run several independent searches in one dispatch, collapsing N ~46 s plan iterations into 1.

**Architecture:** A new pure batch core in the web-search worker loops the **existing** `SearchProvider.search()` seam sequentially, one result-or-error element per query (no silent drops). Works identically for the direct and broker providers → **zero changes** to web-common, the search-broker crate, or core's `broker/` module. The batch method is advertised via a new defaulted `WorkerManifest::tool_docs()`; the size cap is operator-configurable and injected into the jail only when set.

**Tech Stack:** Rust, `serde`/`serde_json`, `kastellan-protocol` JSON-RPC, the existing web-search worker + core manifest/registry.

Spec: `docs/superpowers/specs/2026-07-12-batch-web-search-design.md`.

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** This plan adds **no** new dependency.
- **Cross-platform (Linux + macOS).** All code here is pure Rust with no OS-specific paths — no `#[cfg(target_os)]` needed.
- **`web.search` stays byte-identical.** The single-query request/response is regression-pinned; only a new sibling method is added.
- **No new egress surface.** The batch reaches SearxNG only through the same allowlist/broker route as a single search. Injection screening is unchanged (same `tool_host` output sink, same content type).
- **Prefer pure functions in reusable modules; TDD; ≤500 LOC/file; understandable inline docs for a junior contributor; all tests green before commit.**
- **Build/test in the FOREGROUND** (never background cargo): prefix every cargo invocation with `source "$HOME/.cargo/env"` (cargo is off the non-interactive PATH).
- **`git add <specific files>`** per step — never `git add -A` (untracked docs/lock files must stay out).

---

### Task 1: Batch core (`batch.rs`) + handler dispatch

The pure batch orchestration **and** its handler wiring land together in one commit: `run_batch`/`BatchParams`/etc. are `pub` items in a binary crate, so they would trip `dead_code` under clippy `-D warnings` until the handler references them (the embed-broker arc folded its `BrokeredEmbedder` for the same reason). The unit tests still exercise the pure functions directly.

**Files:**
- Create: `workers/web-search/src/batch.rs`
- Modify: `workers/web-search/src/main.rs:6` (add `mod batch;`)
- Modify: `workers/web-search/src/handler.rs:26` (make `search_err_to_rpc` `pub(crate)`), `:187-189` (add `max_batch` field), `:201-233` (`from_env` + `with_parts`), `:236-261` (dispatch → `match` + batch arm)
- Test: `workers/web-search/src/batch.rs` (`#[cfg(test)] mod tests`) + new tests in `workers/web-search/src/handler.rs` `mod tests`

**Interfaces:**
- Consumes: `SearchProvider` (trait, `handler.rs:63`), `search_err_to_rpc` (`handler.rs:26`), `Hit` (`kastellan_worker_web_common::parse::Hit`), `SearchError` (`kastellan_worker_web_common::search::SearchError`), `DEFAULT_COUNT` (`…::search::DEFAULT_COUNT`).
- Produces:
  - `pub const MAX_BATCH_QUERIES_ENV: &str = "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES"`
  - `pub const DEFAULT_MAX_BATCH_QUERIES: usize = 8`
  - `pub const HARD_MAX_BATCH_QUERIES: usize = 32`
  - `pub struct BatchParams { pub queries: Vec<String>, pub count: Option<usize> }`
  - `pub enum BatchElement` (untagged serialize → `{query,results,count}` | `{query,error}`)
  - `pub fn resolve_max_batch(env_val: Option<&str>) -> usize`
  - `pub fn validate_batch(queries: &[String], max_batch: usize) -> Result<(), String>`
  - `pub fn run_batch(provider: &dyn SearchProvider, queries: &[String], count: usize) -> Vec<BatchElement>`
  - `WebSearchHandler` gains `max_batch: usize`; serves method `"web.search_batch"`.

- [ ] **Step 1: Write `batch.rs` with the pure core + its unit tests (failing — file doesn't compile yet).**

Create `workers/web-search/src/batch.rs`:

```rust
//! Batch web-search: run several INDEPENDENT queries in one `web.search_batch`
//! call so the planner spends one planning iteration instead of N. The searches
//! run sequentially above the `SearchProvider` seam, so this works identically
//! for the direct and broker providers with no change to either. One failing
//! query never sinks the batch — each query yields its own result-or-error
//! element (the "no silent drops" contract, mirroring web-research's
//! `unfetched[]`). Design:
//! docs/superpowers/specs/2026-07-12-batch-web-search-design.md

use serde::{Deserialize, Serialize};

use kastellan_worker_web_common::parse::Hit;
use kastellan_worker_web_common::search::SearchError;

use crate::handler::{search_err_to_rpc, SearchProvider};

/// Env var (set on the daemon, injected into the jail only when set) that
/// overrides the batch-size cap. Kept in sync with the same-named const in
/// `core/src/workers/web_search.rs`.
pub const MAX_BATCH_QUERIES_ENV: &str = "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES";

/// Default max queries per batch when the operator sets no override.
pub const DEFAULT_MAX_BATCH_QUERIES: usize = 8;

/// Hard upper bound on the configurable cap — a backstop against a pathological
/// operator value (the 30 s worker wall watchdog is the ultimate guard).
pub const HARD_MAX_BATCH_QUERIES: usize = 32;

/// Request params for `web.search_batch`.
#[derive(Deserialize)]
pub struct BatchParams {
    pub queries: Vec<String>,
    #[serde(default)]
    pub count: Option<usize>,
}

/// One element of a batch response: a per-query success (identical to a single
/// `web.search` body) or a per-query error. Serialized untagged so the wire
/// shape is exactly `{query,results,count}` or `{query,error}`.
#[derive(Serialize)]
#[serde(untagged)]
pub enum BatchElement {
    Ok { query: String, results: Vec<Hit>, count: usize },
    Err { query: String, error: String },
}

/// Resolve the effective batch cap from the (optional) operator override.
/// Parse → clamp to `[1, HARD_MAX_BATCH_QUERIES]`; unset / blank / unparseable →
/// `DEFAULT_MAX_BATCH_QUERIES`. Pure.
pub fn resolve_max_batch(env_val: Option<&str>) -> usize {
    match env_val.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match s.parse::<usize>() {
            Ok(n) => n.clamp(1, HARD_MAX_BATCH_QUERIES),
            Err(_) => DEFAULT_MAX_BATCH_QUERIES,
        },
        None => DEFAULT_MAX_BATCH_QUERIES,
    }
}

/// Validate a batch request shape. `Err(message)` (mapped by the handler to
/// `INVALID_PARAMS`) for an empty or over-cap query list; the message names the
/// effective cap so the planner can adjust. Pure.
pub fn validate_batch(queries: &[String], max_batch: usize) -> Result<(), String> {
    if queries.is_empty() {
        return Err("queries must be a non-empty array".to_string());
    }
    if queries.len() > max_batch {
        return Err(format!("too many queries: {} (max {max_batch})", queries.len()));
    }
    Ok(())
}

/// Run each query in order through the provider, one element per query. A
/// per-query `SearchError` becomes an `Err` element (never aborts the batch).
/// The `query` field always echoes the input query at that position. Pure with
/// respect to the injected provider — unit-testable with a fake.
pub fn run_batch(
    provider: &dyn SearchProvider,
    queries: &[String],
    count: usize,
) -> Vec<BatchElement> {
    queries
        .iter()
        .map(|q| match provider.search(q, count) {
            Ok(hits) => {
                let n = hits.len();
                BatchElement::Ok { query: q.clone(), results: hits, count: n }
            }
            Err(e) => BatchElement::Err { query: q.clone(), error: search_err_to_rpc(e).message },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake provider: `"bad"` fails; every other query returns one hit whose URL
    /// encodes the query, so ordering + query echo are observable.
    struct FakeProvider;
    impl SearchProvider for FakeProvider {
        fn search(&self, query: &str, _count: usize) -> Result<Vec<Hit>, SearchError> {
            if query == "bad" {
                Err(SearchError::Transport("boom".into()))
            } else {
                Ok(vec![Hit {
                    title: "T".into(),
                    url: format!("https://{query}.test"),
                    snippet: "c".into(),
                    engine: "e".into(),
                }])
            }
        }
    }

    #[test]
    fn run_batch_preserves_order_and_query_fields() {
        let qs = vec!["a".to_string(), "b".to_string()];
        let v = serde_json::to_value(run_batch(&FakeProvider, &qs, 10)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["query"], "a");
        assert_eq!(v[0]["count"], 1);
        assert_eq!(v[0]["results"][0]["url"], "https://a.test");
        assert_eq!(v[1]["query"], "b");
    }

    #[test]
    fn run_batch_one_failure_does_not_sink_batch() {
        let qs = vec!["a".to_string(), "bad".to_string(), "c".to_string()];
        let v = serde_json::to_value(run_batch(&FakeProvider, &qs, 10)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
        assert!(v[0].get("error").is_none());
        assert_eq!(v[1]["query"], "bad");
        assert!(v[1]["error"].is_string(), "element 2 should be an error: {v}");
        assert!(v[1].get("results").is_none());
        assert!(v[2]["results"].is_array());
    }

    #[test]
    fn batch_element_success_serializes_to_query_results_count() {
        let el = BatchElement::Ok { query: "q".into(), results: vec![], count: 0 };
        assert_eq!(
            serde_json::to_value(el).unwrap(),
            serde_json::json!({ "query": "q", "results": [], "count": 0 })
        );
    }

    #[test]
    fn batch_element_error_serializes_to_query_error() {
        let el = BatchElement::Err { query: "q".into(), error: "boom".into() };
        assert_eq!(
            serde_json::to_value(el).unwrap(),
            serde_json::json!({ "query": "q", "error": "boom" })
        );
    }

    #[test]
    fn resolve_max_batch_defaults_and_clamps() {
        assert_eq!(resolve_max_batch(None), DEFAULT_MAX_BATCH_QUERIES);
        assert_eq!(resolve_max_batch(Some("")), DEFAULT_MAX_BATCH_QUERIES);
        assert_eq!(resolve_max_batch(Some("  ")), DEFAULT_MAX_BATCH_QUERIES);
        assert_eq!(resolve_max_batch(Some("abc")), DEFAULT_MAX_BATCH_QUERIES);
        assert_eq!(resolve_max_batch(Some("3")), 3);
        assert_eq!(resolve_max_batch(Some("0")), 1); // clamp low
        assert_eq!(resolve_max_batch(Some("999")), HARD_MAX_BATCH_QUERIES); // clamp high
        assert_eq!(resolve_max_batch(Some("32")), 32);
    }

    #[test]
    fn validate_batch_rejects_empty_and_over_cap() {
        assert!(validate_batch(&[], 8).is_err());
        let three = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(validate_batch(&three, 8).is_ok());
        let msg = validate_batch(&three, 2).unwrap_err();
        assert!(msg.contains('2'), "message should name the cap: {msg}");
    }
}
```

- [ ] **Step 2: Wire the module + make `search_err_to_rpc` visible; add the handler `match` arm + `max_batch` field.**

In `workers/web-search/src/main.rs`, add `mod batch;` directly under `mod handler;` (line 6):
```rust
mod batch;
mod handler;
```

In `workers/web-search/src/handler.rs`:

1. Make the error mapper crate-visible (line 26): change `fn search_err_to_rpc` → `pub(crate) fn search_err_to_rpc`.

2. Add the field to the handler struct (replace lines 187-189):
```rust
pub struct WebSearchHandler {
    provider: Box<dyn SearchProvider>,
    /// Max queries accepted by `web.search_batch` (operator-tunable via
    /// `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`; default 8).
    max_batch: usize,
}
```

3. In `from_env`, read the cap and set the field (change the trailing `Ok(Self { provider })` at line 223):
```rust
        let max_batch = crate::batch::resolve_max_batch(
            std::env::var(crate::batch::MAX_BATCH_QUERIES_ENV).ok().as_deref(),
        );
        Ok(Self { provider, max_batch })
```

4. Update the two `#[cfg(test)]` constructors (replace lines 226-233):
```rust
    #[cfg(test)]
    fn with_parts<T: HttpGet + 'static>(
        endpoint: Url,
        allowlist: HostAllowlist,
        transport: T,
    ) -> Self {
        Self {
            provider: Box::new(DirectSearchProvider::new(endpoint, allowlist, transport)),
            max_batch: crate::batch::DEFAULT_MAX_BATCH_QUERIES,
        }
    }

    #[cfg(test)]
    fn with_parts_and_max_batch<T: HttpGet + 'static>(
        endpoint: Url,
        allowlist: HostAllowlist,
        transport: T,
        max_batch: usize,
    ) -> Self {
        Self {
            provider: Box::new(DirectSearchProvider::new(endpoint, allowlist, transport)),
            max_batch,
        }
    }
```

5. Replace the dispatch body (lines 242-259, from `if method != "web.search"` through the final `Ok(json!{…})`) with a `match`:
```rust
        match method {
            "web.search" => {
                let p: SearchParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let count = p.count.unwrap_or(DEFAULT_COUNT);
                let hits = self.provider.search(&p.query, count).map_err(search_err_to_rpc)?;
                let hit_count = hits.len();
                Ok(serde_json::json!({ "query": p.query, "results": hits, "count": hit_count }))
            }
            "web.search_batch" => {
                let p: crate::batch::BatchParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                crate::batch::validate_batch(&p.queries, self.max_batch)
                    .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
                let count = p.count.unwrap_or(DEFAULT_COUNT);
                let elements = crate::batch::run_batch(&*self.provider, &p.queries, count);
                Ok(serde_json::json!({ "results": elements }))
            }
            other => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        }
```

- [ ] **Step 3: Add handler-level batch tests** (append inside `handler.rs`'s `mod tests`, after `endpoint_failure_maps_to_operation_failed`):

```rust
    #[test]
    fn batch_returns_per_query_results_in_order() {
        let good = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(good), json_resp(good)]);
        let out = h
            .call("web.search_batch", serde_json::json!({"queries": ["a", "b"]}))
            .unwrap();
        let arr = out["results"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["query"], "a");
        assert_eq!(arr[0]["results"][0]["url"], "https://x.test");
        assert_eq!(arr[1]["query"], "b");
    }

    #[test]
    fn batch_one_bad_query_is_error_element_not_whole_failure() {
        let good = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![
            json_resp(good),
            RawResponse { status: 500, location: None, content_type: "text/plain".into(), body: Vec::new() },
        ]);
        let out = h
            .call("web.search_batch", serde_json::json!({"queries": ["a", "b"]}))
            .unwrap();
        let arr = out["results"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["results"][0]["url"], "https://x.test");
        assert!(arr[1]["error"].is_string(), "b should be an error element: {out}");
    }

    #[test]
    fn batch_empty_queries_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h
            .call("web.search_batch", serde_json::json!({"queries": []}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn batch_over_cap_is_invalid_params() {
        let mut h = WebSearchHandler::with_parts_and_max_batch(
            Url::parse("https://searx.example.org/search").unwrap(),
            al(&["searx.example.org"]),
            FakeGet::new(vec![]),
            2,
        );
        let err = h
            .call("web.search_batch", serde_json::json!({"queries": ["a", "b", "c"]}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn single_search_still_byte_identical() {
        // Regression pin: web.search is unchanged by the batch arm.
        let json = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(json)]);
        let out = h.call("web.search", serde_json::json!({"query": "rust"})).unwrap();
        assert_eq!(out["query"], "rust");
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["snippet"], "c");
    }
```

- [ ] **Step 4: Run the worker tests — expect all green.**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-search`
Expected: PASS — includes the 6 new `batch::tests::*` and the 5 new handler tests plus all pre-existing tests.

- [ ] **Step 5: Clippy the worker crate.**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-worker-web-search --all-targets -- -D warnings`
Expected: clean (no dead-code — `run_batch` etc. are now referenced by the handler).

- [ ] **Step 6: Commit.**

```bash
git add workers/web-search/src/batch.rs workers/web-search/src/main.rs workers/web-search/src/handler.rs
git commit -m "feat(web-search): web.search_batch — sequential batch above the SearchProvider seam"
```

---

### Task 2: Advertise `web.search_batch` — trait `tool_docs()` + web-search override + registry collection

**Files:**
- Modify: `core/src/worker_manifest.rs:58-60` (add defaulted `tool_docs()`), `:282-327` (`mod tool_doc_tests` — add a test)
- Modify: `core/src/workers/web_search.rs:161-178` (override `tool_docs()`)
- Modify: `core/src/registry_build.rs:171-173` (production collection), `:361-372` (drift-guard test) + a new advertise test
- Test: the modified test modules above

**Interfaces:**
- Consumes: `WorkerManifest::tool_doc` (existing), `ToolDoc`/`ToolParam` (`core/src/worker_manifest.rs`).
- Produces: `WorkerManifest::tool_docs(&self) -> Vec<ToolDoc>` (default wraps `tool_doc()`); `WebSearchManifest::tool_docs()` returns `[web.search, web.search_batch]`, both `name == "web-search"`.

- [ ] **Step 1: Add the defaulted trait method + its unit test (test fails to compile until the method exists).**

In `core/src/worker_manifest.rs`, immediately after the `tool_doc` default (after line 60, inside the trait):
```rust
    /// All planner-facing tool docs for this worker. Defaults to wrapping the
    /// single [`WorkerManifest::tool_doc`], so single-method workers need no
    /// change. A worker that serves several JSON-RPC methods (e.g. web-search:
    /// `web.search` + `web.search_batch`) overrides this to advertise each. Every
    /// returned doc's `name` must still equal [`WorkerManifest::name`]
    /// (drift-guarded).
    fn tool_docs(&self) -> Vec<ToolDoc> {
        self.tool_doc().into_iter().collect()
    }
```

Add to `mod tool_doc_tests` (after `overridden_tool_doc_carries_fields`, before the closing brace at line 327):
```rust
    #[test]
    fn default_tool_docs_wraps_single_doc() {
        assert!(BareManifest.tool_docs().is_empty());
        let docs = DocManifest.tool_docs();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].method, "doc.run");
    }
```

- [ ] **Step 2: Run the trait test — expect green.**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_manifest::tool_doc_tests`
Expected: PASS (the new test + the two existing).

- [ ] **Step 3: Override `tool_docs()` in the web-search manifest.**

In `core/src/workers/web_search.rs`, inside `impl WorkerManifest for WebSearchManifest`, immediately after the existing `tool_doc` method (after line 178) add:
```rust
    fn tool_docs(&self) -> Vec<ToolDoc> {
        // Reuse the single-query doc, then append the batch method. Both docs
        // carry `name == TOOL_NAME` so the drift guard (doc.name == name())
        // still holds — same worker, two methods. No numeric ceiling is
        // advertised: the batch size is an operator-tunable runtime value
        // (KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES); an over-cap batch is rejected
        // fail-closed with INVALID_PARAMS and surfaced to the planner.
        let mut docs: Vec<ToolDoc> = self.tool_doc().into_iter().collect();
        docs.push(ToolDoc {
            name: TOOL_NAME,
            method: "web.search_batch",
            summary: "Run several INDEPENDENT web searches in one call; returns a \
                      per-query result group for each. Prefer this over multiple \
                      web.search steps when the queries do not depend on each other.",
            params: &[
                ToolParam {
                    name: "queries",
                    description: "list of independent search queries to run in one batch",
                    required: true,
                },
                ToolParam {
                    name: "count",
                    description: "max results per query, default 10 (cap 20)",
                    required: false,
                },
            ],
        });
        docs
    }
```

- [ ] **Step 4: Switch the registry collection + drift guard to `tool_docs()`, add an advertise test.**

In `core/src/registry_build.rs`, the production collection (lines 171-173):
```rust
                for doc in m.tool_docs() {
                    docs.push(doc);
                }
```

The drift-guard test body (lines 364-370) — iterate all docs:
```rust
        for m in WORKER_MANIFESTS {
            for doc in m.tool_docs() {
                assert_eq!(doc.name, m.name(), "tool_doc name drift for {}", m.name());
                assert!(!doc.method.is_empty(), "{} has empty method", m.name());
                assert!(!doc.summary.is_empty(), "{} has empty summary", m.name());
            }
        }
```

Add a new test after `core_web_and_shell_workers_advertise_a_tool_doc` (after line 391):
```rust
    #[test]
    fn web_search_advertises_the_batch_method() {
        let m = WORKER_MANIFESTS
            .iter()
            .find(|m| m.name() == "web-search")
            .expect("web-search manifest");
        let docs = m.tool_docs();
        assert!(docs.iter().any(|d| d.method == "web.search"), "web.search missing");
        let batch = docs
            .iter()
            .find(|d| d.method == "web.search_batch")
            .expect("web.search_batch advertised");
        assert_eq!(batch.name, "web-search");
        assert!(batch.params.iter().any(|p| p.name == "queries" && p.required));
    }
```

- [ ] **Step 5: Run the core manifest + registry tests.**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib registry_build`
Then: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib worker_manifest`
Expected: PASS — drift guard green (both web-search docs named `web-search`), new advertise test green, `every_registered_worker_docs_name_matches_registry_key` green, `core_web_and_shell_workers_advertise_a_tool_doc` still green (`tool_doc()` still returns the `web.search` doc).

- [ ] **Step 6: Clippy core.**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit.**

```bash
git add core/src/worker_manifest.rs core/src/workers/web_search.rs core/src/registry_build.rs
git commit -m "feat(agent): advertise web.search_batch via a defaulted WorkerManifest::tool_docs()"
```

---

### Task 3: Operator-configurable cap — inject `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` into the jail when set

**Files:**
- Modify: `core/src/workers/web_search.rs` (add the env const near line 30; add `maybe_inject_max_batch`; call it in `resolve` at lines 206-210)
- Test: `core/src/workers/web_search.rs` `mod tests` — add inject-when-set + byte-identical-when-unset

**Interfaces:**
- Consumes: `ToolEntry` (its `policy.env: Vec<(String,String)>`), `ResolveCtx::get_env`.
- Produces: `const MAX_BATCH_QUERIES_ENV` (mirrors the worker const); `fn maybe_inject_max_batch(entry: ToolEntry, val: Option<String>) -> ToolEntry` (pure; appends the env pair only for a non-blank value).

- [ ] **Step 1: Write the failing tests.**

Add to `core/src/workers/web_search.rs` `mod tests` (after the existing broker-mode test):
```rust
    #[test]
    fn resolve_injects_max_batch_env_when_set() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("5".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(
                    entry.policy.env.iter().any(|(k, v)| k == MAX_BATCH_QUERIES_ENV && v == "5"),
                    "cap env must be injected when set: {:?}",
                    entry.policy.env
                );
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_omits_max_batch_env_when_unset() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                // Byte-identical direct-mode env: endpoint + allowlist only.
                assert_eq!(entry.policy.env.len(), 2);
                assert!(entry.policy.env.iter().all(|(k, _)| k != MAX_BATCH_QUERIES_ENV));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn maybe_inject_max_batch_skips_blank() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES" => Some("   ".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(entry.policy.env.iter().all(|(k, _)| k != MAX_BATCH_QUERIES_ENV));
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }
```

Note: `outcome_label` is already imported/used in this test module (it appears in the existing broker test). If the compiler reports it unresolved, add `use super::super::...` — but it is already in scope in these tests.

- [ ] **Step 2: Run the tests to confirm they fail** (no const / no injection yet).

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_search 2>&1 | tail -20`
Expected: FAIL — `MAX_BATCH_QUERIES_ENV` not found / cap env not injected.

- [ ] **Step 3: Add the const + helper + wire into `resolve`.**

In `core/src/workers/web_search.rs`, add near the other consts (after line 30):
```rust
/// Operator override for the `web.search_batch` size cap, read from the daemon
/// env and injected into the jail only when set. Kept in sync with the same-named
/// const in `workers/web-search/src/batch.rs`.
const MAX_BATCH_QUERIES_ENV: &str = "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES";
```

Add the pure helper (place it above the `WebSearchManifest` struct, e.g. after `web_search_broker_entry`):
```rust
/// Append the operator's `web.search_batch` size-cap override to a worker
/// entry's env, but only when it is present and non-blank. Leaving it off keeps
/// the worker on its built-in default (8) and the entry's env byte-identical to
/// the pre-batch behaviour. The worker (`batch::resolve_max_batch`) is the
/// authoritative parser/clamper — core passes the raw trimmed value through.
fn maybe_inject_max_batch(mut entry: ToolEntry, val: Option<String>) -> ToolEntry {
    if let Some(v) = val {
        let v = v.trim();
        if !v.is_empty() {
            entry.policy.env.push((MAX_BATCH_QUERIES_ENV.to_string(), v.to_string()));
        }
    }
    entry
}
```

Rewrite the tail of `resolve` (lines 205-211) to build the entry then inject:
```rust
        let use_broker = (ctx.get_env)(USE_BROKER_ENV).unwrap_or_default().trim() == "1";
        let entry = if use_broker {
            web_search_broker_entry(binary, &endpoint)
        } else {
            let allowlist = host_allowlist_from_endpoint(&endpoint);
            web_search_entry(binary, &endpoint, &allowlist)
        };
        let entry = maybe_inject_max_batch(entry, (ctx.get_env)(MAX_BATCH_QUERIES_ENV));
        Resolution::Register(entry)
```

- [ ] **Step 4: Run the tests — expect green.**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib workers::web_search`
Expected: PASS — the 3 new tests plus all pre-existing web_search manifest tests (the direct-mode `env[0]`/`env[1]` positional asserts still hold: injection appends at index 2 only when set, and these tests don't set the cap).

- [ ] **Step 5: Clippy.**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --lib --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit.**

```bash
git add core/src/workers/web_search.rs
git commit -m "feat(web-search): inject KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES into the jail when set"
```

---

### Task 4: Planner guidance + installer env-template documentation

**Files:**
- Modify: `prompts/agent_planner.md:267-269` (add a batch-search guidance line)
- Modify: `core/src/install/plan.rs:135` (add a commented env line), `:417` region (add a `contains` assertion)
- Test: `core/src/install/plan.rs` `mod tests` (the `render_env_file` test)

**Interfaces:**
- Consumes: nothing new. Produces: a documented `# KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES=8` line in the rendered `kastellan.env`.

- [ ] **Step 1: Add the planner guidance line.**

In `prompts/agent_planner.md`, immediately after the "prefer the one that covers it in a single step" rule (after line 269), add a new bullet:
```markdown
- When you need several **independent** web searches, issue them as one
  `web.search_batch` call (its `queries` array) rather than separate
  `web.search` steps — this resolves them in a single planning step. Use plain
  `web.search` for a single query, or when a later query depends on an earlier
  result.
```

- [ ] **Step 2: Add the env-template line + its test assertion (test first — it will fail).**

In `core/src/install/plan.rs` `mod tests`, in the `render_env_file` test, after the timezone assertion (line 417) add:
```rust
        // web.search_batch size cap documented (commented — worker default 8).
        assert!(
            s.contains("# KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES=8\n"),
            "{s}"
        );
```

- [ ] **Step 3: Run the test to confirm it fails.**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::plan 2>&1 | tail -15`
Expected: FAIL — the rendered env file does not yet contain the batch-cap line.

- [ ] **Step 4: Add the line to `render_env_file`.**

In `core/src/install/plan.rs`, after the timezone `push_str` (line 135), add:
```rust
    // web.search_batch size cap (queries per batch). Commented → the worker
    // default (8) applies; raise/lower to tune how many independent searches the
    // planner may issue in one dispatch. Clamped by the worker to [1, 32].
    s.push_str("# KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES=8\n");
```

- [ ] **Step 5: Run the install test — expect green.**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib install::plan`
Expected: PASS.

- [ ] **Step 6: Full workspace build + clippy (the cross-crate gate).**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Then: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0 + clean.

- [ ] **Step 7: Commit.**

```bash
git add prompts/agent_planner.md core/src/install/plan.rs
git commit -m "docs(planner): guide web.search_batch use + document the size-cap env var"
```

---

## Optional follow-up (not required to ship; flag, don't silently skip)

**Live 2-query batch assertion (DGX-gated).** The existing `#[ignore]` `web_search_e2e::real_search_against_searxng` drives a single live query against a real SearxNG. A cheap add is a sibling `#[ignore]` test issuing `web.search_batch{queries:[q1,q2]}` and asserting `out["results"].as_array().len() == 2` with both elements carrying `results`. It proves the sequential loop end-to-end through the real spawn/dispatch path. Requires a live SearxNG + `KASTELLAN_WEB_SEARCH_ENDPOINT` + the DGX (`source dgx run`) — defer to the session's DGX gate if one is run; otherwise note in the handover as an owed-but-optional live check. **Do not** claim live coverage without running it.

## Verification (end-to-end, before opening the PR)

- `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-search` — all green (batch + handler).
- `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib` — all green (worker_manifest, registry_build, workers::web_search, install::plan).
- `source "$HOME/.cargo/env" && cargo build --workspace` — exit 0.
- `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings` — clean.
- (No DGX gate strictly required: pure Rust, no sandbox/seccomp/PG/schema surface. If a DGX session is run, do the optional live batch check above and record the workspace test count.)

## Self-Review — spec coverage map

| Spec section | Task |
|---|---|
| §3 wire contract (request/response, per-query error) | Task 1 (batch.rs + handler arm + tests) |
| §4 data flow (loop above the seam) | Task 1 (`run_batch`) |
| §5.1 batch.rs pure core | Task 1 |
| §5.2 handler `match` + `max_batch` | Task 1 |
| §5.3 trait `tool_docs()` default | Task 2 |
| §5.4 web-search `tool_docs()` override (no numeric ceiling advertised) | Task 2 |
| §5.5 registry collection + drift guard | Task 2 |
| §5.6 planner guidance | Task 4 |
| §6 configuration (env var, only-when-set injection) | Task 3 (+ Task 1 worker read) + Task 4 (template doc) |
| §7 error semantics | Task 1 (validate + per-query) |
| §8 testing plan | Tasks 1-4 tests + optional live |
| §9 scope/non-goals (web-common / broker / core-broker untouched) | Honoured — no task touches them |
