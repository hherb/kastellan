# Large-tool-result Handoff Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cap what a single tool result injects into the planner's context: oversized worker results are stashed in an in-memory per-task cache and replaced with a small placeholder; the planner pulls slices back on demand via a `fetch_handoff` built-in.

**Architecture:** A new pure `core/src/handoff.rs` module holds `HandoffRef`, `HandoffCache` (in-memory, per-task, content-addressed), and the pure stash/fetch/placeholder helpers. The cap/stash/intercept wiring lives in the dispatcher layer (`ToolHostStepDispatcher::dispatch_step`), *after* the sealed `tool_host::dispatch` returns — the chokepoint is untouched. `task_id` is threaded through the `StepDispatcher` trait so the cache keys per task; entries are purged at task terminal.

**Tech Stack:** Rust, tokio, `serde_json`, `sha2` (already a dep), `sqlx` (audit rows). No new dependencies. Spec: `docs/superpowers/specs/2026-06-08-handoff-cache-design.md`.

**Build/test prelude (run once per shell):**
```sh
source "$HOME/.cargo/env"
```
All `cargo` commands below assume this has been sourced. The dispatcher/handoff
unit tests are PG-free and run on macOS skip-as-pass.

---

## File Structure

- **Create** `core/src/handoff.rs` — `HandoffRef`, `HandoffCache`, `Slice`, `StashOutcome`, `FetchResult`, `build_handoff_placeholder`, consts. Pure + in-memory; no PG, no sandbox. (If it grows past ~500 LOC, lift `#[cfg(test)] mod tests` to a `handoff/tests.rs` sibling — see Task 9.)
- **Modify** `core/src/lib.rs` — add `pub mod handoff;`.
- **Modify** `core/src/scheduler/inner_loop.rs` — `StepDispatcher::dispatch_step` gains `task_id: i64`; new `purge_task` default method; pass `ctx.task_id` at the call site.
- **Modify** `core/src/scheduler/runner.rs` — purge the task's cache after `run_to_terminal` returns.
- **Modify** `core/src/scheduler/tool_dispatch.rs` — `HANDOFF_TOOL`/method consts; `ToolHostStepDispatcher` gains a `handoff: Arc<HandoffCache>` field + `new` param; `fetch_handoff` intercept; oversized-stash path; `purge_task` override.
- **Modify** `core/src/registry_build.rs` — skip any manifest claiming the reserved `"handoff"` name.
- **Modify** `core/src/main.rs` — construct + share one `HandoffCache`.
- **Modify** test doubles implementing `StepDispatcher` (signature) and `ToolHostStepDispatcher::new` call sites (extra arg): `core/src/memory/l3_invoke/tests.rs`, `core/tests/memory_l3_crystallise_e2e.rs`, `core/tests/scheduler_lanes_e2e.rs`, `core/tests/scheduler_inner_loop_e2e.rs` (2 impls), `core/tests/scheduler_step_dispatch_e2e.rs`, `core/tests/cli_memory_l3_run_e2e.rs`.

---

## Task 1: `HandoffRef` + `HandoffCache` core (put / get_slice)

**Files:**
- Create: `core/src/handoff.rs`
- Modify: `core/src/lib.rs:32` (add `pub mod handoff;` after `pub mod workspace;`)
- Test: inline `#[cfg(test)] mod tests` in `core/src/handoff.rs`

- [ ] **Step 1: Create the module with consts, types, and the round-trip API (no tests yet)**

Create `core/src/handoff.rs`:

```rust
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
    order: Vec<HandoffRef>,
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
        bucket.order.push(r.clone());
        bucket.total += body.len();
        // Evict oldest-first, but never the entry we just inserted (it is last
        // in `order`, and the loop stops once a single entry remains).
        while bucket.total > PER_TASK_BYTE_BUDGET && bucket.order.len() > 1 {
            let oldest = bucket.order.remove(0);
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
```

Add to `core/src/lib.rs` after line 32 (`pub mod workspace;`):

```rust
pub mod handoff;
```

- [ ] **Step 2: Write the failing tests**

Append to `core/src/handoff.rs`:

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail (module not yet wired) then pass**

Run: `cargo test -p kastellan-core handoff:: 2>&1 | tail -20`
Expected: compiles after Step 1's lib.rs edit; the six tests PASS.

- [ ] **Step 4: Clippy**

Run: `cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5`
Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add core/src/handoff.rs core/src/lib.rs
git commit -m "feat(handoff): HandoffRef + in-memory per-task HandoffCache (put/get_slice)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: per-task budget eviction + `purge_task`

**Files:**
- Modify: `core/src/handoff.rs` (tests only — production already implements both)
- Test: inline `mod tests`

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `core/src/handoff.rs`:

```rust
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
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p kastellan-core handoff:: 2>&1 | tail -20`
Expected: PASS (production from Task 1 already satisfies these).

- [ ] **Step 3: Commit**

```bash
git add core/src/handoff.rs
git commit -m "test(handoff): per-task budget eviction + purge_task isolation

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: placeholder builder + `stash_if_oversized` + `fetch`

**Files:**
- Modify: `core/src/handoff.rs`
- Test: inline `mod tests`

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `core/src/handoff.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail (helpers undefined)**

Run: `cargo test -p kastellan-core handoff:: 2>&1 | tail -20`
Expected: FAIL — `build_handoff_placeholder`, `stash_if_oversized`, `fetch`, `StashOutcome`, `FetchResult` not found.

- [ ] **Step 3: Implement the helpers**

In `core/src/handoff.rs`, add these `use`s near the top (after the existing `use`s):

```rust
use crate::cassandra::injection_guard::extract_scannable_text;
```

Add the placeholder builder (free function, after the `impl HandoffRef` block):

```rust
/// Build the placeholder the planner sees in place of a stashed oversized
/// result. `summary_head` is the readable head of the result (char-boundary
/// safe, via the injection-guard text extractor), so the planner often needs
/// no fetch at all.
pub fn build_handoff_placeholder(
    value: &serde_json::Value,
    r: &HandoffRef,
    byte_len: usize,
) -> serde_json::Value {
    let (head, _truncated) = extract_scannable_text(value, SUMMARY_HEAD_BYTES);
    serde_json::json!({
        "handoff_ref": r.as_str(),
        "byte_len": byte_len,
        "summary_head": head,
        "truncated": true,
    })
}
```

Add the two result types (after `Slice`):

```rust
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
```

Add the two methods inside `impl HandoffCache` (after `purge_task`):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p kastellan-core handoff:: 2>&1 | tail -20`
Expected: all handoff tests PASS.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/handoff.rs
git commit -m "feat(handoff): placeholder builder + stash_if_oversized + fetch helpers

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: thread `task_id` through `StepDispatcher` + `purge_task` + terminal purge

This is one atomic, mechanical change: the trait signature changes, so the
production impl, all six test doubles, the call site, and the runner purge all
land in a single compiling commit.

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs:206` (trait), `:214` area (add default method), `:514` (call site)
- Modify: `core/src/scheduler/runner.rs:535` (purge after terminal)
- Modify: `core/src/scheduler/tool_dispatch.rs:291` (production impl signature only — body stays)
- Modify doubles: `core/src/memory/l3_invoke/tests.rs:206`, `core/tests/memory_l3_crystallise_e2e.rs:152`, `core/tests/scheduler_lanes_e2e.rs:136`, `core/tests/scheduler_inner_loop_e2e.rs:135` and `:497`

- [ ] **Step 1: Change the trait + add `purge_task` default**

In `core/src/scheduler/inner_loop.rs`, replace the trait body (currently lines ~205-217):

```rust
#[async_trait::async_trait]
pub trait StepDispatcher: Send + Sync {
    async fn dispatch_step(&self, task_id: i64, step: &PlannedStep) -> StepOutcome;

    /// Live tool-name set this dispatcher can reach. Used by the agent
    /// L3-invoke path to re-validate a skill against the registry as it is
    /// *now* (the TOCTOU close). Default: empty — only the production
    /// [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`] holds a
    /// registry; non-loop / test doubles that never expand an invoke can
    /// keep the empty default.
    fn known_tools(&self) -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    /// Drop any per-task state this dispatcher holds (e.g. the handoff
    /// cache) once the task reaches a terminal state. Default no-op; the
    /// production dispatcher overrides it. Called once per task by the lane
    /// runner after [`run_to_terminal`].
    fn purge_task(&self, _task_id: i64) {}
}
```

- [ ] **Step 2: Update the call site**

In `core/src/scheduler/inner_loop.rs:514`, change:

```rust
            let outcome = dispatcher.dispatch_step(step).await;
```
to:
```rust
            let outcome = dispatcher.dispatch_step(ctx.task_id, step).await;
```

- [ ] **Step 3: Update the production impl signature**

In `core/src/scheduler/tool_dispatch.rs:291`, change:

```rust
    async fn dispatch_step(&self, step: &PlannedStep) -> StepOutcome {
```
to:
```rust
    async fn dispatch_step(&self, task_id: i64, step: &PlannedStep) -> StepOutcome {
```
(The body is unchanged in this task; `task_id` is unused for now — add `let _ = task_id;` as the first line so it compiles clean under `-D warnings`. Task 6/7 will use it and remove the discard.)

- [ ] **Step 4: Add the terminal purge in the runner**

In `core/src/scheduler/runner.rs`, replace the `match run_to_terminal(...)` block (around line 535) with:

```rust
    let task_id = ctx.task_id;
    let dispatcher_for_purge = std::sync::Arc::clone(&dispatcher);
    let result = match run_to_terminal(pool, formulator, review, dispatcher, ctx).await {
        Ok(r) => r,
        Err(e) => failed_result(format!("inner_loop: {e}")),
    };
    dispatcher_for_purge.purge_task(task_id);
    result
```

(`dispatcher` is already `Arc<dyn StepDispatcher>` here; `ctx` is moved into
`run_to_terminal`, so `task_id` is bound before the move.)

- [ ] **Step 5: Update the six test doubles**

Each is a one-line signature change adding `_task_id: i64` as the first arg.

`core/src/memory/l3_invoke/tests.rs:206`:
```rust
    async fn dispatch_step(&self, _task_id: i64, step: &PS) -> StepOutcome {
```
`core/tests/memory_l3_crystallise_e2e.rs:152`:
```rust
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
```
`core/tests/scheduler_lanes_e2e.rs:136`:
```rust
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
```
`core/tests/scheduler_inner_loop_e2e.rs:135` (ScriptedDispatcher):
```rust
    async fn dispatch_step(&self, _task_id: i64, step: &PlannedStep) -> StepOutcome {
```
`core/tests/scheduler_inner_loop_e2e.rs:497` (BarrierDispatcher):
```rust
    async fn dispatch_step(&self, _task_id: i64, _step: &PlannedStep) -> StepOutcome {
```

(If a double already binds the second param as `step` and uses it, keep that
name; only prepend `_task_id: i64`.)

- [ ] **Step 6: Build + test**

Run: `cargo build -p kastellan-core --all-targets 2>&1 | tail -5`
Expected: compiles clean.
Run: `cargo test -p kastellan-core --lib 2>&1 | tail -10`
Expected: PASS (no behaviour change; signatures only).

- [ ] **Step 7: Clippy + commit**

```bash
cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/scheduler/inner_loop.rs core/src/scheduler/runner.rs \
        core/src/scheduler/tool_dispatch.rs core/src/memory/l3_invoke/tests.rs \
        core/tests/memory_l3_crystallise_e2e.rs core/tests/scheduler_lanes_e2e.rs \
        core/tests/scheduler_inner_loop_e2e.rs
git commit -m "refactor(scheduler): thread task_id through StepDispatcher + purge_task hook

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: wire `HandoffCache` into the dispatcher + stash oversized results

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs` (field, `new`, consts, stash path, `purge_task` override)
- Modify: `core/src/main.rs:293` (construct + pass the cache)
- Modify: `core/tests/scheduler_step_dispatch_e2e.rs:196`, `core/tests/cli_memory_l3_run_e2e.rs:142` (extra `new` arg)
- Test: `core/src/scheduler/tool_dispatch/tests.rs` (pure helper assertions — the stash decision is tested via `HandoffCache`, already covered in Task 3; here we add a dispatcher-construction smoke + a reserved-name test is in Task 7)

- [ ] **Step 1: Add consts + the cache field + `new` param + `purge_task` override**

In `core/src/scheduler/tool_dispatch.rs`, add near the other `const`s (after `ACTION_STEP_SPAWN_FAILED`):

```rust
/// Reserved built-in tool name intercepted before registry lookup; no worker
/// manifest may claim it (enforced in `registry_build::assemble_registry`).
pub const HANDOFF_TOOL: &str = "handoff";
/// Method on [`HANDOFF_TOOL`] that returns a slice of a stashed body.
pub const HANDOFF_METHOD_FETCH: &str = "fetch";
/// `action` for the audit row written when an oversized result is stashed.
const ACTION_HANDOFF_STASHED: &str = "handoff.stashed";
```

(`HANDOFF_TOOL`/`HANDOFF_METHOD_FETCH` are `pub` so they don't trip
`dead_code` while only Task 6/7 consume them. The `handoff.fetched` action
const is added in Task 6, where it's first used, to keep this task's
`-D warnings` clippy clean.)

Add `use crate::handoff::{HandoffCache, DEFAULT_RESULT_BYTE_CAP};` to the imports.

Change the struct (around line 267) to add the field:

```rust
pub struct ToolHostStepDispatcher {
    pool: PgPool,
    vault: Arc<Vault>,
    lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
    registry: Arc<ToolRegistry>,
    handoff: Arc<HandoffCache>,
}
```

Change `new` to take and store it:

```rust
    pub fn new(
        pool: PgPool,
        vault: Arc<Vault>,
        lifecycle: Arc<dyn crate::worker_lifecycle::WorkerLifecycleManager>,
        registry: Arc<ToolRegistry>,
        handoff: Arc<HandoffCache>,
    ) -> Self {
        Self { pool, vault, lifecycle, registry, handoff }
    }
```

Add the `purge_task` override inside `impl StepDispatcher for ToolHostStepDispatcher` (next to `known_tools`):

```rust
    fn purge_task(&self, task_id: i64) {
        self.handoff.purge_task(task_id);
    }
```

- [ ] **Step 2: Replace the `let _ = task_id;` discard with the stash path**

In `dispatch_step` (production impl), remove the `let _ = task_id;` line added in Task 4. Then replace the final two lines of the method:

```rust
        drop(handle);

        map_dispatch_result(result)
```
with:

```rust
        drop(handle);

        // Cap what reaches the planner: an oversized Ok result is stashed in
        // the per-task handoff cache and replaced with a small placeholder.
        // (Errors and the small injection-blocked placeholder pass through
        // untouched — blocked content is never stashed, so never retrievable.)
        //
        // Sentinel: `task_id <= 0` means "no task-scoped handoff" — the
        // operator `memory l3 run` path (l3_invoke::run_steps) passes 0 and
        // feeds a human with no fetch_handoff retrieval loop, so stashing there
        // would only hide content. Real scheduler tasks are bigserial ids ≥ 1,
        // so this never collides with a planner task. Such calls pass through
        // verbatim.
        let result = match result {
            Ok(v) if task_id > 0 => match self.handoff.stash_if_oversized(task_id, &v, DEFAULT_RESULT_BYTE_CAP) {
                Some(stash) => {
                    let payload = serde_json::json!({
                        "tool": step.tool,
                        "method": step.method,
                        "handoff_ref": stash.handoff_ref.as_str(),
                        "byte_len": stash.byte_len,
                    });
                    if let Err(e) = kastellan_db::audit::insert(
                        &self.pool, "policy", ACTION_HANDOFF_STASHED, payload,
                    ).await {
                        tracing::error!(
                            tool = %step.tool, method = %step.method, error = %e,
                            "handoff.stashed audit insert failed; placeholder still returned"
                        );
                    }
                    Ok(stash.placeholder)
                }
                None => Ok(v),
            },
            // Errors, the injection-blocked placeholder, and (task_id <= 0)
            // operator-path results all pass through unchanged.
            passthrough => passthrough,
        };

        map_dispatch_result(result)
```

Note: the `Ok(v) if task_id > 0` guard means the catch-all also matches `Ok(v)` when `task_id <= 0`, returning it verbatim — that is the operator-path passthrough.

- [ ] **Step 3: Update `main.rs` construction**

In `core/src/main.rs`, just before the `ToolHostStepDispatcher::new(` at line ~293, construct the shared cache (place it next to where the registry `Arc` is built; one instance for the daemon's lifetime):

```rust
            let handoff_cache = std::sync::Arc::new(kastellan_core::handoff::HandoffCache::new());
```

and add it as the final argument to `ToolHostStepDispatcher::new(`:

```rust
            kastellan_core::scheduler::tool_dispatch::ToolHostStepDispatcher::new(
                pool.clone(),
                vault.clone(),
                lifecycle.clone(),
                registry.clone(),
                handoff_cache,
            )
```
(Match the exact argument expressions already present for the first four args; only append `handoff_cache`.)

- [ ] **Step 4: Update the two test `new` call sites**

`core/tests/scheduler_step_dispatch_e2e.rs:196` — add the cache arg:
```rust
        let dispatcher = ToolHostStepDispatcher::new(
            pool.clone(),
            vault.clone(),
            lifecycle,
            registry,
            std::sync::Arc::new(kastellan_core::handoff::HandoffCache::new()),
        );
```
(Keep the first four arguments exactly as they already appear in that file; only append the cache arg. Add a `use` or fully-qualify as shown.)

`core/tests/cli_memory_l3_run_e2e.rs:142`:
```rust
    ToolHostStepDispatcher::new(
        pool,
        vault,
        lifecycle,
        registry,
        std::sync::Arc::new(kastellan_core::handoff::HandoffCache::new()),
    )
```

- [ ] **Step 5: Build + test**

Run: `cargo build -p kastellan-core --all-targets 2>&1 | tail -5`
Expected: compiles.
Run: `cargo test -p kastellan-core --lib 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 6: Clippy + commit**

```bash
cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/scheduler/tool_dispatch.rs core/src/main.rs \
        core/tests/scheduler_step_dispatch_e2e.rs core/tests/cli_memory_l3_run_e2e.rs
git commit -m "feat(handoff): stash oversized tool results in the dispatcher + audit row

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `fetch_handoff` intercept in `dispatch_step`

**Files:**
- Modify: `core/src/scheduler/tool_dispatch.rs` (intercept at top of `dispatch_step`)

- [ ] **Step 1: Add the `FetchResult` import + the fetched-action const**

In `core/src/scheduler/tool_dispatch.rs`, extend the handoff import added in
Task 5 to include `FetchResult`:

```rust
use crate::handoff::{FetchResult, HandoffCache, DEFAULT_RESULT_BYTE_CAP};
```

and add the action const next to `ACTION_HANDOFF_STASHED`:

```rust
/// `action` for the audit row written on a `fetch_handoff` call.
const ACTION_HANDOFF_FETCHED: &str = "handoff.fetched";
```

- [ ] **Step 2: Add the intercept as the first thing in `dispatch_step`**

In `core/src/scheduler/tool_dispatch.rs`, at the very top of `dispatch_step` (immediately after `let started = Instant::now();`), insert:

```rust
        // Reserved built-in: serve a slice of a stashed body from the per-task
        // handoff cache. Intercepted *before* the registry lookup, so no worker
        // spawns and the sandbox is never entered. `"handoff"` is a reserved
        // name (registry assembly refuses any manifest claiming it).
        if step.tool == HANDOFF_TOOL && step.method == HANDOFF_METHOD_FETCH {
            let fetched = self.handoff.fetch(task_id, &step.parameters);
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let (outcome_label, step_outcome) = match fetched {
                FetchResult::Ok(v) => ("ok", StepOutcome::Ok(v)),
                FetchResult::NotFound(detail) => (
                    "not_found",
                    StepOutcome::Err { code: "HANDOFF_NOT_FOUND".into(), detail },
                ),
                FetchResult::InvalidParams(detail) => (
                    "invalid_params",
                    StepOutcome::Err { code: "INVALID_PARAMS".into(), detail },
                ),
            };
            let payload = serde_json::json!({
                "handoff_ref": step.parameters.get("handoff_ref"),
                "offset": step.parameters.get("offset"),
                "len": step.parameters.get("len"),
                "outcome": outcome_label,
                "ms": elapsed_ms,
            });
            if let Err(e) = kastellan_db::audit::insert(
                &self.pool, "policy", ACTION_HANDOFF_FETCHED, payload,
            ).await {
                tracing::error!(
                    error = %e,
                    "handoff.fetched audit insert failed; outcome still propagated"
                );
            }
            return step_outcome;
        }
```

- [ ] **Step 3: Build + test**

Run: `cargo build -p kastellan-core --all-targets 2>&1 | tail -5`
Expected: compiles.
Run: `cargo test -p kastellan-core --lib 2>&1 | tail -10`
Expected: PASS. (The `fetch` behaviour itself is unit-tested on `HandoffCache` in Task 3; this step wires it to the chokepoint.)

- [ ] **Step 4: Clippy + commit**

```bash
cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/scheduler/tool_dispatch.rs
git commit -m "feat(handoff): fetch_handoff built-in intercept + handoff.fetched audit

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: reserve the `"handoff"` name in registry assembly

**Files:**
- Modify: `core/src/registry_build.rs` (skip reserved name in `assemble_registry`)
- Test: `core/src/registry_build.rs` inline `mod tests`

- [ ] **Step 1: Write the failing test**

In `core/src/registry_build.rs` `mod tests`, add (the `FakeManifest`/`test_ctx` helpers already exist there):

```rust
    #[test]
    fn manifest_claiming_reserved_handoff_name_is_skipped() {
        let allow = |_t: &str| Vec::<String>::new();
        let ctx = test_ctx(&allow);
        let reserved = FakeManifest {
            name: "handoff",
            outcome: FakeOutcome::Register,
            allowlist_name: None,
        };
        let (reg, loaded) = assemble_registry(&[&reserved], &ctx);
        assert!(reg.lookup("handoff").is_none(), "reserved name must not register");
        assert!(loaded.is_empty(), "reserved name must not appear in loaded records");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kastellan-core registry_build:: 2>&1 | tail -10`
Expected: FAIL — the reserved manifest currently registers.

- [ ] **Step 3: Implement the guard**

In `core/src/registry_build.rs`, add the import and a guard at the top of the
`for m in manifests` loop in `assemble_registry`:

```rust
use crate::scheduler::tool_dispatch::HANDOFF_TOOL;
```

```rust
    for m in manifests {
        if m.name() == HANDOFF_TOOL {
            tracing::warn!(
                tool = m.name(),
                "worker manifest claims the reserved built-in name; skipping"
            );
            continue;
        }
        match m.resolve(ctx) {
            // ... unchanged ...
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p kastellan-core registry_build:: 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -5
git add core/src/registry_build.rs
git commit -m "feat(handoff): reserve \"handoff\" tool name in registry assembly

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: full verification + handover/roadmap update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`, `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace build + core tests + clippy**

Run:
```bash
cargo build --workspace 2>&1 | tail -3
cargo test -p kastellan-core --lib 2>&1 | tail -5
cargo clippy -p kastellan-core --all-targets --locked -- -D warnings 2>&1 | tail -3
```
Expected: build clean; lib tests PASS; clippy exit 0.

- [ ] **Step 2: Confirm `handoff.rs` LOC under the soft cap**

Run: `wc -l core/src/handoff.rs`
Expected: under 500. If over, lift `#[cfg(test)] mod tests` into a new sibling
`core/src/handoff/tests.rs` (de-indent one level, add `//!` header, replace the
inline block with `#[cfg(test)] mod tests;`) and re-run Step 1. Commit that lift
separately as `refactor(handoff): lift tests to sibling to stay under cap`.

- [ ] **Step 3: Tick ROADMAP:129**

In `docs/devel/ROADMAP.md`, change the `- [ ] **Large-tool-result handoff cache**` line (≈line 129) to `- [x]` and condense to a terse one-liner ending with the branch + date, e.g.:

```markdown
- [x] **Large-tool-result handoff cache** — in-memory per-task content-addressed `HandoffCache`; oversized Ok results (> 64 KiB) stashed in the dispatcher layer + replaced with a `{handoff_ref, byte_len, summary_head}` placeholder; reserved `handoff`/`fetch` built-in returns clamped slices; blocked outputs never stashed; per-task purge at terminal — branch `feat/handoff-cache`, 2026-06-08. Deferred: per-tool `result_byte_cap` override; on-disk Workspace-backed store; teaching the planner to call `fetch_handoff` (prompt-surface follow-up).
```

- [ ] **Step 4: Update HANDOVER.md** per its own end-of-session checklist (header `Last updated` + last commit + verification counts; new "Recently completed" section; refresh Next-TODO). Keep under 500 lines.

- [ ] **Step 5: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs: handoff cache shipped (ROADMAP:129)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 6: Push + open PR** to `main`, linking ROADMAP:129 and the spec, summarising the slice + the deferred follow-ups.

---

## Self-review notes (for the implementer)

- **Spec coverage:** §HandoffCache → Tasks 1–3; §cap/stash → Task 5; §fetch_handoff → Tasks 3 (logic) + 6 (wiring); §reserved name → Task 7; §lifecycle threading → Task 4; §security (blocked never stashed) → Task 5 Step 2 comment + the fact blocked outputs arrive as a small placeholder; §tunables → Task 1 consts.
- **Deferred per spec (do NOT implement here):** per-tool `result_byte_cap` field; on-disk store; PG-required e2e through the full chokepoint (the dispatcher-level behaviour is covered by the `HandoffCache` unit tests, which are PG-free); teaching the planner to *use* `fetch_handoff` (a prompt-assembly follow-up).
- **Type consistency:** `HandoffRef`, `Slice`, `StashOutcome`, `FetchResult`, `HandoffCache::{put,get_slice,purge_task,stash_if_oversized,fetch}`, `build_handoff_placeholder`, `HANDOFF_TOOL`/`HANDOFF_METHOD_FETCH`, `DEFAULT_RESULT_BYTE_CAP`/`SUMMARY_HEAD_BYTES`/`MAX_FETCH_BYTES`/`PER_TASK_BYTE_BUDGET` are used identically across tasks.
- **Always-green:** each task compiles and passes on its own commit; the trait signature change (Task 4) updates every impl in one commit.
