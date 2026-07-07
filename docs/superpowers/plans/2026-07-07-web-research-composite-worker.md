# web-research composite worker — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a self-contained sandboxed worker `kastellan-worker-web-research` exposing `web.research { query, max_sources?, max_passages? }` that in one call searches (SearxNG) → fetches the top-N allowlisted result pages → extracts readable text → chunks → lexically ranks passages against the query → returns the relevant passages for the LLM planner to synthesize.

**Architecture:** Reuse-first. Consolidate the reusable pure logic currently trapped in the two bin workers (`search`/`parse` from web-search; `fetch`-`drive` + `extract` from web-fetch) into the shared `web-common` crate behind cargo **features**, re-point both workers (behaviour byte-preserved, proven by their existing tests), then compose all three in a new `web-research` crate that adds pure `chunk` + `rank` modules. Ranking sits behind a `PassageRanker` trait so a future embedding/hybrid (RRF) ranker slots in without restructuring.

**Tech Stack:** Rust (edition/toolchain per workspace, rustc 1.96). `kastellan-worker-web-common` (allowlist + `HttpGet` transport seam + `FakeGet`), `kastellan-protocol` (JSON-RPC `Handler`), `kastellan-worker-prelude` (`serve_stdio`). BM25 lexical ranking is hand-rolled (no new crate). Existing deps `dom_smoothie`(`readable_html`)/`pdf-extract` move with the `extract` feature.

**Design spec:** `docs/superpowers/specs/2026-07-07-web-research-composite-worker-design.md`

## Global Constraints

- **AGPL-3.0; AGPL-compatible deps only.** No new dependency crates are introduced by this plan (BM25 is hand-rolled). Any dep considered later must be Apache/MIT/BSD/MPL/LGPL/(A)GPL.
- **Cross-platform Linux + macOS first-class.** No OS-gated code in the worker or web-common changes; the host manifest mirrors web-fetch's cross-platform host path (Firecracker VM entry is explicitly out of scope for v1).
- **Rust core, Python only inside sandboxed workers.** No in-process untrusted code.
- **Every worker is sandboxed before it runs.** The manifest emits a `SandboxPolicy`; no unsandboxed path.
- **No silent fallbacks.** Search failure → error; per-page fetch failures are *recorded* in `unfetched`, never dropped-to-empty-success.
- **Keep files < 500 LOC.** Each new module is small and single-purpose.
- **TDD, frequent commits, all tests green before commit** (project rule 6).
- **cargo is not on the non-interactive PATH:** every cargo command is preceded by `source "$HOME/.cargo/env"`.
- **Commit staging:** `git add <specific files>`, never `git add -A`.
- Commit trailer on every commit: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

## File Structure

**Slice 1 — web-common consolidation (behaviour-preserving):**
- Modify `workers/web-common/Cargo.toml` — add optional deps + `search`/`fetch`/`extract` features.
- Modify `workers/web-common/src/lib.rs` — feature-gated `pub mod` declarations.
- Create `workers/web-common/src/search.rs` (moved from `workers/web-search/src/search.rs`, verbatim).
- Create `workers/web-common/src/parse.rs` (moved from `workers/web-search/src/parse.rs`, verbatim).
- Create `workers/web-common/src/fetch.rs` (moved from `workers/web-fetch/src/fetch.rs`, verbatim).
- Create `workers/web-common/src/extract.rs` (moved from `workers/web-fetch/src/extract.rs`, verbatim).
- Create `workers/web-common/tests/fixtures/hello.pdf` (moved from `workers/web-fetch/tests/fixtures/hello.pdf`).
- Modify `workers/web-search/Cargo.toml` (enable `web-common/search`), `src/main.rs`, `src/handler.rs` (re-point imports); delete `src/search.rs`, `src/parse.rs`.
- Modify `workers/web-fetch/Cargo.toml` (enable `web-common/fetch`+`extract`, drop moved deps), `src/main.rs`, `src/handler.rs`; delete `src/fetch.rs`, `src/extract.rs`, `tests/fixtures/hello.pdf`.

**Slice 2 — web-research worker crate:**
- Create `workers/web-research/Cargo.toml`
- Create `workers/web-research/src/main.rs` — `serve_stdio` bringup.
- Create `workers/web-research/src/chunk.rs` — pure `chunk_passages`.
- Create `workers/web-research/src/rank.rs` — `ScoredPassage` + `PassageRanker` trait + `LexicalRanker`.
- Create `workers/web-research/src/research.rs` — orchestration over `HttpGet`.
- Create `workers/web-research/src/handler.rs` — JSON-RPC `web.research` + `from_env`.
- Modify root `Cargo.toml` workspace `members` to add `workers/web-research`.

**Slice 3 — core wiring:**
- Create `core/src/workers/web_research.rs` — `WebResearchManifest` + `web_research_entry`.
- Modify `core/src/workers/mod.rs` — `pub mod web_research;`
- Modify `core/src/registry_build.rs` — add `WebResearchManifest` to `WORKER_MANIFESTS`.
- Modify `core/src/cassandra/injection_guard.rs` — add `"web-research"` to the `Relaxed` arm.

**Slice 4 (later, not in this plan's execution scope):** `EmbeddingRanker` + `HybridRanker` (RRF) behind an embedding endpoint; `core/tests/web_research_e2e.rs` live `#[ignore]` e2e; optional Firecracker entry; parallel fetch. Tracked as follow-ups.

---

# SLICE 1 — web-common consolidation

> Each task moves code **verbatim** and re-points a consumer. The gate is that the moved code's own tests (which move with it) plus the consumer's untouched tests stay green — this *is* the behaviour-preservation proof. Do the whole slice before touching Slice 2.

### Task 1.1: Move `search` + `parse` into web-common behind a `search` feature

**Files:**
- Modify: `workers/web-common/Cargo.toml`
- Modify: `workers/web-common/src/lib.rs`
- Create: `workers/web-common/src/search.rs` (from `workers/web-search/src/search.rs`)
- Create: `workers/web-common/src/parse.rs` (from `workers/web-search/src/parse.rs`)

**Interfaces:**
- Produces: `kastellan_worker_web_common::search::{search, validate_endpoint, build_query_url, is_loopback, SearchError, DEFAULT_COUNT, MAX_COUNT}` and `kastellan_worker_web_common::parse::{parse_results, Hit}` — behind `feature = "search"`. Same signatures as today (see `workers/web-search/src/search.rs` and `parse.rs`).

- [ ] **Step 1: Move the two files verbatim**

```bash
cd /Users/hherb/src/kastellan
git mv workers/web-search/src/parse.rs  workers/web-common/src/parse.rs
git mv workers/web-search/src/search.rs workers/web-common/src/search.rs
```

`search.rs` refers to `crate::parse::{parse_results, Hit}` — inside web-common that path is still `crate::parse`, so **no edit needed**. Its imports of `kastellan_worker_web_common::…` for `HostAllowlist`/`HttpGet` become self-referential; change them to `crate::`:

In `workers/web-common/src/search.rs`, replace the two `use kastellan_worker_web_common::` lines:
```rust
use crate::allowlist::HostAllowlist;
use crate::http::HttpGet;
```
and in its `#[cfg(test)] mod tests`, replace `use kastellan_worker_web_common::testing::…` and `use kastellan_worker_web_common::http::RawResponse;` with `use crate::testing::…;` / `use crate::http::RawResponse;`.

- [ ] **Step 2: Add the `search` feature (no new deps)**

In `workers/web-common/Cargo.toml`, under `[features]`:
```toml
[features]
testing = []
# Pure SearxNG query logic (validate_endpoint / build_query_url / search / parse).
# No extra deps — uses url + serde + the always-present allowlist/http modules.
search  = []
```

- [ ] **Step 3: Gate the modules in lib.rs**

In `workers/web-common/src/lib.rs`, add after the existing `pub mod http;`:
```rust
#[cfg(feature = "search")]
pub mod parse;
#[cfg(feature = "search")]
pub mod search;
```
The `search` module's tests use `crate::testing`, so `search` must imply `testing` **for test builds only**. web-common's own tests are compiled with `--features testing,search`; the module's `#[cfg(test)]` block references `crate::testing`, which exists whenever `testing` is on. Document this in the feature comment; no `testing`-implies needed because we always pass both in the test command below.

- [ ] **Step 4: Verify web-common compiles + its moved tests pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-common --features "search,testing"
```
Expected: PASS — includes the moved `search::tests` (8 tests) and `parse::tests` (6 tests).

- [ ] **Step 5: Re-point web-search onto web-common::{search,parse}**

In `workers/web-search/Cargo.toml`, enable the feature on the non-dev dependency:
```toml
kastellan-worker-web-common = { path = "../web-common", version = "0.1.0", features = ["search"] }
```
In `workers/web-search/src/main.rs`, delete the `mod parse;` and `mod search;` lines (keep `mod handler;`).
In `workers/web-search/src/handler.rs`, change:
```rust
use crate::search::{search, validate_endpoint, SearchError, DEFAULT_COUNT};
```
to:
```rust
use kastellan_worker_web_common::search::{search, validate_endpoint, SearchError, DEFAULT_COUNT};
```
(Its `use kastellan_worker_web_common::parse::Hit` is not needed unless referenced; the handler only names `search`/`validate_endpoint`/`SearchError`/`DEFAULT_COUNT` — leave the rest.)

- [ ] **Step 6: Verify web-search still passes (behaviour-preserved)**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-search
cargo clippy -p kastellan-worker-web-search --all-targets -- -D warnings
```
Expected: PASS — the 5 `handler::tests` unchanged; no `search.rs`/`parse.rs` left in the crate.

- [ ] **Step 7: Commit**

```bash
git add workers/web-common/Cargo.toml workers/web-common/src/lib.rs \
        workers/web-common/src/search.rs workers/web-common/src/parse.rs \
        workers/web-search/Cargo.toml workers/web-search/src/main.rs workers/web-search/src/handler.rs
git commit -m "refactor(web-common): consolidate SearxNG search+parse behind 'search' feature

Move search.rs/parse.rs from web-search into web-common (verbatim), gate
behind feature 'search'; web-search re-points, behaviour byte-preserved.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 1.2: Move `fetch`-drive + `extract` into web-common behind `fetch`/`extract` features

**Files:**
- Modify: `workers/web-common/Cargo.toml`
- Modify: `workers/web-common/src/lib.rs`
- Create: `workers/web-common/src/fetch.rs` (from `workers/web-fetch/src/fetch.rs`)
- Create: `workers/web-common/src/extract.rs` (from `workers/web-fetch/src/extract.rs`)
- Create: `workers/web-common/tests/fixtures/hello.pdf` (from `workers/web-fetch/tests/fixtures/hello.pdf`)

**Interfaces:**
- Produces: `kastellan_worker_web_common::fetch::{drive, FetchOutcome, FetchError, MAX_REDIRECTS}` (feature `fetch`); `kastellan_worker_web_common::extract::{extract, main_type, Extracted, MAX_TEXT_BYTES}` (feature `extract`, pulls `readable_html`+`pdf-extract`).

- [ ] **Step 1: Move the files + fixture verbatim**

```bash
cd /Users/hherb/src/kastellan
git mv workers/web-fetch/src/fetch.rs   workers/web-common/src/fetch.rs
git mv workers/web-fetch/src/extract.rs workers/web-common/src/extract.rs
mkdir -p workers/web-common/tests/fixtures
git mv workers/web-fetch/tests/fixtures/hello.pdf workers/web-common/tests/fixtures/hello.pdf
```
In `workers/web-common/src/fetch.rs`, re-point the two `use kastellan_worker_web_common::` lines to `crate::allowlist::HostAllowlist;` / `crate::http::HttpGet;`, and in its test module re-point `use kastellan_worker_web_common::…` → `use crate::…`.
`extract.rs` has no `kastellan_worker_web_common::` imports; its test `include_bytes!("../tests/fixtures/hello.pdf")` now resolves correctly from `web-common/src/extract.rs`. No edit to the include path needed (it is `src/../tests/fixtures/hello.pdf`).

- [ ] **Step 2: Add optional deps + features**

In `workers/web-common/Cargo.toml` `[dependencies]`, add:
```toml
pdf-extract   = { workspace = true, optional = true }
readable_html = { workspace = true, optional = true }
```
In `[features]`, add:
```toml
# Redirect-following drive loop (re-checks allowlist+https per hop). No extra deps.
fetch   = []
# Readable-text extraction (HTML readability / PDF / text). Pulls the parsers
# ONLY when enabled, so `search`-only consumers (web-search) stay lean.
extract = ["dep:pdf-extract", "dep:readable_html"]
```

- [ ] **Step 3: Gate the modules in lib.rs**

In `workers/web-common/src/lib.rs`, add:
```rust
#[cfg(feature = "fetch")]
pub mod fetch;
#[cfg(feature = "extract")]
pub mod extract;
```

- [ ] **Step 4: Verify web-common compiles + moved tests pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-common --features "search,fetch,extract,testing"
```
Expected: PASS — moved `fetch::tests` (6) + `extract::tests` (8) green, plus Task 1.1's search/parse tests.

- [ ] **Step 5: Re-point web-fetch onto web-common::{fetch,extract}**

In `workers/web-fetch/Cargo.toml`: enable the features and **drop the now-transitive parsers** from the direct deps:
```toml
kastellan-worker-web-common = { path = "../web-common", version = "0.1.0", features = ["fetch", "extract"] }
```
Remove these two lines from `[dependencies]` (they now live in web-common behind `extract`):
```toml
pdf-extract              = { workspace = true }
readable_html            = { workspace = true }
```
In `workers/web-fetch/src/main.rs`, delete `mod extract;` and `mod fetch;` (keep `mod handler;`).
In `workers/web-fetch/src/handler.rs`, change:
```rust
use crate::extract::{extract, main_type};
use crate::fetch::{drive, FetchError};
```
to:
```rust
use kastellan_worker_web_common::extract::{extract, main_type};
use kastellan_worker_web_common::fetch::{drive, FetchError};
```

- [ ] **Step 6: Verify web-fetch still passes (behaviour-preserved)**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-fetch
cargo clippy -p kastellan-worker-web-fetch --all-targets -- -D warnings
```
Expected: PASS — 7 `handler::tests`; no `fetch.rs`/`extract.rs`/fixture left in the crate.

- [ ] **Step 7: Verify the whole web trio + core still build/clippy clean**

```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-worker-web-common -p kastellan-worker-web-search -p kastellan-worker-web-fetch
cargo clippy -p kastellan-worker-web-common --all-targets --features "search,fetch,extract,testing" -- -D warnings
```
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add workers/web-common/Cargo.toml workers/web-common/src/lib.rs \
        workers/web-common/src/fetch.rs workers/web-common/src/extract.rs \
        workers/web-common/tests/fixtures/hello.pdf \
        workers/web-fetch/Cargo.toml workers/web-fetch/src/main.rs workers/web-fetch/src/handler.rs
git commit -m "refactor(web-common): consolidate fetch-drive+extract behind features

Move fetch.rs/extract.rs (+pdf fixture) from web-fetch into web-common
(verbatim), gate behind 'fetch'/'extract' (extract pulls the HTML/PDF
parsers only when enabled). web-fetch re-points, behaviour byte-preserved.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

# SLICE 2 — web-research worker crate

### Task 2.1: Scaffold the crate + `chunk` module (pure, TDD)

**Files:**
- Create: `workers/web-research/Cargo.toml`
- Create: `workers/web-research/src/main.rs`
- Create: `workers/web-research/src/chunk.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Produces: `chunk::chunk_passages(text: &str) -> Vec<String>`, `chunk::MAX_PASSAGE_BYTES: usize`.

- [ ] **Step 1: Add the crate to the workspace + write Cargo.toml**

In root `Cargo.toml`, add `"workers/web-research",` to the `members` array (next to the other `workers/*`).

Create `workers/web-research/Cargo.toml`:
```toml
[package]
name        = "kastellan-worker-web-research"
description = "Composite tool worker: search (SearxNG) + fetch top-N allowlisted pages + rank relevant passages, in one call. GET-only."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../../README.md"

[[bin]]
name = "kastellan-worker-web-research"
path = "src/main.rs"

[dependencies]
kastellan-protocol          = { path = "../../protocol", version = "0.1.0" }
kastellan-worker-prelude    = { path = "../prelude", version = "0.1.0" }
kastellan-worker-web-common = { path = "../web-common", version = "0.1.0", features = ["search", "fetch", "extract"] }
serde      = { workspace = true }
serde_json = { workspace = true }
anyhow     = { workspace = true }
url        = { workspace = true }

[dev-dependencies]
kastellan-worker-web-common = { path = "../web-common", features = ["search", "fetch", "extract", "testing"] }
```

Create `workers/web-research/src/main.rs`:
```rust
//! web-research: one-call web research — SearxNG search, fetch the top-N
//! allowlisted result pages, extract readable text, and return the passages
//! most relevant to the query over JSON-RPC stdio. GET-only; the LLM supplies
//! only the query string. Design:
//! docs/superpowers/specs/2026-07-07-web-research-composite-worker-design.md

mod chunk;
mod handler;
mod rank;
mod research;

use kastellan_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::WebResearchHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
```

- [ ] **Step 2: Write the failing `chunk` tests**

Create `workers/web-research/src/chunk.rs`:
```rust
//! Split extracted page text into passages for ranking.
//!
//! Pure and deterministic: paragraphs (blank-line separated) are the natural
//! unit; an over-long paragraph is further split on sentence boundaries so no
//! single passage blows the ranking/context budget. Empty/whitespace-only
//! passages are dropped.

/// Upper bound on a single passage's byte length. Over-long paragraphs are
/// split on sentence boundaries into chunks no larger than this.
pub const MAX_PASSAGE_BYTES: usize = 2000;

/// Chunk `text` into passages. Never returns empty/whitespace-only entries.
pub fn chunk_passages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if para.len() <= MAX_PASSAGE_BYTES {
            out.push(para.to_string());
        } else {
            split_long(para, &mut out);
        }
    }
    out
}

/// Split an over-long paragraph into <= MAX_PASSAGE_BYTES chunks, breaking after
/// sentence terminators (`.`/`!`/`?` followed by whitespace) where possible and
/// falling back to a hard char-boundary cut when a single sentence exceeds the cap.
fn split_long(para: &str, out: &mut Vec<String>) {
    let mut cur = String::new();
    for sentence in split_sentences(para) {
        if !cur.is_empty() && cur.len() + 1 + sentence.len() > MAX_PASSAGE_BYTES {
            out.push(std::mem::take(&mut cur));
        }
        if sentence.len() > MAX_PASSAGE_BYTES {
            // A single mega-sentence: hard-cut on char boundaries.
            for piece in hard_cut(sentence) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(piece);
            }
            continue;
        }
        if cur.is_empty() {
            cur.push_str(sentence);
        } else {
            cur.push(' ');
            cur.push_str(sentence);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
}

/// Split on sentence terminators, keeping the terminator with its sentence.
fn split_sentences(para: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let bytes = para.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if (c == b'.' || c == b'!' || c == b'?')
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_whitespace()
        {
            let s = para[start..=i].trim();
            if !s.is_empty() {
                sentences.push(s);
            }
            start = i + 1;
        }
        i += 1;
    }
    let tail = para[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail);
    }
    sentences
}

/// Hard-cut a string into MAX_PASSAGE_BYTES pieces on char boundaries.
fn hard_cut(s: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = s;
    while rest.len() > MAX_PASSAGE_BYTES {
        let mut end = MAX_PASSAGE_BYTES;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        pieces.push(rest.to_string());
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_blank_lines_and_trims() {
        let text = "First paragraph.\n\n  Second paragraph.  \n\n\n Third. ";
        let p = chunk_passages(text);
        assert_eq!(p, vec!["First paragraph.", "Second paragraph.", "Third."]);
    }

    #[test]
    fn drops_empty_and_whitespace_passages() {
        let p = chunk_passages("\n\n   \n\nreal\n\n\t\n");
        assert_eq!(p, vec!["real"]);
    }

    #[test]
    fn empty_input_is_empty_vec() {
        assert!(chunk_passages("").is_empty());
        assert!(chunk_passages("   \n\n  ").is_empty());
    }

    #[test]
    fn long_paragraph_splits_on_sentence_boundaries_under_cap() {
        let sentence = format!("{}. ", "word".repeat(200)); // ~1200 bytes each
        let para = sentence.repeat(3); // ~3600 bytes, one paragraph
        let p = chunk_passages(&para);
        assert!(p.len() >= 2, "expected multiple chunks, got {}", p.len());
        assert!(p.iter().all(|c| c.len() <= MAX_PASSAGE_BYTES), "a chunk exceeded the cap");
    }

    #[test]
    fn mega_sentence_is_hard_cut_on_char_boundary() {
        let para = "x".repeat(MAX_PASSAGE_BYTES + 500); // no sentence terminator
        let p = chunk_passages(&para);
        assert!(p.iter().all(|c| c.len() <= MAX_PASSAGE_BYTES));
        assert!(p.iter().all(|c| c.is_char_boundary(c.len())));
    }
}
```

- [ ] **Step 3: Run chunk tests to verify they pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research chunk
```
Expected: PASS (5 tests). (The crate compiles even though `handler`/`rank`/`research` don't exist yet? No — `main.rs` declares them. To test `chunk` alone before the others exist, temporarily comment the other `mod` lines in `main.rs`, or implement Steps in order. Simplest: create empty stub files `rank.rs`/`research.rs`/`handler.rs` with `//! stub` now and fill them in the next tasks. Create the three stubs so the crate compiles.)

Create stubs so the crate links:
```bash
printf '//! stub — implemented in Task 2.2\n' > workers/web-research/src/rank.rs
printf '//! stub — implemented in Task 2.3\n' > workers/web-research/src/research.rs
```
For `handler.rs`, `main.rs` references `WebResearchHandler::from_env`; stub it minimally:
```rust
//! stub — implemented in Task 2.4
pub struct WebResearchHandler;
impl WebResearchHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        anyhow::bail!("web-research handler not yet implemented")
    }
}
```
`main.rs` also needs `serve_stdio(&mut handler)` to typecheck; `WebResearchHandler` must impl `kastellan_protocol::server::Handler`. Add a trivial impl to the stub:
```rust
impl kastellan_protocol::server::Handler for WebResearchHandler {
    fn call(&mut self, _m: &str, _p: serde_json::Value)
        -> Result<serde_json::Value, kastellan_protocol::RpcError> {
        Err(kastellan_protocol::RpcError::new(
            kastellan_protocol::codes::METHOD_NOT_FOUND, "stub".into()))
    }
}
```
Re-run the chunk test command; Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml workers/web-research/Cargo.toml workers/web-research/src/main.rs \
        workers/web-research/src/chunk.rs workers/web-research/src/rank.rs \
        workers/web-research/src/research.rs workers/web-research/src/handler.rs
git commit -m "feat(web-research): scaffold crate + pure chunk_passages

New composite worker crate; chunk.rs splits page text into passages
(paragraph-first, sentence-split over cap). rank/research/handler stubbed.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 2.2: `rank` module — `PassageRanker` trait + `LexicalRanker` (BM25, TDD)

**Files:**
- Modify: `workers/web-research/src/rank.rs` (replace stub)

**Interfaces:**
- Consumes: nothing external.
- Produces: `rank::ScoredPassage { text: String, score: f64 }`; `rank::PassageRanker` trait with `fn rank(&self, query: &str, passages: &[String]) -> Vec<ScoredPassage>`; `rank::LexicalRanker` (unit struct) implementing it. `rank()` returns passages with a positive relevance score, sorted best-first (score descending); passages sharing no query term are omitted.

- [ ] **Step 1: Write the failing tests**

Replace `workers/web-research/src/rank.rs` with:
```rust
//! Rank passages by relevance to the query.
//!
//! [`PassageRanker`] is the seam: v1 ships [`LexicalRanker`] (pure BM25, no
//! model). A future `EmbeddingRanker` (semantic, via an embedding endpoint) and
//! a `HybridRanker` (RRF-fused, mirroring `core::memory::recall`) implement the
//! same trait and drop in without touching `research.rs`. See the design spec's
//! "Extensibility" section.

/// A passage with its relevance score (higher = more relevant).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredPassage {
    pub text: String,
    pub score: f64,
}

/// Rank passages against a query, best-first. Implementations omit passages
/// with no relevance signal (score <= 0).
pub trait PassageRanker {
    fn rank(&self, query: &str, passages: &[String]) -> Vec<ScoredPassage>;
}

/// BM25 free-parameters (Robertson/Sparck-Jones defaults).
const K1: f64 = 1.5;
const B: f64 = 0.75;

/// Lexical BM25 ranker. Treats the passage set as the corpus (each passage a
/// document) and scores each against the query terms. Pure + deterministic.
pub struct LexicalRanker;

/// Lowercase unicode-word tokens (alphanumeric runs). Punctuation is a separator.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

impl PassageRanker for LexicalRanker {
    fn rank(&self, query: &str, passages: &[String]) -> Vec<ScoredPassage> {
        let q_terms = tokenize(query);
        if q_terms.is_empty() || passages.is_empty() {
            return Vec::new();
        }
        // Tokenize each passage once.
        let docs: Vec<Vec<String>> = passages.iter().map(|p| tokenize(p)).collect();
        let n = docs.len() as f64;
        let avg_len: f64 =
            docs.iter().map(|d| d.len()).sum::<usize>() as f64 / n.max(1.0);

        // Document frequency per unique query term.
        let mut scored: Vec<ScoredPassage> = Vec::new();
        for (doc, passage) in docs.iter().zip(passages.iter()) {
            let dl = doc.len() as f64;
            let mut score = 0.0_f64;
            for term in unique(&q_terms) {
                let tf = doc.iter().filter(|t| *t == &term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = docs.iter().filter(|d| d.contains(&term)).count() as f64;
                // BM25 idf with the +1 floor so it is never negative.
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                let denom = tf + K1 * (1.0 - B + B * dl / avg_len.max(1.0));
                score += idf * (tf * (K1 + 1.0)) / denom;
            }
            if score > 0.0 {
                scored.push(ScoredPassage { text: passage.clone(), score });
            }
        }
        // Best-first; stable tie-break by original order via sort_by.
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }
}

/// Unique terms preserving first-seen order.
fn unique(terms: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    for t in terms {
        if !seen.contains(t) {
            seen.push(t.clone());
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(scored: &[ScoredPassage]) -> Vec<&str> {
        scored.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn ranks_on_topic_passage_above_off_topic() {
        let passages = vec![
            "The cat sat on the mat and slept all afternoon.".to_string(),
            "Rust uses bwrap to create unprivileged user namespaces for sandboxing.".to_string(),
        ];
        let r = LexicalRanker.rank("bwrap user namespaces sandbox", &passages);
        assert_eq!(r.len(), 1, "off-topic passage should score 0 and be omitted");
        assert!(r[0].text.contains("bwrap"));
        assert!(r[0].score > 0.0);
    }

    #[test]
    fn orders_multiple_matches_by_relevance() {
        let passages = vec![
            "namespaces are mentioned once here.".to_string(),
            "user namespaces user namespaces user namespaces everywhere.".to_string(),
        ];
        let r = LexicalRanker.rank("user namespaces", &passages);
        assert_eq!(r.len(), 2);
        assert!(r[0].text.starts_with("user namespaces user"), "denser match should rank first");
    }

    #[test]
    fn empty_query_or_passages_yields_empty() {
        assert!(LexicalRanker.rank("", &["anything".to_string()]).is_empty());
        assert!(LexicalRanker.rank("q", &[]).is_empty());
    }

    #[test]
    fn no_shared_terms_yields_empty() {
        let passages = vec!["completely unrelated content".to_string()];
        assert!(LexicalRanker.rank("xyzzy plugh", &passages).is_empty());
    }

    #[test]
    fn tokenization_is_case_and_punctuation_insensitive() {
        let passages = vec!["BWRAP, the sandbox!".to_string()];
        let r = LexicalRanker.rank("bwrap", &passages);
        assert_eq!(texts(&r), vec!["BWRAP, the sandbox!"]);
    }
}
```

- [ ] **Step 2: Run to verify pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research rank
```
Expected: PASS (5 tests).

- [ ] **Step 3: Commit**

```bash
git add workers/web-research/src/rank.rs
git commit -m "feat(web-research): PassageRanker trait + BM25 LexicalRanker

Pure lexical ranking behind a trait seam so a future embedding/hybrid
(RRF) ranker slots in without restructuring research.rs.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 2.3: `research` orchestration over the `HttpGet` seam (TDD)

**Files:**
- Modify: `workers/web-research/src/research.rs` (replace stub)

**Interfaces:**
- Consumes: `web_common::search::{search, SearchError, DEFAULT_COUNT}`, `web_common::parse::Hit`, `web_common::fetch::{drive, FetchError}`, `web_common::extract::extract`, `web_common::allowlist::HostAllowlist`, `web_common::http::HttpGet`, `crate::chunk::chunk_passages`, `crate::rank::{PassageRanker, ScoredPassage}`.
- Produces:
  - `research::SourcePassages { url, title, snippet: String, passages: Vec<ScoredPassage> }`
  - `research::UnfetchedSource { url, title, snippet, reason: String }`
  - `research::ResearchOutcome { sources: Vec<SourcePassages>, unfetched: Vec<UnfetchedSource> }`
  - `research::ResearchError { EmptyQuery, Search(SearchError) }`
  - `research::{DEFAULT_MAX_SOURCES=3, MAX_MAX_SOURCES=8, DEFAULT_MAX_PASSAGES=3, MAX_MAX_PASSAGES=10, SEARCH_COUNT=10}`
  - `research::research<T: HttpGet, R: PassageRanker>(transport, endpoint: &Url, allowlist: &HostAllowlist, ranker: &R, query: &str, max_sources: usize, max_passages: usize) -> Result<ResearchOutcome, ResearchError>`

- [ ] **Step 1: Write the failing tests + implementation**

Replace `workers/web-research/src/research.rs` with:
```rust
//! Compose search + fetch + rank into one research pass, pure over the
//! [`HttpGet`] seam so the whole flow is hermetic-testable with `FakeGet`.
//!
//! Flow: reject empty query → `search()` the SearxNG endpoint → for each hit in
//! rank order, if its host is on the content allowlist attempt a fetch; on
//! success extract → chunk → rank passages; on failure record the source in
//! `unfetched` (never drop silently). Off-allowlist hits are recorded too. Stops
//! once `max_sources` pages have been successfully gathered.

use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::extract::extract;
use kastellan_worker_web_common::fetch::{drive, FetchError};
use kastellan_worker_web_common::http::HttpGet;
use kastellan_worker_web_common::parse::Hit;
use kastellan_worker_web_common::search::{search, SearchError};

use crate::chunk::chunk_passages;
use crate::rank::{PassageRanker, ScoredPassage};

/// Default / max number of pages fetched per research call.
pub const DEFAULT_MAX_SOURCES: usize = 3;
pub const MAX_MAX_SOURCES: usize = 8;
/// Default / max passages kept per source.
pub const DEFAULT_MAX_PASSAGES: usize = 3;
pub const MAX_MAX_PASSAGES: usize = 10;
/// How many search hits to consider (before allowlist filtering).
pub const SEARCH_COUNT: usize = 10;

/// A fetched source with its top-ranked passages.
#[derive(Debug)]
pub struct SourcePassages {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub passages: Vec<ScoredPassage>,
}

/// A hit that was not turned into passages, with the reason (never dropped).
#[derive(Debug)]
pub struct UnfetchedSource {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub reason: String,
}

/// The full research result.
#[derive(Debug)]
pub struct ResearchOutcome {
    pub sources: Vec<SourcePassages>,
    pub unfetched: Vec<UnfetchedSource>,
}

/// Failure of the research pass. Only a *search* failure (or empty query) is an
/// error; per-page failures are recorded in `unfetched`.
#[derive(Debug)]
pub enum ResearchError {
    EmptyQuery,
    Search(SearchError),
}

fn short_fetch_reason(e: &FetchError) -> String {
    match e {
        FetchError::HostDenied(h) => format!("fetch-failed: redirect host {h} off-allowlist"),
        FetchError::NonHttps(s) => format!("fetch-failed: redirect scheme {s} not https"),
        FetchError::TooManyRedirects => "fetch-failed: too many redirects".to_string(),
        FetchError::MissingLocation => "fetch-failed: redirect without Location".to_string(),
        FetchError::BadUrl(m) => format!("fetch-failed: bad url: {m}"),
        FetchError::Transport(m) => format!("fetch-failed: {m}"),
    }
}

/// Try to turn one allowlisted hit into a `SourcePassages`. `Err(reason)` on any
/// fetch/parse/extract failure — the caller records it in `unfetched`.
fn gather_source<T: HttpGet, R: PassageRanker>(
    transport: &T,
    allowlist: &HostAllowlist,
    ranker: &R,
    query: &str,
    hit: &Hit,
    max_passages: usize,
) -> Result<SourcePassages, String> {
    let url = Url::parse(&hit.url).map_err(|e| format!("fetch-failed: bad url: {e}"))?;
    let outcome = drive(transport, allowlist, url).map_err(|e| short_fetch_reason(&e))?;
    let extracted = extract(&outcome.content_type, &outcome.body)
        .map_err(|e| format!("fetch-failed: extraction: {e}"))?;
    let passages = chunk_passages(&extracted.text);
    let mut ranked = ranker.rank(query, &passages);
    ranked.truncate(max_passages);
    Ok(SourcePassages {
        url: outcome.final_url,
        title: hit.title.clone(),
        snippet: hit.snippet.clone(),
        passages: ranked,
    })
}

/// Run the research pass. See the module doc for the flow.
pub fn research<T: HttpGet, R: PassageRanker>(
    transport: &T,
    endpoint: &Url,
    allowlist: &HostAllowlist,
    ranker: &R,
    query: &str,
    max_sources: usize,
    max_passages: usize,
) -> Result<ResearchOutcome, ResearchError> {
    if query.trim().is_empty() {
        return Err(ResearchError::EmptyQuery);
    }
    let max_sources = max_sources.clamp(1, MAX_MAX_SOURCES);
    let max_passages = max_passages.clamp(1, MAX_MAX_PASSAGES);

    let hits = search(transport, endpoint, allowlist, query, SEARCH_COUNT)
        .map_err(ResearchError::Search)?;

    let mut sources = Vec::new();
    let mut unfetched = Vec::new();
    for hit in &hits {
        if sources.len() >= max_sources {
            break;
        }
        let host = Url::parse(&hit.url).ok().and_then(|u| u.host_str().map(str::to_string));
        let allowed = host.as_deref().map(|h| allowlist.is_allowed(h)).unwrap_or(false);
        if !allowed {
            unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason: "off-allowlist".to_string(),
            });
            continue;
        }
        match gather_source(transport, allowlist, ranker, query, hit, max_passages) {
            Ok(src) => sources.push(src),
            Err(reason) => unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason,
            }),
        }
    }
    Ok(ResearchOutcome { sources, unfetched })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, ok_resp, FakeGet};
    use crate::rank::LexicalRanker;

    fn endpoint() -> Url {
        Url::parse("https://searx.example.org/search").unwrap()
    }

    // Search JSON returning the given (title, url) pairs with a fixed snippet.
    fn search_json(hits: &[(&str, &str)]) -> String {
        let items: Vec<String> = hits
            .iter()
            .map(|(t, u)| format!(r#"{{"title":"{t}","url":"{u}","content":"snippet about bwrap namespaces","engine":"e"}}"#))
            .collect();
        format!(r#"{{"results":[{}]}}"#, items.join(","))
    }

    #[test]
    fn happy_path_search_then_fetch_ranks_passages() {
        // 1 search response, then 1 fetch response (text/plain).
        let page = "Intro paragraph unrelated.\n\nbwrap creates unprivileged user namespaces to sandbox the worker.";
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Doc", "https://docs.example.org/bwrap")])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain; charset=utf-8".into(), body: page.as_bytes().to_vec() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap user namespaces", 3, 3).unwrap();
        assert_eq!(out.sources.len(), 1);
        assert_eq!(out.sources[0].url, "https://docs.example.org/bwrap");
        assert!(!out.sources[0].passages.is_empty());
        assert!(out.sources[0].passages[0].text.contains("bwrap"));
        assert!(out.unfetched.is_empty());
    }

    #[test]
    fn off_allowlist_hit_is_recorded_not_fetched() {
        // Only the search response is served; the off-allowlist hit must NOT
        // consume a fetch response (FakeGet would run dry otherwise).
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Evil", "https://evil.test/x")])),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "q term", 3, 3).unwrap();
        assert!(out.sources.is_empty());
        assert_eq!(out.unfetched.len(), 1);
        assert_eq!(out.unfetched[0].reason, "off-allowlist");
        assert_eq!(out.unfetched[0].url, "https://evil.test/x");
    }

    #[test]
    fn one_fetch_failure_is_recorded_others_returned() {
        // hit A fetch → 500 (non-3xx terminal, extract of empty body still ok →
        // ranks to nothing) ; use a hit that 200s with content and one that errors.
        // Serve: search, then A=200 with content, then B=transport is simulated
        // by a redirect-loop -> TooManyRedirects.
        let page = "user namespaces sandbox bwrap details here.";
        let mut resps = vec![
            json_resp(&search_json(&[
                ("A", "https://docs.example.org/a"),
                ("B", "https://docs.example.org/b"),
            ])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain".into(), body: page.as_bytes().to_vec() },
        ];
        // B: 6+ redirects to the same allowlisted host → TooManyRedirects.
        for _ in 0..(kastellan_worker_web_common::fetch::MAX_REDIRECTS + 2) {
            resps.push(RawResponse { status: 302,
                location: Some("https://docs.example.org/loop".into()),
                content_type: String::new(), body: Vec::new() });
        }
        let t = FakeGet::new(resps);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap namespaces", 3, 3).unwrap();
        assert_eq!(out.sources.len(), 1, "A should succeed");
        assert_eq!(out.sources[0].url, "https://docs.example.org/a");
        assert_eq!(out.unfetched.len(), 1, "B should be recorded as failed");
        assert!(out.unfetched[0].reason.starts_with("fetch-failed:"), "{}", out.unfetched[0].reason);
    }

    #[test]
    fn max_sources_caps_fetches() {
        let hits: Vec<(&str, &str)> = vec![
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ];
        let t = FakeGet::new(vec![
            json_resp(&search_json(&hits)),
            ok_resp("bwrap namespaces one"),
            ok_resp("bwrap namespaces two"),
            // no third fetch response — the max_sources cap must stop before a
            // 3rd fetch (else FakeGet returns Err "no more canned responses").
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap namespaces", 2, 3).unwrap();
        assert_eq!(out.sources.len(), 2);
    }

    #[test]
    fn empty_query_is_error() {
        let t = FakeGet::new(vec![]);
        let a = al(&["searx.example.org"]);
        let err = research(&t, &endpoint(), &a, &LexicalRanker, "   ", 3, 3).unwrap_err();
        assert!(matches!(err, ResearchError::EmptyQuery));
    }

    #[test]
    fn search_failure_is_error() {
        let t = FakeGet::new(vec![RawResponse { status: 503, location: None,
            content_type: "text/plain".into(), body: Vec::new() }]);
        let a = al(&["searx.example.org"]);
        let err = research(&t, &endpoint(), &a, &LexicalRanker, "q term", 3, 3).unwrap_err();
        assert!(matches!(err, ResearchError::Search(_)));
    }

    #[test]
    fn max_passages_truncates_per_source() {
        let page = (0..6).map(|i| format!("bwrap namespaces passage number {i}."))
            .collect::<Vec<_>>().join("\n\n");
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("A", "https://docs.example.org/a")])),
            RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                body: page.into_bytes() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap namespaces", 3, 2).unwrap();
        assert_eq!(out.sources[0].passages.len(), 2, "capped at max_passages");
    }
}
```

- [ ] **Step 2: Run to verify pass**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research research
```
Expected: PASS (7 tests).

- [ ] **Step 3: Commit**

```bash
git add workers/web-research/src/research.rs
git commit -m "feat(web-research): search+fetch+rank orchestration over HttpGet seam

Pure, hermetic-testable research() with off-allowlist + partial-failure
recording (no silent drops), caps on sources/passages.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 2.4: `handler` — JSON-RPC `web.research` + `from_env` (TDD)

**Files:**
- Modify: `workers/web-research/src/handler.rs` (replace stub)

**Interfaces:**
- Consumes: `research::{research, ResearchError, ResearchOutcome, DEFAULT_MAX_SOURCES, DEFAULT_MAX_PASSAGES}`, `rank::LexicalRanker`, `web_common::search::{validate_endpoint, SearchError}`, `web_common::allowlist::HostAllowlist`, `web_common::http::{make_get, HttpGet}`, `kastellan_protocol::{codes, server::Handler, RpcError}`.
- Produces: `WebResearchHandler<T: HttpGet>` with `from_env() -> anyhow::Result<Self>` (on `Box<dyn HttpGet>`) and the `Handler` impl serving `web.research`.

- [ ] **Step 1: Write the failing tests + implementation**

Replace `workers/web-research/src/handler.rs` with:
```rust
//! JSON-RPC handler for `web.research`.
//!
//! Flow: parse params → run `research` (search + fetch top-N allowlisted pages +
//! rank passages) → build the result object. The endpoint + allowlist are
//! operator-controlled and validated at construction (`from_env`); the LLM
//! supplies only the query + optional caps. Errors map onto the protocol code
//! vocabulary. No silent fallbacks.

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::http::{make_get, HttpGet};
use kastellan_worker_web_common::search::{validate_endpoint, SearchError};

use crate::rank::LexicalRanker;
use crate::research::{
    research, ResearchError, ResearchOutcome, DEFAULT_MAX_PASSAGES, DEFAULT_MAX_SOURCES,
};

#[derive(Deserialize)]
struct ResearchParams {
    query: String,
    #[serde(default)]
    max_sources: Option<usize>,
    #[serde(default)]
    max_passages: Option<usize>,
}

/// Map a [`SearchError`] to a JSON-RPC error (shared shape with web-search).
fn search_err_to_rpc(e: SearchError) -> RpcError {
    match e {
        SearchError::EmptyQuery => RpcError::new(codes::INVALID_PARAMS, "query is empty".into()),
        SearchError::BadEndpoint(m) => {
            RpcError::new(codes::POLICY_DENIED, format!("configured endpoint invalid: {m}"))
        }
        SearchError::SchemeDenied(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("endpoint scheme {s:?} not allowed (https, or http for loopback only)"),
        ),
        SearchError::HostDenied(h) => {
            RpcError::new(codes::POLICY_DENIED, format!("endpoint host {h:?} not on allowlist"))
        }
        SearchError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("search request failed: {m}"))
        }
        SearchError::Redirected => RpcError::new(
            codes::OPERATION_FAILED,
            "search endpoint returned an unexpected redirect".into(),
        ),
        SearchError::BadStatus(s) => {
            RpcError::new(codes::OPERATION_FAILED, format!("search endpoint returned status {s}"))
        }
        SearchError::Parse(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("parsing results failed: {m}"))
        }
    }
}

fn research_err_to_rpc(e: ResearchError) -> RpcError {
    match e {
        ResearchError::EmptyQuery => RpcError::new(codes::INVALID_PARAMS, "query is empty".into()),
        ResearchError::Search(s) => search_err_to_rpc(s),
    }
}

/// Serialize a [`ResearchOutcome`] into the wire JSON (see the design spec).
fn outcome_to_json(query: &str, out: ResearchOutcome) -> serde_json::Value {
    let sources: Vec<serde_json::Value> = out
        .sources
        .iter()
        .map(|s| {
            json!({
                "url": s.url,
                "title": s.title,
                "snippet": s.snippet,
                "fetched": true,
                "passages": s.passages.iter()
                    .map(|p| json!({ "text": p.text, "score": p.score }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let unfetched: Vec<serde_json::Value> = out
        .unfetched
        .iter()
        .map(|u| json!({ "url": u.url, "title": u.title, "snippet": u.snippet, "reason": u.reason }))
        .collect();
    let passage_count: usize = out.sources.iter().map(|s| s.passages.len()).sum();
    json!({
        "query": query,
        "sources": sources,
        "unfetched": unfetched,
        "sources_fetched": out.sources.len(),
        "passage_count": passage_count,
    })
}

/// The worker handler, generic over the transport so tests inject a fake.
pub struct WebResearchHandler<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
    ranker: LexicalRanker,
}

impl WebResearchHandler<Box<dyn HttpGet>> {
    /// Build from env: endpoint + allowlist JSON + env-selected transport.
    /// Validates the endpoint up front and fails closed (the worker never
    /// serves) if it is missing, unparseable, wrong-scheme, or off-allowlist.
    pub fn from_env() -> anyhow::Result<Self> {
        let endpoint_raw = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_WEB_RESEARCH_ENDPOINT not set"))?;
        let allow_raw =
            std::env::var("KASTELLAN_WEB_RESEARCH_ALLOWLIST").unwrap_or_else(|_| "[]".into());
        let allowlist = HostAllowlist::from_env_json(&allow_raw)?;
        let endpoint = validate_endpoint(&endpoint_raw, &allowlist)
            .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
        let transport = make_get("kastellan-web-research/0")?;
        Ok(Self { endpoint, allowlist, transport, ranker: LexicalRanker })
    }
}

impl<T: HttpGet> WebResearchHandler<T> {
    #[cfg(test)]
    fn with_parts(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport, ranker: LexicalRanker }
    }
}

impl<T: HttpGet> Handler for WebResearchHandler<T> {
    fn call(&mut self, method: &str, params: serde_json::Value)
        -> Result<serde_json::Value, RpcError>
    {
        if method != "web.research" {
            return Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {method}")));
        }
        let p: ResearchParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        let max_sources = p.max_sources.unwrap_or(DEFAULT_MAX_SOURCES);
        let max_passages = p.max_passages.unwrap_or(DEFAULT_MAX_PASSAGES);

        let out = research(
            &self.transport, &self.endpoint, &self.allowlist, &self.ranker,
            &p.query, max_sources, max_passages,
        ).map_err(research_err_to_rpc)?;

        Ok(outcome_to_json(&p.query, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, FakeGet};

    fn handler(responses: Vec<RawResponse>) -> WebResearchHandler<FakeGet> {
        WebResearchHandler::with_parts(
            Url::parse("https://searx.example.org/search").unwrap(),
            al(&["searx.example.org", "docs.example.org"]),
            FakeGet::new(responses),
        )
    }

    fn search_json(title: &str, url: &str) -> String {
        format!(r#"{{"results":[{{"title":"{title}","url":"{url}","content":"c","engine":"e"}}]}}"#)
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = handler(vec![]);
        let err = h.call("nope", json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("web.research", json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn empty_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("web.research", json!({"query": "  "})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn happy_path_returns_sources_and_passages() {
        let page = "bwrap creates user namespaces for sandboxing workers.";
        let mut h = handler(vec![
            json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
            RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                body: page.as_bytes().to_vec() },
        ]);
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["query"], "bwrap user namespaces");
        assert_eq!(out["sources_fetched"], 1);
        assert_eq!(out["sources"][0]["url"], "https://docs.example.org/bwrap");
        assert_eq!(out["sources"][0]["fetched"], true);
        assert!(out["passage_count"].as_u64().unwrap() >= 1);
        assert!(out["sources"][0]["passages"][0]["text"].as_str().unwrap().contains("bwrap"));
    }

    #[test]
    fn search_failure_maps_to_operation_failed() {
        let mut h = handler(vec![RawResponse { status: 500, location: None,
            content_type: "text/plain".into(), body: Vec::new() }]);
        let err = h.call("web.research", json!({"query": "q term"})).unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }

    #[test]
    fn off_allowlist_hit_shows_in_unfetched() {
        let mut h = handler(vec![
            json_resp(&search_json("Evil", "https://evil.test/x")),
        ]);
        let out = h.call("web.research", json!({"query": "q term"})).unwrap();
        assert_eq!(out["sources_fetched"], 0);
        assert_eq!(out["unfetched"][0]["reason"], "off-allowlist");
    }
}
```

- [ ] **Step 2: Run the full crate test suite + clippy**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research
cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings
```
Expected: PASS — chunk (5) + rank (5) + research (7) + handler (6) = 23 tests; clippy clean.

- [ ] **Step 3: Commit**

```bash
git add workers/web-research/src/handler.rs
git commit -m "feat(web-research): web.research JSON-RPC handler + fail-closed from_env

Wire params → research() → result JSON (sources+passages+unfetched);
fail-closed endpoint validation; error-code mapping. 23 tests green.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

# SLICE 3 — core wiring

### Task 3.1: Host-side manifest `web_research_entry` + `WebResearchManifest` (TDD)

**Files:**
- Create: `core/src/workers/web_research.rs`
- Modify: `core/src/workers/mod.rs`

**Interfaces:**
- Consumes: `crate::scheduler::ToolEntry`, `crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest}`, `kastellan_sandbox::{Net, Profile, SandboxPolicy}`, `url::Url`.
- Produces: `crate::workers::web_research::{web_research_entry, WebResearchManifest}`. Tool name `"web-research"`; env `KASTELLAN_WEB_RESEARCH_BIN` (override), `KASTELLAN_WEB_RESEARCH_ENDPOINT`, default bin `kastellan-worker-web-research`.

- [ ] **Step 1: Write the manifest module**

Create `core/src/workers/web_research.rs` (mirrors `web_search.rs`; the `Net::Allowlist` is the **union** of the endpoint host:port and the content-domain host:443 entries):
```rust
//! Host-side manifest + `ToolEntry` constructor for the web-research worker.
//!
//! Composite of search + fetch: the LLM supplies only the query; the operator
//! controls the SearxNG endpoint (`KASTELLAN_WEB_RESEARCH_ENDPOINT`) and the
//! content-host allowlist (`tool_allowlists` keyed `"web-research"`). The one
//! allowlist gates both the endpoint host and every fetched result URL. The
//! `Net::Allowlist` is the union of the endpoint host:port and the content
//! host:443 entries; the egress proxy owns IP-level containment. See
//! `docs/threat-model.md` ("Network egress").

use std::path::PathBuf;

use kastellan_sandbox::{Net, Profile, SandboxPolicy};
use url::Url;

use crate::scheduler::ToolEntry;
use crate::worker_manifest::{discover_binary, ResolveCtx, Resolution, WorkerManifest};

const TOOL_NAME: &str = "web-research";
const BIN_ENV: &str = "KASTELLAN_WEB_RESEARCH_BIN";
const DEFAULT_BIN_NAME: &str = "kastellan-worker-web-research";
const ENDPOINT_ENV: &str = "KASTELLAN_WEB_RESEARCH_ENDPOINT";

/// `host:port` for the SearxNG endpoint (port defaults: 443 https / from URL).
fn endpoint_net_entry(endpoint: &str) -> Vec<String> {
    match Url::parse(endpoint) {
        Ok(u) => match u.host_str() {
            Some(host) => vec![format!("{host}:{}", u.port_or_known_default().unwrap_or(443))],
            None => vec![],
        },
        Err(_) => vec![],
    }
}

/// Map the content-domain allowlist to `host:443` entries (wildcard `.d` → `d:443`).
fn content_net_entries(allowlist: &[String]) -> Vec<String> {
    allowlist
        .iter()
        .map(|d| format!("{}:443", d.strip_prefix('.').unwrap_or(d)))
        .collect()
}

/// Union of the endpoint host:port and the content host:443 entries, de-duped
/// (order-preserving: endpoint first).
fn net_entries(endpoint: &str, allowlist: &[String]) -> Vec<String> {
    let mut entries = endpoint_net_entry(endpoint);
    for e in content_net_entries(allowlist) {
        if !entries.contains(&e) {
            entries.push(e);
        }
    }
    entries
}

/// Build the [`ToolEntry`] for the web-research worker. Defaults mirror web-fetch
/// (HTML/PDF parsing over several pages): `Profile::WorkerNetClient`,
/// `cpu_ms = 15_000`, `mem_mb = 512`, `wall_clock_ms = Some(60_000)` (search + N
/// sequential fetches), `SingleUse`. Resolver files in `fs_read` for DNS under
/// `--unshare-all`.
pub fn web_research_entry(binary: PathBuf, endpoint: &str, allowlist: &[String]) -> ToolEntry {
    let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
    let policy = SandboxPolicy {
        fs_read: vec![
            binary.clone(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![],
        net: Net::Allowlist(net_entries(endpoint, allowlist)),
        cpu_ms: 15_000,
        mem_mb: 512,
        profile: Profile::WorkerNetClient,
        env: vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            ("KASTELLAN_WEB_RESEARCH_ALLOWLIST".to_string(), allow_json),
        ],
        cpu_quota_pct: None,
        tasks_max: None,
        proxy_uds: None,
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
    }
}

/// web-research's manifest. Discovery mirrors web-search.
pub struct WebResearchManifest;

impl WorkerManifest for WebResearchManifest {
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
        Resolution::Register(web_research_entry(binary, &endpoint, &allowlist))
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
            canonicalize: &|_p| None,
            allowlist,
        }
    }

    #[test]
    fn resolve_registers_union_net_and_injects_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            _ => None,
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec!["searx.example.org".to_string(), ".docs.example.org".to_string()];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                assert!(matches!(entry.policy.profile, Profile::WorkerNetClient));
                assert_eq!(entry.policy.cpu_ms, 15_000);
                assert_eq!(entry.policy.mem_mb, 512);
                assert_eq!(entry.wall_clock_ms, Some(60_000));
                assert!(entry.policy.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        // endpoint host:443 first, then content docs.example.org:443.
                        assert_eq!(hosts, &vec![
                            "searx.example.org:443".to_string(),
                            "docs.example.org:443".to_string(),
                        ]);
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                assert_eq!(entry.policy.env[0].0, ENDPOINT_ENV);
                assert_eq!(entry.policy.env[0].1, "https://searx.example.org/search");
                assert_eq!(entry.policy.env[1].0, "KASTELLAN_WEB_RESEARCH_ALLOWLIST");
                assert_eq!(entry.policy.env[1].1, r#"["searx.example.org",".docs.example.org"]"#);
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
        match WebResearchManifest.resolve(&c) {
            Resolution::Misconfigured { detail } => {
                assert!(detail.contains("kastellan-worker-web-research"), "detail: {detail}");
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

In `core/src/workers/mod.rs`, add `pub mod web_research;` next to `pub mod web_search;` (keep alphabetical/grouped with the other web modules).

- [ ] **Step 3: Run the manifest tests**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib workers::web_research
```
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add core/src/workers/web_research.rs core/src/workers/mod.rs
git commit -m "feat(core): web-research host manifest + web_research_entry

Union egress allowlist (endpoint host:port ∪ content host:443),
WorkerNetClient/SingleUse, injects endpoint+allowlist env.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 3.2: Register in `WORKER_MANIFESTS` + `Relaxed` guard profile

**Files:**
- Modify: `core/src/registry_build.rs:20-26`
- Modify: `core/src/cassandra/injection_guard.rs:138`

**Interfaces:**
- Consumes: `crate::workers::web_research::WebResearchManifest`.
- Produces: `web-research` present in the built registry; `GuardProfile::for_tool("web-research") == Relaxed`.

- [ ] **Step 1: Add the manifest to the static list**

In `core/src/registry_build.rs`, add to `WORKER_MANIFESTS` (after the `WebSearchManifest` line):
```rust
    &crate::workers::web_research::WebResearchManifest,
```

- [ ] **Step 2: Add web-research to the Relaxed guard arm + a pin test**

In `core/src/cassandra/injection_guard.rs`, change the `for_tool` match arm:
```rust
            "web-fetch" | "web-search" | "web-research" | "browser-driver" => GuardProfile::Relaxed,
```

Add a test in that file's `#[cfg(test)] mod tests` (find the block that tests `for_tool`; if none names web-research, add):
```rust
    #[test]
    fn web_research_uses_relaxed_profile() {
        assert!(matches!(GuardProfile::for_tool("web-research"), GuardProfile::Relaxed));
    }
```

- [ ] **Step 3: Verify registry + guard + full core lib**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib registry_build
cargo test -p kastellan-core --lib injection_guard
cargo build -p kastellan-core
cargo clippy -p kastellan-core --lib --all-targets -- -D warnings
```
Expected: PASS — the registry assembles with 7 manifests; guard test green; clippy clean.

- [ ] **Step 4: Commit**

```bash
git add core/src/registry_build.rs core/src/cassandra/injection_guard.rs
git commit -m "feat(core): register web-research manifest + Relaxed guard profile

web-research joins WORKER_MANIFESTS and the injection-guard Relaxed arm
(it returns fetched document content, like web-fetch/web-search).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

### Task 3.3: Full-workspace verification + planner-surface note

**Files:**
- (verification only; optional docs touch)

- [ ] **Step 1: Build the whole workspace incl. the new worker binary**

```bash
source "$HOME/.cargo/env"
cargo build --workspace
```
Expected: PASS — `target/debug/kastellan-worker-web-research` exists (the manifest's default-bin discovery finds it as an exe-relative sibling).

- [ ] **Step 2: Run the affected crates' tests + workspace clippy**

```bash
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-common --features "search,fetch,extract,testing"
cargo test -p kastellan-worker-web-search
cargo test -p kastellan-worker-web-fetch
cargo test -p kastellan-worker-web-research
cargo test -p kastellan-core --lib
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: PASS across the board (web-search/web-fetch prove the consolidation is behaviour-preserving; core lib unaffected beyond the +2 manifest tests + 1 guard test).

> Note on macOS full-workspace `cargo test`: the standing PG-bring-up flake in `embedding_recall_e2e` applies (skip-as-pass for the whole workspace on the Mac; run live-PG suites individually or on the DGX). This plan adds no PG surface.

- [ ] **Step 3 (optional): teach the planner the capability**

If the agent system prompt enumerates tools with usage hints (grep `core/src/prompt_assembly/` for where `web-fetch`/`web-search` are described), add a one-line `web.research` description: *"web.research{query} — search the web and return the most relevant passages from the top pages in one call; prefer it over chaining web.search + web.fetch when you need to answer a question from the web."* Only do this if such an enumeration exists; do not invent a new prompt section. Commit separately if changed:
```bash
git add core/src/prompt_assembly/<file>
git commit -m "docs(prompt): surface web.research to the planner

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Deferred to follow-ups (Slice 4 — not executed by this plan)

- `EmbeddingRanker` (semantic, embedding-only endpoint over the egress proxy) + `HybridRanker` (RRF fusion, mirroring `core/src/memory/recall.rs`) behind the `PassageRanker` seam; new env `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` (fail-closed) + its allowlist entry.
- `core/tests/web_research_e2e.rs` — `#[ignore]` live round-trip against a real SearxNG (`scripts/web-search/setup-searxng.sh`) + allowlisted content hosts.
- Firecracker micro-VM entry (`web_research_firecracker_entry`) mirroring `web_fetch_firecracker_entry`.
- Parallel fetching of the top-N pages (perf).

File an issue for each when Slice 3 lands.

---

## Self-review notes (author)

- **Spec coverage:** goal/shape → 2.1–2.4; web-common consolidation → 1.1–1.2; new modules chunk/rank/research/handler → 2.1–2.4; result shape (sources/unfetched/counts) → 2.4 `outcome_to_json` + tests; no-silent-fallback → 2.3 tests (`off_allowlist`, `one_fetch_failure`, `search_failure`); security manifest/union-allowlist/Relaxed guard/registry → 3.1–3.2; extensibility seam → 2.2 trait + deferred Slice 4; verification → per-task + 3.3. Live e2e + embedding ranker deliberately deferred (spec Non-goals / Slice 4).
- **Type consistency:** `chunk_passages -> Vec<String>`; `PassageRanker::rank(&self, &str, &[String]) -> Vec<ScoredPassage>`; `ScoredPassage{text,score}`; `research(...) -> Result<ResearchOutcome, ResearchError>` consumed identically in handler; `ToolEntry`/`SandboxPolicy` fields match `web_search.rs`/`web_fetch.rs` verbatim (all 12 policy fields + 8 ToolEntry fields present).
- **Placeholder scan:** none — every code step is complete; the only "stub" files are explicit, transient scaffolding in Task 2.1 replaced in 2.2–2.4.
