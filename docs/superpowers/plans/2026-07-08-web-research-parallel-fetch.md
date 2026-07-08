# web-research Parallel Fetch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fetch `web.research`'s candidate pages concurrently instead of sequentially, cutting wall-clock from ≈ Σ(fetch times) toward ≈ max(single fetch), while keeping the `sources`/`unfetched` result byte-identical to the sequential version.

**Architecture:** Two-phase `research()`: a **parallel** fetch+extract+chunk phase over `std::thread::scope` OS threads (bounded waves), then a **sequential** classify+rank phase on the main thread that walks hits in rank order and reproduces the exact sequential output. Concurrency is confined to the pure network half so the embedder and ranking stay single-threaded.

**Tech Stack:** Rust (edition as workspace), `reqwest::blocking`, `tokio` (transport-internal), `std::thread::scope`, `url`, `serde_json`. Worker crate `kastellan-worker-web-research`; shared crate `kastellan-worker-web-common`.

## Global Constraints

- Toolchain: rustc **1.96.0**. Source cargo first in every shell: `source "$HOME/.cargo/env"`.
- AGPL-3.0 project; no new dependencies (all needed crates already present).
- Cross-platform (Linux + macOS). No OS-gated code in this change.
- `cargo clippy --all-targets -- -D warnings` must stay clean on touched crates.
- Keep files focused; inline docs must be understandable to a junior contributor.
- TDD: write the failing test (or observe the failing build) before implementing.
- Commit after each task's tests pass. Stage **specific files** (never `git add -A`).
- Work on branch `feat/web-research-parallel-fetch` (already created).
- The public signature of `research()` must NOT change (no handler/manifest/e2e-caller edits).

---

## File Structure

- `workers/web-common/src/http.rs` — add `Send + Sync` supertraits to `HttpGet`; `#[derive(Clone)]` on `RawResponse`. (Task 1)
- `workers/web-common/src/testing.rs` — `FakeGet` `RefCell`→`Mutex`; new `KeyedFakeGet`. (Tasks 1, 3)
- `workers/web-common/src/proxy_connect.rs` — current-thread → multi-thread tokio runtime (two build sites). (Task 2)
- `workers/web-common/src/proxy_connect/tests.rs` — concurrency test + multi-accept stub proxy. (Task 2)
- `workers/web-research/src/research.rs` — split `gather_source`; add `MAX_CONCURRENT_FETCHES`, `FetchedPage`, `fetch_and_chunk`, `rank_fetched_page`, `hit_allowed`, `fetch_candidates`; rewire `research()` to two phases; migrate + add tests. (Tasks 4, 5)
- `core/src/workers/web_research.rs` — manifest doc block: sequential-fetch caveat → bounded-parallel. (Task 6)

---

### Task 1: Thread-safe transport seam (`HttpGet: Send + Sync`, `RawResponse: Clone`, `FakeGet` → `Mutex`)

**Files:**
- Modify: `workers/web-common/src/http.rs:19` (RawResponse derive), `:28` (trait decl)
- Modify: `workers/web-common/src/testing.rs:5,14-44` (FakeGet)

**Interfaces:**
- Produces: `trait HttpGet: Send + Sync`; `#[derive(Debug, Clone)] struct RawResponse`; `FakeGet` now `Send + Sync` (Mutex-backed, unchanged API `FakeGet::new(Vec<RawResponse>)`).

- [ ] **Step 1: Add the supertraits and `Clone`, observe the build break**

In `workers/web-common/src/http.rs`, change the `RawResponse` derive:

```rust
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}
```

and the trait declaration:

```rust
/// The transport seam. One GET, no redirect following.
///
/// `Send + Sync` so a single transport can be shared by reference across the
/// scoped fetch threads in `web-research`'s parallel fetch phase. Every concrete
/// impl (reqwest / proxy-connect) is already thread-safe; test doubles must be
/// too (see `FakeGet`).
pub trait HttpGet: Send + Sync {
```

- [ ] **Step 2: Run the build to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-web-common`
Expected: FAIL — `RefCell<VecDeque<RawResponse>>` cannot be shared between threads safely; `impl HttpGet for FakeGet` requires `FakeGet: Sync`.

- [ ] **Step 3: Make `FakeGet` `Sync` via `Mutex`**

In `workers/web-common/src/testing.rs`, replace the `RefCell` import and the struct/impl:

```rust
use std::collections::VecDeque;
use std::sync::Mutex;

use url::Url;

use crate::allowlist::HostAllowlist;
use crate::http::{HttpGet, RawResponse};

/// Fake transport returning canned responses in FIFO order. `Mutex`-backed so it
/// is `Sync` (the `HttpGet` seam now requires it); FIFO order is fine for
/// single-fetch tests — use `KeyedFakeGet` when a test issues concurrent fetches.
pub struct FakeGet {
    responses: Mutex<VecDeque<RawResponse>>,
}

impl FakeGet {
    pub fn new(responses: Vec<RawResponse>) -> Self {
        Self { responses: Mutex::new(responses.into_iter().collect()) }
    }
}

impl HttpGet for FakeGet {
    fn get(&self, _url: &Url) -> Result<RawResponse, String> {
        self.responses
            .lock()
            .expect("FakeGet mutex poisoned")
            .pop_front()
            .ok_or_else(|| "no more canned responses".to_string())
    }

    fn transport_kind(&self) -> &'static str {
        "fake"
    }

    fn post(&self, _url: &Url, _content_type: &str, _body: &[u8])
        -> Result<RawResponse, String>
    {
        self.responses
            .lock()
            .expect("FakeGet mutex poisoned")
            .pop_front()
            .ok_or_else(|| "no more canned responses".to_string())
    }
}
```

(Delete the old `use std::cell::RefCell;` line.)

- [ ] **Step 4: Add a Send+Sync compile assertion test**

Append to `workers/web-common/src/testing.rs` (inside the existing `#[cfg(test)] mod post_fake_tests`, or a new test module):

```rust
#[cfg(test)]
mod send_sync_tests {
    use crate::http::HttpGet;

    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn transport_seam_is_thread_shareable() {
        _assert_send_sync::<super::FakeGet>();
        _assert_send_sync::<Box<dyn HttpGet>>();
    }
}
```

- [ ] **Step 5: Run web-common tests + clippy**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common && cargo clippy -p kastellan-worker-web-common --all-targets -- -D warnings`
Expected: PASS, clippy clean. (The whole workspace must still build: `cargo build --workspace`.)

- [ ] **Step 6: Commit**

```bash
git add workers/web-common/src/http.rs workers/web-common/src/testing.rs
git commit -m "feat(web-common): HttpGet: Send + Sync; RawResponse Clone; FakeGet Mutex-backed

Prepares the transport seam to be shared by reference across scoped fetch
threads. FakeGet moves RefCell->Mutex to satisfy the new Sync bound (FIFO
order unchanged). RawResponse gains Clone for the upcoming KeyedFakeGet.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `ProxyConnectGet` multi-thread runtime

**Files:**
- Modify: `workers/web-common/src/proxy_connect.rs:79-82` and `:120-123` (both runtime builders)
- Modify: `workers/web-common/src/proxy_connect/tests.rs` (add multi-accept stub + concurrency test)

**Interfaces:**
- Consumes: nothing new.
- Produces: `ProxyConnectGet` now safe for concurrent `get()`/`post()` from multiple threads.

**Note on test ordering:** this task is verified **green-after-change**, not strict red-first. A *current-thread* runtime `block_on`'d concurrently can **hang** (deadlock) rather than cleanly fail, which would stall the test run — so we switch the runtime first, then add the concurrency test that would regress (hang/fail) if someone reverts to `new_current_thread`.

- [ ] **Step 1: Switch both runtime builders to multi-thread**

In `workers/web-common/src/proxy_connect.rs`, in `with_trust` (around line 79) and `with_extra_ca` (around line 120), replace each:

```rust
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
```

with:

```rust
        // Multi-thread (not current-thread): `web-research`'s parallel fetch phase
        // calls `get()`/`post()` on ONE shared transport from several scoped threads
        // at once, so `self.rt.block_on(..)` runs concurrently. A current-thread
        // runtime serialises (or deadlocks) under concurrent block_on; a multi-thread
        // runtime services them via its shared I/O driver. `worker_threads(4)` is the
        // shared driver pool — a separate knob from the fetch concurrency cap; each
        // concurrent block_on drives its own future on its calling thread. Workers
        // that issue one request at a time (web-fetch/web-search) are unaffected.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("multi-thread runtime");
```

- [ ] **Step 2: Build + existing proxy_connect tests stay green**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common proxy_connect`
Expected: PASS (round-trip, EOF, 403, CA-trust tests all still pass with the new runtime).

- [ ] **Step 3: Add a multi-accept stub proxy + concurrency test**

Append to `workers/web-common/src/proxy_connect/tests.rs`:

```rust
/// Like `spawn_stub_proxy` but serves `n` sequential connections, each in its own
/// thread so multiple clients are handled concurrently. Every connection gets the
/// same `origin_response` after the CONNECT handshake.
fn spawn_stub_proxy_multi(
    path: std::path::PathBuf,
    origin_response: &'static [u8],
    n: usize,
) {
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        for _ in 0..n {
            let (mut conn, _) = listener.accept().unwrap();
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let mut acc = Vec::new();
                loop {
                    let n = conn.read(&mut buf).unwrap();
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 {
                        break;
                    }
                }
                conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
                let mut req = [0u8; 1024];
                let _ = conn.read(&mut req).unwrap();
                conn.write_all(origin_response).unwrap();
            });
        }
    });
}

#[test]
fn concurrent_gets_share_one_transport() {
    // One ProxyConnectGet (one multi-thread runtime) driven by several threads at
    // once. Guards the runtime-flavour change: a current-thread runtime would
    // serialise/deadlock here.
    let dir = std::env::temp_dir().join(format!("kastellan-pc-conc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("egress.sock");
    let _ = std::fs::remove_file(&uds);
    let n = 4;
    spawn_stub_proxy_multi(
        uds.clone(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        n,
    );

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/search").unwrap();

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n)
            .map(|_| scope.spawn(|| get.get(&url).map(|r| r.status)))
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap().expect("round trip"), 200);
        }
    });
    let _ = std::fs::remove_file(&uds);
}
```

- [ ] **Step 4: Run the concurrency test + clippy**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common proxy_connect && cargo clippy -p kastellan-worker-web-common --all-targets -- -D warnings`
Expected: PASS (all 4 concurrent gets return 200), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add workers/web-common/src/proxy_connect.rs workers/web-common/src/proxy_connect/tests.rs
git commit -m "feat(web-common): ProxyConnectGet multi-thread runtime for concurrent block_on

web-research fetches pages concurrently over one shared transport; a
current-thread tokio runtime serialises/deadlocks under concurrent block_on.
Switch both runtime builders to new_multi_thread(4). New concurrency test drives
4 simultaneous gets through one transport over a multi-accept stub proxy.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `KeyedFakeGet` — order-independent test transport

**Files:**
- Modify: `workers/web-common/src/testing.rs` (add `KeyedFakeGet` + self-test)

**Interfaces:**
- Produces: `pub struct KeyedFakeGet`; `KeyedFakeGet::new(Vec<(&str, RawResponse)>) -> Self` (keys each pair by the URL's host+path, query ignored); implements `HttpGet` (`get`/`post` both look up by host+path). Immutable ⇒ `Send + Sync`.

- [ ] **Step 1: Write the failing self-test**

Append to `workers/web-common/src/testing.rs`:

```rust
#[cfg(test)]
mod keyed_fake_tests {
    use super::*;
    use url::Url;

    #[test]
    fn matches_by_host_and_path_ignoring_query() {
        let t = KeyedFakeGet::new(vec![
            ("https://searx.example.org/search", json_resp(r#"{"results":[]}"#)),
            ("https://docs.example.org/a", ok_resp("page a")),
        ]);
        // Search request carries a ?q=... query — must still match by host+path.
        let s = t.get(&Url::parse("https://searx.example.org/search?q=hello&format=json").unwrap())
            .unwrap();
        assert_eq!(s.status, 200);
        let a = t.get(&Url::parse("https://docs.example.org/a").unwrap()).unwrap();
        assert_eq!(a.body, b"page a");
        // Unregistered URL is an explicit error.
        let miss = t.get(&Url::parse("https://docs.example.org/missing").unwrap());
        assert!(miss.is_err());
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common keyed_fake`
Expected: FAIL — `KeyedFakeGet` not found.

- [ ] **Step 3: Implement `KeyedFakeGet`**

Add to `workers/web-common/src/testing.rs` (near `FakeGet`; add `use std::collections::HashMap;` to the imports):

```rust
/// URL host+path → response. Unlike `FakeGet`'s FIFO queue, lookups are
/// order-independent, so a test can drive concurrent fetches and assert results
/// deterministically. The query string is ignored (search requests carry `?q=…`).
/// Immutable after construction ⇒ `Send + Sync`.
pub struct KeyedFakeGet {
    responses: HashMap<String, RawResponse>,
}

fn keyed_url(url: &Url) -> String {
    format!("{}{}", url.host_str().unwrap_or(""), url.path())
}

impl KeyedFakeGet {
    /// Build from `(url, response)` pairs. Each URL is reduced to its host+path key.
    pub fn new(pairs: Vec<(&str, RawResponse)>) -> Self {
        let responses = pairs
            .into_iter()
            .map(|(u, r)| (keyed_url(&Url::parse(u).expect("valid test url")), r))
            .collect();
        Self { responses }
    }

    fn lookup(&self, url: &Url) -> Result<RawResponse, String> {
        let key = keyed_url(url);
        self.responses
            .get(&key)
            .cloned()
            .ok_or_else(|| format!("no canned response for {key}"))
    }
}

impl HttpGet for KeyedFakeGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        self.lookup(url)
    }

    fn transport_kind(&self) -> &'static str {
        "keyed-fake"
    }

    fn post(&self, url: &Url, _content_type: &str, _body: &[u8]) -> Result<RawResponse, String> {
        self.lookup(url)
    }
}
```

- [ ] **Step 4: Run the self-test + a Sync assertion**

Extend the `send_sync_tests` module (from Task 1) to include `_assert_send_sync::<super::KeyedFakeGet>();`, then run:

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common keyed_fake send_sync && cargo clippy -p kastellan-worker-web-common --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add workers/web-common/src/testing.rs
git commit -m "test(web-common): add KeyedFakeGet (URL host+path keyed, Sync) for concurrent-fetch tests

Order-independent canned-response transport so web-research can drive parallel
fetches and assert results deterministically. Query strings ignored so search
requests match by host+path.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Split `gather_source` into phase-1 / phase-2 helpers (behaviour-preserving, still sequential)

**Files:**
- Modify: `workers/web-research/src/research.rs` (replace `gather_source`; `research()` loop calls the two new helpers inline — no concurrency yet)

**Interfaces:**
- Produces (all `pub(crate)`/private to the crate):
  - `struct FetchedPage { final_url: String, passages: Vec<String> }`
  - `fn fetch_and_chunk<T: HttpGet>(&T, &HostAllowlist, url: &str) -> Result<FetchedPage, String>`
  - `fn rank_fetched_page(embedder: Option<&dyn Embedder>, query_emb: Option<&[f32]>, query: &str, hit: &Hit, page: &FetchedPage, max_passages: usize) -> Result<(SourcePassages, Option<String>), String>`
  - `fn hit_allowed(&HostAllowlist, &Hit) -> bool`

This task keeps ALL existing tests green (byte-identical behaviour); it only restructures the code so Task 5 can parallelise phase 1.

- [ ] **Step 1: Add the helpers, remove `gather_source`**

In `workers/web-research/src/research.rs`, replace the `gather_source` function (lines ~155–195) with:

```rust
/// One fetched + chunked page, not yet ranked. Phase-1 output of the fetch driver.
#[derive(Debug)]
struct FetchedPage {
    final_url: String,
    passages: Vec<String>,
}

/// Is this hit's host on the content allowlist? (Shared by the candidate filter and
/// the classify walk so the two can never drift — a hit fetched in phase 1 must be
/// the same set the classify phase expects.)
fn hit_allowed(allowlist: &HostAllowlist, hit: &Hit) -> bool {
    Url::parse(&hit.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .map(|h| allowlist.is_allowed(&h))
        .unwrap_or(false)
}

/// Phase 1: fetch one allowlisted hit and chunk it into passages. `Err(reason)` on
/// any fetch/redirect/extract failure or a non-2xx terminal status — the exact
/// reason strings the caller records in `unfetched`. Pure over the transport seam.
fn fetch_and_chunk<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    url: &str,
) -> Result<FetchedPage, String> {
    let url = Url::parse(url).map_err(|e| format!("fetch-failed: bad url: {e}"))?;
    let outcome = drive(transport, allowlist, url).map_err(|e| short_fetch_reason(&e))?;
    // `drive` returns any non-3xx terminal response, including 4xx/5xx. An error
    // page (403 bot-challenge, 404, 500) is not a usable source — record it rather
    // than extracting its error HTML into bogus passages.
    if !(200..300).contains(&outcome.status) {
        return Err(format!("fetch-failed: status {}", outcome.status));
    }
    let extracted = extract(&outcome.content_type, &outcome.body)
        .map_err(|e| format!("fetch-failed: extraction: {e}"))?;
    let passages = chunk_passages(&extracted.text);
    Ok(FetchedPage { final_url: outcome.final_url, passages })
}

/// Phase 2: rank one fetched page against the query, truncate to `max_passages`,
/// and apply the empty ⇒ `no-relevant-passages` rule. `Err(reason)` when the page
/// shares no relevant passage with the query (don't consume a source slot).
/// Returns the built source and an optional per-page degrade/cap note.
fn rank_fetched_page(
    embedder: Option<&dyn Embedder>,
    query_emb: Option<&[f32]>,
    query: &str,
    hit: &Hit,
    page: &FetchedPage,
    max_passages: usize,
) -> Result<(SourcePassages, Option<String>), String> {
    let (mut ranked, note) = rank_page(embedder, query_emb, query, &page.passages);
    ranked.truncate(max_passages);
    if ranked.is_empty() {
        return Err("no-relevant-passages".to_string());
    }
    Ok((
        SourcePassages {
            url: page.final_url.clone(),
            title: hit.title.clone(),
            snippet: hit.snippet.clone(),
            passages: ranked,
        },
        note,
    ))
}
```

- [ ] **Step 2: Rewire the sequential loop to call the two helpers**

In `research()`, replace the per-hit loop body (the `let host = …; let allowed = …; if !allowed {…} match gather_source(…) {…}` block, lines ~247–274) with:

```rust
    for hit in &hits {
        if sources.len() >= max_sources {
            break;
        }
        if !hit_allowed(allowlist, hit) {
            unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason: "off-allowlist".to_string(),
            });
            continue;
        }
        let result = fetch_and_chunk(transport, allowlist, &hit.url).and_then(|page| {
            rank_fetched_page(
                eff_embedder, query_emb.as_deref(), query, hit, &page, max_passages,
            )
        });
        match result {
            Ok((src, note)) => {
                if embed_note.is_none() {
                    embed_note = note; // first page-level degrade reason wins
                }
                sources.push(src);
            }
            Err(reason) => unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason,
            }),
        }
    }
```

- [ ] **Step 3: Run the full web-research suite — must be green (byte-identical)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-research && cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings`
Expected: PASS (all existing tests — `happy_path…`, `one_fetch_failure…`, `max_sources_caps_fetches`, `non_2xx…`, `fetched_page_with_no_relevant_passages…`, `hybrid_…`, `rank_page_…`, etc. — unchanged), clippy clean.

- [ ] **Step 4: Commit**

```bash
git add workers/web-research/src/research.rs
git commit -m "refactor(web-research): split gather_source into fetch_and_chunk + rank_fetched_page

Behaviour-preserving: research() still loops sequentially, now via a phase-1
(fetch+extract+chunk) and phase-2 (rank+classify) seam. hit_allowed() shared by
the (future) candidate filter and the classify walk. No output change.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Parallelise phase 1 (`fetch_candidates` driver + two-phase `research`)

**Files:**
- Modify: `workers/web-research/src/research.rs` (add `MAX_CONCURRENT_FETCHES`, `fetch_candidates`; rewrite `research()` loop into fetch-then-classify; migrate 2 tests to `KeyedFakeGet`; add 3 new tests)

**Interfaces:**
- Consumes: `FetchedPage`, `fetch_and_chunk`, `rank_fetched_page`, `hit_allowed` (Task 4); `KeyedFakeGet` (Task 3).
- Produces: `pub const MAX_CONCURRENT_FETCHES: usize`; `fn fetch_candidates<T: HttpGet>(&T, &HostAllowlist, &[(usize, &Hit)]) -> HashMap<usize, Result<FetchedPage, String>>`.

- [ ] **Step 1: Write the failing parallel tests**

In `workers/web-research/src/research.rs` test module, add these tests and the `KeyedFakeGet` import. First add to the test imports:

```rust
    use kastellan_worker_web_common::testing::{al, json_resp, ok_resp, redirect_to, FakeGet, KeyedFakeGet};
```

(keep existing imports; add `redirect_to` and `KeyedFakeGet`). Then add:

```rust
    /// Build a KeyedFakeGet with the search endpoint + a set of page responses.
    fn keyed(search: &str, pages: Vec<(&str, RawResponse)>) -> KeyedFakeGet {
        let mut pairs = vec![("https://searx.example.org/search", json_resp(search))];
        pairs.extend(pages);
        KeyedFakeGet::new(pairs)
    }

    #[test]
    fn parallel_fetch_returns_rank_ordered_sources() {
        // Three allowlisted candidates, all relevant → sources in rank order A,B,C
        // regardless of fetch completion order.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("bwrap namespaces alpha content")),
            ("https://docs.example.org/b", ok_resp("bwrap namespaces bravo content")),
            ("https://docs.example.org/c", ok_resp("bwrap namespaces charlie content")),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
        let urls: Vec<&str> = out.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec![
            "https://docs.example.org/a",
            "https://docs.example.org/b",
            "https://docs.example.org/c",
        ]);
        assert!(out.unfetched.is_empty());
    }

    #[test]
    fn mid_list_fetch_failure_still_surfaces_later_successes() {
        // B 404s; A and C succeed → sources == [A, C], B recorded in unfetched.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("bwrap namespaces alpha")),
            ("https://docs.example.org/b", RawResponse { status: 404, location: None,
                content_type: "text/plain".into(), body: b"nope".to_vec() }),
            ("https://docs.example.org/c", ok_resp("bwrap namespaces charlie")),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
        let urls: Vec<&str> = out.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec!["https://docs.example.org/a", "https://docs.example.org/c"]);
        assert_eq!(out.unfetched.len(), 1);
        assert_eq!(out.unfetched[0].url, "https://docs.example.org/b");
        assert_eq!(out.unfetched[0].reason, "fetch-failed: status 404");
    }

    #[test]
    fn parallel_result_is_deterministic() {
        // Same scenario run repeatedly must yield identical source ordering — the
        // classify phase is rank-ordered, so completion order must not leak out.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let mut seen: Option<Vec<String>> = None;
        for _ in 0..5 {
            let t = keyed(&search, vec![
                ("https://docs.example.org/a", ok_resp("bwrap namespaces alpha")),
                ("https://docs.example.org/b", ok_resp("bwrap namespaces bravo")),
                ("https://docs.example.org/c", ok_resp("bwrap namespaces charlie")),
            ]);
            let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
            let urls: Vec<String> = out.sources.iter().map(|s| s.url.clone()).collect();
            match &seen {
                None => seen = Some(urls),
                Some(prev) => assert_eq!(prev, &urls, "source order must be stable across runs"),
            }
        }
    }
```

- [ ] **Step 2: Run the new tests to verify they compile-fail / fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-research parallel_fetch mid_list parallel_result 2>&1 | tail -20`
Expected: FAIL — `KeyedFakeGet` import resolves (Task 3), but the tests exercise the still-sequential loop; they should PASS on logic alone EXCEPT they will actually pass since sequential is output-identical. **If they pass already, that is expected** (output-identity is the whole point) — proceed; Step 3 introduces the concurrency the determinism/perf goal needs. To force a genuine red for the driver, first add the `fetch_candidates` signature as `unimplemented!()` is unnecessary; instead rely on Step 3's parallel rewrite and re-run. (Skip forcing an artificial red here; the migrated tests in Step 4 are the behavioural guard.)

> NOTE: because the design is output-preserving, these three tests pass against both the sequential (Task 4) and parallel (this task) code. That is intended — they lock the observable contract. The parallelism itself is exercised structurally (the scoped-thread driver) and guarded by `concurrent_gets_share_one_transport` (Task 2).

- [ ] **Step 3: Add `MAX_CONCURRENT_FETCHES` + `fetch_candidates`, rewrite `research()`**

Add the constant near the other consts (after `MAX_EMBED_PASSAGES`):

```rust
/// Max page fetches in flight at once during the parallel fetch phase.
///
/// Allowlisted candidates are bounded by `SEARCH_COUNT` (10), so this caps the
/// burst on the egress proxy / origin servers to a handful while collapsing the
/// common case (≤ this many candidates) into a single wave. At the 10-candidate
/// ceiling the fetch runs in ⌈10 / N⌉ waves ⇒ ~⌈10 / N⌉ × 20 s worst case — under
/// the worker budget and far below the old sequential Σ. Separate from the
/// `ProxyConnectGet` runtime worker-thread count (an unrelated internal knob).
pub const MAX_CONCURRENT_FETCHES: usize = 6;
```

Add `use std::collections::HashMap;` to the top of `research.rs`. Add the driver (after `fetch_and_chunk`):

```rust
/// Phase-1 driver: fetch + chunk every allowlisted candidate concurrently, in
/// bounded waves of `MAX_CONCURRENT_FETCHES`, sharing one `&transport` across
/// scoped threads. Returns a map from each candidate's hit index to its result so
/// the sequential classify phase can consult it in rank order (completion order
/// never leaks into the output).
fn fetch_candidates<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    candidates: &[(usize, &Hit)],
) -> HashMap<usize, Result<FetchedPage, String>> {
    let mut results = HashMap::with_capacity(candidates.len());
    for wave in candidates.chunks(MAX_CONCURRENT_FETCHES) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = wave
                .iter()
                .map(|(idx, hit)| {
                    let idx = *idx;
                    let url = hit.url.clone();
                    scope.spawn(move || (idx, fetch_and_chunk(transport, allowlist, &url)))
                })
                .collect();
            for h in handles {
                let (idx, res) = h.join().expect("fetch thread panicked");
                results.insert(idx, res);
            }
        });
    }
    results
}
```

Replace the sequential loop in `research()` (the Task-4 loop body) with the two-phase version:

```rust
    // Phase 1: fetch+chunk every allowlisted candidate concurrently.
    let candidates: Vec<(usize, &Hit)> = hits
        .iter()
        .enumerate()
        .filter(|(_, hit)| hit_allowed(allowlist, hit))
        .collect();
    let fetched = fetch_candidates(transport, allowlist, &candidates);

    // Phase 2: classify + rank in rank order — output-identical to the sequential
    // loop, including the max_sources-successes break and unfetched ordering.
    let mut sources = Vec::new();
    let mut unfetched = Vec::new();
    for (idx, hit) in hits.iter().enumerate() {
        if sources.len() >= max_sources {
            break;
        }
        if !hit_allowed(allowlist, hit) {
            unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason: "off-allowlist".to_string(),
            });
            continue;
        }
        // Every allowlisted hit was fetched in phase 1.
        let fetch_result = fetched
            .get(&idx)
            .expect("allowlisted candidate must have a phase-1 result");
        let classified = match fetch_result {
            Ok(page) => rank_fetched_page(
                eff_embedder, query_emb.as_deref(), query, hit, page, max_passages,
            ),
            Err(reason) => Err(reason.clone()),
        };
        match classified {
            Ok((src, note)) => {
                if embed_note.is_none() {
                    embed_note = note; // first page-level degrade reason wins (rank order)
                }
                sources.push(src);
            }
            Err(reason) => unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason,
            }),
        }
    }
    Ok(ResearchOutcome { sources, unfetched, ranking, embed_note })
```

(Remove the now-dead Task-4 loop and its trailing `Ok(ResearchOutcome{…})` — there must be exactly one return.)

- [ ] **Step 4: Migrate the two multi-candidate tests to `KeyedFakeGet`**

Replace `one_fetch_failure_is_recorded_others_returned` with a KeyedFakeGet version (self-redirect loop for the failure — `drive` follows it to `TooManyRedirects`):

```rust
    #[test]
    fn one_fetch_failure_is_recorded_others_returned() {
        // A succeeds; B self-redirects until TooManyRedirects. Order-independent.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("user namespaces sandbox bwrap details")),
            // 302 → itself: drive re-fetches the same host+path until MAX_REDIRECTS.
            ("https://docs.example.org/b", redirect_to("https://docs.example.org/b")),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
        assert_eq!(out.sources.len(), 1, "A should succeed");
        assert_eq!(out.sources[0].url, "https://docs.example.org/a");
        assert_eq!(out.unfetched.len(), 1, "B should be recorded as failed");
        assert!(out.unfetched[0].reason.starts_with("fetch-failed:"), "{}", out.unfetched[0].reason);
    }
```

Replace `max_sources_caps_fetches` with a KeyedFakeGet version (note the changed premise — all candidates ARE fetched now; `max_sources` caps the result):

```rust
    #[test]
    fn max_sources_caps_result_not_fetches() {
        // Under fetch-all, all three allowlisted candidates are fetched concurrently;
        // max_sources caps the RESULT to 2 (rank order A, B; C never classified).
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("bwrap namespaces one")),
            ("https://docs.example.org/b", ok_resp("bwrap namespaces two")),
            ("https://docs.example.org/c", ok_resp("bwrap namespaces three")),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 2, 3).unwrap();
        assert_eq!(out.sources.len(), 2);
        let urls: Vec<&str> = out.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec!["https://docs.example.org/a", "https://docs.example.org/b"]);
        assert!(out.unfetched.is_empty(), "C is never classified (break at max_sources)");
    }
```

- [ ] **Step 5: Run the full web-research suite + clippy + workspace build**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-research && cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings && cargo build --workspace`
Expected: PASS (migrated + new + all prior tests), clippy clean, workspace builds.

- [ ] **Step 6: Commit**

```bash
git add workers/web-research/src/research.rs
git commit -m "feat(web-research): parallel fetch phase (bounded scoped-thread waves)

research() now fetches all allowlisted candidates concurrently (fetch_candidates,
bounded by MAX_CONCURRENT_FETCHES=6) then classifies+ranks in rank order — output
byte-identical to the sequential loop (sources/unfetched contents and order,
max_sources break preserved), only the network fetch pattern changes. Multi-
candidate tests migrated to the order-independent KeyedFakeGet; new tests pin
rank-order, mid-list-failure resilience, and cross-run determinism.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Documentation — module + manifest

**Files:**
- Modify: `workers/web-research/src/research.rs:1-10` (module doc)
- Modify: `core/src/workers/web_research.rs` (manifest doc block: sequential-fetch/60 s note)

**Interfaces:** none (docs only).

- [ ] **Step 1: Update the research.rs module doc**

Replace the module doc header lines (1–10) `Flow:` paragraph tail so it reflects the two-phase parallel fetch. Change the sentence "for each hit in rank order, if its host is on the content allowlist attempt a fetch" region to:

```rust
//! Flow: reject empty query → `search()` the SearxNG endpoint → fetch every
//! allowlisted hit concurrently in bounded waves (`fetch_candidates`,
//! `MAX_CONCURRENT_FETCHES`) → classify + rank the fetched pages in rank order.
//! On a 2xx that yields at least one relevant passage the page becomes a source;
//! any other outcome (off-allowlist, transport/redirect failure, non-2xx status,
//! or zero relevant passages) is recorded in `unfetched` with a reason, never
//! dropped silently and never a source slot. The parallel fetch is
//! output-identical to a sequential pass — only the network fetch pattern
//! changes; `sources`/`unfetched` contents and order (including the `max_sources`
//! break) are preserved, so surplus pages fetched past the break are discarded
//! and never ranked/embedded.
```

- [ ] **Step 2: Update the manifest wall-clock note**

In `core/src/workers/web_research.rs`, find the doc comment flagging the sequential-fetch vs 60 s wall-clock interaction (grep `sequential` / `wall-clock` / `60`) and replace it with a bounded-parallel description, e.g.:

```rust
    // Fetches are now bounded-parallel (web-research `MAX_CONCURRENT_FETCHES`
    // scoped-thread waves), so the top-N page fetches run concurrently rather than
    // serially — wall-clock is ~⌈candidates / cap⌉ × the per-fetch timeout, not the
    // sum. The result is byte-identical to the old sequential pass.
```

(Match the surrounding comment style; if the exact wording differs, preserve intent: sequential → bounded-parallel, no 60 s serial worst case.)

- [ ] **Step 3: Build + clippy (doc-only, but doctests/format must hold)**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-web-research -p kastellan-core && cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings`
Expected: PASS, clippy clean.

- [ ] **Step 4: Commit**

```bash
git add workers/web-research/src/research.rs core/src/workers/web_research.rs
git commit -m "docs(web-research): describe two-phase bounded-parallel fetch (module + manifest)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (after all tasks)

- [ ] `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common` — FakeGet/KeyedFakeGet/proxy_connect green.
- [ ] `cargo test -p kastellan-worker-web-research` — all research/handler/rank/chunk/embed tests green (expect +3 new, 2 migrated).
- [ ] `cargo build --workspace` — exit 0 (worker binary present for manifest sibling-discovery).
- [ ] `cargo clippy -p kastellan-worker-web-research -p kastellan-worker-web-common --all-targets -- -D warnings` — clean.
- [ ] Update `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md`: mark parallel fetch done; note the `ProxyConnectGet` multi-thread runtime change is live-exercised only on the DGX egress path (deferred `ProxyConnectGet` e2e family) — low-risk, unit-covered.
- [ ] Push branch, open PR to `main`.

---

## Self-Review

**Spec coverage:**
- Two-phase (parallel fetch / sequential classify) → Tasks 4 + 5. ✓
- Output byte-identity + break/ordering → Task 5 classify walk + `parallel_result_is_deterministic`/rank-order tests. ✓
- `HttpGet: Send + Sync`, `FakeGet` Mutex, `RawResponse: Clone` → Task 1. ✓
- `ProxyConnectGet` multi-thread runtime + concurrency guard → Task 2. ✓
- `KeyedFakeGet` (URL host+path, Sync) → Task 3. ✓
- `MAX_CONCURRENT_FETCHES` + bounded waves + `fetch_candidates` → Task 5. ✓
- `fetch_and_chunk` / `rank_fetched_page` / `hit_allowed` refactor → Task 4. ✓
- Docs (module + manifest) → Task 6. ✓
- Verification commands → per-task + final. ✓
- Embed stays sequential (no `Embedder` Sync change) → honoured (embedder only used in phase-2 classify). ✓

**Placeholder scan:** no TBD/TODO/"handle edge cases"; every code step shows full code. Task 5 Step 2 explicitly explains the "already-green" nature (output-identity) rather than fabricating a red — this is a documented, honest deviation, not a placeholder.

**Type consistency:** `FetchedPage{final_url, passages}`, `fetch_and_chunk(&T,&HostAllowlist,&str)->Result<FetchedPage,String>`, `rank_fetched_page(Option<&dyn Embedder>,Option<&[f32]>,&str,&Hit,&FetchedPage,usize)`, `hit_allowed(&HostAllowlist,&Hit)->bool`, `fetch_candidates(&T,&HostAllowlist,&[(usize,&Hit)])->HashMap<usize,Result<FetchedPage,String>>` — names/signatures consistent across Tasks 4→5. `KeyedFakeGet::new(Vec<(&str,RawResponse)>)` consistent Task 3→5.
