# web-research composite worker — design

**Date:** 2026-07-07
**Status:** approved (design), pending implementation plan
**Author:** session handover (kastellan)

## Problem

`web.search` (query → SearxNG → ranked `{title,url,snippet}` hits) and `web.fetch`
(URL → redirect-checked fetch → readable text) each work well as standalone tools,
and the LLM planner *can* chain them (tool output is fed back to the planner since
issues #337/#338/#339/#344, and oversized results are pageable via the handoff
cache). But there is no single "research a question against the web" capability:
the planner must orchestrate every fetch itself, and the "extract answers from
scraped pages" step has no home. There is also no live end-to-end verification of
the search→fetch→synthesis chain.

## Goal

A new sandboxed worker `kastellan-worker-web-research` exposing one JSON-RPC method
**`web.research { query, max_sources?, max_passages? }`**. In one call it:

1. runs a SearxNG search for `query`,
2. filters hits to operator-allowlisted content hosts and takes the top `max_sources`,
3. fetches each page (redirect-checked, https-only), extracting cleaned readable text,
4. chunks each page into passages,
5. ranks passages against the query (lexical for v1) and keeps the top `max_passages`,
6. returns the relevant passages with their source URLs.

The **LLM planner still performs final answer synthesis** — this worker hands it
clean, on-topic, pre-gathered material (the "retrieval" half of RAG), not a written
answer. (Placement decided: a self-contained worker, not a core-side orchestrator —
matches every existing worker pattern, is fully hermetic-testable, and needs no
change to the core dispatch/handoff/force-routing machinery. Tradeoff accepted: this
one worker's egress allowlist is the *union* of the search endpoint + content hosts,
still enforced by the egress proxy, so blast radius stays bounded.)

## Non-goals (v1)

- LLM answer synthesis inside the worker (stays with the planner).
- Semantic / embedding ranking (designed-for, not built — see Extensibility).
- Category/language/engine search params, pagination (inherit the web-search deferrals).
- Parallel fetching (sequential, best-effort, bounded by wall-clock — parallel is a
  later perf enhancement).

## Reuse: consolidate pure logic into `web-common`

The reusable pieces are currently trapped in the two bin workers. Following the exact
pattern that produced `web-common` in the first place (ROADMAP:267 — extracted from
web-fetch), move them into `web-common` behind cargo **features**, so each consumer
pulls only what it needs and no behaviour changes:

- `search` feature — SearxNG `validate_endpoint` / `build_query_url` / `search()` /
  `parse_results` + `Hit` + `is_loopback` (moved from `workers/web-search/src/{search,parse}.rs`).
- `fetch` feature — the `drive()` redirect loop + `FetchOutcome` / `FetchError`
  (moved from `workers/web-fetch/src/fetch.rs`; adds **no** new deps — only `url` +
  `allowlist` + `http`, all already in web-common).
- `extract` feature — HTML-readability / PDF / text extraction + `Extracted` +
  `main_type` + `cap_text` + `MAX_TEXT_BYTES` (moved from `workers/web-fetch/src/extract.rs`;
  pulls `readable-html` + `pdf-extract` **only when the feature is enabled**, so
  web-search's build stays lean).

Consumer wiring:

- `web-search` enables `web-common/search`; deletes its own `search.rs`/`parse.rs`,
  re-points `handler.rs` imports. Behaviour byte-preserved (its existing unit +
  `#[ignore]` e2e tests are the proof).
- `web-fetch` enables `web-common/fetch` + `web-common/extract`; deletes its own
  `fetch.rs`/`extract.rs`, re-points `handler.rs`. Behaviour byte-preserved (its
  existing unit + `web_fetch_e2e` tests are the proof). The PDF test fixture moves
  with `extract` (`workers/web-common/tests/fixtures/hello.pdf`).
- New `web-research` enables all three features.

Rationale for consolidation over "web-research depends on web-search/web-fetch as
libraries": those are bin-only crates; giving them lib targets purely to be imported
is more awkward than the established shared-crate pattern, and duplicating the logic
would violate the DRY / no-tech-debt rules. Feature-gating keeps web-search lean.

## New crate `workers/web-research`

Depends on `web-common` (features `search`, `fetch`, `extract`) + `web-common/testing`
(dev). Modules:

- `chunk.rs` — **pure** `chunk_passages(text: &str) -> Vec<String>`: paragraph-first
  segmentation (split on blank lines), with over-long paragraphs further split on
  sentence boundaries, and empty/whitespace passages dropped. Bounded passage length.
- `rank.rs` — the extensibility seam:
  - `struct ScoredPassage { text: String, score: f64 }`
  - `trait PassageRanker { fn rank(&self, query: &str, passages: &[String]) -> Vec<ScoredPassage>; }`
    (returns passages sorted best-first with their scores).
  - `struct LexicalRanker` — **pure** BM25/TF-IDF over tokenised passages+query
    (lowercase, unicode-word tokenisation, no external model). Deterministic and
    unit-tested.
- `research.rs` — orchestration, **pure over the `HttpGet` seam** (so hermetic with
  `FakeGet`): `research(transport, endpoint, search_allowlist, content_allowlist,
  ranker, query, max_sources, max_passages) -> Result<ResearchOutcome, ResearchError>`.
  Flow: validate query non-empty → `search()` → for each hit, keep those whose host
  is on `content_allowlist`, up to `max_sources` → `drive()` + `extract()` each
  (best-effort: a per-page failure records an `unfetched` entry, does not abort) →
  `chunk_passages` → `ranker.rank` → take `max_passages` per source. Enforces caps
  (`DEFAULT_MAX_SOURCES` = 3, `MAX_MAX_SOURCES` = 8; `DEFAULT_MAX_PASSAGES` = 3,
  `MAX_MAX_PASSAGES` = 10 — clamped, mirroring web-search's `count.clamp`).
- `handler.rs` — JSON-RPC `web.research` dispatch, generic over `HttpGet` (fake in
  tests), fail-closed `from_env` (endpoint + both allowlists + transport), error
  mapping to the protocol code vocabulary (`INVALID_PARAMS` / `POLICY_DENIED` /
  `OPERATION_FAILED` / `METHOD_NOT_FOUND`). No silent fallbacks.
- `main.rs` — stdio JSON-RPC server bringup (mirrors web-fetch/web-search `main.rs`).

### Result shape

```json
{
  "query": "how does bwrap create user namespaces",
  "sources": [
    { "url": "https://man.example.org/bwrap", "title": "bwrap(1)",
      "snippet": "…", "fetched": true,
      "passages": [ { "text": "…relevant passage…", "score": 8.42 } ] }
  ],
  "unfetched": [
    { "url": "https://blocked.test/x", "title": "…", "snippet": "…",
      "reason": "off-allowlist" },
    { "url": "https://flaky.example.org/y", "title": "…", "snippet": "…",
      "reason": "fetch-failed: too many redirects" }
  ],
  "sources_fetched": 1,
  "passage_count": 1
}
```

**No silent fallbacks (project invariant):** a search failure is an `OPERATION_FAILED`
error, never an empty-but-success result. Per-page fetch/extract failures are
*recorded* in `unfetched` with a reason (honest partial success) rather than dropped,
and do not fail the call so long as the search itself succeeded. If every candidate
is off-allowlist or fails, `sources` is empty but `unfetched` explains why — still a
success (the planner can see the search found nothing usable).

## Extensibility: hybrid semantic search (designed-in, not built)

The `PassageRanker` trait is the seam. A future `EmbeddingRanker` ranks passages by
cosine similarity to the query embedding, obtained from an **embedding-only** HTTP
endpoint (e.g. embeddinggemma via Ollama — see the `llm-backend-direction` memory)
reached **over the egress proxy** (one more allowlisted host + a
`KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` env, fail-closed like the search endpoint —
recorded here as the extension point, unimplemented in v1). A `HybridRanker` then
fuses the lexical and semantic rankings with **Reciprocal Rank Fusion**, the exact
technique already used in `core/src/memory/recall.rs` for three-lane recall. Because
ranking is behind the trait and the orchestrator takes a `ranker: &dyn PassageRanker`,
adding these is additive — no restructuring of `research.rs`. (Motivation: pure
lexical ranking alone yields too many false negatives on paraphrased queries.)

## Security / sandbox

Host-side manifest `core/src/workers/web_research.rs`, mirroring `web_fetch.rs`:

- `Net::Allowlist` = **union** of the SearxNG endpoint `host:port` (from
  `KASTELLAN_WEB_RESEARCH_ENDPOINT`) ∪ the content-domain `host:443` entries (from
  the `web-research` `tool_allowlists` row, `.domain` wildcard → bare `domain:443`,
  the web-fetch mapping). https-only content, so port 443; the search endpoint may be
  `http://` loopback-only (the web-search rule).
- `Profile::WorkerNetClient`, `SingleUse`, `cpu_ms` ~10_000, `mem_mb` ~512 (HTML/PDF
  parsing over several pages), `wall_clock_ms` ~45_000 (search + N sequential fetches),
  resolver files in `fs_read`. Force-routable through the egress proxy exactly like
  web-fetch (the proxy owns IP-level containment → the union allowlist is enforced
  there; the worker has no direct route when force-routed).
- The worker fetches **only** result URLs whose host is on the content allowlist. The
  LLM supplies only the `query` (no URL-injection surface); result URLs are
  attacker-influenceable (SEO), so the content-allowlist gate + per-hop re-check
  (inherited from `drive()`) is the containment, identical to web-fetch.
- Env injected by the manifest: `KASTELLAN_WEB_RESEARCH_ENDPOINT` (the SearxNG
  endpoint) + `KASTELLAN_WEB_RESEARCH_ALLOWLIST` (content domains JSON). **v1 uses a
  single operator allowlist** — the content allowlist — for both roles: the worker's
  `from_env` validates that the configured search endpoint's host is present in it
  (fail-closed if not, like web-search's `endpoint host ∈ allowlist` check), and every
  fetched result URL is gated against the same list. This keeps configuration to one
  list and one endpoint; a separate search-only allowlist is deliberately *not*
  introduced (the search endpoint is operator-fixed, not LLM-influenced). The operator
  therefore adds the SearxNG host to the `web-research` `tool_allowlists` row alongside
  the content hosts.
- Add `"web-research"` to `GuardProfile::for_tool`'s `Relaxed` arm
  (`core/src/cassandra/injection_guard.rs`) — it returns fetched document content,
  like web-fetch/web-search. Injection screening remains at the core dispatch **sink**
  (`inner_loop/summary.rs::render_step_outcome`); the worker performs none itself.
- Register `WebResearchManifest` in `WORKER_MANIFESTS`
  (`core/src/registry_build.rs`).

Firecracker micro-VM mode is **out of scope for v1** (web-fetch's VM path is opt-in
and separate); the host bwrap/seatbelt + force-routing path is the target. A VM entry
can be added later mirroring `web_fetch_firecracker_entry`.

## Verification

- **Hermetic unit tests** (`FakeGet`, no network):
  - `chunk.rs`: paragraph/sentence segmentation, whitespace dropping, long-paragraph split.
  - `rank.rs`: `LexicalRanker` orders an on-topic passage above an off-topic one;
    stable/deterministic; empty-query and empty-passages edge cases.
  - `research.rs`: happy path (search → 2 fetches → ranked passages); off-allowlist
    hit recorded in `unfetched` and never fetched; one fetch fails → recorded, others
    still returned; caps clamped; empty query → error; search transport error → error.
  - `handler.rs`: method routing, param validation, error-code mapping.
  - `core/src/workers/web_research.rs`: manifest resolve → `Net::Allowlist` union,
    profile, env injection, misconfigured-when-no-binary.
- **Behaviour-preserving proof for the consolidation:** the full existing
  web-search + web-fetch unit suites and `web_search_e2e` / `web_fetch_e2e` stay green
  after re-pointing to `web-common`.
- **`#[ignore]` live e2e** `core/tests/web_research_e2e.rs`: against a real SearxNG
  (`scripts/web-search/setup-searxng.sh`) + a couple of allowlisted content hosts,
  assert the full `query → passages` round-trip returns relevant passages. Runs on
  demand (dev Mac or DGX), same posture as `web_search_e2e::real_search_against_searxng`.

## Implementation slices (for the plan)

1. **web-common consolidation** — move `search`/`fetch`/`extract` behind features;
   re-point web-search + web-fetch; prove byte-preserved (existing tests green).
2. **web-research crate** — `chunk` + `rank` (LexicalRanker) + `research` + `handler`
   + `main`; hermetic FakeGet tests.
3. **core wiring** — `WebResearchManifest` + `web_research_entry`, register in
   `WORKER_MANIFESTS`, add to `GuardProfile::for_tool` Relaxed; manifest unit test.
   Optionally teach the planner the `web.research` capability in the agent system
   prompt.
4. **later** — `EmbeddingRanker` + `HybridRanker` (RRF) behind an embedding endpoint;
   live e2e; optional micro-VM entry; parallel fetch.

## References

- `workers/web-common/src/{allowlist,http}.rs` — the shared allowlist + `HttpGet` seam.
- `workers/web-search/src/{search,parse,handler}.rs` — SearxNG logic to consolidate.
- `workers/web-fetch/src/{fetch,extract,handler}.rs` — drive + extract to consolidate.
- `core/src/workers/web_fetch.rs` — the manifest pattern to mirror.
- `core/src/cassandra/injection_guard.rs` — `GuardProfile::for_tool`.
- `core/src/memory/recall.rs` — the RRF fusion the hybrid ranker will reuse.
- `core/src/scheduler/inner_loop/summary.rs` — the dispatch sink screen (why the
  worker needs no screening of its own).
- ROADMAP:266–267 — the web-fetch / web-search entries and their deferrals.
