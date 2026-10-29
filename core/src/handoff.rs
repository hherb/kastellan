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

use crate::cassandra::injection_guard::{screened_head, SCAN_BYTE_CAP};

/// Result larger than this (bytes of its serialized JSON) is stashed and
/// replaced with a placeholder. 64 KiB ≈ 16k tokens — generous for one
/// document, below web-fetch's 100 KiB `MAX_TEXT_BYTES`.
pub const DEFAULT_RESULT_BYTE_CAP: usize = 64 * 1024;

/// Budget of the head carried inline in the placeholder (see
/// [`screened_head`] for what it charges) so the planner often needs no fetch
/// at all.
pub const SUMMARY_HEAD_BYTES: usize = 1024;

// The head's text lies inside the text the output screen read only while the
// head's budget is within the screen's window.
const _: () = assert!(SUMMARY_HEAD_BYTES <= SCAN_BYTE_CAP);

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
/// result. `summary_head` is the start of the result as JSON, keys, numbers
/// and booleans included ([`screened_head`] at [`SUMMARY_HEAD_BYTES`], #705),
/// so the planner often needs no fetch at all.
///
/// The head is deliberately **not** rendered here. The planner summary renders
/// every step result, this placeholder included, and a second render would
/// garble the first one's omission markers. Its text is a prefix of the text the
/// output screen read ([`screened_head`] builds it so). Handing the summary the
/// whole result instead would not be safe: its render trims strings evenly
/// across its input and would show text past the [`SCAN_BYTE_CAP`] window. The
/// placeholder's `truncated: true` means the
/// *body* was stashed (always true on this path), not that the head was cut.
pub fn build_handoff_placeholder(
    value: &serde_json::Value,
    r: &HandoffRef,
    byte_len: usize,
) -> serde_json::Value {
    let head = screened_head(value, SUMMARY_HEAD_BYTES);
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
                    // The backstop should never fire in normal operation
                    // (purge_task drops each bucket at task terminal, well
                    // before MAX_TRACKED_TASKS accumulates). If it does, the
                    // victim may be a still-active task whose planner will then
                    // see HANDOFF_NOT_FOUND — surface it so the assumption is
                    // auditable rather than silently wrong.
                    tracing::warn!(
                        evicted_task = victim,
                        tracked = MAX_TRACKED_TASKS,
                        "handoff cache global backstop evicted a task bucket \
                         wholesale; a missed purge_task or unexpectedly high \
                         concurrent-task count is implied"
                    );
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
mod tests;
