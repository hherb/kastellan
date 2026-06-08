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

/// A byte slice plus whether it reached the end of the stashed body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slice {
    pub bytes: Vec<u8>,
    pub eof: bool,
}

/// One task's stashed bodies, in insertion order for oldest-first eviction.
#[derive(Default)]
struct TaskBucket {
    order: VecDeque<HandoffRef>,
    map: HashMap<HandoffRef, Vec<u8>>,
    total: usize,
}

/// In-memory cache shared (behind `Arc`) by the production dispatcher.
#[derive(Default)]
pub struct HandoffCache {
    inner: Mutex<HashMap<i64, TaskBucket>>,
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
        let bucket = guard.entry(task_id).or_default();
        if bucket.map.contains_key(&r) {
            return r;
        }
        bucket.map.insert(r.clone(), body.to_vec());
        bucket.order.push_back(r.clone());
        bucket.total += body.len();
        // Evict oldest-first, but never the entry we just inserted (it is last
        // in `order`, and the loop stops once a single entry remains).
        while bucket.total > PER_TASK_BYTE_BUDGET && bucket.order.len() > 1 {
            let oldest = bucket.order.pop_front().expect("order non-empty while len > 1");
            if let Some(b) = bucket.map.remove(&oldest) {
                bucket.total -= b.len();
            }
        }
        r
    }

    /// Up to `len` bytes of the body starting at `offset`. `None` if the
    /// `(task_id, ref)` is unknown or was evicted. Callers clamp `len` to
    /// [`MAX_FETCH_BYTES`] before calling.
    pub fn get_slice(&self, task_id: i64, r: &HandoffRef, offset: usize, len: usize) -> Option<Slice> {
        let guard = self.inner.lock().expect("handoff cache mutex poisoned");
        let body = guard.get(&task_id)?.map.get(r)?;
        let start = offset.min(body.len());
        let end = offset.saturating_add(len).min(body.len());
        let bytes = body[start..end].to_vec();
        let eof = end >= body.len();
        Some(Slice { bytes, eof })
    }

    /// Drop every entry for `task_id`. Called at task terminal.
    pub fn purge_task(&self, task_id: i64) {
        self.inner.lock().expect("handoff cache mutex poisoned").remove(&task_id);
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
}
