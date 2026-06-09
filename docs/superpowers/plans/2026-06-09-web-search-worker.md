# web-search worker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a sandboxed `hhagent-worker-web-search` exposing one JSON-RPC method `web.search` that queries an operator-configured SearxNG instance and returns ranked structured hits, and extract the reusable allowlist + HTTP transport into a shared `workers/web-common` crate.

**Architecture:** Mirror the `web-fetch` worker. First extract `HostAllowlist` + the `HttpGet`/`ReqwestGet` transport seam + the `FakeGet` test helper out of `web-fetch` into a new `workers/web-common` lib crate (single source of truth for the security-critical allowlist matcher), re-pointing `web-fetch` at it with no behaviour change. Then build `web-search` on top: pure `parse.rs` (SearxNG JSON → `Vec<Hit>`) + pure `search.rs` (endpoint validation, request build, one-GET drive, count cap) + `handler.rs` (RPC dispatch) + a host-side `WebSearchManifest`. The LLM supplies only the query string; the endpoint is operator-configured, so there is no URL-injection surface — `http://` is therefore allowed for loopback only, `https://` mandatory elsewhere.

**Tech Stack:** Rust, `serde`/`serde_json`, `reqwest::blocking` + rustls, `url`, `hhagent-protocol` (JSON-RPC), `hhagent-worker-prelude` (`serve_stdio` + sandbox lockdown). SearxNG (Docker) for the live backend.

**Reference (read before starting):** the merged `web-fetch` worker — `workers/web-fetch/src/{allowlist,fetch,handler,extract,test_transport,main}.rs`, `core/src/workers/web_fetch.rs`, `core/tests/web_fetch_e2e.rs` — and the design spec `docs/superpowers/specs/2026-06-09-web-search-worker-design.md`.

**Build/test prelude (Rust):** Cargo is not on the non-interactive `PATH`; every shell step that runs cargo must first `source "$HOME/.cargo/env"`.

---

## Phase A — Extract `workers/web-common` (refactor, behaviour-preserving)

The safety net for this whole phase is `web-fetch`'s existing 29 unit tests + its e2e: they must stay green after the move.

### Task 1: Create the `web-common` crate with the moved `HostAllowlist`

**Files:**
- Create: `workers/web-common/Cargo.toml`
- Create: `workers/web-common/src/lib.rs`
- Create: `workers/web-common/src/allowlist.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Add the crate to the workspace members**

In the root `Cargo.toml`, add `"workers/web-common",` to the `members` array (immediately before `"workers/web-fetch",`):

```toml
    "workers/prelude",
    "workers/shell-exec",
    "workers/web-common",
    "workers/web-fetch",
```

- [ ] **Step 2: Write `workers/web-common/Cargo.toml`**

```toml
[package]
name        = "hhagent-worker-web-common"
description = "Shared building blocks for net-egress tool workers: host allowlist matcher + capped HTTP transport seam."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme.workspace       = true

[features]
# Pulls in the FakeGet test transport + helpers. Enable from a consumer's
# [dev-dependencies] so unit tests across workers share one fake.
testing = []

[dependencies]
serde      = { workspace = true }
serde_json = { workspace = true }
anyhow     = { workspace = true }
reqwest    = { workspace = true, features = ["blocking"] } # blocking: synchronous transport for stdio workers
url        = { workspace = true }
```

- [ ] **Step 3: Write `workers/web-common/src/lib.rs`**

```rust
//! Shared building blocks for net-egress tool workers.
//!
//! - [`allowlist`] — host allowlist matcher (exact + `.domain` wildcard).
//! - [`http`] — the `HttpGet` transport seam + the real `ReqwestGet`.
//! - [`testing`] (feature `testing`) — a fake transport + builders for unit tests.

pub mod allowlist;
pub mod http;

#[cfg(feature = "testing")]
pub mod testing;
```

- [ ] **Step 4: Move `allowlist.rs` verbatim**

Copy `workers/web-fetch/src/allowlist.rs` to `workers/web-common/src/allowlist.rs` **unchanged** (it has no `crate::` imports — it only uses `serde_json` and `anyhow` — so it moves byte-for-byte, tests included).

- [ ] **Step 5: Build + test web-common**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-common`
Expected: PASS — the 8 moved allowlist tests (`exact_matches_only_that_host`, `leading_dot_*`, `matching_is_case_insensitive`, `empty_allowlist_denies_everything`, `malformed_json_is_an_error`, `whitespace_padded_entry_is_trimmed`, `lone_dot_entry_is_ignored`).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml workers/web-common/Cargo.toml workers/web-common/src/lib.rs workers/web-common/src/allowlist.rs
git commit -m "refactor(web-common): new shared crate with HostAllowlist moved from web-fetch"
```

---

### Task 2: Move the HTTP transport seam into `web-common::http`

**Files:**
- Create: `workers/web-common/src/http.rs`

- [ ] **Step 1: Write `workers/web-common/src/http.rs`**

This is the transport half of the current `workers/web-fetch/src/fetch.rs` (`HttpGet`, `RawResponse`, `ReqwestGet`, and the `MAX_BODY_BYTES`/`TIMEOUT_SECS` constants), lifted out. The redirect-following `drive()` + `FetchError`/`FetchOutcome`/`MAX_REDIRECTS` stay in `web-fetch` (Task 4). User-agent generalised to `hhagent/0`.

```rust
//! HTTP transport seam shared by net-egress workers.
//!
//! `HttpGet` is the seam tests fake; [`ReqwestGet`] is the real
//! `reqwest::blocking` + rustls implementation. Redirects are disabled at the
//! client — callers that need them drive redirects themselves so they can
//! re-check their allowlist on every hop. The body is capped while reading.

use std::time::Duration;

use url::Url;

/// Per-request timeout.
pub const TIMEOUT_SECS: u64 = 20;
/// Response body byte cap (5 MiB).
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// A single raw HTTP response, transport-agnostic.
pub struct RawResponse {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// The transport seam. One GET, no redirect following.
pub trait HttpGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String>;
}

/// Real transport over `reqwest::blocking` + rustls. Redirects disabled; body
/// capped while reading via `Read::take`.
pub struct ReqwestGet {
    client: reqwest::blocking::Client,
}

impl ReqwestGet {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .user_agent("hhagent/0")
            .build()?;
        Ok(Self { client })
    }
}

impl HttpGet for ReqwestGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        use std::io::Read;

        let resp = self
            .client
            .get(url.clone())
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let header = |name: reqwest::header::HeaderName| -> Option<String> {
            resp.headers()
                .get(&name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let location = header(reqwest::header::LOCATION);
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();

        let mut body = Vec::new();
        resp.take((MAX_BODY_BYTES as u64) + 1)
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        if body.len() > MAX_BODY_BYTES {
            return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
        }

        Ok(RawResponse { status, location, content_type, body })
    }
}
```

- [ ] **Step 2: Build web-common**

Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-worker-web-common`
Expected: PASS (compiles; no tests added in this task).

- [ ] **Step 3: Commit**

```bash
git add workers/web-common/src/http.rs
git commit -m "refactor(web-common): move HttpGet transport seam out of web-fetch"
```

---

### Task 3: Move the `FakeGet` test transport into `web-common::testing`

**Files:**
- Create: `workers/web-common/src/testing.rs`

- [ ] **Step 1: Write `workers/web-common/src/testing.rs`**

This is the current `workers/web-fetch/src/test_transport.rs`, with imports re-pointed to `crate::allowlist` / `crate::http` (no longer `crate::fetch`). It is **not** `#[cfg(test)]` — it must compile into the library when the `testing` feature is on so other crates can use it.

```rust
//! Shared unit-test helpers: a fake [`HttpGet`] transport plus small
//! allowlist/response builders, behind the `testing` cargo feature so each
//! worker's unit suite shares one canned-response transport.

use std::cell::RefCell;
use std::collections::VecDeque;

use url::Url;

use crate::allowlist::HostAllowlist;
use crate::http::{HttpGet, RawResponse};

/// Fake transport returning canned responses in FIFO order.
pub struct FakeGet {
    responses: RefCell<VecDeque<RawResponse>>,
}

impl FakeGet {
    pub fn new(responses: Vec<RawResponse>) -> Self {
        Self { responses: RefCell::new(responses.into_iter().collect()) }
    }
}

impl HttpGet for FakeGet {
    fn get(&self, _url: &Url) -> Result<RawResponse, String> {
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| "no more canned responses".to_string())
    }
}

/// Build a [`HostAllowlist`] from bare string entries.
pub fn al(entries: &[&str]) -> HostAllowlist {
    let json = serde_json::to_string(entries).unwrap();
    HostAllowlist::from_env_json(&json).unwrap()
}

/// A `200 text/plain` response carrying `body`.
pub fn ok_resp(body: &str) -> RawResponse {
    RawResponse {
        status: 200,
        location: None,
        content_type: "text/plain".to_string(),
        body: body.as_bytes().to_vec(),
    }
}

/// A `302` redirect to `loc`.
pub fn redirect_to(loc: &str) -> RawResponse {
    RawResponse {
        status: 302,
        location: Some(loc.to_string()),
        content_type: String::new(),
        body: Vec::new(),
    }
}

/// A `200 application/json` response carrying `json` (for search-style workers).
pub fn json_resp(json: &str) -> RawResponse {
    RawResponse {
        status: 200,
        location: None,
        content_type: "application/json".to_string(),
        body: json.as_bytes().to_vec(),
    }
}
```

- [ ] **Step 2: Build web-common with the testing feature**

Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-worker-web-common --features testing`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add workers/web-common/src/testing.rs
git commit -m "refactor(web-common): move FakeGet test transport behind a testing feature"
```

---

### Task 4: Re-point `web-fetch` at `web-common`

**Files:**
- Modify: `workers/web-fetch/Cargo.toml`
- Modify: `workers/web-fetch/src/fetch.rs`
- Modify: `workers/web-fetch/src/handler.rs`
- Modify: `workers/web-fetch/src/main.rs`
- Delete: `workers/web-fetch/src/allowlist.rs`
- Delete: `workers/web-fetch/src/test_transport.rs`

- [ ] **Step 1: Update `workers/web-fetch/Cargo.toml`**

Add the `web-common` dependency, add a `[dev-dependencies]` entry enabling `testing`, and drop the now-transitive direct `reqwest` dep (web-fetch no longer names `reqwest` types directly — `ReqwestGet` comes from web-common). Keep `url`, `pdf-extract`, `readable_html` (used by `fetch.rs`/`extract.rs`). Resulting dependency sections:

```toml
[dependencies]
hhagent-protocol         = { path = "../../protocol" }
hhagent-worker-prelude   = { path = "../prelude" }
hhagent-worker-web-common = { path = "../web-common" }
serde                    = { workspace = true }
serde_json               = { workspace = true }
anyhow                   = { workspace = true }
url                      = { workspace = true }
pdf-extract              = { workspace = true }
readable_html            = { workspace = true }

[dev-dependencies]
hhagent-worker-web-common = { path = "../web-common", features = ["testing"] }
```

- [ ] **Step 2: Delete the moved files**

```bash
git rm workers/web-fetch/src/allowlist.rs workers/web-fetch/src/test_transport.rs
```

- [ ] **Step 3: Rewrite `workers/web-fetch/src/fetch.rs` to keep only the drive loop**

Replace the whole file with the version below: the transport (`HttpGet`/`RawResponse`/`ReqwestGet`) now comes from `web-common`; this file keeps `drive()`, `FetchError`, `FetchOutcome`, `MAX_REDIRECTS`. The `drive` unit tests are retained but now import the fake + `RawResponse` from `web-common`.

```rust
//! The redirect-following drive loop for web-fetch.
//!
//! `drive()` is pure over the [`HttpGet`] seam so the redirect cap and the
//! per-hop allowlist + https re-check (the security-critical bit: a 3xx to a
//! non-allowlisted or non-https target is refused) are unit-tested with a fake
//! transport. The transport itself lives in `hhagent_worker_web_common::http`.

use url::Url;

use hhagent_worker_web_common::allowlist::HostAllowlist;
use hhagent_worker_web_common::http::HttpGet;

/// Max redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 5;

/// Terminal outcome of a successful drive.
pub struct FetchOutcome {
    pub final_url: String,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Errors from the drive loop. The handler maps these to JSON-RPC codes.
pub enum FetchError {
    /// A redirect targeted a host not on the allowlist.
    HostDenied(String),
    /// A redirect targeted a non-https scheme.
    NonHttps(String),
    TooManyRedirects,
    MissingLocation,
    BadUrl(String),
    Transport(String),
}

/// Follow redirects from `start`, re-validating https + allowlist on every hop,
/// up to [`MAX_REDIRECTS`]. Returns the terminal (non-3xx) response.
pub fn drive<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    start: Url,
) -> Result<FetchOutcome, FetchError> {
    let mut url = start;
    for _hop in 0..=MAX_REDIRECTS {
        if url.scheme() != "https" {
            return Err(FetchError::NonHttps(url.scheme().to_string()));
        }
        let host = url
            .host_str()
            .ok_or_else(|| FetchError::BadUrl("url has no host".to_string()))?;
        if !allowlist.is_allowed(host) {
            return Err(FetchError::HostDenied(host.to_string()));
        }

        let resp = transport.get(&url).map_err(FetchError::Transport)?;

        // Any 3xx is treated as a redirect requiring a `Location`. A bodyless
        // 3xx without `Location` fails closed as MissingLocation.
        if (300..400).contains(&resp.status) {
            let loc = resp.location.ok_or(FetchError::MissingLocation)?;
            url = url
                .join(&loc)
                .map_err(|e| FetchError::BadUrl(e.to_string()))?;
            continue;
        }

        return Ok(FetchOutcome {
            final_url: url.to_string(),
            status: resp.status,
            content_type: resp.content_type,
            body: resp.body,
        });
    }
    Err(FetchError::TooManyRedirects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_worker_web_common::http::RawResponse;
    use hhagent_worker_web_common::testing::{al, ok_resp, redirect_to, FakeGet};

    #[test]
    fn terminal_response_is_returned() {
        let t = FakeGet::new(vec![ok_resp("hello")]);
        let out = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .unwrap_or_else(|_| panic!("expected ok"));
        assert_eq!(out.status, 200);
        assert_eq!(out.body, b"hello");
        assert_eq!(out.final_url, "https://example.com/");
    }

    #[test]
    fn redirect_to_allowlisted_host_is_followed() {
        let t = FakeGet::new(vec![
            redirect_to("https://a.example.com/page"),
            ok_resp("landed"),
        ]);
        let out = drive(&t, &al(&[".example.com"]), Url::parse("https://example.com/").unwrap())
            .unwrap_or_else(|_| panic!("expected ok"));
        assert_eq!(out.body, b"landed");
        assert_eq!(out.final_url, "https://a.example.com/page");
    }

    #[test]
    fn redirect_to_non_allowlisted_host_is_refused() {
        let t = FakeGet::new(vec![redirect_to("https://evil.test/")]);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must refuse");
        assert!(matches!(err, FetchError::HostDenied(h) if h == "evil.test"));
    }

    #[test]
    fn redirect_to_non_https_is_refused() {
        let t = FakeGet::new(vec![redirect_to("http://example.com/")]);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must refuse");
        assert!(matches!(err, FetchError::NonHttps(s) if s == "http"));
    }

    #[test]
    fn redirect_loop_hits_the_cap() {
        let resps: Vec<RawResponse> =
            (0..MAX_REDIRECTS + 2).map(|_| redirect_to("https://example.com/next")).collect();
        let t = FakeGet::new(resps);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must error");
        assert!(matches!(err, FetchError::TooManyRedirects));
    }

    #[test]
    fn redirect_without_location_errors() {
        let t = FakeGet::new(vec![RawResponse {
            status: 302,
            location: None,
            content_type: String::new(),
            body: Vec::new(),
        }]);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must error");
        assert!(matches!(err, FetchError::MissingLocation));
    }
}
```

- [ ] **Step 4: Update `workers/web-fetch/src/handler.rs` imports**

Change the transport/allowlist imports at the top of the file from the local modules to `web-common`. Replace:

```rust
use crate::allowlist::HostAllowlist;
use crate::extract::{extract, main_type};
use crate::fetch::{drive, FetchError, HttpGet, ReqwestGet};
```

with:

```rust
use hhagent_worker_web_common::allowlist::HostAllowlist;
use hhagent_worker_web_common::http::{HttpGet, ReqwestGet};

use crate::extract::{extract, main_type};
use crate::fetch::{drive, FetchError};
```

Then in the handler's `#[cfg(test)] mod tests`, replace the two test-helper imports:

```rust
    use crate::fetch::RawResponse;
    use crate::test_transport::{al, FakeGet};
```

with:

```rust
    use hhagent_worker_web_common::http::RawResponse;
    use hhagent_worker_web_common::testing::{al, FakeGet};
```

(The body of every test is unchanged.)

- [ ] **Step 5: Update `workers/web-fetch/src/main.rs` module list**

Remove the `mod allowlist;` and `#[cfg(test)] mod test_transport;` lines. The module list becomes:

```rust
mod extract;
mod fetch;
mod handler;
```

- [ ] **Step 6: Build + test web-fetch (the safety net)**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-fetch`
Expected: PASS — all web-fetch unit tests still green (allowlist tests now run in web-common; web-fetch retains its extract/fetch/handler tests). If the count looks lower than 29, that is expected: the 8 allowlist tests moved to web-common.

- [ ] **Step 7: Build the whole workspace (catch the e2e dependency)**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Expected: PASS — `core/tests/web_fetch_e2e.rs` still references `hhagent_core::workers::web_fetch::web_fetch_entry` (unchanged by this phase).

- [ ] **Step 8: Commit**

```bash
git add workers/web-fetch/Cargo.toml workers/web-fetch/src/fetch.rs workers/web-fetch/src/handler.rs workers/web-fetch/src/main.rs
git commit -m "refactor(web-fetch): consume HostAllowlist + transport from web-common"
```

---

## Phase B — Build the `web-search` worker crate

### Task 5: `web-search` crate skeleton + `parse.rs` (SearxNG JSON → Vec<Hit>)

**Files:**
- Create: `workers/web-search/Cargo.toml`
- Create: `workers/web-search/src/main.rs`
- Create: `workers/web-search/src/parse.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Add the crate to the workspace members**

In root `Cargo.toml`, add `"workers/web-search",` right after `"workers/web-fetch",`:

```toml
    "workers/web-common",
    "workers/web-fetch",
    "workers/web-search",
```

- [ ] **Step 2: Write `workers/web-search/Cargo.toml`**

```toml
[package]
name        = "hhagent-worker-web-search"
description = "Tool worker: query an operator-configured SearxNG instance and return ranked structured hits. GET-only."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme.workspace       = true

[[bin]]
name = "hhagent-worker-web-search"
path = "src/main.rs"

[dependencies]
hhagent-protocol          = { path = "../../protocol" }
hhagent-worker-prelude    = { path = "../prelude" }
hhagent-worker-web-common = { path = "../web-common" }
serde                     = { workspace = true }
serde_json                = { workspace = true }
anyhow                    = { workspace = true }
url                       = { workspace = true }

[dev-dependencies]
hhagent-worker-web-common = { path = "../web-common", features = ["testing"] }
```

- [ ] **Step 3: Write the failing test for `parse.rs`**

Create `workers/web-search/src/parse.rs` with the types and a test module (implementation stubbed to fail to compile-or-assert first):

```rust
//! Parse SearxNG's `/search?format=json` response into a bounded list of hits.
//!
//! We deserialize only the subset we surface. The mapping is lenient: a result
//! with no `url` is dropped (a hit the agent cannot follow is useless); missing
//! `title`/`content`/`engine` default to empty strings.

/// One search result surfaced to the planner.
#[derive(serde::Serialize, Debug, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
}

#[derive(serde::Deserialize)]
struct RawSearchResponse {
    #[serde(default)]
    results: Vec<RawResult>,
}

#[derive(serde::Deserialize)]
struct RawResult {
    #[serde(default)]
    title: String,
    url: Option<String>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    engine: String,
}

/// Parse a SearxNG JSON body into hits. Errors only on malformed JSON.
pub fn parse_results(body: &[u8]) -> anyhow::Result<Vec<Hit>> {
    let raw: RawSearchResponse = serde_json::from_slice(body)
        .map_err(|e| anyhow::anyhow!("malformed SearxNG JSON: {e}"))?;
    Ok(raw
        .results
        .into_iter()
        .filter_map(|r| {
            r.url.map(|url| Hit {
                title: r.title,
                url,
                snippet: r.content,
                engine: r.engine,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_into_hits() {
        let json = r#"{"results":[
            {"title":"Rust","url":"https://rust-lang.org","content":"systems lang","engine":"duckduckgo"},
            {"title":"Cargo","url":"https://doc.rust-lang.org/cargo","content":"build tool","engine":"google"}
        ]}"#;
        let hits = parse_results(json.as_bytes()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], Hit {
            title: "Rust".into(),
            url: "https://rust-lang.org".into(),
            snippet: "systems lang".into(),
            engine: "duckduckgo".into(),
        });
    }

    #[test]
    fn result_without_url_is_skipped() {
        let json = r#"{"results":[
            {"title":"no link","content":"x"},
            {"title":"ok","url":"https://example.com","content":"y"}
        ]}"#;
        let hits = parse_results(json.as_bytes()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com");
    }

    #[test]
    fn missing_optional_fields_default_to_empty() {
        let json = r#"{"results":[{"url":"https://example.com"}]}"#;
        let hits = parse_results(json.as_bytes()).unwrap();
        assert_eq!(hits[0].title, "");
        assert_eq!(hits[0].snippet, "");
        assert_eq!(hits[0].engine, "");
    }

    #[test]
    fn empty_results_is_empty_vec() {
        let hits = parse_results(br#"{"results":[]}"#).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn missing_results_key_is_empty_vec() {
        // SearxNG always sends `results`, but be defensive.
        let hits = parse_results(br#"{"query":"x"}"#).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_results(b"not json").is_err());
    }
}
```

Also create a minimal `workers/web-search/src/main.rs` so the crate compiles:

```rust
//! web-search: query an operator-configured SearxNG instance and return ranked
//! structured hits over JSON-RPC stdio. Design:
//! docs/superpowers/specs/2026-06-09-web-search-worker-design.md

mod parse;

fn main() -> anyhow::Result<()> {
    Ok(())
}
```

- [ ] **Step 4: Run the parse tests**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-search parse`
Expected: PASS (6 parse tests). (`main` is an empty stub for now; a dead-code warning on `parse` is acceptable until Task 8 wires it.)

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml workers/web-search/Cargo.toml workers/web-search/src/main.rs workers/web-search/src/parse.rs
git commit -m "feat(web-search): crate skeleton + SearxNG JSON parser (parse.rs)"
```

---

### Task 6: `search.rs` — `is_loopback` + endpoint validation + request build

**Files:**
- Create: `workers/web-search/src/search.rs`
- Modify: `workers/web-search/src/main.rs` (add `mod search;`)

- [ ] **Step 1: Write `search.rs` with the validation + URL-building helpers and their tests**

```rust
//! Pure search logic: endpoint validation, request-URL building, and the
//! one-GET drive with the count cap. Pure over the [`HttpGet`] seam so the
//! security checks (scheme + host allowlist) are unit-tested with a fake.

use std::net::IpAddr;

use url::Url;

use hhagent_worker_web_common::allowlist::HostAllowlist;
use hhagent_worker_web_common::http::HttpGet;

use crate::parse::{parse_results, Hit};

/// Default number of hits returned when the caller does not specify `count`.
pub const DEFAULT_COUNT: usize = 10;
/// Hard cap on hits returned regardless of caller request.
pub const MAX_COUNT: usize = 20;

/// Failure modes of a search. The handler maps these to JSON-RPC codes.
pub enum SearchError {
    /// Configured endpoint URL is unparseable / has no host.
    BadEndpoint(String),
    /// Endpoint scheme not permitted (https everywhere; http loopback-only).
    SchemeDenied(String),
    /// Endpoint host is not on the allowlist.
    HostDenied(String),
    /// The query string was empty/blank.
    EmptyQuery,
    /// Transport error talking to the endpoint.
    Transport(String),
    /// Endpoint returned a redirect (unexpected for a search endpoint).
    Redirected,
    /// Endpoint returned a non-200 status.
    BadStatus(u16),
    /// Response body was not valid SearxNG JSON.
    Parse(String),
}

/// True if `host` is loopback: a loopback IP (covers `127.0.0.0/8` and `::1`)
/// or the literal `localhost`.
pub fn is_loopback(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// Validate the configured endpoint: parse, enforce the scheme rule, and
/// require the host be on the allowlist. Returns the parsed `Url` on success.
pub fn validate_endpoint(raw: &str, allowlist: &HostAllowlist) -> Result<Url, SearchError> {
    let url = Url::parse(raw).map_err(|e| SearchError::BadEndpoint(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| SearchError::BadEndpoint("endpoint has no host".to_string()))?
        .to_string();
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&host) => {}
        other => return Err(SearchError::SchemeDenied(other.to_string())),
    }
    if !allowlist.is_allowed(&host) {
        return Err(SearchError::HostDenied(host));
    }
    Ok(url)
}

/// Build the SearxNG request URL from the validated endpoint: replace the query
/// string with `q=<query>&format=json`, preserving scheme/host/port/path.
pub fn build_query_url(endpoint: &Url, query: &str) -> Url {
    let mut url = endpoint.clone();
    url.query_pairs_mut()
        .clear()
        .append_pair("q", query)
        .append_pair("format", "json");
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_worker_web_common::testing::al;

    #[test]
    fn loopback_recognises_localhost_and_loopback_ips() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("LocalHost"));
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.0.0.5"));
        assert!(is_loopback("::1"));
        assert!(!is_loopback("example.org"));
        assert!(!is_loopback("10.0.0.1"));
        assert!(!is_loopback("8.8.8.8"));
    }

    #[test]
    fn https_endpoint_on_allowlisted_host_is_accepted() {
        let a = al(&["searx.example.org"]);
        let u = validate_endpoint("https://searx.example.org/search", &a).unwrap();
        assert_eq!(u.host_str(), Some("searx.example.org"));
    }

    #[test]
    fn http_loopback_endpoint_is_accepted() {
        let a = al(&["127.0.0.1"]);
        let u = validate_endpoint("http://127.0.0.1:8888/search", &a).unwrap();
        assert_eq!(u.port(), Some(8888));
    }

    #[test]
    fn http_remote_endpoint_is_scheme_denied() {
        let a = al(&["searx.example.org"]);
        let err = validate_endpoint("http://searx.example.org/search", &a)
            .err()
            .expect("must deny");
        assert!(matches!(err, SearchError::SchemeDenied(s) if s == "http"));
    }

    #[test]
    fn endpoint_host_not_on_allowlist_is_denied() {
        let a = al(&["searx.example.org"]);
        let err = validate_endpoint("https://evil.test/search", &a)
            .err()
            .expect("must deny");
        assert!(matches!(err, SearchError::HostDenied(h) if h == "evil.test"));
    }

    #[test]
    fn unparseable_endpoint_is_bad_endpoint() {
        let a = al(&["x"]);
        let err = validate_endpoint("not a url", &a).err().expect("must error");
        assert!(matches!(err, SearchError::BadEndpoint(_)));
    }

    #[test]
    fn build_query_url_sets_q_and_format_preserving_path() {
        let endpoint = Url::parse("https://searx.example.org/search").unwrap();
        let req = build_query_url(&endpoint, "rust lifetimes");
        assert_eq!(req.path(), "/search");
        let pairs: Vec<(String, String)> = req
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(pairs.contains(&("q".into(), "rust lifetimes".into())));
        assert!(pairs.contains(&("format".into(), "json".into())));
    }
}
```

- [ ] **Step 2: Add `mod search;` to `main.rs`**

Update `workers/web-search/src/main.rs` module list to:

```rust
mod parse;
mod search;
```

- [ ] **Step 3: Run the search-validation tests**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-search search::tests`
Expected: PASS (7 tests). A dead-code warning on `SearchError::{Transport,Redirected,BadStatus,Parse,EmptyQuery}` and on `parse_results`/`DEFAULT_COUNT`/`MAX_COUNT` is acceptable until Task 7.

- [ ] **Step 4: Commit**

```bash
git add workers/web-search/src/search.rs workers/web-search/src/main.rs
git commit -m "feat(web-search): endpoint validation + loopback rule + request-URL builder"
```

---

### Task 7: `search.rs` — the one-GET `search()` drive with the count cap

**Files:**
- Modify: `workers/web-search/src/search.rs`

- [ ] **Step 1: Write the failing test for `search()`**

Append these tests to the `mod tests` block in `search.rs`:

```rust
    use hhagent_worker_web_common::http::RawResponse;
    use hhagent_worker_web_common::testing::{json_resp, redirect_to, FakeGet};

    fn endpoint() -> Url {
        Url::parse("https://searx.example.org/search").unwrap()
    }

    #[test]
    fn search_returns_parsed_hits() {
        let json = r#"{"results":[
            {"title":"A","url":"https://a.test","content":"x","engine":"e"},
            {"title":"B","url":"https://b.test","content":"y","engine":"e"}
        ]}"#;
        let t = FakeGet::new(vec![json_resp(json)]);
        let a = al(&["searx.example.org"]);
        let hits = search(&t, &endpoint(), &a, "q", DEFAULT_COUNT).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://a.test");
    }

    #[test]
    fn search_truncates_to_count() {
        let results: String = (0..5)
            .map(|i| format!(r#"{{"url":"https://h{i}.test"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(r#"{{"results":[{results}]}}"#);
        let t = FakeGet::new(vec![json_resp(&json)]);
        let a = al(&["searx.example.org"]);
        let hits = search(&t, &endpoint(), &a, "q", 3).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn search_clamps_count_to_max() {
        let results: String = (0..30)
            .map(|i| format!(r#"{{"url":"https://h{i}.test"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(r#"{{"results":[{results}]}}"#);
        let t = FakeGet::new(vec![json_resp(&json)]);
        let a = al(&["searx.example.org"]);
        let hits = search(&t, &endpoint(), &a, "q", 999).unwrap();
        assert_eq!(hits.len(), MAX_COUNT);
    }

    #[test]
    fn empty_query_is_rejected() {
        let t = FakeGet::new(vec![]);
        let a = al(&["searx.example.org"]);
        let err = search(&t, &endpoint(), &a, "   ", DEFAULT_COUNT)
            .err()
            .expect("must reject");
        assert!(matches!(err, SearchError::EmptyQuery));
    }

    #[test]
    fn non_200_status_is_bad_status() {
        let t = FakeGet::new(vec![RawResponse {
            status: 503,
            location: None,
            content_type: "text/plain".into(),
            body: Vec::new(),
        }]);
        let a = al(&["searx.example.org"]);
        let err = search(&t, &endpoint(), &a, "q", DEFAULT_COUNT)
            .err()
            .expect("must error");
        assert!(matches!(err, SearchError::BadStatus(503)));
    }

    #[test]
    fn redirect_from_endpoint_is_rejected() {
        let t = FakeGet::new(vec![redirect_to("https://elsewhere.test/")]);
        let a = al(&["searx.example.org"]);
        let err = search(&t, &endpoint(), &a, "q", DEFAULT_COUNT)
            .err()
            .expect("must error");
        assert!(matches!(err, SearchError::Redirected));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-search search::tests::search_returns_parsed_hits`
Expected: FAIL to compile — `search` function not found.

- [ ] **Step 3: Implement `search()`**

Add this function to `search.rs` (after `build_query_url`):

```rust
/// Run one search: validate the host against the allowlist (defense in depth —
/// the endpoint was validated at startup, but re-check), reject an empty query,
/// GET the request URL once, reject redirects and non-200s, parse, and slice to
/// `count` (clamped to `1..=MAX_COUNT`).
pub fn search<T: HttpGet>(
    transport: &T,
    endpoint: &Url,
    allowlist: &HostAllowlist,
    query: &str,
    count: usize,
) -> Result<Vec<Hit>, SearchError> {
    if query.trim().is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    let host = endpoint
        .host_str()
        .ok_or_else(|| SearchError::BadEndpoint("endpoint has no host".to_string()))?;
    if !allowlist.is_allowed(host) {
        return Err(SearchError::HostDenied(host.to_string()));
    }

    let req = build_query_url(endpoint, query);
    let resp = transport.get(&req).map_err(SearchError::Transport)?;
    if (300..400).contains(&resp.status) {
        return Err(SearchError::Redirected);
    }
    if resp.status != 200 {
        return Err(SearchError::BadStatus(resp.status));
    }

    let mut hits = parse_results(&resp.body).map_err(|e| SearchError::Parse(e.to_string()))?;
    hits.truncate(count.clamp(1, MAX_COUNT));
    Ok(hits)
}
```

- [ ] **Step 4: Run the full search test module**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-search search`
Expected: PASS (13 search tests: 7 from Task 6 + 6 here).

- [ ] **Step 5: Commit**

```bash
git add workers/web-search/src/search.rs
git commit -m "feat(web-search): one-GET search drive with count cap"
```

---

### Task 8: `handler.rs` — `web.search` dispatch + `from_env`

**Files:**
- Create: `workers/web-search/src/handler.rs`
- Modify: `workers/web-search/src/main.rs`

- [ ] **Step 1: Write `handler.rs` (handler + error mapping + tests)**

```rust
//! JSON-RPC handler for `web.search`.
//!
//! Flow: parse params → run `search` against the configured endpoint → build
//! the result object. The endpoint is validated once at construction
//! (`from_env`); each call re-checks the host (defense in depth). Errors map
//! onto the protocol code vocabulary. No silent fallbacks.

use hhagent_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use url::Url;

use hhagent_worker_web_common::allowlist::HostAllowlist;
use hhagent_worker_web_common::http::{HttpGet, ReqwestGet};

use crate::search::{search, validate_endpoint, SearchError, DEFAULT_COUNT};

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

/// Map a [`SearchError`] to a JSON-RPC error.
fn search_err_to_rpc(e: SearchError) -> RpcError {
    match e {
        SearchError::EmptyQuery => {
            RpcError::new(codes::INVALID_PARAMS, "query is empty".to_string())
        }
        SearchError::BadEndpoint(m) => RpcError::new(
            codes::POLICY_DENIED,
            format!("configured endpoint invalid: {m}"),
        ),
        SearchError::SchemeDenied(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("endpoint scheme {s:?} not allowed (https, or http for loopback only)"),
        ),
        SearchError::HostDenied(h) => RpcError::new(
            codes::POLICY_DENIED,
            format!("endpoint host {h:?} not on allowlist"),
        ),
        SearchError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("search request failed: {m}"))
        }
        SearchError::Redirected => RpcError::new(
            codes::OPERATION_FAILED,
            "search endpoint returned an unexpected redirect".to_string(),
        ),
        SearchError::BadStatus(s) => RpcError::new(
            codes::OPERATION_FAILED,
            format!("search endpoint returned status {s}"),
        ),
        SearchError::Parse(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("parsing results failed: {m}"))
        }
    }
}

/// The worker handler, generic over the transport so tests inject a fake.
pub struct WebSearchHandler<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
}

impl WebSearchHandler<ReqwestGet> {
    /// Build from env: endpoint + allowlist JSON + real reqwest transport.
    /// Validates the endpoint up front and fails closed (the worker never
    /// serves) if it is missing, unparseable, wrong-scheme, or off-allowlist.
    pub fn from_env() -> anyhow::Result<Self> {
        let endpoint_raw = std::env::var("HHAGENT_WEB_SEARCH_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("HHAGENT_WEB_SEARCH_ENDPOINT not set"))?;
        let allow_raw =
            std::env::var("HHAGENT_WEB_SEARCH_ALLOWLIST").unwrap_or_else(|_| "[]".to_string());
        let allowlist = HostAllowlist::from_env_json(&allow_raw)?;
        let endpoint = validate_endpoint(&endpoint_raw, &allowlist)
            .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
        let transport = ReqwestGet::new()?;
        Ok(Self { endpoint, allowlist, transport })
    }
}

impl<T: HttpGet> WebSearchHandler<T> {
    #[cfg(test)]
    fn with_parts(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }
}

impl<T: HttpGet> Handler for WebSearchHandler<T> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if method != "web.search" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: SearchParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        let count = p.count.unwrap_or(DEFAULT_COUNT);

        let hits = search(&self.transport, &self.endpoint, &self.allowlist, &p.query, count)
            .map_err(search_err_to_rpc)?;

        Ok(serde_json::json!({
            "query": p.query,
            "results": hits,
            "count": hits.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_worker_web_common::http::RawResponse;
    use hhagent_worker_web_common::testing::{al, json_resp, FakeGet};

    fn handler(responses: Vec<RawResponse>) -> WebSearchHandler<FakeGet> {
        WebSearchHandler::with_parts(
            Url::parse("https://searx.example.org/search").unwrap(),
            al(&["searx.example.org"]),
            FakeGet::new(responses),
        )
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = handler(vec![]);
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("web.search", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn empty_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h
            .call("web.search", serde_json::json!({"query": "  "}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn happy_path_returns_hits() {
        let json = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(json)]);
        let out = h
            .call("web.search", serde_json::json!({"query": "rust"}))
            .unwrap();
        assert_eq!(out["query"], "rust");
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["url"], "https://x.test");
        assert_eq!(out["results"][0]["snippet"], "c");
    }

    #[test]
    fn endpoint_failure_maps_to_operation_failed() {
        let mut h = handler(vec![RawResponse {
            status: 500,
            location: None,
            content_type: "text/plain".into(),
            body: Vec::new(),
        }]);
        let err = h
            .call("web.search", serde_json::json!({"query": "rust"}))
            .unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }
}
```

- [ ] **Step 2: Wire `handler` into `main.rs`**

Replace `workers/web-search/src/main.rs` with:

```rust
//! web-search: query an operator-configured SearxNG instance and return ranked
//! structured hits over JSON-RPC stdio. GET-only; the LLM supplies only the
//! query string. Design:
//! docs/superpowers/specs/2026-06-09-web-search-worker-design.md

mod handler;
mod parse;
mod search;

use hhagent_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::WebSearchHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
```

- [ ] **Step 3: Run the whole web-search unit suite**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-search`
Expected: PASS — parse (6) + search (13) + handler (5) = 24 tests, zero dead-code warnings now that everything is wired.

- [ ] **Step 4: Commit**

```bash
git add workers/web-search/src/handler.rs workers/web-search/src/main.rs
git commit -m "feat(web-search): web.search JSON-RPC handler + from_env fail-closed"
```

---

## Phase C — Host-side manifest, registration, e2e, setup script

### Task 9: Host-side manifest `core/src/workers/web_search.rs`

**Files:**
- Create: `core/src/workers/web_search.rs`
- Modify: `core/src/workers.rs`
- Modify: `core/src/registry_build.rs`
- Modify: `core/Cargo.toml`

- [ ] **Step 1: Add `url` to core's dependencies**

In `core/Cargo.toml`, under `[dependencies]`, add (alphabetical-ish, near `serde`):

```toml
url                = { workspace = true }
```

- [ ] **Step 2: Write `core/src/workers/web_search.rs`**

Mirrors `core/src/workers/web_fetch.rs`. The one structural difference: `Net::Allowlist` is derived from the **endpoint** URL's host:port (so a loopback `:8888` is correct), not from the domain list mapped to `:443`.

```rust
//! Host-side manifest + `ToolEntry` constructor for the web-search worker.
//!
//! Containment caveat: until the egress proxy lands, the host allowlist is
//! enforced *inside* the worker (scheme + host) and matches host **names**, not
//! resolved IPs — it does not contain SSRF / DNS-rebinding to internal
//! addresses. The `Net::Allowlist` data built here is populated for the future
//! proxy, which owns IP-level containment. See `docs/threat-model.md`
//! ("Network egress").

use std::path::PathBuf;

use hhagent_sandbox::{Net, Profile, SandboxPolicy};
use url::Url;

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

/// Tool name the registry keys web-search on.
const TOOL_NAME: &str = "web-search";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "HHAGENT_WEB_SEARCH_BIN";
/// Exe-relative sibling default (cargo `target/debug` + flat installs).
const DEFAULT_BIN_NAME: &str = "hhagent-worker-web-search";
/// Operator-configured SearxNG endpoint, read from the daemon's own env.
const ENDPOINT_ENV: &str = "HHAGENT_WEB_SEARCH_ENDPOINT";

/// Derive the `Net::Allowlist` `host:port` entry from the endpoint URL. Returns
/// an empty list if the endpoint is unset or unparseable — the worker fails
/// closed at startup in that case, so an empty net policy is safe.
fn net_entries_from_endpoint(endpoint: &str) -> Vec<String> {
    match Url::parse(endpoint) {
        Ok(u) => match u.host_str() {
            Some(host) => {
                let port = u.port_or_known_default().unwrap_or(443);
                vec![format!("{host}:{port}")]
            }
            None => vec![],
        },
        Err(_) => vec![],
    }
}

/// Build the [`ToolEntry`] for the web-search worker.
///
/// The administrator controls both the endpoint (`HHAGENT_WEB_SEARCH_ENDPOINT`
/// on the daemon) and the host allowlist (`tool_allowlists` keyed
/// `"web-search"`); the LLM-supplied params carry only the query string and
/// cannot influence the URL. `Net::Allowlist` derives from the endpoint's
/// host:port; the allowlist gates which host the endpoint may name (the worker
/// re-checks `endpoint host ∈ allowlist` at startup).
///
/// Defaults: `Net::Allowlist`, `Profile::WorkerNetClient`, `cpu_ms = 5_000`,
/// `mem_mb = 256` (JSON parsing only — lighter than web-fetch's HTML/PDF),
/// `wall_clock_ms = Some(30_000)`, `SingleUse`. `fs_read` includes the resolver
/// config files so DNS works under the `--unshare-all` jail.
pub fn web_search_entry(binary: PathBuf, endpoint: &str, allowlist: &[String]) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries_from_endpoint(endpoint)),
        cpu_ms: 5_000,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        env: vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            ("HHAGENT_WEB_SEARCH_ALLOWLIST".to_string(), allow_json),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
    };
    ToolEntry {
        binary,
        policy,
        wall_clock_ms: Some(30_000),
        lifecycle: crate::worker_lifecycle::Lifecycle::SingleUse,
        sandbox_backend: None,
        container_image: None,
    }
}

/// web-search's manifest. Discovery mirrors web-fetch: a set
/// `HHAGENT_WEB_SEARCH_BIN` override is authoritative (honoured iff it names a
/// runnable file, else fails closed); only when unset do we fall back to the
/// exe-relative sibling `hhagent-worker-web-search`. The endpoint is read from
/// the daemon env at resolve time and injected into the worker policy.
pub struct WebSearchManifest;

impl WorkerManifest for WebSearchManifest {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn allowlist_tool(&self) -> Option<&'static str> {
        Some(TOOL_NAME)
    }

    fn resolve(&self, ctx: &ResolveCtx<'_>) -> Resolution {
        let binary = match discover_binary(ctx, BIN_ENV, DEFAULT_BIN_NAME) {
            Some(b) => b,
            None => {
                return Resolution::Misconfigured {
                    detail: format!(
                        "could not resolve worker binary: {BIN_ENV} set but not a \
                         runnable file, or unset with no sibling {DEFAULT_BIN_NAME} found"
                    ),
                };
            }
        };
        let endpoint = (ctx.get_env)(ENDPOINT_ENV).unwrap_or_default();
        let allowlist = (ctx.allowlist)(TOOL_NAME);
        Resolution::Register(web_search_entry(binary, &endpoint, &allowlist))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(
        get_env: &'a dyn Fn(&str) -> Option<String>,
        exists: &'a dyn Fn(&Path) -> bool,
        allowlist: &'a dyn Fn(&str) -> Vec<String>,
    ) -> ResolveCtx<'a> {
        ResolveCtx {
            get_env,
            exists,
            is_dir: &|_p| false,
            exe_dir: None,
            allowlist,
        }
    }

    #[test]
    fn resolve_registers_with_net_client_policy_and_endpoint_net() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("http://127.0.0.1:8888/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["127.0.0.1".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert_eq!(entry.binary, PathBuf::from("/opt/web-search"));
                assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
                assert_eq!(entry.policy.cpu_ms, 5_000);
                assert_eq!(entry.policy.mem_mb, 256);
                assert_eq!(entry.wall_clock_ms, Some(30_000));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
                // Net::Allowlist carries the endpoint host:port (loopback :8888).
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert_eq!(hosts, &vec!["127.0.0.1:8888".to_string()]);
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // Env carries the endpoint + the verbatim allowlist JSON.
                assert_eq!(entry.policy.env[0].0, ENDPOINT_ENV);
                assert_eq!(entry.policy.env[0].1, "http://127.0.0.1:8888/search");
                assert_eq!(entry.policy.env[1].0, "HHAGENT_WEB_SEARCH_ALLOWLIST");
                assert_eq!(entry.policy.env[1].1, r#"["127.0.0.1"]"#);
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_https_endpoint_maps_to_port_443() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-search".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Register(entry) => match &entry.policy.net {
                Net::Allowlist(hosts) => {
                    assert_eq!(hosts, &vec!["searx.example.org:443".to_string()]);
                }
                other => panic!("expected Net::Allowlist, got {other:?}"),
            },
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_misconfigured_when_no_binary_found() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match WebSearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("hhagent-worker-web-search"), "detail: {detail}");
            }
            other => panic!("expected Misconfigured, got {}", outcome_label(&other)),
        }
    }

    fn outcome_label(r: &Resolution) -> &'static str {
        match r {
            Resolution::Register(_) => "Register",
            Resolution::Disabled { .. } => "Disabled",
            Resolution::Misconfigured { .. } => "Misconfigured",
        }
    }
}
```

- [ ] **Step 3: Register the module + manifest**

In `core/src/workers.rs`, add after `pub mod web_fetch;`:

```rust
pub mod web_search;
```

In `core/src/registry_build.rs`, add to the `WORKER_MANIFESTS` array (after the web-fetch line):

```rust
    &crate::workers::web_search::WebSearchManifest,
```

- [ ] **Step 4: Run the manifest tests + core build**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --lib web_search`
Expected: PASS (3 manifest tests).

Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/Cargo.toml core/src/workers/web_search.rs core/src/workers.rs core/src/registry_build.rs
git commit -m "feat(core): web-search host manifest + register in WORKER_MANIFESTS"
```

---

### Task 10: End-to-end test `core/tests/web_search_e2e.rs`

**Files:**
- Create: `core/tests/web_search_e2e.rs`

- [ ] **Step 1: Write the e2e test**

Mirrors `core/tests/web_fetch_e2e.rs`. Hermetic deny-path: an endpoint whose host is **not** on the allowlist makes the worker fail closed at startup, so `dispatch` errors — no server needed. The `#[ignore]` test needs a live SearxNG (set `HHAGENT_WEB_SEARCH_ENDPOINT` before running with `--ignored`).

```rust
//! End-to-end: agent core spawns the `web-search` worker under the platform
//! sandbox and round-trips a `web.search` call through `tool_host::dispatch`.
//!
//! Hermetic test (`endpoint_off_allowlist_fails_closed`): the configured
//! endpoint host is NOT on the worker's allowlist, so the worker refuses at
//! startup (fail-closed `from_env`) and the dispatch errors before any network
//! egress — no server required.
//!
//! Ignored test (`real_search_against_searxng`): a real query against a live
//! SearxNG instance. Run manually with `--ignored` and
//! `HHAGENT_WEB_SEARCH_ENDPOINT` set; also validates DNS/TLS (or loopback)
//! inside the sandbox jail.
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the worker binary, or a working
//! sandbox is missing — same posture as `web_fetch_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use hhagent_core::secrets::Vault;
use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_core::workers::web_search::web_search_entry;
use hhagent_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

async fn probe_and_pool(conn_spec: &hhagent_db::conn::ConnectSpec) -> sqlx::PgPool {
    hhagent_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-search-e2e"}),
    )
    .await
    .expect("probe run");
    hhagent_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

fn dispatch_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    endpoint: String,
    allowlist: Vec<String>,
}

fn ready_or_skip(endpoint: &str, allowlist: &[&str]) -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("hhagent-worker-web-search");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] web-search worker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ws-d",
        "ws-l",
        &format!("hhagent-supervisor-test-pg-websearch-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        endpoint: endpoint.to_string(),
        allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
    })
}

#[test]
fn endpoint_off_allowlist_fails_closed() {
    // Endpoint host NOT on the allowlist → worker refuses at startup. Hermetic.
    let env = match ready_or_skip("https://searx.example.org/search", &["other.example.org"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_search_entry(env.worker_path.clone(), &env.endpoint, &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };

        // The worker exits non-zero at startup (fail-closed from_env), so either
        // spawn yields a worker whose first dispatch errors, or dispatch surfaces
        // the broken pipe — both are errors. Assert the round trip does NOT
        // succeed.
        let spawned = spawn_worker(&*backend, &spec);
        if let Ok(mut sworker) = spawned {
            let result = dispatch(
                &pool,
                &Vault::new(),
                &mut sworker,
                "web-search",
                "web.search",
                serde_json::json!({"query": "anything"}),
            )
            .await;
            assert!(
                result.is_err(),
                "expected dispatch to fail (worker fails closed on off-allowlist endpoint), got: {result:?}"
            );
            let _ = sworker.close();
        }
        pool.close().await;
    });
}

#[test]
#[ignore = "hits a live SearxNG; set HHAGENT_WEB_SEARCH_ENDPOINT; validates DNS/TLS/loopback in jail"]
fn real_search_against_searxng() {
    let endpoint = std::env::var("HHAGENT_WEB_SEARCH_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:8888/search".to_string());
    // Allowlist the endpoint host so the worker accepts it.
    let host = url_host(&endpoint);
    let env = match ready_or_skip(&endpoint, &[&host]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_search_entry(env.worker_path.clone(), &env.endpoint, &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-search under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-search",
            "web.search",
            serde_json::json!({"query": "rust programming language", "count": 5}),
        )
        .await
        .expect("web.search round trip (network + DNS in jail)");

        let results = result["results"].as_array().expect("results array");
        assert!(!results.is_empty(), "expected at least one hit");
        assert!(results[0]["url"].as_str().unwrap_or("").starts_with("http"));

        let _ = sworker.close();
        pool.close().await;
    });
}

/// Extract the host from a URL string for the ignored test's allowlist.
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}
```

**Important:** this test file uses `url::Url`, and integration-test crates do **not** inherit the library's normal `[dependencies]` — they only see the crate plus its `[dev-dependencies]`. So add `url` to `core/Cargo.toml` `[dev-dependencies]` (in addition to the normal-dep line added in Task 9):

```toml
[dev-dependencies]
url = { workspace = true }
# ... existing dev-deps unchanged
```

- [ ] **Step 2: Build the test (typecheck) and run the hermetic arm**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --test web_search_e2e -- --nocapture`
Expected: PASS or `[SKIP]` lines (no PG/supervisor/sandbox/binary). On the dev Mac without `HHAGENT_PG_BIN_DIR`, a clean `[SKIP]` is the expected pass posture. If it compiles and the hermetic test runs where PG is available, it passes.

- [ ] **Step 3: Commit**

```bash
git add core/tests/web_search_e2e.rs core/Cargo.toml
git commit -m "test(web-search): e2e fail-closed deny-path + ignored real-SearxNG round trip"
```

---

### Task 11: SearxNG dev setup script

**Files:**
- Create: `scripts/web-search/setup-searxng.sh`

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
# Stand up a local SearxNG instance for the hhagent web-search worker.
#
# SearxNG serves plain HTTP on a loopback port and DISABLES the JSON format by
# default — this script writes a settings.yml that enables JSON and runs the
# official container bound to 127.0.0.1:8888. Cross-platform: Docker Desktop on
# macOS, docker or podman on Linux. Dev convenience only; not part of the
# worker's trust boundary.
set -euo pipefail

PORT="${HHAGENT_SEARXNG_PORT:-8888}"
NAME="${HHAGENT_SEARXNG_NAME:-hhagent-searxng}"
STATE_DIR="${HHAGENT_SEARXNG_STATE:-$HOME/.local/state/hhagent/searxng}"
IMAGE="searxng/searxng:latest"

# Pick a container runtime.
if command -v docker >/dev/null 2>&1; then
  RT=docker
elif command -v podman >/dev/null 2>&1; then
  RT=podman
else
  echo "error: need docker or podman on PATH to run SearxNG" >&2
  exit 1
fi

mkdir -p "$STATE_DIR"
SETTINGS="$STATE_DIR/settings.yml"

# Generate a random secret_key and enable the JSON output format.
if [ ! -f "$SETTINGS" ]; then
  SECRET="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  cat >"$SETTINGS" <<YAML
# Minimal SearxNG settings for hhagent web-search (dev). The key line is
# search.formats — the JSON API is off by default.
use_default_settings: true
server:
  secret_key: "$SECRET"
  bind_address: "0.0.0.0"
  port: 8080
search:
  formats:
    - html
    - json
YAML
  echo "wrote $SETTINGS"
fi

# (Re)start the container.
if "$RT" ps -a --format '{{.Names}}' | grep -qx "$NAME"; then
  echo "restarting existing container $NAME"
  "$RT" rm -f "$NAME" >/dev/null
fi

"$RT" run -d \
  --name "$NAME" \
  -p "127.0.0.1:${PORT}:8080" \
  -v "$SETTINGS:/etc/searxng/settings.yml:ro" \
  "$IMAGE" >/dev/null

cat <<MSG

SearxNG running at http://127.0.0.1:${PORT}/

Export these for the hhagent daemon / web-search worker:

  export HHAGENT_WEB_SEARCH_ENDPOINT='http://127.0.0.1:${PORT}/search'
  export HHAGENT_WEB_SEARCH_ALLOWLIST='["127.0.0.1"]'

Smoke test the JSON API:

  curl -s 'http://127.0.0.1:${PORT}/search?q=rust&format=json' | head -c 400

Stop it with:  $RT rm -f $NAME
MSG
```

- [ ] **Step 2: Make it executable**

```bash
chmod +x scripts/web-search/setup-searxng.sh
```

- [ ] **Step 3: Shellcheck (if available) and a syntax check**

Run: `bash -n scripts/web-search/setup-searxng.sh`
Expected: no output (syntax OK). If `shellcheck` is installed, run `shellcheck scripts/web-search/setup-searxng.sh` and address warnings.

- [ ] **Step 4: Commit**

```bash
git add scripts/web-search/setup-searxng.sh
git commit -m "chore(web-search): dev setup script for a local SearxNG (JSON enabled)"
```

---

### Task 12: Workspace verification + docs (HANDOVER + ROADMAP)

**Files:**
- Modify: `docs/devel/ROADMAP.md`
- Modify: `docs/devel/handovers/HANDOVER.md`

- [ ] **Step 1: Full clean build + workspace test**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Expected: PASS (12 crates now — web-common + web-search added).

Run: `source "$HOME/.cargo/env" && cargo test --workspace`
Expected: PASS on macOS skip-as-pass posture (live-PG suites `[SKIP]`); the new web-common + web-search unit suites + web-search manifest tests are green. Record the new counts.

- [ ] **Step 2: Clippy the new crates + core**

Run: `source "$HOME/.cargo/env" && cargo clippy -p hhagent-worker-web-common -p hhagent-worker-web-search -p hhagent-worker-web-fetch --all-targets -- -D warnings`
Expected: exit 0.

Run: `source "$HOME/.cargo/env" && cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 3: Tick ROADMAP:146**

In `docs/devel/ROADMAP.md`, change the `web-search` line (currently `- [ ] \`web-search\` worker (SearxNG default)`) to `- [x]` with a one-line summary mirroring the web-fetch entry's style: crate `workers/web-search` + shared `workers/web-common` extraction, `web.search` returning structured hits, operator-configured endpoint (http loopback-only), Net::Allowlist from endpoint host:port, setup script; branch `feat/web-search-worker`, 2026-06-09. Note the deferred items (category/lang params, pagination, hermetic mock e2e).

- [ ] **Step 4: Update HANDOVER**

Follow the HANDOVER "How to update this document at session end" checklist: bump `Last updated`/current-state/last-commit, move the web-search work into "Recently completed (this session)" with file paths + the shared-crate extraction note + the test-count delta, refresh the "Working state" crate tree (add `workers/web-common` + `workers/web-search`, update the crate count to 12, add the web-search suite rows to the suite table), and write a fresh "Next TODO".

- [ ] **Step 5: Commit the docs**

```bash
git add docs/devel/ROADMAP.md docs/devel/handovers/HANDOVER.md
git commit -m "docs(handover): web-search worker shipped (ROADMAP:146)"
```

- [ ] **Step 6: Push + open the PR**

```bash
git push -u origin feat/web-search-worker
gh pr create --base main --title "feat: web-search worker (SearxNG) + shared web-common crate (ROADMAP:146)" --body "<summary + test counts + link ROADMAP:146>"
```

---

## Self-review checklist (completed during planning)

- **Spec coverage:** structured-hits contract (Task 5/8), shared web-common extraction (Tasks 1–4), endpoint+scheme rule incl. loopback (Task 6), Net::Allowlist from endpoint host:port (Task 9), `web.search` params + count cap (Tasks 7–8), host manifest + registration (Task 9), e2e fail-closed + ignored real (Task 10), setup script with JSON enabled (Task 11), threat-model caveat (Task 9 rustdoc), docs (Task 12). All spec sections map to a task.
- **Placeholder scan:** every code step carries complete code; the only free-form steps are the HANDOVER prose update (Task 12 Step 4, governed by the in-repo checklist) and the PR body.
- **Type consistency:** `Hit`, `SearchError`, `search()`/`validate_endpoint()`/`build_query_url()`/`is_loopback()`, `WebSearchHandler::{from_env,with_parts}`, `web_search_entry(binary, endpoint, allowlist)`, `WebSearchManifest`, `DEFAULT_COUNT`/`MAX_COUNT` are named identically everywhere they appear. Transport types (`HttpGet`/`RawResponse`/`ReqwestGet`/`FakeGet`) resolve to `hhagent_worker_web_common` after Phase A.
