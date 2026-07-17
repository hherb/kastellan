# web-research × search-broker (single-broker XOR) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give web-research the search-broker option web-search already has — a force-routed (or VM) web-research worker reaches a loopback/name-form SearxNG through a core-spawned trusted search-broker sidecar with **zero direct search egress** — as a search XOR embed choice under the single-broker-per-worker model.

**Architecture:** Lift web-search's `SearchProvider` seam (trait + direct/brokered providers + chooser) into `web-common` behind the `search` feature; rework web-research's `research()`/handler over that seam; add two core manifest entries (`web_research_search_broker_entry` host + `web_research_firecracker_search_broker_entry` VM) carrying `BrokerSpec::search(endpoint)`; make `resolve()` a three-way broker choice (none / embed / search) × (host / VM) with both-flags → `Misconfigured`. **No new sandbox/microvm/broker mechanism** — the `kastellan-worker-search-broker` binary, the kind-agnostic spawn chokepoint, and the single vsock-1026 channel are used as-is (#440/#446/#451 proved each piece).

**Tech Stack:** Rust workspace; crates touched: `kastellan-worker-web-common`, `kastellan-worker-web-search` (re-point only), `kastellan-worker-web-research`, `kastellan-core` (manifest + tests), plus one new `core/tests/` e2e file.

**Spec:** `docs/superpowers/specs/2026-07-16-web-research-search-broker-arc-design.md`. **Issue:** [#464](https://github.com/hherb/kastellan/issues/464). **Branch:** `feat/web-research-search-broker` (off `main`@`6584b87a`).

## Global Constraints

- AGPL-compatible dependencies only. (This plan adds NO new external deps — `kastellan-protocol` becomes an *optional* internal dep of web-common.)
- Cross-platform: Linux + macOS. All Firecracker/VM code stays behind `#[cfg(target_os = "linux")]` (issue-#144 rule: on macOS the `FirecrackerVm` variant and `USE_MICROVM` env must never be referenced).
- Every worker sandboxed; no unsandboxed spawn path. This plan only *narrows* worker egress (SearxNG host leaves `Net::Allowlist` in broker mode).
- Host direct-mode behaviour must stay **byte-identical** when the new gate env is unset (existing tests are the pin; do not weaken them).
- Files ≤ 500 LOC where feasible — Task 3 (test-lift) exists to keep `core/src/workers/web_research.rs` under control before Task 4 grows it.
- TDD: each task writes its failing tests first, then the implementation. Run cargo in the **FOREGROUND** (never as background jobs — subagents have wedged waiting on them; standing dispatch rule).
- Commit per task; stage **specific files** (`git add <paths>` — never `git add -A`).
- End commit messages with: `Co-Authored-By: Claude <noreply@anthropic.com>` per repo convention (check `git log` for the exact trailer used on this machine and match it).
- Mac verification per task: the named `cargo test -p <crate>` runs + `cargo clippy -p <crate> --all-targets -- -D warnings`. Linux-gated tests and live e2es are the Task 6 DGX gate.

---

### Task 1: Lift the search-provider seam into `web-common::search_provider`

The seam (`SearchProvider` trait, `SearchProviderChoice` + `choose_search_provider`, `DirectSearchProvider`, `BrokeredSearchProvider`) and the `SearchError → RpcError` mapper (`search_err_to_rpc`, today duplicated **verbatim** in both workers' handlers) currently live in `workers/web-search/src/handler.rs`. Move them to web-common so web-research can consume them. This is the established web-common consolidation pattern (2026-07-07): **behaviour byte-preserved, code moved verbatim, consumers re-pointed**.

**Files:**
- Create: `workers/web-common/src/search_provider.rs`
- Modify: `workers/web-common/Cargo.toml`
- Modify: `workers/web-common/src/lib.rs`
- Modify: `workers/web-search/src/handler.rs` (delete moved code, re-point imports)
- Modify: `workers/web-search/src/batch.rs` (re-point two imports)
- Modify: `workers/web-research/src/handler.rs` (delete its duplicate `search_err_to_rpc`, re-point — the *seam* adoption is Task 2; only the mapper re-point lands here)

**Interfaces:**
- Consumes: `web_common::search::{search, SearchError}`, `web_common::allowlist::HostAllowlist`, `web_common::http::HttpGet`, `web_common::parse::Hit`, `kastellan_protocol::{Request, Response, RpcError, codes, read_capped_record, Record, MAX_RECORD_BYTES}`.
- Produces (later tasks rely on these exact paths):
  - `kastellan_worker_web_common::search_provider::SearchProvider` — `fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError>`
  - `kastellan_worker_web_common::search_provider::{SearchProviderChoice, choose_search_provider}` — `fn choose_search_provider<'a>(broker_uds: Option<&'a str>, endpoint: Option<&'a str>) -> SearchProviderChoice<'a>`
  - `kastellan_worker_web_common::search_provider::DirectSearchProvider<T: HttpGet>` — `fn new(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self`
  - `kastellan_worker_web_common::search_provider::BrokeredSearchProvider` — `fn new(uds: PathBuf) -> Self`
  - `kastellan_worker_web_common::search_provider::search_err_to_rpc` — `fn search_err_to_rpc(e: SearchError) -> RpcError`

- [ ] **Step 1: Wire the optional protocol dep + feature + dev-dep**

In `workers/web-common/Cargo.toml`, add to `[dependencies]`:

```toml
kastellan-protocol = { path = "../../protocol", version = "0.1.0", optional = true }
```

Change the `search` feature line to:

```toml
search  = ["dep:kastellan-protocol"]
```

(update the feature's doc comment: it is no longer "no extra deps" — it now pulls the internal `kastellan-protocol` for the brokered provider). Add a dev-dependencies section (the moved broker tests bind real Unix sockets in a tempdir):

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create `workers/web-common/src/search_provider.rs` with the moved code**

Move **verbatim** from `workers/web-search/src/handler.rs` (adjusting only `use` paths from `kastellan_worker_web_common::X` to `crate::X`, and visibility of `search_err_to_rpc` from `pub(crate)` to `pub`): the `search_err_to_rpc` fn, the `SearchProvider` trait, `SearchProviderChoice` + `choose_search_provider`, `DirectSearchProvider`, `BrokerSearchResult` (stays private), `BrokeredSearchProvider`, and — into a `#[cfg(test)] mod tests` — the six seam tests + the `stub_broker` helper (`choose_broker_wins_when_both_set`, `choose_endpoint_when_only_endpoint_set`, `choose_none_when_neither_and_blank_is_unset`, `brokered_search_round_trip_returns_hits`, `brokered_search_maps_broker_error`, `brokered_search_absent_socket_is_transport_error`). Module header:

```rust
//! The search-provider seam shared by web-search and web-research: one trait
//! (`SearchProvider`) with a direct SearxNG implementation and a brokered one
//! that reaches SearxNG only through the trusted search-broker sidecar's UDS
//! (zero worker search egress). `choose_search_provider` is the pure
//! precedence rule (broker UDS wins over a direct endpoint); the
//! `SearchError → RpcError` mapper lives here too so both workers share one
//! error vocabulary. Lifted verbatim from web-search's handler (2026-07-17,
//! #464) so web-research can adopt the same seam.
```

- [ ] **Step 3: Register the module in `workers/web-common/src/lib.rs`**

Below the existing `search` module lines:

```rust
#[cfg(feature = "search")]
pub mod search_provider;
```

and add a line to the lib doc list: `//! - [`search_provider`] (feature `search`) — the direct/brokered SearchProvider seam + RpcError mapper.`

- [ ] **Step 4: Re-point web-search**

In `workers/web-search/src/handler.rs`: delete the moved items and their six tests + `stub_broker`; add

```rust
use kastellan_worker_web_common::search_provider::{
    choose_search_provider, search_err_to_rpc, BrokeredSearchProvider, DirectSearchProvider,
    SearchProvider, SearchProviderChoice,
};
```

In `workers/web-search/src/batch.rs`: replace `use crate::handler::{search_err_to_rpc, SearchProvider};` with `use kastellan_worker_web_common::search_provider::{search_err_to_rpc, SearchProvider};`.

In `workers/web-research/src/handler.rs`: delete its private duplicate `fn search_err_to_rpc` and add `use kastellan_worker_web_common::search_provider::search_err_to_rpc;` (nothing else changes in web-research this task).

- [ ] **Step 5: Run the moved + downstream tests**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-common --features search,testing
cargo test -p kastellan-worker-web-search
cargo test -p kastellan-worker-web-research
cargo clippy -p kastellan-worker-web-common --features search,testing --all-targets -- -D warnings
cargo clippy -p kastellan-worker-web-search -p kastellan-worker-web-research --all-targets -- -D warnings
```

Expected: web-common gains the 6 moved tests (all green); web-search count drops by exactly those 6, everything green (handler/batch behaviour untouched); web-research unchanged and green; clippy clean.

- [ ] **Step 6: Commit**

```sh
git add workers/web-common/Cargo.toml workers/web-common/src/lib.rs workers/web-common/src/search_provider.rs workers/web-search/src/handler.rs workers/web-search/src/batch.rs workers/web-research/src/handler.rs Cargo.lock
git commit -m "refactor(web-common): lift the SearchProvider seam + search_err_to_rpc out of web-search (#464)"
```

---

### Task 2: Rework web-research's search step over the `SearchProvider` seam

`research()` stops calling the free `search()` fn against an endpoint and instead consumes a `&dyn SearchProvider`; the handler builds the provider at startup exactly like web-search's `from_env` (broker UDS wins; endpoint not required in broker mode). The content-fetch path (transport + allowlist) and the embedder selection are untouched.

**Files:**
- Modify: `workers/web-research/src/research.rs` (signature + call site + tests)
- Modify: `workers/web-research/src/handler.rs` (struct + `from_env` + tests)

**Interfaces:**
- Consumes (from Task 1): `web_common::search_provider::{SearchProvider, SearchProviderChoice, choose_search_provider, DirectSearchProvider, BrokeredSearchProvider, search_err_to_rpc}`.
- Produces: `research(search: &dyn SearchProvider, transport: &T, allowlist: &HostAllowlist, embedder: Option<&dyn Embedder>, query: &str, max_sources: usize, max_passages: usize) -> Result<ResearchOutcome, ResearchError>` — Task 5's hermetic tests and the existing `web_research_e2e` rely on the worker's *wire* behaviour being unchanged in direct mode.

- [ ] **Step 1: Write the failing tests (worker selects the brokered provider; direct mode byte-identical)**

In `workers/web-research/src/handler.rs` tests: add a brokered round-trip test that binds a stub broker UDS (same `stub_broker` shape as web-common's moved test — a one-shot `UnixListener` returning a canned `{"results":[...]}` JSON-RPC reply), builds the handler with `BrokeredSearchProvider`, and asserts a `web.research` call searches via the broker then fetches content via the fetch transport:

```rust
#[test]
fn brokered_search_feeds_research_pipeline() {
    use kastellan_worker_web_common::search_provider::BrokeredSearchProvider;
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("search.sock");
    let broker = stub_broker(
        sock.clone(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"results":[{"title":"Doc","url":"https://docs.example.org/bwrap","snippet":"c","engine":"e"}]}}"#.to_string(),
    );
    let page = "bwrap creates user namespaces for sandboxing workers.";
    let mut h = WebResearchHandler::with_parts(
        Box::new(BrokeredSearchProvider::new(sock)),
        al(&["searx.example.org", "docs.example.org"]),
        FakeGet::new(vec![RawResponse { status: 200, location: None,
            content_type: "text/plain".into(), body: page.as_bytes().to_vec() }]),
    );
    let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
    assert_eq!(out["sources_fetched"], 1);
    assert_eq!(out["sources"][0]["url"], "https://docs.example.org/bwrap");
    broker.join().unwrap();
}
```

(`stub_broker` is test-local here — copy the 11-line helper from web-common's `search_provider::tests`; it is not exported.) Run: `cargo test -p kastellan-worker-web-research brokered_search_feeds_research_pipeline` — expected: FAIL to compile (`with_parts` has the old signature).

- [ ] **Step 2: Change `research()`**

In `workers/web-research/src/research.rs`:

```rust
use kastellan_worker_web_common::search_provider::SearchProvider;

pub fn research<T: HttpGet>(
    search: &dyn SearchProvider,
    transport: &T,
    allowlist: &HostAllowlist,
    embedder: Option<&dyn Embedder>,
    query: &str,
    max_sources: usize,
    max_passages: usize,
) -> Result<ResearchOutcome, ResearchError> {
```

and the call site becomes:

```rust
    let hits = search.search(query, SEARCH_COUNT).map_err(ResearchError::Search)?;
```

Drop the now-unused `use kastellan_worker_web_common::search::{search, SearchError};` import of the free fn (keep `SearchError` — `ResearchError::Search` wraps it) and the `endpoint: &Url` parameter everywhere. Update the module doc's flow line ("`search()` the SearxNG endpoint" → "run the configured `SearchProvider` (direct SearxNG or the search-broker UDS)").

- [ ] **Step 3: Update `research.rs` tests**

Every test currently builds ONE `FakeGet` whose queue serves the search response first, then the page fetches. Split per test: the search response(s) go into a `DirectSearchProvider` and the page responses stay on the fetch transport. Add one helper beside the existing `endpoint()`/`al()` helpers and mechanically rewrite the ~15 call sites:

```rust
use kastellan_worker_web_common::search_provider::DirectSearchProvider;

/// Direct provider over the standard test endpoint/allowlist, fed only the
/// SEARCH responses (page fetches stay on the separate fetch transport).
fn direct(search_responses: Vec<RawResponse>) -> DirectSearchProvider<FakeGet> {
    DirectSearchProvider::new(
        endpoint(),
        al(&["searx.example.org", "docs.example.org"]),
        FakeGet::new(search_responses),
    )
}
```

A call that was `research(&t, &endpoint(), &a, None, "q", 3, 3)` with `t = FakeGet::new(vec![search_json, page1, page2])` becomes:

```rust
let s = direct(vec![search_json]);
let t = FakeGet::new(vec![page1, page2]);
let out = research(&s, &t, &a, None, "q", 3, 3);
```

The empty-query test keeps a `direct(vec![])` provider (never called). The search-failure test moves its 500 response into the provider's queue.

- [ ] **Step 4: Rework the handler**

In `workers/web-research/src/handler.rs`:

```rust
use kastellan_worker_web_common::search_provider::{
    choose_search_provider, search_err_to_rpc, BrokeredSearchProvider, DirectSearchProvider,
    SearchProvider, SearchProviderChoice,
};

pub struct WebResearchHandler<T: HttpGet> {
    search: Box<dyn SearchProvider>,
    allowlist: HostAllowlist,
    transport: T,
    embedder: Option<Box<dyn Embedder>>,
}
```

`from_env` (broker UDS wins; the endpoint env is REQUIRED only in direct mode; the content allowlist is required in both modes — it gates every fetched page):

```rust
pub fn from_env() -> anyhow::Result<Self> {
    let allow_raw =
        std::env::var("KASTELLAN_WEB_RESEARCH_ALLOWLIST").unwrap_or_else(|_| "[]".into());
    let allowlist = HostAllowlist::from_env_json(&allow_raw)?;
    // Search-provider selection mirrors web-search: the broker UDS
    // (KASTELLAN_SEARCH_BROKER_UDS, injected by core at spawn) wins over a
    // direct endpoint; in broker mode no endpoint env is needed — the broker
    // holds the only SearxNG route, so there is no endpoint host to validate.
    let broker_uds = std::env::var("KASTELLAN_SEARCH_BROKER_UDS").ok();
    let endpoint_raw = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT").ok();
    let search: Box<dyn SearchProvider> =
        match choose_search_provider(broker_uds.as_deref(), endpoint_raw.as_deref()) {
            SearchProviderChoice::Broker { uds } => {
                Box::new(BrokeredSearchProvider::new(std::path::PathBuf::from(uds)))
            }
            SearchProviderChoice::Endpoint { endpoint } => {
                // Direct mode keeps the #428 fail-closed rule: endpoint host must
                // be on the operator allowlist or the worker never serves.
                let url = validate_endpoint(endpoint, &allowlist)
                    .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
                // HostAllowlist is not Clone: parse the JSON a second time for the
                // provider's own copy (startup-only cost).
                let search_allowlist = HostAllowlist::from_env_json(&allow_raw)?;
                let search_transport = make_get("kastellan-web-research/0")?;
                Box::new(DirectSearchProvider::new(url, search_allowlist, search_transport))
            }
            SearchProviderChoice::None => anyhow::bail!(
                "web-research: neither KASTELLAN_SEARCH_BROKER_UDS nor \
                 KASTELLAN_WEB_RESEARCH_ENDPOINT set"
            ),
        };
    let transport = make_get("kastellan-web-research/0")?;
    // ... embedder selection: UNCHANGED from today (broker-vs-endpoint choose_embedder block) ...
    Ok(Self { search, allowlist, transport, embedder })
}
```

The `call()` body passes the provider through:

```rust
let out = research(
    &*self.search, &self.transport, &self.allowlist,
    self.embedder.as_deref(), &p.query, max_sources, max_passages,
).map_err(research_err_to_rpc)?;
```

Test ctor:

```rust
#[cfg(test)]
fn with_parts(search: Box<dyn SearchProvider>, allowlist: HostAllowlist, transport: T) -> Self {
    Self { search, allowlist, transport, embedder: None }
}
```

Existing handler tests: rewrite the `handler(responses)` helper to split queues the same way as research.rs (search responses → a `DirectSearchProvider` built from the same endpoint/allowlist; page responses → the fetch `FakeGet`).

- [ ] **Step 5: Run the tests**

```sh
cargo test -p kastellan-worker-web-research
cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings
```

Expected: all pre-existing tests green (direct-mode behaviour pinned), plus the new `brokered_search_feeds_research_pipeline` green; clippy clean.

- [ ] **Step 6: Commit**

```sh
git add workers/web-research/src/research.rs workers/web-research/src/handler.rs
git commit -m "feat(web-research): run the search step over the SearchProvider seam (broker UDS wins) (#464)"
```

---

### Task 3: Test-lift `core/src/workers/web_research.rs` → `web_research/tests.rs`

Pure mechanical move BEFORE Task 4 grows the file (mirrors the #451 `web_search.rs` lift — `core/src/workers/web_search.rs` ends in `#[cfg(test)] mod tests;` resolving to `web_search/tests.rs`; copy that exact pattern). Zero behaviour change.

**Files:**
- Create: `core/src/workers/web_research/tests.rs`
- Modify: `core/src/workers/web_research.rs` (delete the inline `#[cfg(test)] mod tests { ... }` block; append `#[cfg(test)] mod tests;`)

**Interfaces:** none — test-only move. Task 4 adds its new tests to the lifted file.

- [ ] **Step 1: Move the block**

Cut the entire inline `#[cfg(test)] mod tests { ... }` body (everything INSIDE the braces, byte-identical, starting `use super::*;` — compare with `core/src/workers/web_search/tests.rs` line 1 for the exact opening) into the new `core/src/workers/web_research/tests.rs`. Replace the block in the parent with:

```rust
#[cfg(test)]
mod tests;
```

- [ ] **Step 2: Verify counts unchanged**

```sh
cargo test -p kastellan-core --lib workers::web_research
wc -l core/src/workers/web_research.rs
```

Expected: identical test names/counts to before the move (compare `cargo test` output); parent file now ~530 LOC (prod only).

- [ ] **Step 3: Commit**

```sh
git add core/src/workers/web_research.rs core/src/workers/web_research/tests.rs
git commit -m "refactor(core): lift web_research.rs inline tests to web_research/tests.rs (Item 9b)"
```

---

### Task 4: Core manifest — XOR gate, search-broker entries (host + VM), guard remedy

**Files:**
- Modify: `core/src/workers/web_research.rs`
- Modify: `core/src/workers/web_research/tests.rs`

**Interfaces:**
- Consumes: `crate::broker::BrokerSpec::search(endpoint)` (exists, #451); `endpoint_guard::{forced_localhost_misconfig, egress_will_force_route, endpoint_is_localhost_name}` (exist).
- Produces:
  - `web_research_search_broker_entry(binary: PathBuf, endpoint: &str, embed_endpoint: Option<&str>, embed_model: Option<&str>, allowlist: &[String]) -> ToolEntry`
  - `#[cfg(target_os = "linux")] web_research_firecracker_search_broker_entry(binary: PathBuf, image_dir: String, endpoint: &str, embed_endpoint: Option<&str>, embed_model: Option<&str>, allowlist: &[String]) -> ToolEntry`
  - env const `KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER` (gate; Task 5/6 and the operator use it)

- [ ] **Step 1: Write the failing resolve tests** (in `core/src/workers/web_research/tests.rs`)

Follow the existing tests' `ResolveCtx` fixture style in that file. New tests:

```rust
#[test]
fn both_broker_flags_is_misconfigured() {
    // USE_SEARCH_BROKER=1 + USE_EMBED_BROKER=1 (+ an embed endpoint so the embed
    // flag would otherwise be effective) → Misconfigured naming both envs.
    // assert detail contains "KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER" and
    // "KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER" and "one" (single-broker).
}

#[test]
fn search_broker_entry_has_no_searxng_egress_and_no_endpoint_env() {
    // USE_SEARCH_BROKER=1, endpoint http://127.0.0.1:8888/search, content
    // allowlist ["docs.example.org"], no embed endpoint. Expect Register with:
    // - entry.broker == Some(BrokerSpec::search("http://127.0.0.1:8888/search"))
    // - Net::Allowlist does NOT contain "127.0.0.1:8888"; DOES contain "docs.example.org:443"
    // - env has KASTELLAN_WEB_RESEARCH_ALLOWLIST but NOT KASTELLAN_WEB_RESEARCH_ENDPOINT
    //   and NOT KASTELLAN_SEARCH_BROKER_UDS (core injects that at spawn)
    // - policy.broker_uds == None (set at spawn)
}

#[test]
fn search_broker_entry_keeps_direct_embed() {
    // Same but embed endpoint https://embed.example.org:11434 set (no embed-broker
    // flag): Net::Allowlist contains "embed.example.org:11434", env carries
    // KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT + ..._EMBED_MODEL.
}

#[test]
fn localhost_name_endpoint_with_search_broker_registers() {
    // KASTELLAN_EGRESS_FORCE_ROUTING=1 + endpoint http://searxng.localhost:8888
    // + USE_SEARCH_BROKER=1 → Register (the #452 guard must NOT fire: the broker
    // holds the route). Without the flag this exact env is Misconfigured today —
    // keep that sibling assertion in the same test for contrast.
}

#[test]
fn misconfigured_remedy_offers_search_broker() {
    // The existing forced-localhost Misconfigured detail (no broker flags) now
    // also contains "KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER=1" alongside the
    // existing pins (endpoint env name, "127.0.0.1", "tool_allowlists", "https://").
}

#[cfg(target_os = "linux")]
#[test]
fn resolve_uses_vm_search_broker_entry_when_opted_in() {
    // USE_MICROVM=1 + USE_SEARCH_BROKER=1: FirecrackerVm backend, empty fs_read,
    // env carries KASTELLAN_MICROVM_DIR + KASTELLAN_MICROVM_ROOTFS=web-research.ext4,
    // broker == Some(BrokerSpec::search(..)), Net::Allowlist has no SearxNG entry.
}
```

Write them fully against the fixture style already in the file. Run: `cargo test -p kastellan-core --lib workers::web_research` — expected: the new tests FAIL (missing const/fns/arms).

- [ ] **Step 2: Widen the two pure builders to an optional endpoint**

Change `net_entries` and `base_env` to take `endpoint: Option<&str>` (a `None` skips the SearxNG entry / the `ENDPOINT_ENV` pair; all five existing callers wrap their argument in `Some(...)` — output byte-identical for them):

```rust
fn net_entries(endpoint: Option<&str>, embed_endpoint: Option<&str>, allowlist: &[String]) -> Vec<String> {
    let mut entries = endpoint.map(endpoint_net_entry).unwrap_or_default();
    // ... rest unchanged ...
}

fn base_env(
    endpoint: Option<&str>,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> Vec<(String, String)> {
    let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let mut env = Vec::new();
    if let Some(ep) = endpoint {
        env.push((ENDPOINT_ENV.to_string(), ep.to_string()));
    }
    env.push(("KASTELLAN_WEB_RESEARCH_ALLOWLIST".to_string(), allow_json));
    // ... embed pair unchanged ...
}
```

- [ ] **Step 3: Add the gate const + the two entries**

```rust
/// Opt into the trusted search-broker sidecar: the worker reaches SearxNG only
/// through a core-spawned broker over a bound UDS — the SearxNG host is dropped
/// from `Net::Allowlist` and no endpoint env is injected (core injects
/// `KASTELLAN_SEARCH_BROKER_UDS` at spawn; the worker's
/// `choose_search_provider` then selects the brokered provider). Mutually
/// exclusive with [`USE_EMBED_BROKER_ENV`]: a worker binds at most ONE broker
/// socket (single `broker_uds`, one vsock channel) — search XOR embed.
const USE_SEARCH_BROKER_ENV: &str = "KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER";
```

Host entry (doc comment should name the XOR trade-off: choosing the search-broker keeps the embed path DIRECT — a loopback-name embed endpoint then still degrades to lexical, warned per #429):

```rust
pub fn web_research_search_broker_entry(
    binary: PathBuf,
    endpoint: &str,
    embed_endpoint: Option<&str>,
    embed_model: Option<&str>,
    allowlist: &[String],
) -> ToolEntry {
    let env = base_env(None, embed_endpoint, embed_model, allowlist);
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        // No SearxNG host — the broker holds the only route to the search endpoint.
        net: Net::Allowlist(net_entries(None, embed_endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env,
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
        broker_uds: None, // set at spawn (rewrite_policy_for_broker)
        persistent_store: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(60_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
        lockdown_shim: None,
        ephemeral_scratch: false,
        broker: Some(crate::broker::BrokerSpec::search(endpoint)),
    }
}
```

VM entry — same shape as `web_research_firecracker_broker_entry` (empty `fs_read`, `FirecrackerVm` backend, the two `KASTELLAN_MICROVM_*` env pairs appended after `base_env(None, ...)`, `web-research.ext4`), with `net: Net::Allowlist(net_entries(None, embed_endpoint, allowlist))` and `broker: Some(crate::broker::BrokerSpec::search(endpoint))`. `#[cfg(target_os = "linux")]`.

- [ ] **Step 4: Guard + resolve rework**

`forced_localhost_misconfig` gains the short-circuit and the new remedy (mirrors web-search's `use_broker` arm; the old "web-research has no search-broker" sentence is now false — delete it):

```rust
fn forced_localhost_misconfig(
    use_search_broker: bool,
    force_routed: bool,
    endpoint: &str,
) -> Option<String> {
    if use_search_broker {
        // The search-broker owns SearxNG egress host-side; the worker never
        // dials the endpoint, so a localhost NAME is fine here.
        return None;
    }
    endpoint_guard::forced_localhost_misconfig(
        ENDPOINT_ENV,
        endpoint,
        force_routed,
        "use the literal-IP form (e.g. http://127.0.0.1:<port> — an \
         allowlisted literal is dialed via the proxy's carve-out) or an \
         https:// routable SearxNG host (plain http is loopback-only) — either \
         way the new host must also be on this tool's `tool_allowlists` row \
         (the worker validates the endpoint host against it and fail-closes \
         when missing) — or set KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER=1 \
         (the worker then reaches SearxNG only through the trusted \
         search-broker sidecar: no worker search egress, no endpoint-host \
         row needed).",
    )
}
```

In `resolve()`, read both RAW flags before anything else and refuse the pair; then thread the search flag through guard + dispatch:

```rust
let use_search_broker = (ctx.get_env)(USE_SEARCH_BROKER_ENV).unwrap_or_default().trim() == "1";
let use_embed_broker_flag =
    (ctx.get_env)(USE_EMBED_BROKER_ENV).unwrap_or_default().trim() == "1";
if use_search_broker && use_embed_broker_flag {
    return Resolution::Misconfigured {
        detail: format!(
            "{USE_SEARCH_BROKER_ENV}=1 and {USE_EMBED_BROKER_ENV}=1 are mutually \
             exclusive: a worker binds at most one broker socket (single \
             `broker_uds`, one vsock channel — search XOR embed). Keep the broker \
             for the backend that is local-only and make the other one routable \
             (or unset its flag)."
        ),
    };
}
// (existing) effective embed-broker condition, unchanged:
let use_broker = use_embed_broker_flag && embed_endpoint.is_some();
```

Guard call becomes `forced_localhost_misconfig(use_search_broker, force_routed, &endpoint)`. The `embed_local_warning` call is UNCHANGED (its `use_broker` is the embed-broker; in search-broker mode a loopback-name direct embed endpoint should still warn). Dispatch — insert the search-broker arm FIRST in both the VM and host branches:

```rust
// VM branch:
if use_search_broker {
    return Resolution::Register(web_research_firecracker_search_broker_entry(
        binary, image_dir, &endpoint,
        embed_endpoint.as_deref(), embed_model.as_deref(), &allowlist,
    ));
}
// host branch (after discover_binary):
if use_search_broker {
    return Resolution::Register(web_research_search_broker_entry(
        binary, &endpoint,
        embed_endpoint.as_deref(), embed_model.as_deref(), &allowlist,
    ));
}
```

Update the module doc header (the `Net::Allowlist` union sentence gains "…unless the search-broker is enabled, in which case the SearxNG host is dropped").

- [ ] **Step 5: Run the tests**

```sh
cargo test -p kastellan-core --lib workers::web_research
cargo test -p kastellan-core --lib workers::web_search   # sibling guard untouched — regression pin
cargo test -p kastellan-core --lib workers::endpoint_guard
cargo test -p kastellan-core --lib registry_build          # #459 generic screen interplay
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: all green on the Mac (the `#[cfg(linux)]` test compiles but only runs on the DGX — Task 6). If any existing test pinned the deleted "web-research has no search-broker" remedy phrasing, update that pin to the new remedy in the same commit.

- [ ] **Step 6: Commit**

```sh
git add core/src/workers/web_research.rs core/src/workers/web_research/tests.rs
git commit -m "feat(core): web-research search-broker entries + XOR gate + guard remedy (#464)"
```

---

### Task 5: Hermetic policy-pin + DGX live e2e

**Files:**
- Create: `core/tests/web_research_search_broker_e2e.rs`

**Interfaces:**
- Consumes: `web_research_search_broker_entry` (Task 4); `kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker` (`#[doc(hidden)] pub`, same as `embed_broker_egress_e2e` uses); `SingleUseLifecycle::with_force_routing` + `.acquire` (the #451/#448 manager-level pattern).
- Produces: the arc's containment proof.

- [ ] **Step 1: Hermetic pin (runs everywhere, no PG/KVM)**

Mirror `core/tests/embed_broker_egress_e2e.rs::brokered_policy_has_broker_uds_and_zero_embed_egress`, but for search: build `web_research_search_broker_entry` with endpoint `http://127.0.0.1:8888/search` + content allowlist `["docs.example.org"]`, drive the REAL `rewrite_policy_for_broker` onto a tempdir UDS path, then assert on the post-rewrite policy:

- `policy.broker_uds == Some(<the uds>)`
- env contains `("KASTELLAN_SEARCH_BROKER_UDS", <uds>)` and does NOT contain `KASTELLAN_WEB_RESEARCH_ENDPOINT` or `KASTELLAN_EMBED_BROKER_UDS`
- `Net::Allowlist` does NOT contain `"127.0.0.1:8888"` and DOES contain `"docs.example.org:443"` — match host:PORT, not bare host (the #448 DGX lesson: SearxNG and the embed backend share 127.0.0.1 on the DGX, only the port distinguishes them)
- fail-closed `match` on the net variant (panic on non-`Allowlist`), copying the embed pin's shape.

Open the embed file first and copy its structure (helpers, naming, comments) — reviewers expect the twins to read identically.

- [ ] **Step 2: DGX `#[ignore]` manager-level live test**

Mirror `core/tests/web_search_firecracker_egress_e2e.rs::brokered_web_search_vm_returns_results_with_zero_egress` (the #451 manager-level pattern: `firecracker_backend()` + `probe_and_pool` + `SingleUseLifecycle::with_force_routing(sandboxes, Some(force), broker_configs)` + `.acquire("web-research", &entry)`), swapping in `web_research_firecracker_search_broker_entry` and dispatching `web.research {query}` against the live loopback SearxNG (`http://127.0.0.1:8888/search`). Assertions:

- the call returns `sources`/`unfetched`/`ranking` (any non-error result — content hosts are live-internet so don't over-pin counts; assert the result object parses and `ranking` is `"hybrid"` or `"lexical"`)
- the acquired worker policy's `Net::Allowlist` has NO `127.0.0.1:8888` entry (zero direct search egress — again host:PORT, the SearxNG port, since content/embed may legitimately share the host)
- `#[ignore = "..."]` message names the requirements (DGX: KVM + vsock + web-research.ext4 + search-broker binary + live SearxNG :8888), copying #451's message style.

The content allowlist for the live test: reuse whatever `web_research_vm_force_route_daemon_e2e.rs` uses for its live content hosts (open it and copy — it solved the same live-content problem).

- [ ] **Step 3: Verify hermetic locally, compile the rest**

```sh
cargo test -p kastellan-core --test web_research_search_broker_e2e
cargo clippy -p kastellan-core --all-targets -- -D warnings
```

Expected: hermetic pin green on the Mac; the `#[ignore]` test compiles (runs in Task 6).

- [ ] **Step 4: Commit**

```sh
git add core/tests/web_research_search_broker_e2e.rs
git commit -m "test(core): web-research search-broker policy pin + DGX VM live e2e (#464)"
```

---

### Task 6: DGX gate, docs, PR

- [ ] **Step 1: Mac full-workspace sanity**

```sh
cargo build --workspace && cargo clippy --workspace --all-targets -- -D warnings
```

(Full Mac `cargo test --workspace` is optional — the standing PG flake note applies; the per-crate runs in Tasks 1–5 are the Mac gate.)

- [ ] **Step 2: DGX targeted + full gate** (drive as `ssh dgx '<cmd>'`; long runs via `setsid bash -lc '... > ~/dgx-<name>.log 2>&1' </dev/null &` then poll — never log to /tmp, the workspace test run scrubs it)

On the DGX, on this branch:
1. `cargo build --workspace` and `cargo build --release -p kastellan-microvm-run` (stale release-launcher gotcha).
2. Rebuild the rootfs — the worker binary changed: `scripts/workers/microvm/build-web-research-rootfs.sh` → fresh `web-research.ext4`.
3. `export PATH=$HOME/.local/bin:$PATH` (firecracker is off the non-interactive ssh PATH — without it the e2e silently SKIP-passes).
4. Targeted: `cargo test -p kastellan-core --lib workers::web_research` (the Linux-gated resolve tests), then the new e2e: `cargo test -p kastellan-core --test web_research_search_broker_e2e -- --ignored --nocapture` (needs live SearxNG :8888 + PG). Also re-run the sibling live e2es touched by the worker rework: `web_research_vm_force_route_daemon_e2e`, `web_research_firecracker_broker_e2e` (worker binary changed — prove the embed-broker + direct paths still pass).
5. Full: `cargo test --workspace -- --nocapture` + `cargo clippy --workspace --all-targets -- -D warnings`. Expected: current baseline 2555/0/46 plus this branch's additions, 0 failed; `[SKIP]` lines only the 4 gliner-relex opt-ins.

- [ ] **Step 3: Update docs + memory**

- `docs/devel/handovers/HANDOVER.md`: new header entry (branch, PR, what shipped, verification incl. the DGX counts), Current state, Next TODO refresh.
- `docs/devel/ROADMAP.md`: add the item under the web-worker arc, ticked with the merge hash once merged.
- The spec file: add a "Shipped" note pointing at the PR + this plan.

- [ ] **Step 4: Commit docs, push, open PR**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md docs/superpowers/specs/2026-07-16-web-research-search-broker-arc-design.md
git commit -m "docs(handover): web-research search-broker arc shipped (#464)"
git push -u origin feat/web-research-search-broker
gh pr create --title "web-research × search-broker: single-broker XOR slice (#464)" --body "<summary + verification + Closes #464>"
```

Do NOT self-merge; the operator merges.

---

## Self-review notes (spec coverage)

- Spec item 1 (lift the seam) → Task 1. Item 2 (rework search step, `KASTELLAN_SEARCH_BROKER_UDS` selection, hermetic fakes) → Task 2. Item 3 (entries, gate env, three-way resolve, both-flags Misconfigured, SearxNG dropped from `Net::Allowlist`) → Task 4. Item 4 (reuse-only) → enforced by omission: no task touches `core/src/broker/`, the sandbox crates, or `microvm-*`. Item 5 (DGX gates + rootfs rebuild) → Tasks 5–6. Prerequisite "guard remedy gains the USE_SEARCH_BROKER option" → Task 4 Step 4 + the `misconfigured_remedy_offers_search_broker` test. "Issue to file when the arc starts" → filed, #464.
- XOR trade-off (search-broker costs brokered-embed; loopback-name embed degrades to lexical, warned) is preserved by keeping `embed_local_warning`'s condition untouched (Task 4 Step 4).
- Task 3 (test-lift) is not in the spec — it discharges the standing Item-9b backlog entry for this exact file before Task 4 grows it, mirroring how #451 bundled the web_search lift.
