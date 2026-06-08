//! In-memory, per-task, content-addressed cache for oversized tool results.
//!
//! `tool_host::dispatch` returns a worker's full result; the dispatcher layer
//! caps what actually reaches the planner's context. A result larger than
//! [`DEFAULT_RESULT_BYTE_CAP`] is stashed here and replaced with a small
//! placeholder ([`build_handoff_placeholder`]); the planner pulls slices back
//! on demand through the `fetch_handoff` built-in (see
//! `scheduler::tool_dispatch`). Entries are keyed by `(task_id, HandoffRef)`
//! and purged when the task reaches a terminal state.
//!
//! Design: `docs/superpowers/specs/2026-06-08-handoff-cache-design.md`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::cassandra::injection_guard::extract_scannable_text;

/// Result larger than this (bytes of its serialized JSON) is stashed and
/// replaced with a placeholder. 64 KiB ≈ 16k tokens — generous for one
/// document, below web-fetch's 100 KiB `MAX_TEXT_BYTES`.
pub const DEFAULT_RESULT_BYTE_CAP: usize = 64 * 1024;

/// Bytes of human-readable head carried inline in the placeholder so the
/// planner often needs no fetch at all.
pub const SUMMARY_HEAD_BYTES: usize = 1024;

/// Per-`fetch_handoff` slice ceiling, so one fetch can't blow the context.
pub const MAX_FETCH_BYTES: usize = 256 * 1024;

/// Per-task cache budget; the oldest entries for a task are evicted past it.
pub const PER_TASK_BYTE_BUDGET: usize = 64 * 1024 * 1024;

/// Backstop on the number of distinct task buckets retained. Past this, the
/// oldest-inserted task bucket is evicted wholesale. Guards against a missed
/// [`HandoffCache::purge_task`] (defence-in-depth — normal operation purges at
/// every task terminal, and the cache is process-local so it cannot accumulate
/// across a daemon restart). 4096 concurrent-ish tasks is far above any real
/// scheduler fan-out.
pub const MAX_TRACKED_TASKS: usize = 4096;

/// `"sha256:<64-lowercase-hex>"`. The only way to name a stashed body;
/// opaque to the planner. Content-addressed, so identical bodies share a ref.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HandoffRef(String);

impl HandoffRef {
    /// Content-address `body`: `"sha256:" + hex(sha256(body))`.
    pub fn of(body: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(body);
        let digest = h.finalize();
        let mut s = String::with_capacity(7 + 64);
        s.push_str("sha256:");
        for b in digest {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        HandoffRef(s)
    }

    /// Parse a ref the planner supplied. Validates `sha256:` + exactly 64
    /// lowercase hex digits. `None` on any deviation (the caller surfaces a
    /// planner-visible `INVALID_PARAMS`).
    pub fn parse(s: &str) -> Option<Self> {
        let hex = s.strip_prefix("sha256:")?;
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            Some(HandoffRef(s.to_string()))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Build the placeholder the planner sees in place of a stashed oversized
/// result. `summary_head` is the readable head of the result (char-boundary
/// safe, via the injection-guard text extractor), so the planner often needs
/// no fetch at all.
pub fn build_handoff_placeholder(
    value: &serde_json::Value,
    r: &HandoffRef,
    byte_len: usize,
) -> serde_json::Value {
    // `_truncated` reports whether the *head* hit SUMMARY_HEAD_BYTES; we don't
    // surface it. The placeholder's `truncated: true` means the *body* was
    // stashed (always true on this path), a different fact.
    let (head, _truncated) = extract_scannable_text(value, SUMMARY_HEAD_BYTES);
    serde_json::json!({
        "handoff_ref": r.as_str(),
        "byte_len": byte_len,
        "summary_head": head,
        "truncated": true,
    })
}

/// A byte slice plus whether it reached the end of the stashed body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slice {
    pub bytes: Vec<u8>,
    pub eof: bool,
}

/// Returned by [`HandoffCache::stash_if_oversized`] when a result is stashed.
#[derive(Clone, Debug)]
pub struct StashOutcome {
    pub placeholder: serde_json::Value,
    pub handoff_ref: HandoffRef,
    pub byte_len: usize,
}

/// Outcome of a `fetch_handoff` call. The dispatcher maps each arm to a
/// `StepOutcome` and writes a `policy/handoff.fetched` audit row.
#[derive(Clone, Debug)]
pub enum FetchResult {
    Ok(serde_json::Value),
    NotFound(String),
    InvalidParams(String),
}

/// One task's stashed bodies, in insertion order for oldest-first eviction.
#[derive(Default)]
struct TaskBucket {
    order: VecDeque<HandoffRef>,
    map: HashMap<HandoffRef, Vec<u8>>,
    total: usize,
}

/// Internal cache state behind a single mutex: the per-task buckets plus the
/// task-id insertion order used by the global backstop ([`MAX_TRACKED_TASKS`]).
#[derive(Default)]
struct Inner {
    buckets: HashMap<i64, TaskBucket>,
    /// Task ids in insertion order, oldest at the front. Used only to pick a
    /// victim when the bucket count exceeds [`MAX_TRACKED_TASKS`].
    order: VecDeque<i64>,
}

/// In-memory cache shared (behind `Arc`) by the production dispatcher.
#[derive(Default)]
pub struct HandoffCache {
    inner: Mutex<Inner>,
}

impl HandoffCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stash `body` for `task_id`, returning its content-addressed ref.
    /// Re-storing an identical body returns the same ref without growing the
    /// bucket. Evicts the task's oldest entries if the per-task budget would
    /// be exceeded (a body larger than the whole budget is still stored — it
    /// just evicts everything else for the task; never refused).
    pub fn put(&self, task_id: i64, body: &[u8]) -> HandoffRef {
        let r = HandoffRef::of(body);
        let mut guard = self.inner.lock().expect("handoff cache mutex poisoned");
        let is_new_task = !guard.buckets.contains_key(&task_id);
        {
            let bucket = guard.buckets.entry(task_id).or_default();
            if bucket.map.contains_key(&r) {
                // Idempotent: identical body already stashed for this task.
                // (A pre-existing task is never `is_new_task`, so `order` is
                // untouched on this early return.)
                return r;
            }
            bucket.map.insert(r.clone(), body.to_vec());
            bucket.order.push_back(r.clone());
            bucket.total += body.len();
            // Per-task budget: evict this task's oldest bodies, never the one
            // just inserted (it is last in `order`; loop stops at len 1).
            while bucket.total > PER_TASK_BYTE_BUDGET && bucket.order.len() > 1 {
                let oldest = bucket.order.pop_front().expect("order non-empty while len > 1");
                if let Some(b) = bucket.map.remove(&oldest) {
                    bucket.total -= b.len();
                }
            }
        }
        // Global backstop: a brand-new task bucket extends `order`; evict the
        // oldest-inserted task(s) wholesale if we exceed MAX_TRACKED_TASKS.
        if is_new_task {
            guard.order.push_back(task_id);
            while guard.order.len() > MAX_TRACKED_TASKS {
                if let Some(victim) = guard.order.pop_front() {
                    guard.buckets.remove(&victim);
                }
            }
        }
        r
    }

    /// Up to `len` bytes of the body starting at `offset`. `None` if the
    /// `(task_id, ref)` is unknown or was evicted. Callers clamp `len` to
    /// [`MAX_FETCH_BYTES`] before calling.
    pub fn get_slice(&self, task_id: i64, r: &HandoffRef, offset: usize, len: usize) -> Option<Slice> {
        let guard = self.inner.lock().expect("handoff cache mutex poisoned");
        let body = guard.buckets.get(&task_id)?.map.get(r)?;
        let start = offset.min(body.len());
        let end = offset.saturating_add(len).min(body.len());
        let bytes = body[start..end].to_vec();
        let eof = end >= body.len();
        Some(Slice { bytes, eof })
    }

    /// Drop every entry for `task_id`. Called at task terminal.
    pub fn purge_task(&self, task_id: i64) {
        let mut guard = self.inner.lock().expect("handoff cache mutex poisoned");
        guard.buckets.remove(&task_id);
        guard.order.retain(|&t| t != task_id);
    }

    /// If `value`'s serialized JSON exceeds `cap`, stash it and return the
    /// placeholder + ref + byte length. `None` when within cap (the caller
    /// passes `value` through unchanged). The stashed body is the serialized
    /// JSON, so it is always valid UTF-8 (slices may split a multibyte char at
    /// the edges — [`fetch`](Self::fetch) handles that lossily).
    pub fn stash_if_oversized(
        &self,
        task_id: i64,
        value: &serde_json::Value,
        cap: usize,
    ) -> Option<StashOutcome> {
        let body = serde_json::to_vec(value).unwrap_or_default();
        if body.len() <= cap {
            return None;
        }
        let handoff_ref = self.put(task_id, &body);
        let placeholder = build_handoff_placeholder(value, &handoff_ref, body.len());
        Some(StashOutcome { placeholder, handoff_ref, byte_len: body.len() })
    }

    /// Serve a `fetch_handoff` request: `params = {handoff_ref, offset?, len?}`.
    /// `len` is clamped to [`MAX_FETCH_BYTES`]. The stashed body is serialized
    /// JSON (UTF-8); a slice split mid-char is rendered with
    /// `from_utf8_lossy`, so `data` is always a string and `encoding` is
    /// always `"utf8"`.
    /// In the returned JSON, `len` is the count of bytes actually returned
    /// (≤ the requested `len`).
    pub fn fetch(&self, task_id: i64, params: &serde_json::Value) -> FetchResult {
        let Some(ref_str) = params.get("handoff_ref").and_then(|v| v.as_str()) else {
            return FetchResult::InvalidParams("missing 'handoff_ref'".into());
        };
        let Some(r) = HandoffRef::parse(ref_str) else {
            return FetchResult::InvalidParams(format!("malformed handoff_ref: {ref_str}"));
        };
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let len = params
            .get("len")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(MAX_FETCH_BYTES)
            .min(MAX_FETCH_BYTES);
        match self.get_slice(task_id, &r, offset, len) {
            Some(slice) => {
                let data = String::from_utf8_lossy(&slice.bytes).into_owned();
                FetchResult::Ok(serde_json::json!({
                    "handoff_ref": r.as_str(),
                    "offset": offset,
                    "len": slice.bytes.len(),
                    "data": data,
                    "encoding": "utf8",
                    "eof": slice.eof,
                }))
            }
            None => FetchResult::NotFound(format!(
                "no stashed body for {} in this task (unknown or evicted)",
                r.as_str()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_of_is_stable_and_well_formed() {
        let r = HandoffRef::of(b"hello");
        assert!(r.as_str().starts_with("sha256:"));
        assert_eq!(r.as_str().len(), 7 + 64);
        // Deterministic: same bytes → same ref.
        assert_eq!(r, HandoffRef::of(b"hello"));
        assert_ne!(r, HandoffRef::of(b"world"));
    }

    #[test]
    fn ref_parse_accepts_canonical_and_rejects_junk() {
        let good = HandoffRef::of(b"x");
        assert_eq!(HandoffRef::parse(good.as_str()), Some(good));
        assert_eq!(HandoffRef::parse("sha256:abc"), None); // too short
        assert_eq!(HandoffRef::parse("nope"), None); // no prefix
        assert_eq!(HandoffRef::parse(&format!("sha256:{}", "Z".repeat(64))), None); // non-hex
        assert_eq!(HandoffRef::parse(&format!("sha256:{}", "A".repeat(64))), None); // uppercase rejected
    }

    #[test]
    fn put_then_get_slice_round_trips() {
        let cache = HandoffCache::new();
        let body = b"abcdefghij".to_vec();
        let r = cache.put(7, &body);
        let s = cache.get_slice(7, &r, 0, 100).expect("present");
        assert_eq!(s.bytes, body);
        assert!(s.eof);
    }

    #[test]
    fn get_slice_honours_offset_len_and_eof() {
        let cache = HandoffCache::new();
        let r = cache.put(1, b"0123456789");
        let mid = cache.get_slice(1, &r, 2, 3).unwrap();
        assert_eq!(mid.bytes, b"234");
        assert!(!mid.eof);
        let tail = cache.get_slice(1, &r, 8, 100).unwrap();
        assert_eq!(tail.bytes, b"89");
        assert!(tail.eof);
        // Offset past the end → empty + eof.
        let past = cache.get_slice(1, &r, 50, 10).unwrap();
        assert!(past.bytes.is_empty());
        assert!(past.eof);
    }

    #[test]
    fn identical_body_returns_same_ref_without_duplicate_storage() {
        let cache = HandoffCache::new();
        let r1 = cache.put(3, b"same");
        let r2 = cache.put(3, b"same");
        assert_eq!(r1, r2);
    }

    #[test]
    fn unknown_ref_or_task_is_none() {
        let cache = HandoffCache::new();
        let r = cache.put(1, b"present");
        assert!(cache.get_slice(2, &r, 0, 10).is_none()); // wrong task
        assert!(cache.get_slice(1, &HandoffRef::of(b"absent"), 0, 10).is_none());
    }

    #[test]
    fn per_task_budget_evicts_oldest_first() {
        let cache = HandoffCache::new();
        // Two bodies that together exceed the budget; the second eviction
        // round must drop the first-inserted one.
        let big = vec![b'a'; PER_TASK_BYTE_BUDGET - 1];
        let r_old = cache.put(9, &big);
        // Adding a second body pushes total over budget → r_old evicted.
        let r_new = cache.put(9, b"second body that tips us over the budget");
        assert!(cache.get_slice(9, &r_old, 0, 1).is_none(), "oldest must be evicted");
        assert!(cache.get_slice(9, &r_new, 0, 1).is_some(), "newest must survive");
    }

    #[test]
    fn body_larger_than_budget_is_still_stored() {
        let cache = HandoffCache::new();
        let huge = vec![b'z'; PER_TASK_BYTE_BUDGET + 10];
        let r = cache.put(4, &huge);
        let s = cache.get_slice(4, &r, 0, 16).expect("stored despite exceeding budget");
        assert_eq!(s.bytes.len(), 16);
    }

    #[test]
    fn purge_task_removes_only_that_task() {
        let cache = HandoffCache::new();
        let ra = cache.put(1, b"task-1 body");
        let rb = cache.put(2, b"task-2 body");
        cache.purge_task(1);
        assert!(cache.get_slice(1, &ra, 0, 4).is_none(), "purged task gone");
        assert!(cache.get_slice(2, &rb, 0, 4).is_some(), "other task intact");
    }

    #[test]
    fn placeholder_has_ref_len_head_and_truncated_flag() {
        let value = serde_json::json!({"text": "the quick brown fox jumps over the lazy dog"});
        let r = HandoffRef::of(b"whatever");
        let p = build_handoff_placeholder(&value, &r, 123_456);
        assert_eq!(p["handoff_ref"], r.as_str());
        assert_eq!(p["byte_len"], 123_456);
        assert_eq!(p["truncated"], true);
        assert!(p["summary_head"].as_str().unwrap().contains("quick brown fox"));
    }

    #[test]
    fn stash_if_oversized_passes_through_under_cap() {
        let cache = HandoffCache::new();
        let small = serde_json::json!({"ok": true});
        assert!(cache.stash_if_oversized(1, &small, DEFAULT_RESULT_BYTE_CAP).is_none());
    }

    #[test]
    fn stash_if_oversized_stashes_over_cap() {
        let cache = HandoffCache::new();
        let big = serde_json::json!({"blob": "x".repeat(DEFAULT_RESULT_BYTE_CAP + 10)});
        let out = cache.stash_if_oversized(2, &big, DEFAULT_RESULT_BYTE_CAP).expect("stashed");
        assert!(out.byte_len > DEFAULT_RESULT_BYTE_CAP);
        // The body is retrievable by the ref the placeholder advertises.
        let r = HandoffRef::parse(out.placeholder["handoff_ref"].as_str().unwrap()).unwrap();
        assert_eq!(r, out.handoff_ref);
        assert!(cache.get_slice(2, &r, 0, 8).is_some());
    }

    #[test]
    fn fetch_returns_utf8_slice_with_eof() {
        let cache = HandoffCache::new();
        let value = serde_json::json!({"k": "v".repeat(100)});
        let out = cache.stash_if_oversized(5, &value, 8).expect("stashed (cap=8)");
        let params = serde_json::json!({"handoff_ref": out.handoff_ref.as_str(), "offset": 0, "len": 1_000_000});
        match cache.fetch(5, &params) {
            FetchResult::Ok(v) => {
                assert_eq!(v["encoding"], "utf8");
                assert_eq!(v["eof"], true);
                assert!(v["data"].as_str().unwrap().contains("vvv"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn fetch_clamps_len_to_max() {
        let cache = HandoffCache::new();
        let value = serde_json::json!({"k": "y".repeat(MAX_FETCH_BYTES * 2)});
        let out = cache.stash_if_oversized(6, &value, 8).unwrap();
        let params = serde_json::json!({"handoff_ref": out.handoff_ref.as_str(), "offset": 0, "len": u64::MAX});
        match cache.fetch(6, &params) {
            FetchResult::Ok(v) => {
                assert_eq!(v["len"].as_u64().unwrap() as usize, MAX_FETCH_BYTES);
                assert_eq!(v["eof"], false);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn fetch_unknown_ref_is_not_found() {
        let cache = HandoffCache::new();
        let params = serde_json::json!({"handoff_ref": HandoffRef::of(b"absent").as_str()});
        assert!(matches!(cache.fetch(1, &params), FetchResult::NotFound(_)));
    }

    #[test]
    fn fetch_malformed_params_is_invalid() {
        let cache = HandoffCache::new();
        assert!(matches!(cache.fetch(1, &serde_json::json!({})), FetchResult::InvalidParams(_)));
        assert!(matches!(
            cache.fetch(1, &serde_json::json!({"handoff_ref": "bogus"})),
            FetchResult::InvalidParams(_)
        ));
    }

    #[test]
    fn fetch_cannot_cross_task_boundary() {
        let cache = HandoffCache::new();
        let value = serde_json::json!({"k": "v".repeat(100)});
        let out = cache.stash_if_oversized(10, &value, 8).expect("stashed under task 10");
        // Task 11 supplies task 10's ref — must NOT resolve.
        let params = serde_json::json!({"handoff_ref": out.handoff_ref.as_str()});
        assert!(matches!(cache.fetch(11, &params), FetchResult::NotFound(_)));
        // Sanity: the owning task still resolves it.
        assert!(matches!(cache.fetch(10, &params), FetchResult::Ok(_)));
    }

    #[test]
    fn stash_if_oversized_passes_through_at_exactly_cap() {
        let cache = HandoffCache::new();
        // Serialize a value, measure it, and use its exact length as the cap:
        // a body whose length == cap must pass through (None), not stash.
        let value = serde_json::json!({"k": "z".repeat(50)});
        let exact = serde_json::to_vec(&value).unwrap().len();
        assert!(cache.stash_if_oversized(1, &value, exact).is_none(), "== cap must pass through");
        assert!(cache.stash_if_oversized(1, &value, exact - 1).is_some(), "cap-1 must stash");
    }

    #[test]
    fn global_backstop_evicts_oldest_task_past_cap() {
        let cache = HandoffCache::new();
        // Insert MAX_TRACKED_TASKS + 1 distinct tasks with a small body each.
        for t in 1..=(MAX_TRACKED_TASKS as i64 + 1) {
            cache.put(t, format!("body-{t}").as_bytes());
        }
        // The oldest task (1) was evicted wholesale by the backstop...
        let r_old = HandoffRef::of(b"body-1");
        assert!(cache.get_slice(1, &r_old, 0, 1).is_none(), "oldest task evicted by backstop");
        // ...while the newest survives.
        let last = MAX_TRACKED_TASKS as i64 + 1;
        let r_new = HandoffRef::of(format!("body-{last}").as_bytes());
        assert!(cache.get_slice(last, &r_new, 0, 1).is_some(), "newest task retained");
    }
}
