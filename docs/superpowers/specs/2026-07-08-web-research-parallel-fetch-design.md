# web-research parallel fetch — design

**Date:** 2026-07-08
**Status:** approved, ready for implementation plan
**Scope:** one session. Worker `kastellan-worker-web-research` + shared `kastellan-worker-web-common` (transport seam + test doubles). No `core`, PG, sandbox, or manifest surface.

## Problem

`web.research` fetches the top-N candidate pages **sequentially**: `research()` walks
search hits in rank order and, for each allowlisted hit, fetches → extracts → chunks →
ranks inline before moving to the next. Each fetch carries a 20 s transport timeout
(`web-common::http::TIMEOUT_SECS`), so N slow pages cost ≈ N × 20 s of wall-clock — the
worker can brush its ~60 s budget with only three sources. This is the last functional
follow-up flagged for web-research Slice 4 ("Parallel fetch of the top-N pages").

## Goal

Fetch the candidate pages **concurrently**, cutting wall-clock from ≈ Σ(fetch times)
toward ≈ max(single fetch), **while keeping the `sources` / `unfetched` result
byte-identical to today's sequential behaviour** — contents *and* order, including the
`max_sources`-successes break. Only the network fetch *pattern* changes.

## Non-goals

- Async rewrite of the worker. It stays `reqwest::blocking`; concurrency is OS threads.
- Parallelising the embed POSTs. Embedding stays single-threaded (see "Why two phases").
- Firecracker VM entry, parallel search, or the deferred `ProxyConnectGet` egress e2e —
  those remain separate Slice-4 follow-ups.

## Key design: two phases

Split `research()`'s per-hit loop into a parallel network phase and a sequential
classify/rank phase.

### Phase 1 — parallel fetch (the latency win, network-bound)

For each **allowlisted** candidate hit (rank order preserved via its hit index), run
`fetch → status-check → extract → chunk` concurrently across scoped OS threads. Each
produces a per-candidate `Result<FetchedPage, String>` where

```rust
struct FetchedPage {
    final_url: String,
    passages: Vec<String>,   // chunk_passages() output; NOT yet ranked
}
```

and the `String` error is the same `reason` string the sequential loop records today
(`fetch-failed: …`). **No embedder is touched in this phase.** Off-allowlist hits are
*not* fetched (same as today) — they are classified in phase 2.

Results are collected into a `HashMap<usize, Result<FetchedPage, String>>` keyed by the
original hit index, so phase 2 can consult them deterministically regardless of thread
completion order.

### Phase 2 — sequential classify + rank (main thread, rank order)

Unchanged in logic from the current tail of `research()`:

1. Query-embed once up front (existing code): on failure, degrade the whole call to
   lexical and drop the embedder; set `embed_note`.
2. Walk the **full `hits` list in original order**. For each hit:
   - **off-allowlist** → push to `unfetched` (reason `off-allowlist`) — same as today;
   - else consult its phase-1 result:
     - `Err(reason)` → push to `unfetched(reason)`;
     - `Ok(page)` → `rank_fetched_page()` → truncate to `max_passages`; if the ranked
       list is empty → `unfetched("no-relevant-passages")`, else push a `SourcePassages`
       and fold its optional degrade note into `embed_note` (first reason wins).
   - **break once `sources.len() == max_sources`.**

Because phase 2 applies the *same rank-order stop rule over the same hit list*, the
resulting `sources` and `unfetched` (contents and order, including the break) are
**identical** to the sequential implementation. Pages we over-fetched past the break are
discarded and never ranked/embedded — so there are **no extra embed POSTs**, only extra
GETs.

### Why two phases (embed stays sequential)

Confining concurrency to the pure `fetch+extract+chunk` half means:

- `Embedder` needs **no** thread-safety change (it is only used in phase 2, single-thread);
- `embed_note` "first reason wins" stays **deterministic** (sequential rank order);
- `rank_page` / `cosine` / `rrf_fuse` are **untouched**;
- the dominant latency (external page fetches, 20 s each) is fully parallelised; the
  embed endpoint (local Ollama, ≤ `max_sources` calls) serialises harmlessly.

## Threading mechanics

1. **`HttpGet: Send + Sync`.** Add the supertraits so a `&transport` can be shared across
   `std::thread::scope` fetch threads. Blast radius is web-common only (5 impls):
   - `ReqwestGet` (blocking client is `Send + Sync`) ✓
   - `ProxyConnectGet` (`Runtime` + `Arc<ClientConfig>` + `String`/`PathBuf`) ✓
   - `Box<dyn HttpGet>` — auto `Send + Sync` once the trait carries the supertraits ✓
   - `GetOnly` (empty test double) ✓
   - `FakeGet` — **forced change**: `RefCell<VecDeque>` → `Mutex<VecDeque>` to become `Sync`
     (transparent to web-fetch/web-search, which never share it across threads).

2. **`ProxyConnectGet` → multi-thread runtime.** Today it builds
   `tokio::runtime::Builder::new_current_thread()`; a current-thread runtime cannot be
   `block_on`'d concurrently from multiple threads (it serialises at best), which would
   defeat parallelism on the **production force-routed path**. Switch to
   `new_multi_thread().worker_threads(W).enable_all()`. `W` is the runtime's shared I/O
   worker count — a **separate knob** from the fetch-phase concurrency cap
   (`MAX_CONCURRENT_FETCHES`): each concurrent `block_on` drives its own future on its
   calling (fetch) thread while sharing the runtime's I/O driver, so `W` only needs to be
   a small fixed number (plan picks the literal, e.g. `W = 4`) — it does **not** have to
   equal the fetch cap. `ReqwestGet` already runs its own internal runtime and is
   concurrency-safe. Other net-egress workers (web-fetch, web-search) only ever issue one
   request at a time, so the change is behaviourally invisible to them (a few idle worker
   threads).

3. **Scoped fan-out with a concurrency cap.** Use `std::thread::scope`; process the
   allowlisted candidate list in chunks of `MAX_CONCURRENT_FETCHES` (one scope per chunk)
   so we bound simultaneous CONNECTs to the egress proxy and simultaneous requests to
   origins. No semaphore needed — chunking is enough given the small candidate ceiling.

### New constant

```rust
/// Max page fetches in flight at once during the parallel fetch phase.
///
/// Candidates are bounded by SEARCH_COUNT (10) after allowlist filtering, so this
/// caps the burst on the egress proxy / origin servers to a handful while still
/// collapsing the common case (≤ this many allowlisted candidates) into a single
/// wave. At the 10-candidate ceiling the fetch runs in ⌈10 / N⌉ waves, so wall-clock
/// is ⌈10 / N⌉ × ~20 s worst case — under the worker's budget, and far below the old
/// sequential Σ.
pub const MAX_CONCURRENT_FETCHES: usize = 6;
```

(`N = 6`: comfortably covers the default `max_sources = 3` and typical curated
allowlists in one wave; ≤ 2 waves at the 10-candidate ceiling. The plan may tune the
literal, but the rationale above fixes the shape.)

## Refactor shape (`workers/web-research/src/research.rs`)

`gather_source()` splits along the phase boundary:

- `fn fetch_and_chunk<T: HttpGet>(&T, &HostAllowlist, Url) -> Result<FetchedPage, String>`
  — the fetch / status-check / extract / chunk half. Pure over the transport seam, hermetic.
- `fn rank_fetched_page(embedder, query_emb, query, &FetchedPage, max_passages)
  -> Result<(SourcePassages, Option<String>), String>` — ranks a fetched page, truncates,
  and applies the empty ⇒ `no-relevant-passages` rule. Wraps the existing `rank_page`.
- A small parallel driver `fn fetch_candidates<T: HttpGet + Sync>(&T, &HostAllowlist,
  &[(usize, &Hit)]) -> HashMap<usize, Result<FetchedPage, String>>` that scopes the
  chunked fan-out.

`research()`'s public signature is **unchanged** — no edits to `handler.rs`, the manifest
(`core/src/workers/web_research.rs`), or the e2e callers.

## Testing (TDD — watch each new test fail first)

### New shared test double

Add `KeyedFakeGet` to `workers/web-common/src/testing.rs`: a `HashMap<String,
RawResponse>` (URL string → response) with `RawResponse: Clone` so `get`/`post` clone the
matched response. Immutable after construction ⇒ `Send + Sync`, and lookups are
order-independent — exactly what concurrent fetches need. The existing FIFO `FakeGet`
stays (now `Mutex`-backed, `Sync`) for the single-fetch tests that don't care about order.

### Migrations

- `research::tests::one_fetch_failure_is_recorded_others_returned` (2 allowlisted hits) →
  `KeyedFakeGet` (A ⇒ 200 page, B ⇒ redirect-loop / failure). Assert both `sources` (A) and
  `unfetched` (B, `fetch-failed:`) as today.
- `research::tests::max_sources_caps_fetches` → `KeyedFakeGet`. **Semantics note:** under
  fetch-all we now fetch *all three* candidates; `max_sources` caps the *result*, not the
  fetch count. Provide all three fetch responses; assert `sources.len() == 2`.

### New tests

- `parallel_fetch_returns_rank_ordered_sources`: 3 allowlisted candidates, all succeed →
  `sources` in hit/rank order (A, B, C), matching a sequential reference.
- `mid_list_fetch_failure_still_surfaces_later_successes`: candidate B fails, A and C
  succeed → `sources == [A, C]`, `unfetched` contains B — proving a failure doesn't stall
  or drop later results.
- `parallel_result_is_deterministic`: run the same `KeyedFakeGet` scenario a few times;
  assert identical `sources`/`unfetched` ordering each run (completion order must not leak
  into the output).

### Unaffected

Embed/rank tests, `rank_page_*` cap tests, `HttpEmbedder`, and the handler single-candidate
tests keep passing byte-for-byte.

## Documentation

- `research.rs` module doc: "fetch top-N sequentially" → "fetch allowlisted candidates in
  bounded-parallel waves, then classify/rank in rank order (output-identical to
  sequential)".
- Manifest doc block in `core/src/workers/web_research.rs`: replace the sequential-fetch /
  60 s wall-clock caveat with the bounded-parallel behaviour.
- Doc the `MAX_CONCURRENT_FETCHES` const (above) and the `ProxyConnectGet` runtime-flavour
  change (why multi-thread; who it affects).

## Verification

- `cargo test -p kastellan-worker-web-research`
- `cargo test -p kastellan-worker-web-common` (FakeGet Mutex + KeyedFakeGet)
- `cargo build --workspace`
- `cargo clippy -p kastellan-worker-web-research -p kastellan-worker-web-common --all-targets -- -D warnings`

Pure worker/test code plus a shared-runtime tweak; no PG / sandbox / schema surface, so
the DGX **2270/0/34** baseline carries forward. The `ProxyConnectGet` multi-thread change
is live-exercised only on the DGX egress path (the deferred `ProxyConnectGet` e2e family);
it is low-risk (idle worker threads) and unit-covered here — flag it in the handover for
the next DGX pass.

## Risks / mitigations

- **Concurrent `block_on` correctness on `ProxyConnectGet`.** Multi-thread runtimes
  support concurrent `block_on` from external threads by design; a unit test that fires
  several concurrent `get()`s through a multi-thread-backed transport (or the parallel
  driver over `KeyedFakeGet`) guards the seam.
- **Over-fetch cost.** We fetch all allowlisted candidates (≤ 10) even past the
  `max_sources` break. Accepted (operator decision): bounded, improves recall, and no
  extra embed POSTs since ranking stops at the break. `MAX_CONCURRENT_FETCHES` bounds the
  simultaneous burst.
- **Output drift.** The whole point is output-identity; the `parallel_result_is_deterministic`
  and rank-order tests pin it so a future change can't silently let completion order leak
  into results.
