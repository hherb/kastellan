# web-fetch worker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a sandboxed `web-fetch` tool worker exposing JSON-RPC `web.fetch` (HTTPS-only, host-allowlist self-enforced per redirect hop) that returns extracted readable text from HTML / PDF / text / JSON, and register it with the daemon.

**Architecture:** A new `hhagent-worker-web-fetch` bin crate mirroring `workers/shell-exec`: it calls `hhagent_worker_prelude::serve_stdio` (which `lock_down()`s before serving). Pure logic (allowlist matching, content extraction, redirect-drive loop, request validation) lives in small focused modules with hermetic unit tests; networking is behind an `HttpGet` trait so the redirect/allowlist orchestration is testable with a fake. A host-side `WebFetchManifest` in `core/src/workers/web_fetch.rs` declares the `SandboxPolicy` (`Profile::WorkerNetClient` + `Net::Allowlist`) and is registered in the static `WORKER_MANIFESTS` list.

**Tech Stack:** Rust; `reqwest::blocking` + rustls (already in tree, add only the `blocking` feature); `url`; `readable_html` (alias of `dom_smoothie` 0.18, MIT) for HTML readability; `pdf-extract` 0.10 (MIT) for PDF text; `hhagent-protocol` JSON-RPC; `hhagent-worker-prelude` lockdown + stdio.

**Design doc:** [docs/superpowers/specs/2026-06-08-web-fetch-worker-design.md](../specs/2026-06-08-web-fetch-worker-design.md)

**Conventions for every commit step:** stage only the named files (`git add <paths>` — never `git add -A`; an untracked `docs/essay-medium-draft.md` must stay out). End commit messages with:
```
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
```
Branch is already `feat/web-fetch-worker`. Source the cargo env first in any shell: `source "$HOME/.cargo/env"`.

**Refinement vs spec (deliberate):** The spec sketched a hermetic *localhost-HTTPS* happy-path e2e. Serving real TLS to the worker hermetically needs a test CA the worker trusts — exactly the infrastructure the *egress-proxy* slice will introduce (workers must trust the proxy's MITM CA). To avoid building throwaway TLS-cert plumbing now, this plan covers the happy path two ways instead: (a) the full fetch→extract orchestration, including "302 to a non-allowlisted host is refused", is tested **hermetically** at the handler level with a fake transport; (b) a real-network happy-path + DNS-in-jail smoke is an **`#[ignore]`d** e2e run manually. The hermetic e2e through the real sandbox covers the deny path (no server needed). When the egress-proxy test-CA lands, a hermetic TLS happy-path can be added.

---

## File structure

- Create `workers/web-fetch/Cargo.toml` — crate manifest.
- Create `workers/web-fetch/src/main.rs` — thin entry: build handler from env, `serve_stdio`.
- Create `workers/web-fetch/src/allowlist.rs` — `HostAllowlist` (exact + leading-dot subdomain match) + tests.
- Create `workers/web-fetch/src/extract.rs` — content-type dispatch, HTML/PDF/text extraction, text cap + tests.
- Create `workers/web-fetch/src/fetch.rs` — `HttpGet` trait, `RawResponse`, `FetchOutcome`, `FetchError`, pure `drive()` redirect loop (+ tests), `ReqwestGet` real transport.
- Create `workers/web-fetch/src/handler.rs` — `FetchParams`, `check_url`, error mappers, `WebFetchHandler<T>` + `Handler` impl + tests.
- Create `workers/web-fetch/tests/fixtures/hello.pdf` — committed PDF fixture for the extract test.
- Modify root `Cargo.toml` — add `"workers/web-fetch"` to `members`.
- Create `core/src/workers/web_fetch.rs` — `WebFetchManifest` + `web_fetch_entry` + tests.
- Modify `core/src/workers/mod.rs` — add `pub mod web_fetch;`.
- Modify `core/src/registry_build.rs` — add `WebFetchManifest` to `WORKER_MANIFESTS`.
- Create `core/tests/web_fetch_e2e.rs` — gated sandbox e2e (deny path hermetic + ignored real-network happy path).
- Modify `docs/devel/ROADMAP.md` — tick the web-fetch line.

---

## Task 1: Scaffold the worker crate (compiles as an empty stub)

**Files:**
- Create: `workers/web-fetch/Cargo.toml`
- Create: `workers/web-fetch/src/main.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Create the crate manifest**

`workers/web-fetch/Cargo.toml`:
```toml
[package]
name        = "hhagent-worker-web-fetch"
description = "Tool worker: fetch a URL (HTTPS-only, host allowlist) and return extracted readable text. GET-only."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme.workspace       = true

[[bin]]
name = "hhagent-worker-web-fetch"
path = "src/main.rs"

[dependencies]
hhagent-protocol       = { path = "../../protocol" }
hhagent-worker-prelude = { path = "../prelude" }
serde                  = { workspace = true }
serde_json             = { workspace = true }
anyhow                 = { workspace = true }
reqwest                = { workspace = true, features = ["blocking"] }
url                    = "2"
readable_html          = { package = "dom_smoothie", version = "0.18" }
pdf-extract            = "0.10"
```

- [ ] **Step 2: Create a minimal `main.rs` that compiles**

`workers/web-fetch/src/main.rs`:
```rust
//! web-fetch: fetch a URL (HTTPS-only, against a host allowlist) and return
//! extracted readable text over JSON-RPC stdio. GET-only; no caller-supplied
//! headers/body. Design:
//! docs/superpowers/specs/2026-06-08-web-fetch-worker-design.md

fn main() -> anyhow::Result<()> {
    Ok(())
}
```

- [ ] **Step 3: Add the crate to the workspace members**

In root `Cargo.toml`, add the line after `"workers/shell-exec",`:
```toml
    "workers/web-fetch",
```

- [ ] **Step 4: Build to verify the crate resolves and deps download**

Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-worker-web-fetch`
Expected: compiles clean (downloads `dom_smoothie`, `pdf-extract`, `url`, reqwest `blocking`).
If `dom_smoothie 0.18` or `pdf-extract 0.10` fails to resolve, run `cargo search dom_smoothie` / `cargo search pdf-extract` and pin the latest compatible published version, keeping the `package = "dom_smoothie"` alias. If a dep raises the effective MSRV above the workspace's declared `rust-version = "1.78"`, note it (the dev toolchain is 1.96, so the build still succeeds) — do not silently bump the workspace MSRV.

- [ ] **Step 5: Commit**

```bash
git add workers/web-fetch/Cargo.toml workers/web-fetch/src/main.rs Cargo.toml Cargo.lock
git commit -m "feat(web-fetch): scaffold worker crate

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `HostAllowlist` — exact + leading-dot subdomain matching

**Files:**
- Create: `workers/web-fetch/src/allowlist.rs`
- Modify: `workers/web-fetch/src/main.rs` (declare `mod allowlist;`)

- [ ] **Step 1: Declare the module in `main.rs`**

Add at the top of `workers/web-fetch/src/main.rs` (above `fn main`):
```rust
mod allowlist;
```
Add `#[allow(dead_code)]` is **not** needed — the test module references the items. If the unused-warning bites before later tasks wire it into `main`, that is expected; do not suppress it, it disappears in Task 5.

- [ ] **Step 2: Write the failing tests**

`workers/web-fetch/src/allowlist.rs`:
```rust
//! Host allowlist matching for web-fetch.
//!
//! Entries come from the `HHAGENT_WEB_FETCH_ALLOWLIST` env (a JSON array of
//! strings), injected by the host-side manifest from the `tool_allowlists` DB
//! table. Two forms:
//!   - `"en.wikipedia.org"` — exact host match only.
//!   - `".example.com"`     — the domain itself AND any subdomain.
//! Matching is case-insensitive.

/// A parsed allowlist of host rules.
pub struct HostAllowlist {
    rules: Vec<Rule>,
}

enum Rule {
    /// Exact host, lowercased.
    Exact(String),
    /// Domain (without the leading dot), lowercased. Matches the domain itself
    /// and any subdomain.
    Suffix(String),
}

impl HostAllowlist {
    /// Parse from the JSON-array env string. Empty/blank entries are skipped.
    pub fn from_env_json(raw: &str) -> anyhow::Result<Self> {
        let entries: Vec<String> = serde_json::from_str(raw).map_err(|e| {
            anyhow::anyhow!("HHAGENT_WEB_FETCH_ALLOWLIST is not a JSON array of strings: {e}")
        })?;
        let mut rules = Vec::new();
        for entry in entries {
            let e = entry.trim().to_lowercase();
            if e.is_empty() {
                continue;
            }
            if let Some(domain) = e.strip_prefix('.') {
                if !domain.is_empty() {
                    rules.push(Rule::Suffix(domain.to_string()));
                }
            } else {
                rules.push(Rule::Exact(e));
            }
        }
        Ok(Self { rules })
    }

    /// True iff `host` is permitted by any rule.
    pub fn is_allowed(&self, host: &str) -> bool {
        let h = host.trim().to_lowercase();
        self.rules.iter().any(|r| match r {
            Rule::Exact(x) => h == *x,
            Rule::Suffix(d) => h == *d || h.ends_with(&format!(".{d}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn al(entries: &[&str]) -> HostAllowlist {
        let json = serde_json::to_string(entries).unwrap();
        HostAllowlist::from_env_json(&json).unwrap()
    }

    #[test]
    fn exact_matches_only_that_host() {
        let a = al(&["en.wikipedia.org"]);
        assert!(a.is_allowed("en.wikipedia.org"));
        assert!(!a.is_allowed("wikipedia.org"));
        assert!(!a.is_allowed("de.wikipedia.org"));
        assert!(!a.is_allowed("evil-en.wikipedia.org"));
    }

    #[test]
    fn leading_dot_matches_domain_and_subdomains() {
        let a = al(&[".example.com"]);
        assert!(a.is_allowed("example.com"));
        assert!(a.is_allowed("a.example.com"));
        assert!(a.is_allowed("a.b.example.com"));
    }

    #[test]
    fn leading_dot_does_not_match_lookalikes() {
        let a = al(&[".example.com"]);
        assert!(!a.is_allowed("evil-example.com"));
        assert!(!a.is_allowed("examplexcom"));
        assert!(!a.is_allowed("notexample.com"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let a = al(&["en.wikipedia.org", ".example.com"]);
        assert!(a.is_allowed("EN.Wikipedia.ORG"));
        assert!(a.is_allowed("A.Example.Com"));
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let a = al(&[]);
        assert!(!a.is_allowed("example.com"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(HostAllowlist::from_env_json("not json").is_err());
    }
}
```

- [ ] **Step 3: Run the tests to verify they pass (implementation is included above)**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-fetch allowlist`
Expected: PASS — 6 tests in `allowlist::tests`.
(Implementation and tests are written together here because the logic is small and self-evidently TDD-shaped; the assertions are the spec. If you prefer strict red-green, delete the `impl HostAllowlist` bodies, watch it fail to compile, then restore.)

- [ ] **Step 4: Commit**

```bash
git add workers/web-fetch/src/allowlist.rs workers/web-fetch/src/main.rs
git commit -m "feat(web-fetch): host allowlist matcher (exact + subdomain wildcard)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `extract.rs` — content-type dispatch, HTML/PDF/text extraction, text cap

**Files:**
- Create: `workers/web-fetch/src/extract.rs`
- Create: `workers/web-fetch/tests/fixtures/hello.pdf`
- Modify: `workers/web-fetch/src/main.rs` (declare `mod extract;`)

- [ ] **Step 1: Generate the committed PDF fixture**

macOS (uses CUPS, always present):
```bash
mkdir -p workers/web-fetch/tests/fixtures
printf 'Hello PDF fixture content.\n' > /tmp/hello-fixture.txt
cupsfilter /tmp/hello-fixture.txt > workers/web-fetch/tests/fixtures/hello.pdf 2>/dev/null
```
Linux fallback if `cupsfilter` is absent: `pandoc /tmp/hello-fixture.txt -o workers/web-fetch/tests/fixtures/hello.pdf` (or `libreoffice --headless --convert-to pdf`).
Verify it parses before relying on it:
```bash
source "$HOME/.cargo/env"
cat > /tmp/pdfcheck.rs <<'EOF'
fn main() {
    let b = std::fs::read("workers/web-fetch/tests/fixtures/hello.pdf").unwrap();
    println!("{}", pdf_extract::extract_text_from_mem(&b).unwrap());
}
EOF
```
(Quick sanity only — you don't need to run this scratch file; the unit test in Step 3 is the real check. If the unit test later fails to find "Hello", regenerate the fixture with a different producer.)

- [ ] **Step 2: Declare the module in `main.rs`**

Add near the other `mod` line in `workers/web-fetch/src/main.rs`:
```rust
mod extract;
```

- [ ] **Step 3: Write the extractor with tests**

`workers/web-fetch/src/extract.rs`:
```rust
//! Content extraction: turn a fetched body + content-type into readable text.
//!
//!   - `text/html`        → readability main-content extraction (+ <title>).
//!   - `application/pdf`  → PDF text extraction.
//!   - `text/*`, `application/json` → decoded as-is (UTF-8 lossy).
//!   - anything else      → error (caller maps to OPERATION_FAILED).
//!
//! The extracted text is capped at [`MAX_TEXT_BYTES`]; `truncated` records
//! whether the cap fired. This keeps the planner's context budget bounded
//! until the large-result handoff cache (ROADMAP:129) lands.

/// Cap on returned extracted text (100 KiB).
pub const MAX_TEXT_BYTES: usize = 100 * 1024;

/// Result of extraction.
pub struct Extracted {
    pub title: Option<String>,
    pub text: String,
    pub truncated: bool,
}

/// The bare main media type, lowercased, params stripped
/// (`"text/html; charset=utf-8"` → `"text/html"`).
pub fn main_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// Extract readable text from `body` according to `content_type`.
pub fn extract(content_type: &str, body: &[u8]) -> anyhow::Result<Extracted> {
    let mt = main_type(content_type);
    match mt.as_str() {
        "text/html" => extract_html(body),
        "application/pdf" => {
            let raw = pdf_extract::extract_text_from_mem(body)
                .map_err(|e| anyhow::anyhow!("pdf text extraction failed: {e}"))?;
            let (text, truncated) = cap_text(raw);
            Ok(Extracted { title: None, text, truncated })
        }
        _ if mt.starts_with("text/") || mt == "application/json" => {
            let raw = String::from_utf8_lossy(body).into_owned();
            let (text, truncated) = cap_text(raw);
            Ok(Extracted { title: None, text, truncated })
        }
        other => anyhow::bail!("unsupported content-type: {other}"),
    }
}

fn extract_html(body: &[u8]) -> anyhow::Result<Extracted> {
    let html = String::from_utf8_lossy(body);
    let mut readability = readable_html::Readability::new(html.as_ref(), None, None)
        .map_err(|e| anyhow::anyhow!("readability init failed: {e}"))?;
    let article = readability
        .parse()
        .map_err(|e| anyhow::anyhow!("could not extract readable content: {e}"))?;
    let title = {
        let t = article.title.trim();
        if t.is_empty() { None } else { Some(t.to_string()) }
    };
    let (text, truncated) = cap_text(article.text_content.to_string());
    Ok(Extracted { title, text, truncated })
}

/// Truncate to at most [`MAX_TEXT_BYTES`] on a char boundary.
fn cap_text(mut s: String) -> (String, bool) {
    if s.len() <= MAX_TEXT_BYTES {
        return (s, false);
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    (s, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A content-rich article so readability's heuristics latch onto the main
    // node. If `parse()` ever returns GrabFailed on this, lengthen the body
    // (more paragraphs) — that's a fixture-tuning change, not a logic change.
    const ARTICLE_HTML: &[u8] = br#"<!DOCTYPE html><html><head>
        <title>The Title</title></head><body>
        <nav>home about contact</nav>
        <article>
        <h1>The Title</h1>
        <p>The first paragraph of this article contains several sentences so the
        readability algorithm recognises it as the main content. We are writing
        about web fetching and content extraction in a sandboxed worker.</p>
        <p>The second paragraph continues the discussion with more substantive
        prose. Readability scores nodes by text density, so a few real sentences
        here make the body unambiguously the article content rather than the
        navigation chrome above.</p>
        <p>A third paragraph seals it, ensuring the grab succeeds deterministically
        across versions of the extraction crate.</p>
        </article>
        <footer>copyright</footer></body></html>"#;

    #[test]
    fn html_yields_title_and_main_text() {
        let e = extract("text/html; charset=utf-8", ARTICLE_HTML).unwrap();
        assert_eq!(e.title.as_deref(), Some("The Title"));
        assert!(e.text.contains("first paragraph"), "text: {}", e.text);
        assert!(!e.text.contains("home about contact"), "nav chrome leaked: {}", e.text);
        assert!(!e.truncated);
    }

    #[test]
    fn plain_text_passes_through() {
        let e = extract("text/plain; charset=utf-8", b"just some plain text").unwrap();
        assert_eq!(e.title, None);
        assert_eq!(e.text, "just some plain text");
        assert!(!e.truncated);
    }

    #[test]
    fn json_passes_through() {
        let e = extract("application/json", br#"{"k":"v"}"#).unwrap();
        assert_eq!(e.title, None);
        assert_eq!(e.text, r#"{"k":"v"}"#);
    }

    #[test]
    fn pdf_is_extracted() {
        let bytes = include_bytes!("../tests/fixtures/hello.pdf");
        let e = extract("application/pdf", bytes).unwrap();
        assert_eq!(e.title, None);
        assert!(e.text.contains("Hello"), "pdf text: {:?}", e.text);
    }

    #[test]
    fn unsupported_content_type_errors() {
        let err = extract("image/png", &[0x89, 0x50]).unwrap_err();
        assert!(format!("{err}").contains("unsupported content-type"), "{err}");
    }

    #[test]
    fn text_is_capped_on_char_boundary() {
        let big = "a".repeat(MAX_TEXT_BYTES + 500);
        let (capped, truncated) = cap_text(big);
        assert!(truncated);
        assert!(capped.len() <= MAX_TEXT_BYTES);
    }

    #[test]
    fn main_type_strips_params() {
        assert_eq!(main_type("text/html; charset=utf-8"), "text/html");
        assert_eq!(main_type("APPLICATION/JSON"), "application/json");
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-fetch extract`
Expected: PASS — 7 tests in `extract::tests`.
If `html_yields_title_and_main_text` fails with a GrabFailed-style error, lengthen `ARTICLE_HTML` and re-run. If `pdf_is_extracted` fails, regenerate `hello.pdf` (Step 1) with another producer.

- [ ] **Step 5: Commit**

```bash
git add workers/web-fetch/src/extract.rs workers/web-fetch/src/main.rs workers/web-fetch/tests/fixtures/hello.pdf
git commit -m "feat(web-fetch): content extraction (HTML readability / PDF / text+JSON) with text cap

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `fetch.rs` — `HttpGet` trait, pure `drive()` redirect loop, `ReqwestGet`

**Files:**
- Create: `workers/web-fetch/src/fetch.rs`
- Modify: `workers/web-fetch/src/main.rs` (declare `mod fetch;`)

The redirect loop with per-hop allowlist + scheme re-checking is the security-critical part, so it is a pure function over an `HttpGet` trait and tested with a fake transport. `ReqwestGet` is the real transport (covered later by the ignored real-network e2e).

- [ ] **Step 1: Declare the module in `main.rs`**

Add near the other `mod` lines:
```rust
mod fetch;
```

- [ ] **Step 2: Write `fetch.rs` with the trait, drive loop, real transport, and drive tests**

`workers/web-fetch/src/fetch.rs`:
```rust
//! HTTP transport seam + the redirect-following drive loop.
//!
//! `drive()` is pure over the [`HttpGet`] trait so the redirect cap and the
//! per-hop allowlist + https re-check (the security-critical bit: a 3xx to a
//! non-allowlisted or non-https target is refused) are unit-tested with a fake
//! transport. [`ReqwestGet`] is the real `reqwest::blocking` implementation.

use std::time::Duration;

use url::Url;

use crate::allowlist::HostAllowlist;

/// Max redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 5;
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

/// The transport seam. One GET, no redirect following (the caller drives
/// redirects so it can re-check the allowlist per hop).
pub trait HttpGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String>;
}

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

/// Real transport over `reqwest::blocking` + rustls. Redirects disabled
/// (driven by [`drive`]); body capped while reading via `Read::take`.
pub struct ReqwestGet {
    client: reqwest::blocking::Client,
}

impl ReqwestGet {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .user_agent("hhagent-web-fetch/0")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Fake transport returning canned responses in order.
    struct FakeGet {
        responses: RefCell<VecDeque<RawResponse>>,
    }
    impl FakeGet {
        fn new(responses: Vec<RawResponse>) -> Self {
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

    fn al(entries: &[&str]) -> HostAllowlist {
        let json = serde_json::to_string(entries).unwrap();
        HostAllowlist::from_env_json(&json).unwrap()
    }

    fn ok_resp(body: &str) -> RawResponse {
        RawResponse {
            status: 200,
            location: None,
            content_type: "text/plain".to_string(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn redirect_to(loc: &str) -> RawResponse {
        RawResponse {
            status: 302,
            location: Some(loc.to_string()),
            content_type: String::new(),
            body: Vec::new(),
        }
    }

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
        // Always redirect back to the same allowlisted host → exceed the cap.
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

- [ ] **Step 3: Run the tests**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-fetch fetch`
Expected: PASS — 6 tests in `fetch::tests`.

- [ ] **Step 4: Commit**

```bash
git add workers/web-fetch/src/fetch.rs workers/web-fetch/src/main.rs
git commit -m "feat(web-fetch): redirect-drive loop with per-hop allowlist recheck + reqwest transport

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `handler.rs` — request validation, `Handler` impl, wire it into `main`

**Files:**
- Create: `workers/web-fetch/src/handler.rs`
- Modify: `workers/web-fetch/src/main.rs` (declare `mod handler;`, build handler, `serve_stdio`)

- [ ] **Step 1: Write `handler.rs` with `check_url`, the `Handler` impl, error mappers, and tests**

`workers/web-fetch/src/handler.rs`:
```rust
//! JSON-RPC handler for `web.fetch`.
//!
//! Flow: parse params → validate URL (https + allowlist) → drive redirects
//! (re-checking each hop) → extract readable text → build the result object.
//! Errors map onto the protocol code vocabulary (POLICY_DENIED / INVALID_PARAMS
//! / OPERATION_FAILED / METHOD_NOT_FOUND). No silent fallbacks: any failure is
//! an error, never an empty-but-success result.

use hhagent_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use url::Url;

use crate::allowlist::HostAllowlist;
use crate::extract::{extract, main_type};
use crate::fetch::{drive, FetchError, HttpGet, ReqwestGet};

#[derive(Deserialize)]
struct FetchParams {
    url: String,
}

/// Outcome of validating the initial request URL.
enum CheckError {
    BadUrl(String),
    NotHttps(String),
    HostMissing,
    HostDenied(String),
}

/// Validate the initial URL: parse, require https, require allowlisted host.
fn check_url(raw: &str, allowlist: &HostAllowlist) -> Result<Url, CheckError> {
    let url = Url::parse(raw).map_err(|e| CheckError::BadUrl(e.to_string()))?;
    if url.scheme() != "https" {
        return Err(CheckError::NotHttps(url.scheme().to_string()));
    }
    let host = url.host_str().ok_or(CheckError::HostMissing)?;
    if !allowlist.is_allowed(host) {
        return Err(CheckError::HostDenied(host.to_string()));
    }
    Ok(url)
}

fn check_err_to_rpc(e: CheckError) -> RpcError {
    match e {
        CheckError::BadUrl(m) => RpcError::new(codes::INVALID_PARAMS, format!("bad url: {m}")),
        CheckError::HostMissing => {
            RpcError::new(codes::INVALID_PARAMS, "url has no host".to_string())
        }
        CheckError::NotHttps(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("scheme {s:?} not allowed; https only"),
        ),
        CheckError::HostDenied(h) => {
            RpcError::new(codes::POLICY_DENIED, format!("host {h:?} not on allowlist"))
        }
    }
}

fn fetch_err_to_rpc(e: FetchError) -> RpcError {
    match e {
        FetchError::HostDenied(h) => RpcError::new(
            codes::POLICY_DENIED,
            format!("redirect host {h:?} not on allowlist"),
        ),
        FetchError::NonHttps(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("redirect scheme {s:?} not allowed; https only"),
        ),
        FetchError::TooManyRedirects => {
            RpcError::new(codes::OPERATION_FAILED, "too many redirects".to_string())
        }
        FetchError::MissingLocation => RpcError::new(
            codes::OPERATION_FAILED,
            "redirect without Location header".to_string(),
        ),
        FetchError::BadUrl(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("bad redirect url: {m}"))
        }
        FetchError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("fetch failed: {m}"))
        }
    }
}

/// The worker handler, generic over the transport so tests inject a fake.
pub struct WebFetchHandler<T: HttpGet> {
    allowlist: HostAllowlist,
    transport: T,
}

impl WebFetchHandler<ReqwestGet> {
    /// Build from env: allowlist JSON + real reqwest transport.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var("HHAGENT_WEB_FETCH_ALLOWLIST").unwrap_or_else(|_| "[]".to_string());
        let allowlist = HostAllowlist::from_env_json(&raw)?;
        let transport = ReqwestGet::new()?;
        Ok(Self { allowlist, transport })
    }
}

impl<T: HttpGet> WebFetchHandler<T> {
    #[cfg(test)]
    fn with_parts(allowlist: HostAllowlist, transport: T) -> Self {
        Self { allowlist, transport }
    }
}

impl<T: HttpGet> Handler for WebFetchHandler<T> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if method != "web.fetch" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: FetchParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;

        let url = check_url(&p.url, &self.allowlist).map_err(check_err_to_rpc)?;
        let outcome = drive(&self.transport, &self.allowlist, url).map_err(fetch_err_to_rpc)?;
        let extracted = extract(&outcome.content_type, &outcome.body).map_err(|e| {
            RpcError::new(codes::OPERATION_FAILED, format!("extraction failed: {e}"))
        })?;

        Ok(serde_json::json!({
            "final_url": outcome.final_url,
            "status": outcome.status,
            "content_type": main_type(&outcome.content_type),
            "title": extracted.title,
            "text": extracted.text,
            "truncated": extracted.truncated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::RawResponse;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct FakeGet {
        responses: RefCell<VecDeque<RawResponse>>,
    }
    impl FakeGet {
        fn new(responses: Vec<RawResponse>) -> Self {
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

    fn al(entries: &[&str]) -> HostAllowlist {
        let json = serde_json::to_string(entries).unwrap();
        HostAllowlist::from_env_json(&json).unwrap()
    }

    fn handler(entries: &[&str], responses: Vec<RawResponse>) -> WebFetchHandler<FakeGet> {
        WebFetchHandler::with_parts(al(entries), FakeGet::new(responses))
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_url_is_invalid_params() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h.call("web.fetch", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn non_https_is_policy_denied() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h
            .call("web.fetch", serde_json::json!({"url": "http://example.com/"}))
            .unwrap_err();
        assert_eq!(err.code, codes::POLICY_DENIED);
    }

    #[test]
    fn non_allowlisted_host_is_policy_denied() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h
            .call("web.fetch", serde_json::json!({"url": "https://evil.test/"}))
            .unwrap_err();
        assert_eq!(err.code, codes::POLICY_DENIED);
    }

    #[test]
    fn happy_path_returns_extracted_text() {
        let body = "just some plain text body";
        let resp = RawResponse {
            status: 200,
            location: None,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: body.as_bytes().to_vec(),
        };
        let mut h = handler(&["example.com"], vec![resp]);
        let out = h
            .call("web.fetch", serde_json::json!({"url": "https://example.com/page"}))
            .unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["content_type"], "text/plain");
        assert_eq!(out["text"], body);
        assert_eq!(out["final_url"], "https://example.com/page");
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn redirect_to_denied_host_is_policy_denied_end_to_end() {
        let resp = RawResponse {
            status: 302,
            location: Some("https://evil.test/".to_string()),
            content_type: String::new(),
            body: Vec::new(),
        };
        let mut h = handler(&["example.com"], vec![resp]);
        let err = h
            .call("web.fetch", serde_json::json!({"url": "https://example.com/"}))
            .unwrap_err();
        assert_eq!(err.code, codes::POLICY_DENIED);
    }
}
```

NOTE on `err.code`: this assumes `RpcError` exposes a public `code` field. Verify with `grep -n "pub struct RpcError" -A6 protocol/src/lib.rs`. If `code` is private, assert on the rendered message instead, e.g. `assert!(format!("{err}").contains(&codes::POLICY_DENIED.to_string()))` (the pattern `shell_exec_e2e.rs` uses).

- [ ] **Step 2: Run the handler tests**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-fetch handler`
Expected: PASS — 6 tests in `handler::tests`. (If `err.code` doesn't compile, switch to the message-substring assertions noted above and re-run.)

- [ ] **Step 3: Wire the handler into `main.rs`**

Replace the body of `workers/web-fetch/src/main.rs` so it declares all modules and serves:
```rust
//! web-fetch: fetch a URL (HTTPS-only, against a host allowlist) and return
//! extracted readable text over JSON-RPC stdio. GET-only; no caller-supplied
//! headers/body. Design:
//! docs/superpowers/specs/2026-06-08-web-fetch-worker-design.md

mod allowlist;
mod extract;
mod fetch;
mod handler;

use hhagent_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::WebFetchHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
```

- [ ] **Step 4: Build the whole crate and run all its tests**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-worker-web-fetch`
Expected: PASS — all unit tests (allowlist 6 + extract 7 + fetch 6 + handler 6), no dead-code warnings.

- [ ] **Step 5: Commit**

```bash
git add workers/web-fetch/src/handler.rs workers/web-fetch/src/main.rs
git commit -m "feat(web-fetch): web.fetch JSON-RPC handler + serve_stdio wiring

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Host-side manifest + registration

**Files:**
- Create: `core/src/workers/web_fetch.rs`
- Modify: `core/src/workers/mod.rs`
- Modify: `core/src/registry_build.rs`

- [ ] **Step 1: Write the manifest with tests**

`core/src/workers/web_fetch.rs`:
```rust
//! Host-side manifest + `ToolEntry` constructor for the web-fetch worker.

use std::path::PathBuf;

use hhagent_sandbox::{Net, Profile, SandboxPolicy};

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

/// Tool name the registry keys web-fetch on.
const TOOL_NAME: &str = "web-fetch";
/// Operator override for the worker binary path.
const BIN_ENV: &str = "HHAGENT_WEB_FETCH_BIN";
/// Exe-relative sibling default.
const DEFAULT_BIN_NAME: &str = "hhagent-worker-web-fetch";

/// Build the [`ToolEntry`] for the web-fetch worker.
///
/// The administrator controls the domain allowlist (sourced from the
/// `tool_allowlists` DB table by the daemon, keyed `"web-fetch"`); the
/// LLM-supplied `step.parameters` cannot widen it. The same allowlist is
/// represented twice from one source:
///   - injected verbatim as the `HHAGENT_WEB_FETCH_ALLOWLIST` env JSON for the
///     worker's own per-hop check (which understands the `.domain` wildcard), and
///   - mapped to `host:443` entries for `Net::Allowlist`, so the policy is
///     correct for the future egress proxy. (Wildcard `.domain` entries map to
///     their bare `domain:443`; the egress-proxy slice refines wildcard egress
///     semantics.)
///
/// Defaults: `Net::Allowlist`, `Profile::WorkerNetClient` (permits `socket(2)`),
/// `cpu_ms = 10_000`, `mem_mb = 512` (HTML/PDF parsing is heavier than argv
/// exec), `wall_clock_ms = Some(30_000)`, `SingleUse`. `fs_read` includes the
/// resolver config files so DNS works under the `--unshare-all` jail.
pub fn web_fetch_entry(binary: PathBuf, allowlist: &[String]) -> ToolEntry {
    let allow_json =
        serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let net_entries: Vec<String> = allowlist
        .iter()
        .map(|d| {
            let host = d.strip_prefix('.').unwrap_or(d);
            format!("{host}:443")
        })
        .collect();
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries),
        cpu_ms: 10_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![("HHAGENT_WEB_FETCH_ALLOWLIST".to_string(), allow_json)],
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

/// web-fetch's manifest. Discovery mirrors shell-exec: a set
/// `HHAGENT_WEB_FETCH_BIN` override is authoritative (honoured iff it names a
/// runnable file, else fails closed); only when unset do we fall back to the
/// exe-relative sibling `hhagent-worker-web-fetch`. See [`discover_binary`].
pub struct WebFetchManifest;

impl WorkerManifest for WebFetchManifest {
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
        let allowlist = (ctx.allowlist)(TOOL_NAME);
        Resolution::Register(web_fetch_entry(binary, &allowlist))
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
    fn resolve_registers_with_net_client_policy_and_dual_allowlist() {
        let get_env = |k: &str| (k == BIN_ENV).then(|| "/opt/web-fetch".to_string());
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["en.wikipedia.org".to_string(), ".example.com".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);

        match WebFetchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert_eq!(entry.binary, PathBuf::from("/opt/web-fetch"));
                assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
                assert_eq!(entry.policy.cpu_ms, 10_000);
                assert_eq!(entry.policy.mem_mb, 512);
                assert_eq!(entry.wall_clock_ms, Some(30_000));
                // fs_read carries the binary + resolver files.
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/opt/web-fetch")));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
                // Net::Allowlist derived from the domains (wildcard → bare host).
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert_eq!(
                            hosts,
                            &vec![
                                "en.wikipedia.org:443".to_string(),
                                "example.com:443".to_string()
                            ]
                        );
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                // Env carries the verbatim domain list (wildcard preserved).
                let (k, v) = &entry.policy.env[0];
                assert_eq!(k, "HHAGENT_WEB_FETCH_ALLOWLIST");
                assert_eq!(v, r#"["en.wikipedia.org",".example.com"]"#);
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }

    #[test]
    fn resolve_misconfigured_when_no_binary_found() {
        let get_env = |_k: &str| None;
        let exists = |_p: &Path| false;
        let allowlist = |_t: &str| Vec::new();
        let c = ctx(&get_env, &exists, &allowlist);

        match WebFetchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("hhagent-worker-web-fetch"), "detail: {detail}");
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

- [ ] **Step 2: Declare the module**

In `core/src/workers/mod.rs`, add (keep alphabetical-ish with the others):
```rust
pub mod web_fetch;
```

- [ ] **Step 3: Register the manifest**

In `core/src/registry_build.rs`, add a line inside the `WORKER_MANIFESTS` array (after the gliner_relex entry):
```rust
    &crate::workers::web_fetch::WebFetchManifest,
```

- [ ] **Step 4: Run the manifest tests + confirm the registry still builds**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core web_fetch`
Expected: PASS — 2 tests in `workers::web_fetch::tests`.
Run: `source "$HOME/.cargo/env" && cargo build -p hhagent-core`
Expected: compiles (web-fetch now in `WORKER_MANIFESTS`).

- [ ] **Step 5: Commit**

```bash
git add core/src/workers/web_fetch.rs core/src/workers/mod.rs core/src/registry_build.rs
git commit -m "feat(web-fetch): host-side manifest + register in WORKER_MANIFESTS

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Integration e2e — deny path (hermetic) + happy path (ignored, real network)

**Files:**
- Create: `core/tests/web_fetch_e2e.rs`

This mirrors `core/tests/shell_exec_e2e.rs`'s harness (PG cluster + real sandbox + `tool_host::dispatch`). The hermetic test asserts the **deny path** (non-allowlisted host → `POLICY_DENIED`), which needs no server because the worker's allowlist check fires before any socket. The **happy path + DNS-in-jail** is `#[ignore]`d (needs real internet) and run manually.

- [ ] **Step 1: Write the e2e file**

`core/tests/web_fetch_e2e.rs`:
```rust
//! End-to-end: agent core spawns the `web-fetch` worker under the platform
//! sandbox and round-trips a `web.fetch` call through `tool_host::dispatch`.
//!
//! Hermetic test (`host_outside_allowlist_is_denied`): a non-allowlisted URL
//! is refused by the worker's own allowlist check before any network egress,
//! so it needs no server — it verifies the worker runs under the real
//! net-enabled sandbox and the wire contract holds.
//!
//! Ignored test (`real_fetch_extracts_readable_text`): a real HTTPS GET against
//! an allowlisted public host. Run manually with `--ignored`; it also validates
//! that DNS + TLS work inside the `--unshare-all` (Linux) / Seatbelt (macOS)
//! jail, which the hermetic test cannot.
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the worker binary, or a working
//! sandbox is missing — same posture as `shell_exec_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use hhagent_core::secrets::Vault;
use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_core::workers::web_fetch::web_fetch_entry;
use hhagent_protocol::codes;
use hhagent_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

async fn probe_and_pool(conn_spec: &hhagent_db::conn::ConnectSpec) -> sqlx::PgPool {
    hhagent_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-fetch-e2e"}),
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
    allowlist: Vec<String>,
}

fn ready_or_skip(allowlist: &[&str]) -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("hhagent-worker-web-fetch");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] web-fetch worker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wf-d",
        "wf-l",
        &format!("hhagent-supervisor-test-pg-webfetch-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
    })
}

#[test]
fn host_outside_allowlist_is_denied() {
    // Allowlist a host we will NOT request, so the request is denied before
    // any egress — hermetic, no server required.
    let env = match ready_or_skip(&["en.wikipedia.org"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_fetch_entry(env.worker_path.clone(), &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-fetch under sandbox");

        let err = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({"url": "https://not-allowlisted.example/"}),
        )
        .await
        .expect_err("non-allowlisted host must be denied");

        let msg = format!("{err}");
        assert!(
            msg.contains(&format!("{}", codes::POLICY_DENIED)),
            "expected POLICY_DENIED ({}), got: {msg}",
            codes::POLICY_DENIED
        );

        let _ = sworker.close();
        pool.close().await;
    });
}

#[test]
#[ignore = "hits the real network; validates DNS+TLS inside the sandbox jail"]
fn real_fetch_extracts_readable_text() {
    let env = match ready_or_skip(&["example.com"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_fetch_entry(env.worker_path.clone(), &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-fetch under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({"url": "https://example.com/"}),
        )
        .await
        .expect("web.fetch round trip (network + DNS in jail)");

        assert_eq!(result["status"], 200);
        let text = result["text"].as_str().unwrap_or("");
        assert!(
            text.to_lowercase().contains("example"),
            "expected readable text to mention 'example', got: {text}"
        );

        let _ = sworker.close();
        pool.close().await;
    });
}
```

- [ ] **Step 2: Run the hermetic e2e**

Run (macOS, live Seatbelt; no PG bin dir ⇒ skip-as-pass is acceptable):
`source "$HOME/.cargo/env" && cargo build --workspace && cargo test -p hhagent-core --test web_fetch_e2e -- --nocapture`
Expected: `host_outside_allowlist_is_denied` PASSES (or prints a `[SKIP]` line if PG/sandbox/supervisor unavailable — then run on a host that has them, e.g. the DGX, or set `HHAGENT_PG_BIN_DIR` per the memory note). `real_fetch_extracts_readable_text` shows as `ignored`.

- [ ] **Step 3: Run the ignored real-network test manually to validate DNS-in-jail**

Run: `source "$HOME/.cargo/env" && cargo test -p hhagent-core --test web_fetch_e2e -- --ignored --nocapture`
Expected: `real_fetch_extracts_readable_text` PASSES.
**If it fails with a DNS resolution error** (the flagged risk): the resolver files in `fs_read` weren't enough. First confirm `/etc/resolv.conf` exists on the host. If glibc NSS is the problem, switch the worker to reqwest's pure-Rust resolver: in `workers/web-fetch/Cargo.toml` change the reqwest features to `["blocking", "hickory-dns"]`, rebuild, and re-run. Record whichever path worked in the handover. (This step requires network; skip in offline/CI-restricted environments and run it where outbound HTTPS is allowed.)

- [ ] **Step 4: Commit**

```bash
git add core/tests/web_fetch_e2e.rs
git commit -m "test(web-fetch): sandbox e2e — deny path hermetic + ignored real-network happy path

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Workspace verification + ROADMAP tick

**Files:**
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full workspace build + clippy + test**

Run:
```bash
source "$HOME/.cargo/env"
cargo build --workspace
cargo clippy -p hhagent-worker-web-fetch -p hhagent-core --all-targets --locked -- -D warnings
cargo test --workspace
```
Expected: build clean; clippy exit 0 (no warnings); tests green (macOS skip-as-pass for PG-required suites is acceptable per the project posture — the new worker's unit tests must all pass unconditionally). On the dev Mac, `core`'s Linux-gated paths cannot be cross-tested (the #144 `ring` wall); that's expected and CI-covered.

- [ ] **Step 2: Tick the ROADMAP line**

In `docs/devel/ROADMAP.md`, change the web-fetch line under Phase 3 from:
```markdown
- [ ] `web-fetch` worker: HTTPS-only, host allowlist, body cap, redirect cap
```
to:
```markdown
- [x] `web-fetch` worker: HTTPS-only, host allowlist (self-enforced per redirect hop) + `Net::Allowlist` policy data for the egress proxy, 5 MiB body cap, 5-redirect cap, extracted readable text (HTML readability via `dom_smoothie`/`pdf-extract`/text+JSON), `Profile::WorkerNetClient` + `reqwest::blocking`+rustls — branch `feat/web-fetch-worker`, 2026-06-08. Deferred: egress-proxy enforcement (its consumer is now this worker); `web-search`; hermetic TLS happy-path e2e (waits on the proxy test-CA).
```

- [ ] **Step 3: Commit**

```bash
git add docs/devel/ROADMAP.md
git commit -m "docs(roadmap): web-fetch worker shipped (Phase 3)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-review notes (addressed)

- **Spec coverage:** output schema (Task 5 result object), self-enforced allowlist + `Net::Allowlist` data (Tasks 2/6), exact + `.subdomain` matching (Task 2), HTML/PDF/text+JSON dispatch (Task 3), GET-only HTTPS-only + caps + manual redirect recheck (Tasks 4/5), manifest+registration (Task 6), `tool_allowlists` reuse via `allowlist_tool()` (Task 6), DNS-in-jail risk + `hickory-dns` fallback (Task 7), license vetting done (both MIT) — all mapped.
- **Deviation:** hermetic happy-path e2e replaced by hermetic handler tests + ignored real-network e2e (documented at top), because hermetic TLS needs the egress-proxy test-CA. Flag for user review.
- **Type consistency:** `HostAllowlist::{from_env_json,is_allowed}`, `extract(&str,&[u8])->Extracted{title,text,truncated}`, `main_type`, `HttpGet::get`, `RawResponse{status,location,content_type,body}`, `drive(&T,&HostAllowlist,Url)->FetchOutcome`, `FetchError` variants, `WebFetchHandler<T>`, `web_fetch_entry(PathBuf,&[String])->ToolEntry`, `WebFetchManifest` — names consistent across tasks.
- **Open verification carried into steps (not placeholders):** crate versions resolve (1.4), `RpcError.code` visibility (5.1 note), PDF fixture parses (3.1), readability grabs the fixture (3.4), DNS-in-jail (7.3).
