# Design: `web-search` worker

**Date:** 2026-06-09
**ROADMAP item:** "`web-search` worker (SearxNG default)" (ROADMAP.md:146, Phase 3)
**Status:** approved, pre-implementation

## Problem

The `web-fetch` worker (PR #197) gave the agent a way to *read* a known URL, but
there is still no way for the agent to *discover* URLs — to turn a question into a
ranked list of candidate sources. Without search, the "submit a research task, get
a report" loop can only follow links the user already supplied.

This worker is the natural next net-egress tool: it queries an operator-configured
[SearxNG](https://docs.searxng.org/) meta-search instance and returns structured
hits (`title`, `url`, `snippet`). The agent then `web-fetch`es the URLs it judges
worth reading. Clean single-responsibility split: **web-search finds, web-fetch
reads.**

## Decision summary

Add a tool worker `kastellan-worker-web-search` exposing a single JSON-RPC method
`web.search` that takes a query string and returns a ranked list of result hits
from a SearxNG instance's JSON API (`/search?format=json`). It mirrors the
`web-fetch` worker pattern: a small Rust binary using
`kastellan_worker_prelude::serve_stdio` (which calls `lock_down()` before serving),
plus a host-side `WorkerManifest` in `core/src/workers/web_search.rs` declaring the
`SandboxPolicy` (`Net::Allowlist` + `Profile::WorkerNetClient`).

Two pieces that `web-fetch` already owns are needed verbatim by `web-search`: the
host **allowlist matcher** and an HTTP **transport seam**. Rather than copy the
security-critical allowlist matcher into a second crate that must stay in sync, we
**extract a shared `workers/web-common` lib crate** as part of this work, and
re-point `web-fetch` at it (behaviour byte-preserved). This is the rule-1 reuse
move and the single-source-of-truth for the matcher.

### Responsibility boundary

`web.search` returns **structured hits only** — no page bodies. The payload is a
small bounded list of `{title, url, snippet, engine}`. The agent issues follow-up
`web.fetch` calls for the URLs it wants to read. We deliberately do **not** fan out
fetches from inside the search worker: that would couple the two workers, multiply
egress inside one sandbox, and produce large (handoff-cache-territory) payloads.

## Architecture

### New crate: `workers/web-common` (lib)

Extract from `web-fetch` the two genuinely reusable, security-relevant pieces:

- **`allowlist.rs`** — `HostAllowlist` (`from_env_json` + `is_allowed`; exact +
  `.domain` subdomain-wildcard, case-insensitive). Moved verbatim from
  `web-fetch`; its unit tests move with it. **Host-matching only — scheme-agnostic.**
- **`http.rs`** — the `HttpGet` trait + `RawResponse` + `ReqwestGet` (the
  redirect-disabled, body-capped `reqwest::blocking` + rustls transport;
  `TIMEOUT_SECS`, `MAX_BODY_BYTES`). Generic user-agent `kastellan/0` (was
  `kastellan-web-fetch/0`).
- **`testing.rs`** (behind a `testing` cargo feature) — `FakeGet` + the
  `ok_resp`/`redirect_to`/`al` test helpers, so both workers' unit suites share the
  one fake transport. Consumed via `web-common = { features = ["testing"] }` in each
  worker's `[dev-dependencies]`.

`web-fetch` keeps only its fetch-specific code: `extract.rs` (HTML/PDF/text
extraction) and the `drive()` redirect-following loop + `FetchError` /
`FetchOutcome` / `MAX_REDIRECTS` in `fetch.rs`, now importing `HttpGet` /
`RawResponse` / `ReqwestGet` from `web-common`. No behaviour change; its 29 unit
tests + e2e stay green (the allowlist tests now live in `web-common`).

### New crate: `workers/web-search` (bin)

- **`search.rs`** (pure, the heart) — given the endpoint `Url`, the allowlist, the
  query, and the count cap: validate the endpoint (scheme + host, see below); build
  the request URL (`?q=<query>&format=json`, preserving any path the operator
  configured); do **one** GET via the `HttpGet` seam; treat any 3xx or non-200 as
  an error (a search endpoint redirecting is anomalous — fail closed, do not
  follow); hand the body to the parser; return a `Vec<Hit>` sliced to the count cap.
- **`parse.rs`** (pure) — serde structs for the SearxNG `/search?format=json`
  subset: `{ results: [{ title?, url, content?, engine? }] }`. Maps each to
  `Hit { title, url, snippet, engine }` (`content` → `snippet`). Lenient: entries
  without a `url` are skipped; missing `title`/`content`/`engine` default to empty.
- **`handler.rs`** — `web.search` JSON-RPC dispatch over the `HttpGet` seam. Params
  `{ query: String, count?: usize }` (default 10, hard cap 20 — a client-side slice;
  SearxNG has no result-count param, it returns a page). Error vocab:
  `INVALID_PARAMS` (missing/empty query), `POLICY_DENIED` (endpoint host not on the
  allowlist, or disallowed scheme), `OPERATION_FAILED` (transport / non-200 /
  redirect / JSON parse), `METHOD_NOT_FOUND`.
- **`main.rs`** — `serve_stdio` + `WebSearchHandler::from_env`, which reads
  `KASTELLAN_WEB_SEARCH_ENDPOINT` + `KASTELLAN_WEB_SEARCH_ALLOWLIST`, parses + validates
  the endpoint **at startup**, and **fails closed** (returns `Err`, the worker never
  serves) if the endpoint is missing, unparseable, has a disallowed scheme, or its
  host is not on the allowlist.

**Wire contract:**

```
web.search { "query": "<text>", "count": <1..=20, optional> }
  → { "query": "<text>",
      "results": [ { "title": "...", "url": "...", "snippet": "...", "engine": "..." }, ... ],
      "count": <results.len()> }
```

### Endpoint scheme rule (the one deviation from web-fetch)

`web-fetch` is strictly HTTPS-only because it fetches *arbitrary* LLM-influenced
URLs (SSRF surface). `web-search` talks to exactly **one operator-configured
endpoint** and the LLM supplies only the query string — there is no URL-injection
surface. So the rule is relaxed *only* for loopback, to make a local self-hosted
SearxNG trivial to run:

- **https://** — allowed for any allowlisted host.
- **http://** — allowed **only** when the endpoint host is loopback
  (`localhost`, or an IP that parses as `IpAddr::is_loopback()` — covers `127.0.0.0/8`
  and `::1`). `http://` to any non-loopback host is `POLICY_DENIED`.

A small pure `is_loopback(host: &str) -> bool` helper in `search.rs`: parse the host
as `IpAddr` and use `.is_loopback()`; if it does not parse as an IP, return
`host.eq_ignore_ascii_case("localhost")`.

### Host-side manifest: `core/src/workers/web_search.rs`

`WebSearchManifest` + `web_search_entry(binary, endpoint, allowlist)`, mirroring
`web_fetch.rs`:

- `Profile::WorkerNetClient` (permits `socket(2)`), `Net::Allowlist` derived from
  the **endpoint URL's host:port** — `host:port` where `port` is the explicit port,
  else 443 for https / 80 for http. (This is why we derive from the endpoint, not
  from the domain list mapped to `:443` as web-fetch does — a loopback endpoint on
  `:8888` must produce `127.0.0.1:8888`, not `:443`.) If the endpoint is
  unparseable, `Net::Allowlist` is empty and the worker fails closed at startup
  anyway.
- `fs_read`: the worker binary + `/etc/{resolv.conf,hosts,nsswitch.conf}` (DNS under
  `--unshare-all`).
- `cpu_ms = 5_000`, `mem_mb = 256` (lighter than web-fetch's 512 — JSON parsing
  only, no HTML readability or PDF), `wall_clock_ms = Some(30_000)`, `SingleUse`.
- Env injected: `KASTELLAN_WEB_SEARCH_ENDPOINT` (read on the host from the daemon's
  own env via `ctx.get_env` — operator-controlled, **not** LLM-supplied) and
  `KASTELLAN_WEB_SEARCH_ALLOWLIST` (the `tool_allowlists` rows keyed `"web-search"`,
  same governance path web-fetch uses).
- `allowlist_tool()` → `Some("web-search")`. Registered in `WORKER_MANIFESTS`.

The `"web-search"` tool_allowlist gates which host the endpoint may name (defense
in depth: even though the endpoint is operator-set, the worker re-checks its host ∈
allowlist at startup). The operator keeps the endpoint host and the allowlist
consistent — the same "represent the allowlist from one source" philosophy as
web-fetch.

### Containment caveat (same as web-fetch)

Until the egress proxy lands, `Net::Allowlist` is enforced *inside* the worker
(scheme + host check) and matches host **names, not resolved IPs** — so it does not
contain SSRF / DNS-rebinding to internal addresses. The proxy (ROADMAP:141) will
own IP-level containment and read this worker's `Net::Allowlist` data. This caveat
is repeated in the `web_search.rs` rustdoc; the existing "Network egress" note in
`threat-model.md` already covers all net workers and is referenced.

## SearxNG setup script

Local SearxNG runs as a Docker container that serves **plain HTTP on a loopback
port** and — importantly — **disables the JSON format by default**. A setup script
makes the dev path one command. `scripts/web-search/setup-searxng.sh` (bash,
cross-platform: Docker Desktop on macOS, docker/podman on Linux):

1. Detect a container runtime (`docker` or `podman`); error with guidance if none.
2. Write a `settings.yml` into a state dir with a random `server.secret_key` and
   **`search.formats: [html, json]`** (the gotcha — JSON is off by default).
3. Run `searxng/searxng` bound to `127.0.0.1:8888` with that settings file mounted,
   under a stable container name (idempotent: reuse/restart if already present).
4. Print the two env lines to export:
   - `KASTELLAN_WEB_SEARCH_ENDPOINT=http://127.0.0.1:8888/search`
   - `KASTELLAN_WEB_SEARCH_ALLOWLIST=["127.0.0.1"]`

The script is a dev convenience, not part of the worker's trust boundary; it is
documented in the worker crate README / HANDOFF, not invoked by the daemon.

## Testing (TDD)

- **`web-common` unit** — the moved allowlist matcher tests (exact / wildcard /
  case / lookalike / empty / malformed-json / trim / lone-dot) + a couple for the
  `ReqwestGet` body cap shape. Fully hermetic.
- **`web-search` unit** — against `FakeGet`: request-URL build (q + format=json,
  path preserved), `is_loopback` truth table, scheme rule (https ok, http-loopback
  ok, http-remote denied), host-not-allowlisted denied, JSON parse (happy,
  missing-url skipped, missing-title/content defaulted, malformed → error),
  count default + cap, and the handler error arms (unknown method, missing query,
  non-200, redirect). Hermetic — no network.
- **`core` integration `web_search_e2e.rs`** — mirrors `web_fetch_e2e.rs`:
  - `host_outside_allowlist_is_denied` (hermetic: endpoint host not on allowlist →
    worker refuses at startup / `POLICY_DENIED`, real sandbox, no server),
  - `#[ignore]` `real_search_against_searxng` (needs a live instance; reads
    `KASTELLAN_WEB_SEARCH_ENDPOINT`; validates DNS+TLS/loopback in-jail and a real
    SearxNG round-trip).
  - `[SKIP]`s cleanly when PG / supervisor / worker binary / sandbox are missing.
- **`core` unit (manifest)** — `web_search.rs` resolve tests: registers with
  `WorkerNetClient` + endpoint-derived `Net::Allowlist` (incl. the loopback-port
  case) + both env vars; `Misconfigured` when no binary found. Mirrors web-fetch's
  manifest tests.

## Files

```
workers/web-common/              NEW lib crate
  Cargo.toml                     reqwest(blocking)+rustls, url, serde, serde_json, anyhow; feature "testing"
  src/lib.rs                     pub mod allowlist; pub mod http; #[cfg(feature="testing")] pub mod testing;
  src/allowlist.rs               moved from web-fetch (HostAllowlist)
  src/http.rs                    moved from web-fetch (HttpGet/RawResponse/ReqwestGet + caps)
  src/testing.rs                 moved from web-fetch test_transport.rs, feature-gated

workers/web-fetch/               MODIFIED
  Cargo.toml                     depends on web-common (+ dev-dep testing feature); drops its direct `reqwest` dep (now transitive via web-common); keeps `url`, `pdf-extract`, `readable_html` (used by fetch.rs/extract.rs)
  src/allowlist.rs               DELETED (now web-common)
  src/test_transport.rs          DELETED (now web-common::testing)
  src/fetch.rs                   keeps drive()/FetchError/FetchOutcome/MAX_REDIRECTS; imports transport from web-common
  src/handler.rs / extract.rs    unchanged behaviour, import path updates

workers/web-search/              NEW bin crate
  Cargo.toml
  src/main.rs                    mod search/parse/handler; serve_stdio
  src/search.rs                  pure: endpoint validation + request build + drive-one-GET + count cap
  src/parse.rs                   pure: SearxNG JSON → Vec<Hit>
  src/handler.rs                 web.search dispatch + error vocab

core/src/workers/web_search.rs   NEW host-side manifest (WebSearchManifest + web_search_entry)
core/src/workers.rs              + pub mod web_search;
core/src/registry_build.rs       + &WebSearchManifest in WORKER_MANIFESTS
core/tests/web_search_e2e.rs     NEW (hermetic deny + #[ignore] real)
scripts/web-search/setup-searxng.sh  NEW dev setup script
Cargo.toml (workspace)           + workers/web-common, workers/web-search members
```

## Deferred (YAGNI)

- Category / language / engine / safe-search params on `web.search` (add when a
  caller needs them).
- Pagination (`pageno`) — the count cap on page 1 is enough for the research loop.
- web-search-side content fetching (that is web-fetch's job, by design).
- Hermetic SearxNG mock e2e behind a test server (the unit suite's `FakeGet`
  already covers the JSON contract; the real round-trip stays `#[ignore]`).

## Key decisions (recap)

- **Structured hits only** — web-search finds, web-fetch reads.
- **Shared `web-common` crate** — single source of truth for the allowlist matcher
  + transport seam; web-fetch re-pointed, behaviour byte-preserved.
- **Operator-configured endpoint** via `KASTELLAN_WEB_SEARCH_ENDPOINT`; LLM supplies
  only the query — no URL-injection surface.
- **http allowed for loopback only**; https mandatory for every other host.
- **`Net::Allowlist` derived from the endpoint host:port** (correct for loopback
  custom ports); the `"web-search"` tool_allowlist gates the endpoint host.
- **Setup script** stands up a local SearxNG with the JSON format enabled.
