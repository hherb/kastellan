# Batch web-search — design

**Date:** 2026-07-12
**Status:** Approved (design); ready for implementation plan
**Author:** session handover (batch web-search, ROADMAP "NEXT — batch web-search, PR 2")
**Related:** search-broker sidecar (PR #440), registry-driven `<tools>` block (PR #437),
planner `<now>` block (PR #441). Distinct from the date-resolution fix in #441: that
handled *dependent* date loops; this cuts iterations when several *independent* searches
are needed.

---

## 1. Motivation

Today the planner can issue only one search per step via `web.search{query}`. When a task
needs several **independent** searches (e.g. "compare the flooding response in Berlin,
Munich, and Hamburg"), the planner emits them across separate plan iterations. Each plan
iteration on the DGX costs **~46 s** (generation-bound — see memory
`dgx-planner-latency-generation-bound`), whereas a single search costs only **~1.7 s**
(audit id 1010, loopback SearxNG through the broker). So N independent searches cost
roughly N × 46 s of *planning* — the dominant, collapsible cost.

**Goal:** let the planner submit several independent queries as **one** dispatch
(`web.search_batch{queries:[…]}`), collapsing N plan iterations into 1. The N searches run
**sequentially** inside the worker; their combined network time (~N × 1.7 s) is secondary
and stays well under the worker's 30 s wall budget.

## 2. Decisions (locked)

| Axis | Decision | Rationale |
|---|---|---|
| **API shape** | A **separate** JSON-RPC method `web.search_batch`; `web.search` stays byte-identical. | Two clean `ToolDoc` entries; the single-query path (live on the DGX) is untouched; the planner keys on the distinct `method`. |
| **Concurrency** | **Sequential loop** of the existing `SearchProvider.search()` seam. | The collapsible cost is plan iterations, not network. The production (broker) path serves connections **serially** anyway, so worker-side parallelism would not help it. Zero broker changes. |
| **Partial failure** | **Per-query results** — one failing query never sinks the batch. | Matches the codebase's "no silent drops" philosophy (web-research `unfetched[]`, the planner-feedback arc). The planner sees exactly which queries worked. |
| **Batch-size cap** | `DEFAULT_MAX_BATCH_QUERIES = 8`, **operator-configurable** via `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`, clamped to a hard ceiling `HARD_MAX_BATCH_QUERIES = 32`. | 8 × ~1.7 s ≈ 14 s < the 30 s wall. Configurable per operator/host; the hard ceiling + the 30 s wall watchdog are the fail-closed backstops against a pathological value. |

**Key architectural property:** the entire feature lives **above** the `SearchProvider`
seam in the web-search worker. **Untouched:** `web-common` (`search`/`parse` are pure and
reused as-is), the **search-broker** crate, and the **core `broker/`** plumbing
(`BrokerSpec`/`spawn_broker` are method-agnostic). Batching therefore works identically in
**direct** mode (worker → SearxNG) and **broker** mode (worker → search-broker → SearxNG,
the default force-routed DGX deployment) with no new wire contract between worker and broker.

## 3. Wire contract

### Request — `web.search_batch`
```json
{
  "queries": ["berlin flooding july 2026", "munich flooding response", "hamburg dike status"],
  "count": 10
}
```
- `queries`: **required**, non-empty array of strings. Length capped at the effective
  `max_batch` (default 8, see §6). Empty array, missing field, or length > `max_batch`
  → whole-call `INVALID_PARAMS` (fail-closed).
- `count`: **optional**, a **single** value applied to **every** query. Same default (10)
  and cap (20, `web_common::search::MAX_COUNT`) as single `web.search`. No per-query count.
- Individual empty/blank query strings are **not** a whole-call error: they fail *per-query*
  (SearxNG empty-query → `SearchError::EmptyQuery`) and surface as an `error` element, so one
  bad string never discards the rest.

### Response
Top-level envelope `{ "results": [ <element>, … ] }`, one element per input query, **in input
order**. Each element is one of:

- **Success** — identical to a single `web.search` response body:
  ```json
  { "query": "munich flooding response", "results": [ {"title":…,"url":…,"snippet":…,"engine":…}, … ], "count": 8 }
  ```
- **Failure**:
  ```json
  { "query": "hamburg dike status", "error": "operation failed: status 502" }
  ```

The batch RPC returns **`Ok`** whenever the request was well-formed, regardless of how many
individual queries failed. Only malformed input (empty/oversized `queries`, bad param types)
returns a JSON-RPC error (`INVALID_PARAMS`). `METHOD_NOT_FOUND` is unchanged for any other
method.

## 4. Architecture & data flow

```
planner step {tool:"web-search", method:"web.search_batch", params:{queries,count}}
   │  (routed by tool NAME → ToolEntry → spawn worker; METHOD passed to the handler)
   ▼
WebSearchHandler::call(method="web.search_batch", params)      [handler.rs]
   │  parse BatchParams → validate non-empty + ≤ max_batch      [batch.rs, pure]
   ▼
run_batch(&*self.provider, &queries, count) -> Vec<BatchElement> [batch.rs, PURE]
   │  for q in queries: provider.search(q, count)
   │      Ok(hits)  → BatchElement::Ok{query, results, count}
   │      Err(e)    → BatchElement::Err{query, error: e.to_string()}
   ▼
json!({ "results": elements })
```

`provider` is the **existing** `Box<dyn SearchProvider>` (`&self` method), so `run_batch`
works for `DirectSearchProvider` (N GETs to SearxNG) and `BrokeredSearchProvider` (N JSON-RPC
`search` round-trips over the broker UDS) with **no provider change**.

The whole result value is screened by the **same** `tool_host` injection-guard output sink
that already screens `web.search` — the batch carries the same SearxNG-snippet content type,
just more of it. **No new injection surface.**

## 5. Component-by-component changes

### 5.1 `workers/web-search/src/batch.rs` — NEW (pure core)
The testable heart of the feature. No I/O, no network — depends only on the `SearchProvider`
trait and `Hit`.
- `struct BatchParams { queries: Vec<String>, count: Option<usize> }` (`Deserialize`).
- `enum BatchElement` (serialize to the two response shapes above). Model as an enum with a
  custom/`untagged`-style `Serialize`, or two structs behind `serde(untagged)` — whichever
  yields exactly the §3 JSON; unit tests pin the wire bytes.
- `pub fn run_batch(provider: &dyn SearchProvider, queries: &[String], count: usize)
  -> Vec<BatchElement>` — the sequential loop mapping `Ok`/`Err` per query. **Pure** (the
  only effect is the injected provider's `search`), unit-testable with a fake provider.
- `pub fn validate_batch(params: &BatchParams, max_batch: usize) -> Result<(), &'static str>`
  (or return the parsed/validated queries) — non-empty + `len <= max_batch`; the handler maps
  its `Err` to `INVALID_PARAMS`.
- `const DEFAULT_MAX_BATCH_QUERIES: usize = 8; const HARD_MAX_BATCH_QUERIES: usize = 32;`
- `pub fn resolve_max_batch(env_val: Option<&str>) -> usize` — pure: parse → clamp to
  `[1, HARD_MAX_BATCH_QUERIES]` → default `DEFAULT_MAX_BATCH_QUERIES` on unset/invalid.
- `const MAX_BATCH_QUERIES_ENV: &str = "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES";` (kept in
  sync with the manifest const — a `// keep in sync with core/src/workers/web_search.rs`
  note on both, mirroring python-exec's `PARAMS_FILE_MAX_ENV`).

Keeps `handler.rs` thin and under the 500-LOC target.

### 5.2 `workers/web-search/src/handler.rs`
- Add `max_batch: usize` to `WebSearchHandler`; set it in `from_env` via
  `resolve_max_batch(std::env::var(MAX_BATCH_QUERIES_ENV).ok().as_deref())`.
- Change the dispatch guard `if method != "web.search"` → a `match method`:
  - `"web.search"` — **unchanged** body (regression-pinned byte-identical).
  - `"web.search_batch"` — parse `BatchParams` (→ `INVALID_PARAMS` on serde error),
    `validate_batch(&p, self.max_batch)` (→ `INVALID_PARAMS`), `count = p.count.unwrap_or(DEFAULT_COUNT)`,
    `run_batch(&*self.provider, &p.queries, count)`, return `json!({"results": elements})`.
  - `_` — `METHOD_NOT_FOUND` (unchanged).
- Per-query errors use `SearchError`'s `Display` (a helper `search_err_to_string`), **not**
  `search_err_to_rpc` — a per-query failure is data, not an RPC error.
- The `#[cfg(test)] with_parts` constructor gains a `max_batch` (default 8) so existing
  single-search tests are unaffected.

### 5.3 `core/src/worker_manifest.rs` — defaulted `tool_docs()`
Add to the `WorkerManifest` trait (the desirable minimal-ripple change):
```rust
/// All planner-facing tool docs for this worker. Defaults to wrapping the
/// single `tool_doc()`, so single-method workers need no change. A worker that
/// serves several JSON-RPC methods (e.g. web-search: `web.search` +
/// `web.search_batch`) overrides this to advertise each.
fn tool_docs(&self) -> Vec<ToolDoc> {
    self.tool_doc().into_iter().collect()
}
```
`tool_doc()` stays as the single-doc convenience the other six manifests already implement;
**they are untouched.** Only the collection sites move to `tool_docs()`.

### 5.4 `core/src/workers/web_search.rs`
- Add `const MAX_BATCH_QUERIES_ENV: &str = "KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES";`
  (keep-in-sync note with the worker const).
- Override `tool_docs()` to return **both** docs, **both** with `name: TOOL_NAME`
  (`"web-search"`) so the drift guard `doc.name == manifest.name()` still holds:
  - `web.search` — the existing doc verbatim.
  - `web.search_batch` — `method: "web.search_batch"`, summary explaining "several
    independent searches in one call; returns per-query result groups", params
    `[queries (required: "list of independent search queries to run in one batch"),
    count (optional)]`.
  - **No numeric ceiling is advertised.** The batch size is a runtime, operator-tunable
    preference (`KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`, §6), and `ToolDoc` params are
    `&'static str` — a baked-in number would misrepresent a value the operator can change, so
    the description names no cap. Enforcement stays fail-closed at the worker: a batch that
    exceeds the effective runtime cap is rejected with `INVALID_PARAMS` (whose message names
    the effective cap) and surfaced to the planner via the existing feedback arc — never
    silently truncated.
- Inject the cap env into **both** entry builders **only when the operator set it**, mirroring
  python-exec: read `(ctx.get_env)(MAX_BATCH_QUERIES_ENV)` in `resolve`, and when `Some`, push
  `(MAX_BATCH_QUERIES_ENV, value)` onto the entry's `policy.env`. When unset, nothing is
  injected and the worker uses its built-in default 8 → **byte-identical `policy.env` to today**.
  (Implementation choice: thread the optional value into `web_search_entry` /
  `web_search_broker_entry` as a parameter, or push it after construction — either keeps the
  no-override path byte-identical; the plan picks one.)

### 5.5 `core/src/registry_build.rs`
- The two doc-collection sites (`:171`, `:365`) switch `m.tool_doc()` →
  `for doc in m.tool_docs()` (flatten into the `docs` vec).
- The drift-guard test iterates `m.tool_docs()` and asserts `doc.name == m.name()` for **each**
  doc (both web-search docs use `name == "web-search"`, so it passes). Add a positive
  assertion that web-search now advertises **two** methods including `web.search_batch`.
- Note: the guard pins `doc.name`, not `doc.method` — that residual is already tracked by
  [#438](https://github.com/hherb/kastellan/issues/438) and is out of scope here.

### 5.6 `prompts/agent_planner.md`
Add one guidance line near the existing "prefer single-step tools" paragraph (≈ line 267):
> When you need several **independent** web searches, issue them as a single
> `web.search_batch` call (its `queries` array) rather than separate `web.search` steps —
> this resolves them in one planning step. Use plain `web.search` for a single query or when
> a later query depends on an earlier result.

The `<tools>` block already surfaces `web.search_batch` automatically via `tool_docs()`, so no
tool list is hard-coded in the prompt.

## 6. Configuration

| Env var (set on the **daemon**) | Default | Effect |
|---|---|---|
| `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` | `8` (worker built-in) | Max queries per `web.search_batch`. Parsed, clamped to `[1, 32]`. Injected into the worker jail only when set; unset ⇒ no env injected ⇒ default 8. |

The installer env template (`kastellan.env`) gets a commented line documenting the var and its
default, alongside the other `KASTELLAN_WEB_SEARCH_*` entries.

## 7. Error semantics (summary)

| Condition | Result |
|---|---|
| `queries` missing / not an array / non-string element | whole-call `INVALID_PARAMS` |
| `queries` empty (`[]`) | whole-call `INVALID_PARAMS` |
| `queries.len() > max_batch` | whole-call `INVALID_PARAMS` (message names the cap) |
| one query fails (empty string, non-200, transport, policy) | that element = `{query, error}`; batch stays `Ok` |
| all queries fail | `{results:[…]}` with every element an `error`; batch stays `Ok` |
| method not `web.search` / `web.search_batch` | `METHOD_NOT_FOUND` (unchanged) |
| whole worker exceeds 30 s wall | `tool_host` wall watchdog kills it (unchanged; the real backstop against a pathological cap) |

## 8. Testing plan (TDD)

**`workers/web-search` unit (`batch.rs`, with a fake `SearchProvider`):**
- `run_batch` happy path — 3 queries, all succeed, order preserved, each element mirrors a
  single-search body.
- `run_batch` mixed — query 2 returns `Err`, queries 1 & 3 succeed; element 2 is `{query,error}`,
  batch length == 3, order preserved.
- `run_batch` all-fail — every element an `error`.
- `resolve_max_batch` — unset → 8; `"3"` → 3; `"0"`/`"-1"`/`"abc"` → 8; `"999"` → 32 (clamp);
  `"32"` → 32.
- `validate_batch` — empty → Err; `len == max` → Ok; `len == max+1` → Err.
- Serialization pins — a success element and an error element serialize to exactly the §3 JSON
  (guards the `untagged`/custom `Serialize`).

**`workers/web-search` handler:**
- `web.search_batch` well-formed → `{results:[…]}` with the expected element count/shapes.
- `web.search_batch` empty `queries` / over-cap → `INVALID_PARAMS`.
- `web.search` single-query response **byte-identical** to today (regression pin).
- unknown method → `METHOD_NOT_FOUND`.
- `from_env` honours `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` (set → clamped; unset → 8).

**`core` manifest / registry:**
- `WebSearchManifest::tool_docs()` returns two docs; both `name == "web-search"`; methods are
  `web.search` and `web.search_batch`.
- The default `tool_docs()` on a single-doc manifest wraps its `tool_doc()` (one doc).
- drift guard passes for both docs; positive assertion that `web.search_batch` is advertised.
- Manifest env injection: with `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` set, both entries carry
  the env pair; unset ⇒ `policy.env` byte-identical to today (both direct and broker entries).
- The rendered `<tools>` block (`prompt_assembly`) includes a `web.search_batch` line.

**e2e:** none required — the batch rides the same spawn/dispatch/broker paths already covered
by `web_search_e2e` and the broker e2e. **Optional, DGX-gated:** extend the existing `#[ignore]`
`real_search_against_searxng` with a 2-query `web.search_batch` assertion (cheap; proves the
live loop end-to-end). Flag it in the plan as optional.

## 9. Scope / non-goals (YAGNI)

**Out of scope:**
- Parallel fan-out (worker-side or a broker `search_batch` method) — rejected; the broker
  serves serially and plan-iteration collapse is the real win. If per-query network latency
  ever dominates, add a broker batch method later (the seam is method-agnostic, so it's
  additive).
- Per-query `count`, dedup of duplicate queries, category/language/engine params.
- Any change to `web-common`, the search-broker crate, or the core `broker/` module.
- `web.search` behaviour — must stay byte-identical (regression-pinned).

**Non-goals that remain true:** injection screening is unchanged (same sink, same content
type); no new egress surface (the batch reaches SearxNG only through the same allowlist/broker
route as a single search); cross-platform posture unchanged (pure Rust, no OS-specific code).

## 10. Files touched

| File | Change |
|---|---|
| `workers/web-search/src/batch.rs` | **NEW** — pure `run_batch` + `BatchParams`/`BatchElement` + `resolve_max_batch`/`validate_batch` + consts |
| `workers/web-search/src/handler.rs` | `match` dispatch + `web.search_batch` arm; `max_batch` field; `from_env` reads the cap |
| `workers/web-search/src/lib.rs` | `mod batch;` (+ re-exports if needed) |
| `core/src/worker_manifest.rs` | defaulted `tool_docs()` on the trait |
| `core/src/workers/web_search.rs` | `tool_docs()` override (two docs) + cap-env const + only-when-set injection into both entries |
| `core/src/registry_build.rs` | two collection sites → `tool_docs()`; drift guard iterates; +batch assertion |
| `prompts/agent_planner.md` | one guidance line for `web.search_batch` |
| `kastellan.env` template (installer) | commented `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES` line |

**Untouched:** `workers/web-common`, `workers/search-broker`, `core/src/broker/*`.
