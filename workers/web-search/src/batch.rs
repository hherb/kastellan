//! Batch web-search: run several INDEPENDENT queries in one `web.search_batch`
//! call so the planner spends one planning iteration instead of N. The searches
//! run sequentially above the `SearchProvider` seam, so this works identically
//! for the direct and broker providers with no change to either. One failing
//! query never sinks the batch — each query yields its own result-or-error
//! element (the "no silent drops" contract, mirroring web-research's
//! `unfetched[]`). Design:
//! docs/superpowers/specs/2026-07-12-batch-web-search-design.md

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use kastellan_worker_web_common::parse::Hit;

use crate::handler::{search_err_to_rpc, SearchProvider};

/// Env var (set on the daemon, injected into the jail only when set) that
/// overrides the batch-size cap. Defined once in `web-common` so core (which
/// injects it into the jail) and this worker (which reads it) share a single
/// definition rather than two "kept in sync" string literals.
pub use kastellan_worker_web_common::WEB_SEARCH_MAX_BATCH_QUERIES_ENV as MAX_BATCH_QUERIES_ENV;

/// Default max queries per batch when the operator sets no override.
pub const DEFAULT_MAX_BATCH_QUERIES: usize = 8;

/// Hard upper bound on the configurable cap — a backstop against a pathological
/// operator value (the soft batch deadline + 60 s worker wall watchdog are the
/// ultimate guards).
pub const HARD_MAX_BATCH_QUERIES: usize = 32;

/// Soft wall-clock budget for a whole batch. The queries run sequentially, each
/// bounded by the transport's 20 s per-request timeout; the worker's overall
/// wall-clock watchdog (`wall_clock_ms`, 60 s) SIGKILLs the process — and with
/// it the *entire* batch, including already-completed queries — if it trips. To
/// keep that from silently discarding good results, [`run_batch`] stops issuing
/// NEW queries once this budget is reached and returns a per-query error for the
/// remainder (upholding the "one failing query never sinks the batch" contract
/// under load). Set to `wall − transport` so the last in-flight query still
/// finishes within the wall.
pub const BATCH_SOFT_DEADLINE: Duration = Duration::from_secs(40);

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
/// The `query` field always echoes the input query at that position.
///
/// `deadline` is a soft wall-clock budget (see [`BATCH_SOFT_DEADLINE`]): once it
/// is reached, the remaining queries are NOT issued and each yields a "budget
/// reached" `Err` element instead. This bounds the total time so the worker's
/// hard wall-clock watchdog cannot SIGKILL the process mid-batch and discard the
/// already-completed queries. `None` disables the budget (used by the pure unit
/// tests). Pure with respect to the injected provider and clock.
pub fn run_batch(
    provider: &dyn SearchProvider,
    queries: &[String],
    count: usize,
    deadline: Option<Instant>,
) -> Vec<BatchElement> {
    queries
        .iter()
        .map(|q| {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return BatchElement::Err {
                    query: q.clone(),
                    error: "batch wall-clock budget reached before this query ran".to_string(),
                };
            }
            match provider.search(q, count) {
                Ok(hits) => {
                    let n = hits.len();
                    BatchElement::Ok { query: q.clone(), results: hits, count: n }
                }
                Err(e) => {
                    BatchElement::Err { query: q.clone(), error: search_err_to_rpc(e).message }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::search::SearchError;

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
        let v = serde_json::to_value(run_batch(&FakeProvider, &qs, 10, None)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["query"], "a");
        assert_eq!(v[0]["count"], 1);
        assert_eq!(v[0]["results"][0]["url"], "https://a.test");
        assert_eq!(v[1]["query"], "b");
    }

    #[test]
    fn run_batch_one_failure_does_not_sink_batch() {
        let qs = vec!["a".to_string(), "bad".to_string(), "c".to_string()];
        let v = serde_json::to_value(run_batch(&FakeProvider, &qs, 10, None)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
        assert!(v[0].get("error").is_none());
        assert_eq!(v[1]["query"], "bad");
        assert!(v[1]["error"].is_string(), "element 2 should be an error: {v}");
        assert!(v[1].get("results").is_none());
        assert!(v[2]["results"].is_array());
    }

    #[test]
    fn run_batch_past_deadline_marks_remaining_queries_without_sinking_batch() {
        // A deadline already reached (captured now; the per-query check calls
        // `Instant::now()` again, which is `>=` it on the monotonic clock) → no
        // query is issued, but every input still gets its own error element with
        // its `query` echoed. The whole batch is never lost.
        let qs = vec!["a".to_string(), "b".to_string()];
        let v =
            serde_json::to_value(run_batch(&FakeProvider, &qs, 10, Some(Instant::now()))).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[0]["query"], "a");
        assert!(v[0]["error"].as_str().unwrap().contains("budget"), "{v}");
        assert!(v[0].get("results").is_none());
        assert_eq!(v[1]["query"], "b");
        assert!(v[1]["error"].is_string());
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
        // Boundary: exactly at the cap succeeds (`>` not `>=`).
        assert!(validate_batch(&three, 3).is_ok());
        let msg = validate_batch(&three, 2).unwrap_err();
        assert!(msg.contains("max 2"), "message should name the cap: {msg}");
    }
}
