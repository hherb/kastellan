# web-research — EmbeddingRanker + HybridRanker (RRF) design

**Date:** 2026-07-08
**Status:** design approved, ready for implementation plan
**Crate:** `workers/web-research` (+ `workers/web-common`, `core/src/workers/web_research.rs` manifest)
**Predecessor:** `2026-07-07-web-research-composite-worker-design.md` (Slice 4 deferral: "EmbeddingRanker + HybridRanker(RRF) via an embedding endpoint")

---

## Goal

Add **semantic** and **hybrid** passage ranking to the `web.research` composite
worker, so paraphrased queries (whose relevant passages share few surface terms
with the query) are no longer BM25 false-negatives. Ranking becomes:

- **no embed endpoint configured →** pure BM25 lexical (today's behaviour, byte-identical).
- **embed endpoint configured →** **HybridRanker**: BM25 lexical lane ⊕ embedding-cosine
  semantic lane, fused with parameter-free Reciprocal Rank Fusion (RRF, k=60,
  mirroring `core/src/memory/recall.rs`).

The feature is **opt-in and backward compatible**: the worker reads a new
`KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT`; when it is unset the worker ranks
exactly as it does today and every existing test stays green.

## Non-goals (deferred)

- **Live `#[ignore]` e2e** against a real embedding endpoint — staged as a
  follow-up (needs a real embed server), exactly as the SearxNG live e2e was
  staged separately from the composite worker.
- **EmbeddingRanker-only mode** (semantic without lexical) — the default and only
  wired multi-lane mode is Hybrid. The pure `cosine` primitive exists and could
  back an embedding-only ranker later, but no env flag exposes it in v1.
- **Firecracker VM entry** and **parallel fetch** — separate Slice-4 deferrals,
  out of scope here.
- **Query-embedding cache / batching across pages** — v1 embeds the query once
  up front and each page's passages in one POST per page (see Data flow).

---

## Where embedding happens

**Inside the worker.** The worker POSTs passage/query text to an
**embedding-only** endpoint over its **existing egress transport** — the same
networking path it already uses to reach the SearxNG endpoint (a
local/allowlisted service; under `KASTELLAN_EGRESS_FORCE_ROUTING` it is reached
through the egress proxy with the per-instance CA trust). This keeps the
composite self-contained (one dispatch does search → fetch → rank end-to-end) and
matches the Slice-4 design intent ("via an embedding endpoint over the egress
proxy").

The embed endpoint is modelled **exactly like the SearxNG endpoint**: an
operator-configured URL whose host:port is unioned into the worker's
`Net::Allowlist`, reached via the shared transport. It may be plaintext-HTTP
loopback (e.g. a local Ollama) or HTTPS — identical to the SearxNG endpoint's
handling.

Rationale for worker-side (not core-side) embedding: the passages already live in
the worker; returning raw passages for the core to rank would (a) enlarge the
JSON-RPC payload with un-ranked text, (b) split a single logical operation across
the process boundary, and (c) require core to re-derive the same chunking. The
embed endpoint is **embedding-only** (no text generation), which keeps the
worker's egress blast radius minimal.

---

## Architecture — two seams, pure scoring

The codebase's own precedent (`core/src/memory/embed.rs` deliberately pushes I/O
out of pure `recall`) drives the shape: **all scoring is pure; the single piece
of I/O is isolated behind an `Embedder` seam.** This also satisfies the project
coding rule "prefer pure functions in reusable modules."

### `rank.rs` (evolve) — pure ranking primitives, no I/O

Each is a pure, deterministic function (same input → same output), independently
unit-testable without any transport.

```rust
/// A passage with its relevance score (higher = more relevant). (unchanged)
pub struct ScoredPassage { pub text: String, pub score: f64 }

/// Lexical BM25 lane. Extracted verbatim from today's `LexicalRanker::rank`
/// body so the lexical path stays byte-for-byte behaviour-identical.
pub fn bm25(query: &str, passages: &[String]) -> Vec<ScoredPassage>;

/// Semantic lane. Cosine similarity of each passage embedding to the query
/// embedding. Passages with a zero-norm embedding or a non-positive similarity
/// are omitted (mirrors bm25's "no signal → omit"). `passage_embs[i]` pairs
/// with `passages[i]`; caller guarantees equal length.
pub fn cosine(query_emb: &[f32], passages: &[String], passage_embs: &[Vec<f32>])
    -> Vec<ScoredPassage>;

/// Fuse two ranked lists (best-first) via parameter-free RRF. Score for a
/// passage = Σ over lanes of 1/(k + rank), k = RRF_K (60.0). Keyed by passage
/// text (the two lanes rank the same passage set). Best-first; stable tie-break
/// by first-seen order. Mirrors `core::memory::recall::reciprocal_rank_fusion`.
pub fn rrf_fuse(lexical: &[ScoredPassage], semantic: &[ScoredPassage])
    -> Vec<ScoredPassage>;

pub const RRF_K: f64 = 60.0;
```

Note: today's `LexicalRanker` unit struct + `PassageRanker` trait are **retired**
— a single `rank(&self, query, passages) -> Vec<ScoredPassage>` method cannot
carry a two-lane hybrid (it has neither the embeddings nor a way to signal
degradation). The extensibility surface becomes the pure primitives + the
`Embedder` seam, which is cleaner. `research()` calls the primitives directly.

### `embed.rs` (new) — the only I/O

```rust
/// Turn texts into embedding vectors. The one network-touching seam; faked in
/// tests. Batches all input texts into one request.
pub trait Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

pub enum EmbedError {
    Transport(String),   // socket / TLS / timeout
    Status(u16),         // non-2xx from the endpoint
    Decode(String),      // envelope did not decode
    CountMismatch { requested: usize, returned: usize },
    DimMismatch { expected: usize, actual: usize }, // if a fixed dim is enforced
}

/// Real embedder: POST {model, input:[...]} to the endpoint via the shared
/// transport, decode the OpenAI-compatible envelope {data:[{index,embedding[]}]},
/// return vectors ordered to match `input`. Reuses the llm-router wire shapes.
pub struct HttpEmbedder<T: HttpGet> { transport: T, endpoint: Url, model: String }

/// Test embedder: canned vectors keyed by text, or an injected failure.
pub struct FakeEmbedder { /* map text -> vec, or forced error */ }
```

**Dimension policy:** cosine similarity is dimension-agnostic (it normalises), so
`cosine` does **not** require a fixed dim; it only requires the query and passage
vectors to share a length. `HttpEmbedder` therefore does **not** Matryoshka-
truncate (unlike `core`, which truncates to the `vector(256)` storage contract —
irrelevant here since nothing is stored). It only checks that all returned
vectors share one length and that the count matches the request; a length
mismatch within a single response is a `DimMismatch`/`Decode` error.

### Transport — POST on the shared seam (`workers/web-common/src/http.rs`)

Add one method to `HttpGet` with a **default that fails**, so the two sibling
workers (web-search, web-fetch) that never POST are untouched:

```rust
pub trait HttpGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String>;
    fn transport_kind(&self) -> &'static str;
    /// POST a body. Default: unsupported (siblings never call it).
    fn post(&self, _url: &Url, _content_type: &str, _body: &[u8])
        -> Result<RawResponse, String> {
        Err("post: unsupported by this transport".to_string())
    }
}
```

Implemented for `ReqwestGet` (`.post()`) and `ProxyConnectGet` (same CONNECT
tunnel, POST method) so the embed POST rides the **same proxy UDS + CA trust** as
content fetches. `Box<dyn HttpGet>` forwards `post` like the other methods.

---

## Data flow & the degrade signal

`research()` gains an `Option<&dyn Embedder>` parameter (replacing the retired
`R: PassageRanker` generic). The fetch loop is otherwise unchanged.

1. **Embed the query once, up front** (only if an `Embedder` is present). On
   failure: the whole call ranks **lexical**, `embed_note = Some(reason)` is set,
   and the embedder is dropped for the rest of the call — **fail-fast**, so a dead
   endpoint is not re-hit once per page.
2. **Per page** (inside `gather_source`, structure unchanged): compute `bm25`
   always. If the query embedding is live, embed *that page's* passages (one POST
   per page) → `cosine` → `rrf_fuse(bm25_result, cosine_result)`. A page-level
   embed failure → that page falls back to `bm25` and records the **first** reason
   into `embed_note` (best-effort; never silent). `max_passages` truncation and
   the `no-relevant-passages` recording are applied to the fused result exactly as
   today.
3. `ResearchOutcome` gains two fields:

```rust
pub struct ResearchOutcome {
    pub sources: Vec<SourcePassages>,
    pub unfetched: Vec<UnfetchedSource>,
    pub ranking: RankMode,          // Hybrid | Lexical
    pub embed_note: Option<String>, // Some(reason) iff a semantic lane was
                                    // configured but fell back to lexical
}
pub enum RankMode { Lexical, Hybrid }
```

`ranking` is `Hybrid` iff an embedder was configured **and** the query embedded
successfully (individual pages may still have degraded — `embed_note` says so);
`Lexical` when no embedder is configured **or** the query embed failed.

### `handler.rs` (evolve)

`from_env` reads `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` and (optional)
`KASTELLAN_WEB_RESEARCH_EMBED_MODEL` (default `embeddinggemma`). When the endpoint
is set it constructs an `HttpEmbedder` over the same transport the handler already
builds; when unset it passes `None` and behaviour is identical to today. The
JSON-RPC result surfaces `"ranking": "hybrid"|"lexical"` and, when present,
`"embed_note": "<reason>"` — so the LLM planner and the operator can see that a
configured semantic lane degraded.

### Manifest (`core/src/workers/web_research.rs`)

- New const `EMBED_ENDPOINT_ENV = "KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT"` (+
  `EMBED_MODEL_ENV`).
- When the embed endpoint is set, union its host:port into `Net::Allowlist`
  (reuse `endpoint_net_entry`, order after the SearxNG endpoint and before the
  content hosts) and inject both env vars into the worker policy.
- When unset, the manifest is byte-identical to today (no extra net entry, no
  extra env). Force-routable, injection-guard profile unchanged (`Relaxed`).

---

## Testing (TDD — write each test before its code)

**Pure primitives (`rank.rs`), no I/O:**
- `bm25` — the existing 5 `LexicalRanker` tests, re-pointed at `bm25(...)`
  (regression pin: lexical path unchanged).
- `cosine` — on-topic vector ranks above off-topic; zero-norm passage omitted;
  empty query-emb / empty passages → empty; identical vectors → score 1.0.
- `rrf_fuse` — a passage top-ranked in both lanes wins; a passage strong in only
  one lane still surfaces; tie-break is stable; fusing with an empty lane equals
  the non-empty lane's order.

**`HttpEmbedder` over `FakeGet`:**
- decodes the canonical OpenAI/vLLM envelope; preserves input order via `index`;
- non-2xx → `Status`; undecodable body → `Decode`; wrong count → `CountMismatch`.

**`research()` over `FakeGet` + `FakeEmbedder`:**
- hybrid surfaces a paraphrase passage that BM25 alone ranks 0 (the motivating
  case); `ranking == Hybrid`, `embed_note == None`.
- query-embed failure → whole call lexical, `ranking == Lexical`,
  `embed_note == Some`, and **no per-page embed attempted** (FakeEmbedder call
  count asserted, proving the fail-fast latch).
- one page's embed fails, another succeeds → per-page lexical fallback,
  `ranking == Hybrid`, `embed_note == Some`.
- **`None` embedder → outcome byte-identical to today** (all pre-existing
  `research` tests keep passing unmodified except the `research(...)` call gains a
  `None` arg).

**Manifest:**
- embed endpoint set → `Net::Allowlist` carries the extra host:port and the two
  env vars are injected (mirror `resolve_registers_union_net_and_injects_env`).
- embed endpoint unset → net + env identical to today.

**Transport:**
- `post` default returns the unsupported error; `ReqwestGet`/`ProxyConnectGet`
  `post` construct-and-return over a fake/loopback (mirror existing transport
  tests).

---

## Files touched

| File | Change |
|---|---|
| `workers/web-common/src/http.rs` | add `post` (default-Err) to `HttpGet`; impl for `ReqwestGet`, `ProxyConnectGet`, `Box<dyn HttpGet>` |
| `workers/web-research/src/rank.rs` | retire `PassageRanker`/`LexicalRanker`; add pure `bm25`, `cosine`, `rrf_fuse`, `RRF_K` |
| `workers/web-research/src/embed.rs` | **new** — `Embedder` trait, `HttpEmbedder`, `FakeEmbedder`, `EmbedError` |
| `workers/web-research/src/research.rs` | `Option<&dyn Embedder>` param; query-embed-up-front + per-page fuse; `RankMode`/`embed_note` on `ResearchOutcome` |
| `workers/web-research/src/handler.rs` | read embed endpoint/model env; build `HttpEmbedder`; surface `ranking`/`embed_note` in JSON |
| `workers/web-research/src/main.rs` / `lib` wiring | `mod embed;` |
| `core/src/workers/web_research.rs` | embed endpoint env consts; union host:port into `Net::Allowlist`; inject env |
| `docs/devel/ROADMAP.md` | tick the Slice-4 EmbeddingRanker/HybridRanker deferral |

Keep each file under the 500-LOC cap; `rank.rs` grows but the pure primitives are
small — if it approaches the cap, split `embed.rs` out (already planned) and keep
fusion in `rank.rs`.

---

## Verification

Mac (Seatbelt): `cargo build --workspace`; `cargo clippy --workspace
--all-targets -- -D warnings`; `cargo test -p kastellan-worker-web-research`,
`-p kastellan-worker-web-common`, `-p kastellan-worker-web-search`,
`-p kastellan-worker-web-fetch` (prove the shared-transport change is
behaviour-preserving for the siblings), and `core` lib. No PG/DGX/sandbox
behaviour surface is touched, so the DGX `2270/0/34` baseline carries forward; the
live embed e2e is the deferred follow-up.
