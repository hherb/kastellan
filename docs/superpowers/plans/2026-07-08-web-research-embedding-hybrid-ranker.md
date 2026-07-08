# web-research EmbeddingRanker + HybridRanker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in semantic + hybrid (RRF) passage ranking to the `web.research` worker, degrading to lexical-with-a-signal when the configured embed endpoint fails.

**Architecture:** All scoring is pure (`bm25`, `cosine`, `rrf_fuse` in `rank.rs`); the only network I/O sits behind an `Embedder` seam (`embed.rs`). `research()` embeds the query once up front (fail-fast), then embeds each page's passages and RRF-fuses the two lanes. When no `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` is set, behaviour is byte-identical to today.

**Tech Stack:** Rust (rustc 1.96), `reqwest::blocking` + rustls (via the shared `web-common` transport), `serde_json`, `url`. No new external dependencies.

**Spec:** `docs/superpowers/specs/2026-07-08-web-research-embedding-hybrid-ranker-design.md`

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible deps only.** This plan adds **no** new dependencies — reuse what the crate already has.
- **Cross-platform (Linux + macOS).** No OS-specific code is introduced here (pure Rust + the existing transport).
- **Keep files under 500 LOC** where feasible. `rank.rs` and `research.rs` grow; `embed.rs` is new. If `rank.rs` nears the cap, that is acceptable for pure primitives; do not pre-emptively split.
- **rustc 1.96**, source cargo first every shell: `source "$HOME/.cargo/env"`.
- **Run all `cargo` commands in the FOREGROUND** — never background a `cargo test`/`clippy` and wait on it.
- **TDD:** write the failing test first, watch it fail, implement minimally, watch it pass, commit.
- **Stage specific files** in every commit (`git add <paths>`), never `git add -A`.
- **Branch:** all work lands on `feat/web-research-embedding-hybrid-ranker` (already created; the design doc is committed there).
- Every task ends green: `cargo build -p <crate>` + `cargo clippy -p <crate> --all-targets -- -D warnings` + the task's tests.

---

### Task 1: `post` on the shared transport seam

Add a POST method to `HttpGet` (default = unsupported, so web-search/web-fetch are untouched), implement it for the real transports and the test fake.

**Files:**
- Modify: `workers/web-common/src/http.rs` (trait + `ReqwestGet` + `Box<dyn HttpGet>` impls + default-post test)
- Modify: `workers/web-common/src/proxy_connect.rs` (`ProxyConnectGet` impl)
- Modify: `workers/web-common/src/testing.rs` (`FakeGet` impl — pops the same FIFO queue)

**Interfaces:**
- Produces: `HttpGet::post(&self, url: &Url, content_type: &str, body: &[u8]) -> Result<RawResponse, String>` with a **default impl** returning `Err("post: unsupported by this transport")`. `FakeGet::post` returns the next canned `RawResponse` (shared queue with `get`).

- [ ] **Step 1: Write the failing test** — append to the `make_get_tests` (or a new `post_tests`) module in `workers/web-common/src/http.rs`:

```rust
#[cfg(test)]
mod post_tests {
    use super::*;

    struct GetOnly;
    impl HttpGet for GetOnly {
        fn get(&self, _url: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "get-only" }
        // deliberately does NOT override post -> exercises the default
    }

    #[test]
    fn default_post_is_unsupported() {
        let t = GetOnly;
        let err = t.post(&Url::parse("https://x.test/e").unwrap(), "application/json", b"{}")
            .unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run it, expect FAIL** (method `post` does not exist yet):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-common --lib post_tests -- --nocapture
```
Expected: compile error / FAIL — no method `post`.

- [ ] **Step 3: Add the trait method with a default impl** — in `workers/web-common/src/http.rs`, inside `pub trait HttpGet`, after `transport_kind`:

```rust
    /// POST `body` with `content_type` to `url`, no redirect following.
    /// Default: unsupported — only transports that need it (the embedding POST)
    /// override this, so GET-only siblings (web-search, web-fetch) are untouched.
    fn post(&self, _url: &Url, _content_type: &str, _body: &[u8])
        -> Result<RawResponse, String>
    {
        Err("post: unsupported by this transport".to_string())
    }
```

- [ ] **Step 4: Forward `post` on `Box<dyn HttpGet>`** — in the `impl HttpGet for Box<dyn HttpGet>` block, add:

```rust
    fn post(&self, url: &Url, content_type: &str, body: &[u8])
        -> Result<RawResponse, String>
    {
        (**self).post(url, content_type, body)
    }
```

- [ ] **Step 5: Implement `post` for `ReqwestGet`** — in `impl HttpGet for ReqwestGet`, add (mirror `get`, body-capped read):

```rust
    fn post(&self, url: &Url, content_type: &str, body: &[u8])
        -> Result<RawResponse, String>
    {
        use std::io::Read;
        let resp = self
            .client
            .post(url.clone())
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body.to_vec())
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let header = |name: reqwest::header::HeaderName| -> Option<String> {
            resp.headers().get(&name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        };
        let location = header(reqwest::header::LOCATION);
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();
        let mut out = Vec::new();
        resp.take((MAX_BODY_BYTES as u64) + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
        if out.len() > MAX_BODY_BYTES {
            return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
        }
        Ok(RawResponse { status, location, content_type, body: out })
    }
```

- [ ] **Step 6: Implement `post` for `ProxyConnectGet`** — in `workers/web-common/src/proxy_connect.rs`, generalize the GET-only path into a method+body-carrying one, then add `post`. Three concrete edits:

  (a) Rename `get_async` → `request_async` and add method/body params. Change its signature and its final `run_get(...)` calls:
  ```rust
  async fn request_async(
      &self,
      url: &Url,
      method: &str,
      content_type: Option<&str>,
      body: Vec<u8>,
  ) -> Result<RawResponse, String> {
      // ... unchanged: host/port, dial UDS, CONNECT, require 200 ...
      // step 3 becomes:
      match url.scheme() {
          "https" => {
              let tls = tls_connect(stream, url, Arc::clone(&self.tls)).await?;
              run_request(tls, url, host, &self.user_agent, method, content_type, body).await
          }
          "http" => run_request(stream, url, host, &self.user_agent, method, content_type, body).await,
          other => Err(format!("unsupported scheme: {other}")),
      }
  }
  ```

  (b) In `impl HttpGet for ProxyConnectGet`, make `get` delegate and add `post`:
  ```rust
  fn get(&self, url: &Url) -> Result<RawResponse, String> {
      self.rt.block_on(async {
          match tokio::time::timeout(
              Duration::from_secs(TIMEOUT_SECS),
              self.request_async(url, "GET", None, Vec::new()),
          ).await {
              Ok(r) => r,
              Err(_) => Err(format!("request exceeded {TIMEOUT_SECS}s")),
          }
      })
  }

  fn post(&self, url: &Url, content_type: &str, body: &[u8]) -> Result<RawResponse, String> {
      let ct = content_type.to_string();
      let body = body.to_vec();
      self.rt.block_on(async {
          match tokio::time::timeout(
              Duration::from_secs(TIMEOUT_SECS),
              self.request_async(url, "POST", Some(&ct), body),
          ).await {
              Ok(r) => r,
              Err(_) => Err(format!("request exceeded {TIMEOUT_SECS}s")),
          }
      })
  }
  ```

  (c) Generalize `run_get` → `run_request` — add `method`/`content_type`/`body` params and switch the body type from `Empty` to `Full` (add a `CONTENT_TYPE` header only when present):
  ```rust
  async fn run_request<IO>(
      io: IO, url: &Url, host: &str, user_agent: &str,
      method: &str, content_type: Option<&str>, body: Vec<u8>,
  ) -> Result<RawResponse, String>
  where IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
  {
      // ... unchanged handshake + path_and_query ...
      let mut builder = Request::builder()
          .method(method)
          .uri(&path_and_query)
          .header(hyper::header::HOST, host)
          .header(hyper::header::USER_AGENT, user_agent)
          .header(hyper::header::ACCEPT_ENCODING, "identity")
          .header(hyper::header::CONNECTION, "close");
      if let Some(ct) = content_type {
          builder = builder.header(hyper::header::CONTENT_TYPE, ct);
      }
      let req = builder
          .body(http_body_util::Full::<bytes::Bytes>::new(bytes::Bytes::from(body)))
          .map_err(|e| format!("build request: {e}"))?;
      // ... unchanged send + status/headers + body-cap collection ...
  }
  ```
  (`http_body_util::Full` replaces `Empty`; `BodyExt` is already imported. GET passes an empty `Vec`, yielding an empty `Full` body — byte-identical on the wire to the old empty GET.)

- [ ] **Step 7: Implement `post` for `FakeGet`** — in `workers/web-common/src/testing.rs`, inside `impl HttpGet for FakeGet`, add (same FIFO queue as `get`, so a test can enqueue a search GET then an embed POST response):

```rust
    fn post(&self, _url: &Url, _content_type: &str, _body: &[u8])
        -> Result<RawResponse, String>
    {
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| "no more canned responses".to_string())
    }
```

- [ ] **Step 8: Add a `FakeGet::post` test** — in `workers/web-common/src/testing.rs` (add a `#[cfg(test)] mod tests` if none exists, else append):

```rust
#[cfg(test)]
mod post_fake_tests {
    use super::*;
    #[test]
    fn fake_post_pops_next_response() {
        let f = FakeGet::new(vec![ok_resp("embedded")]);
        let r = f.post(&url::Url::parse("http://e.test/embeddings").unwrap(),
                       "application/json", b"{}").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"embedded");
    }
}
```

- [ ] **Step 9: Run the web-common tests + build the siblings** (prove nothing broke):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-common
cargo build -p kastellan-worker-web-search -p kastellan-worker-web-fetch
cargo clippy -p kastellan-worker-web-common --all-targets -- -D warnings
```
Expected: all PASS; siblings build (they never call `post`).

- [ ] **Step 10: Commit**

```sh
git add workers/web-common/src/http.rs workers/web-common/src/proxy_connect.rs workers/web-common/src/testing.rs
git commit -m "feat(web-common): add post() to the HttpGet transport seam (default-unsupported)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Pure ranking primitives — `bm25`, `cosine`, `rrf_fuse`

Add the pure scoring functions to `rank.rs`. Keep `LexicalRanker`/`PassageRanker` for now (Task 4 retires them) so every current caller still compiles; `LexicalRanker::rank` delegates to `bm25`.

**Files:**
- Modify: `workers/web-research/src/rank.rs`

**Interfaces:**
- Produces:
  - `pub fn bm25(query: &str, passages: &[String]) -> Vec<ScoredPassage>`
  - `pub fn cosine(query_emb: &[f32], passages: &[String], passage_embs: &[Vec<f32>]) -> Vec<ScoredPassage>`
  - `pub fn rrf_fuse(lexical: &[ScoredPassage], semantic: &[ScoredPassage]) -> Vec<ScoredPassage>`
  - `pub const RRF_K: f64 = 60.0;`
  - `ScoredPassage` unchanged.

- [ ] **Step 1: Write failing tests** — add to the `#[cfg(test)] mod tests` in `workers/web-research/src/rank.rs`:

```rust
    #[test]
    fn bm25_matches_legacy_lexical_ranker() {
        let passages = vec![
            "The cat sat on the mat.".to_string(),
            "Rust uses bwrap to create user namespaces for sandboxing.".to_string(),
        ];
        let r = bm25("bwrap user namespaces sandbox", &passages);
        assert_eq!(r.len(), 1);
        assert!(r[0].text.contains("bwrap") && r[0].score > 0.0);
    }

    #[test]
    fn cosine_ranks_similar_vector_first_and_skips_zero_norm() {
        let passages = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let embs = vec![
            vec![1.0_f32, 0.0],   // identical direction to query -> sim 1.0
            vec![0.0_f32, 1.0],   // orthogonal -> sim 0.0 -> omitted
            vec![0.0_f32, 0.0],   // zero-norm -> omitted
        ];
        let q = vec![1.0_f32, 0.0];
        let r = cosine(&q, &passages, &embs);
        assert_eq!(r.len(), 1, "only the similar passage survives");
        assert_eq!(r[0].text, "a");
        assert!((r[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_empty_inputs_yield_empty() {
        assert!(cosine(&[], &["x".to_string()], &[vec![1.0]]).is_empty());
        assert!(cosine(&[1.0], &[], &[]).is_empty());
        // length mismatch between passages and embeddings -> empty (defensive)
        assert!(cosine(&[1.0], &["x".to_string()], &[]).is_empty());
    }

    #[test]
    fn rrf_fuse_rewards_agreement_and_unions_lanes() {
        let lex = vec![
            ScoredPassage { text: "top-both".into(), score: 9.0 },
            ScoredPassage { text: "lex-only".into(), score: 1.0 },
        ];
        let sem = vec![
            ScoredPassage { text: "top-both".into(), score: 0.9 },
            ScoredPassage { text: "sem-only".into(), score: 0.5 },
        ];
        let f = rrf_fuse(&lex, &sem);
        let texts: Vec<&str> = f.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts[0], "top-both", "ranked #1 in both lanes wins");
        // union: a passage in only one lane still appears
        assert!(texts.contains(&"lex-only") && texts.contains(&"sem-only"));
    }

    #[test]
    fn rrf_fuse_with_empty_lane_equals_other_lane_order() {
        let lex = vec![
            ScoredPassage { text: "x".into(), score: 3.0 },
            ScoredPassage { text: "y".into(), score: 1.0 },
        ];
        let f = rrf_fuse(&lex, &[]);
        let texts: Vec<&str> = f.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["x", "y"]);
    }
```

- [ ] **Step 2: Run, expect FAIL** (functions not defined):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research --lib rank -- --nocapture
```
Expected: FAIL — `bm25` / `cosine` / `rrf_fuse` not found.

- [ ] **Step 3: Extract `bm25` + delegate `LexicalRanker`** — in `workers/web-research/src/rank.rs`, move the body of `LexicalRanker::rank` into a free `pub fn bm25`, and make the trait impl delegate:

```rust
/// Lexical BM25 lane. Pure + deterministic; treats the passage set as the corpus.
pub fn bm25(query: &str, passages: &[String]) -> Vec<ScoredPassage> {
    // (verbatim body of the former LexicalRanker::rank)
    let q_terms = unique(&tokenize(query));
    if q_terms.is_empty() || passages.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = passages.iter().map(|p| tokenize(p)).collect();
    let n = docs.len() as f64;
    let avg_len: f64 = docs.iter().map(|d| d.len()).sum::<usize>() as f64 / n.max(1.0);
    let dfs: Vec<f64> = q_terms
        .iter()
        .map(|term| docs.iter().filter(|d| d.contains(term)).count() as f64)
        .collect();
    let mut scored: Vec<ScoredPassage> = Vec::new();
    for (doc, passage) in docs.iter().zip(passages.iter()) {
        let dl = doc.len() as f64;
        let mut score = 0.0_f64;
        for (term, &df) in q_terms.iter().zip(dfs.iter()) {
            let tf = doc.iter().filter(|t| *t == term).count() as f64;
            if tf == 0.0 { continue; }
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            let denom = tf + K1 * (1.0 - B + B * dl / avg_len.max(1.0));
            score += idf * (tf * (K1 + 1.0)) / denom;
        }
        if score > 0.0 {
            scored.push(ScoredPassage { text: passage.clone(), score });
        }
    }
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

impl PassageRanker for LexicalRanker {
    fn rank(&self, query: &str, passages: &[String]) -> Vec<ScoredPassage> {
        bm25(query, passages)
    }
}
```

- [ ] **Step 4: Add `cosine`** — in `rank.rs`:

```rust
/// L2 norm of a vector.
fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Semantic lane: cosine similarity of each passage embedding to the query
/// embedding. Pure. Passages whose embedding is zero-norm, is a different length
/// than the query embedding, or yields a non-positive similarity are omitted
/// (mirrors `bm25`'s "no signal -> omit"). `passage_embs[i]` pairs with
/// `passages[i]`; a length mismatch between the two slices yields an empty result.
pub fn cosine(query_emb: &[f32], passages: &[String], passage_embs: &[Vec<f32>])
    -> Vec<ScoredPassage>
{
    if query_emb.is_empty() || passages.len() != passage_embs.len() {
        return Vec::new();
    }
    let qn = l2_norm(query_emb);
    if qn == 0.0 {
        return Vec::new();
    }
    let mut scored: Vec<ScoredPassage> = Vec::new();
    for (p, e) in passages.iter().zip(passage_embs.iter()) {
        if e.len() != query_emb.len() {
            continue;
        }
        let en = l2_norm(e);
        if en == 0.0 {
            continue;
        }
        let dot: f32 = query_emb.iter().zip(e.iter()).map(|(a, b)| a * b).sum();
        let sim = (dot / (qn * en)) as f64;
        if sim > 0.0 {
            scored.push(ScoredPassage { text: p.clone(), score: sim });
        }
    }
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored
}
```

- [ ] **Step 5: Add `rrf_fuse` + `RRF_K`** — in `rank.rs`:

```rust
/// RRF damping constant (classical k = 60, matching `core::memory::recall`).
pub const RRF_K: f64 = 60.0;

/// Fuse two best-first ranked lists via parameter-free Reciprocal Rank Fusion.
/// Each passage's fused score = sum over lanes of 1/(RRF_K + rank), where `rank`
/// is 1-based position in that lane. Keyed by passage text (both lanes rank the
/// same passage set). Best-first; stable tie-break by first-seen order (a passage
/// appearing in only one lane still surfaces — the union is deliberate recall).
pub fn rrf_fuse(lexical: &[ScoredPassage], semantic: &[ScoredPassage])
    -> Vec<ScoredPassage>
{
    use std::collections::HashMap;
    let mut scores: HashMap<&str, f64> = HashMap::new();
    let mut order: Vec<&str> = Vec::new(); // first-seen order for stable ties
    for lane in [lexical, semantic] {
        for (i, sp) in lane.iter().enumerate() {
            let rank = (i + 1) as f64;
            let key = sp.text.as_str();
            scores
                .entry(key)
                .and_modify(|s| *s += 1.0 / (RRF_K + rank))
                .or_insert_with(|| {
                    order.push(key);
                    1.0 / (RRF_K + rank)
                });
        }
    }
    let mut out: Vec<ScoredPassage> = order
        .iter()
        .map(|k| ScoredPassage { text: (*k).to_string(), score: scores[*k] })
        .collect();
    // Stable sort: ties keep first-seen (lexical-first) order.
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}
```

- [ ] **Step 6: Run, expect PASS**:

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research --lib rank
cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings
```
Expected: all rank tests PASS (the 5 legacy `LexicalRanker` tests still pass via the delegate); clippy clean.

- [ ] **Step 7: Commit**

```sh
git add workers/web-research/src/rank.rs
git commit -m "feat(web-research): pure bm25/cosine/rrf_fuse ranking primitives

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `embed.rs` — the `Embedder` I/O seam

**Files:**
- Create: `workers/web-research/src/embed.rs`
- Modify: `workers/web-research/src/main.rs` (add `mod embed;`)

**Interfaces:**
- Produces:
  - `pub trait Embedder { fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>; }`
  - `pub enum EmbedError { Transport(String), Status(u16), Decode(String), CountMismatch { requested: usize, returned: usize } }` (impl `Display`)
  - `pub struct HttpEmbedder<T: HttpGet> { transport: T, endpoint: Url, model: String }` with `pub fn new(transport: T, endpoint: Url, model: String) -> Self`
  - `#[cfg(test)] pub(crate) struct FakeEmbedder` used by `research.rs`/`handler.rs` tests.
- Consumes: `HttpGet::post` (Task 1); the OpenAI-compatible embedding wire shape.

- [ ] **Step 1: Write the failing tests** — create `workers/web-research/src/embed.rs` with only the test module + `use super::*;`, or write tests inline after the impl. Tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::FakeGet;

    fn embed_json(vectors: &[&[f32]]) -> String {
        let data: Vec<String> = vectors.iter().enumerate().map(|(i, v)| {
            let nums: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            format!(r#"{{"index":{i},"embedding":[{}]}}"#, nums.join(","))
        }).collect();
        format!(r#"{{"data":[{}]}}"#, data.join(","))
    }

    fn endpoint() -> Url { Url::parse("http://127.0.0.1:11434/v1/embeddings").unwrap() }

    #[test]
    fn decodes_envelope_in_input_order() {
        let body = embed_json(&[&[1.0, 2.0], &[3.0, 4.0]]);
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None,
            content_type: "application/json".into(), body: body.into_bytes(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "embeddinggemma".into());
        let out = e.embed(&["a".into(), "b".into()]).unwrap();
        assert_eq!(out, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn non_2xx_is_status_error() {
        let t = FakeGet::new(vec![RawResponse {
            status: 503, location: None, content_type: "text/plain".into(), body: Vec::new(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        assert!(matches!(e.embed(&["a".into()]), Err(EmbedError::Status(503))));
    }

    #[test]
    fn undecodable_body_is_decode_error() {
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None, content_type: "application/json".into(),
            body: b"not json".to_vec(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        assert!(matches!(e.embed(&["a".into()]), Err(EmbedError::Decode(_))));
    }

    #[test]
    fn count_mismatch_is_error() {
        let body = embed_json(&[&[1.0]]); // 1 vector for 2 inputs
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None, content_type: "application/json".into(),
            body: body.into_bytes(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        assert!(matches!(
            e.embed(&["a".into(), "b".into()]),
            Err(EmbedError::CountMismatch { requested: 2, returned: 1 })
        ));
    }

    // Exercises FakeEmbedder here so it is not dead code before Task 4 wires it
    // into research()/handler() tests.
    #[test]
    fn fake_embedder_returns_canned_and_counts_and_fails() {
        let e = FakeEmbedder::new(&[("a", vec![1.0_f32, 0.0])]);
        assert_eq!(e.embed(&["a".into(), "b".into()]).unwrap(),
                   vec![vec![1.0, 0.0], vec![]]); // absent text -> empty vec
        assert_eq!(e.calls.get(), 1);
        assert!(FakeEmbedder::failing().embed(&["x".into()]).is_err());
    }
}
```

- [ ] **Step 2: Add `mod embed;`** to `workers/web-research/src/main.rs` (keep the module list alphabetical: `chunk, embed, handler, rank, research`).

- [ ] **Step 3: Run, expect FAIL** (types not defined):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research --lib embed -- --nocapture
```
Expected: FAIL — `HttpEmbedder`/`EmbedError` not found.

- [ ] **Step 4: Implement `embed.rs`** — write the module (above the test block):

```rust
//! Embed passage/query text into vectors via an embedding-only endpoint.
//!
//! [`Embedder`] is the single network-touching seam (faked in tests). The real
//! [`HttpEmbedder`] POSTs the OpenAI-compatible `{model, input:[...]}` body to the
//! configured endpoint over the shared [`HttpGet`] transport (the same proxy path
//! content fetches use) and decodes `{data:[{index, embedding:[...]}]}`. Cosine
//! ranking is dimension-agnostic, so no Matryoshka truncation happens here — only
//! a count check (one vector per input) and a shared-length check.

use serde::Deserialize;
use url::Url;

use kastellan_worker_web_common::http::HttpGet;

/// Turn texts into embedding vectors. Batches all inputs into one request.
pub trait Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Why an embedding call failed.
#[derive(Debug)]
pub enum EmbedError {
    Transport(String),
    Status(u16),
    Decode(String),
    CountMismatch { requested: usize, returned: usize },
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Transport(m) => write!(f, "transport: {m}"),
            EmbedError::Status(s) => write!(f, "endpoint status {s}"),
            EmbedError::Decode(m) => write!(f, "decode: {m}"),
            EmbedError::CountMismatch { requested, returned } =>
                write!(f, "vector count mismatch: requested {requested}, returned {returned}"),
        }
    }
}

#[derive(serde::Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingData {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

/// Real embedder over the shared transport.
pub struct HttpEmbedder<T: HttpGet> {
    transport: T,
    endpoint: Url,
    model: String,
}

impl<T: HttpGet> HttpEmbedder<T> {
    pub fn new(transport: T, endpoint: Url, model: String) -> Self {
        Self { transport, endpoint, model }
    }
}

impl<T: HttpGet> Embedder for HttpEmbedder<T> {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let req = EmbeddingRequest { model: &self.model, input: texts };
        let body = serde_json::to_vec(&req)
            .map_err(|e| EmbedError::Decode(format!("request encode: {e}")))?;
        let resp = self
            .transport
            .post(&self.endpoint, "application/json", &body)
            .map_err(EmbedError::Transport)?;
        if !(200..300).contains(&resp.status) {
            return Err(EmbedError::Status(resp.status));
        }
        let decoded: EmbeddingResponse = serde_json::from_slice(&resp.body)
            .map_err(|e| EmbedError::Decode(e.to_string()))?;
        if decoded.data.len() != texts.len() {
            return Err(EmbedError::CountMismatch {
                requested: texts.len(),
                returned: decoded.data.len(),
            });
        }
        // Order by `index` so the result pairs with `texts[i]` even if the
        // backend returns rows out of order.
        let mut rows = decoded.data;
        rows.sort_by_key(|d| d.index);
        Ok(rows.into_iter().map(|d| d.embedding).collect())
    }
}

/// Test embedder: canned vectors keyed by exact text, or a forced failure.
#[cfg(test)]
pub(crate) struct FakeEmbedder {
    /// text -> vector; any text absent yields a zero-length vector.
    pub map: std::collections::HashMap<String, Vec<f32>>,
    /// When true, every `embed` call returns an error (simulates a dead endpoint).
    pub fail: bool,
    /// Number of times `embed` was invoked (interior-mutable for assertions).
    pub calls: std::cell::Cell<usize>,
}

#[cfg(test)]
impl FakeEmbedder {
    pub fn new(pairs: &[(&str, Vec<f32>)]) -> Self {
        Self {
            map: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            fail: false,
            calls: std::cell::Cell::new(0),
        }
    }
    pub fn failing() -> Self {
        Self { map: Default::default(), fail: true, calls: std::cell::Cell::new(0) }
    }
}

#[cfg(test)]
impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.set(self.calls.get() + 1);
        if self.fail {
            return Err(EmbedError::Transport("fake endpoint down".into()));
        }
        Ok(texts.iter().map(|t| self.map.get(t).cloned().unwrap_or_default()).collect())
    }
}
```

- [ ] **Step 5: Run, expect PASS**:

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research --lib embed
cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings
```
Expected: 5 embed tests PASS (incl. the `FakeEmbedder` self-test, which keeps it from being dead code); clippy clean.

- [ ] **Step 6: Commit**

```sh
git add workers/web-research/src/embed.rs workers/web-research/src/main.rs
git commit -m "feat(web-research): Embedder seam + HttpEmbedder (OpenAI-compat POST)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Wire hybrid ranking into `research()` + retire `PassageRanker`

Replace the `R: PassageRanker` generic with `Option<&dyn Embedder>`; embed the query once up front (fail-fast), fuse per page, degrade-with-signal. Retire the now-unused trait/struct. Update `handler.rs`'s call site to pass `None` (env-driven embedder arrives in Task 5).

**Files:**
- Modify: `workers/web-research/src/research.rs`
- Modify: `workers/web-research/src/rank.rs` (delete `PassageRanker` + `LexicalRanker`)
- Modify: `workers/web-research/src/handler.rs` (call site: drop `ranker`, pass `None`)

**Interfaces:**
- Consumes: `bm25`, `cosine`, `rrf_fuse` (Task 2); `Embedder`, `FakeEmbedder` (Task 3).
- Produces:
  - `pub enum RankMode { Lexical, Hybrid }`
  - `ResearchOutcome { sources, unfetched, ranking: RankMode, embed_note: Option<String> }`
  - `pub fn research<T: HttpGet>(transport: &T, endpoint: &Url, allowlist: &HostAllowlist, embedder: Option<&dyn Embedder>, query: &str, max_sources: usize, max_passages: usize) -> Result<ResearchOutcome, ResearchError>`

- [ ] **Step 1: Write the failing tests** — in `research.rs` `#[cfg(test)] mod tests`, replace `use crate::rank::LexicalRanker;` with `use crate::embed::FakeEmbedder;`, update EVERY existing `research(&t, &endpoint(), &a, &LexicalRanker, ...)` call to `research(&t, &endpoint(), &a, None, ...)`, and add assertions that `out.ranking` is `RankMode::Lexical` and `out.embed_note.is_none()` on the happy-path test. Then add:

```rust
    #[test]
    fn hybrid_surfaces_paraphrase_passage_bm25_misses() {
        // Query shares NO surface terms with the relevant passage, but the fake
        // embedder gives them near-identical vectors -> cosine lane surfaces it.
        // FakeEmbedder keys MUST equal chunk_passages() output exactly: it splits
        // on "\n\n" and trims, so this page yields
        //   ["unrelated filler line.", "Containers isolate processes from the host."]
        // (an unmapped key -> empty vec -> cosine skips it -> the test would fail).
        let page = "unrelated filler line.\n\nContainers isolate processes from the host.";
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Doc", "https://docs.example.org/x")])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain".into(), body: page.as_bytes().to_vec() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let query = "sandboxing";
        let emb = FakeEmbedder::new(&[
            ("sandboxing", vec![1.0_f32, 0.0]),
            ("Containers isolate processes from the host.", vec![1.0_f32, 0.05]),
            ("unrelated filler line.", vec![0.0_f32, 1.0]),
        ]);
        let out = research(&t, &endpoint(), &a, Some(&emb), query, 3, 3).unwrap();
        assert!(matches!(out.ranking, RankMode::Hybrid));
        assert!(out.embed_note.is_none());
        assert_eq!(out.sources.len(), 1);
        assert!(out.sources[0].passages.iter().any(|p| p.text.contains("Containers isolate")));
    }

    #[test]
    fn query_embed_failure_degrades_whole_call_to_lexical_and_is_fail_fast() {
        let page = "bwrap namespaces sandbox details here.";
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Doc", "https://docs.example.org/x")])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain".into(), body: page.as_bytes().to_vec() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let emb = FakeEmbedder::failing();
        let out = research(&t, &endpoint(), &a, Some(&emb), "bwrap namespaces", 3, 3).unwrap();
        assert!(matches!(out.ranking, RankMode::Lexical));
        assert!(out.embed_note.is_some(), "degrade must be signalled");
        assert_eq!(out.sources.len(), 1, "lexical still ranks the page");
        assert_eq!(emb.calls.get(), 1, "fail-fast: only the query embed is attempted");
    }
```

- [ ] **Step 2: Run, expect FAIL** (signature changed / `RankMode` missing):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research --lib research -- --nocapture
```
Expected: compile error — `research` arity, `RankMode` undefined.

- [ ] **Step 3: Update `ResearchOutcome` + add `RankMode`** — in `research.rs`:

```rust
/// How a research call ranked its passages.
#[derive(Debug, PartialEq)]
pub enum RankMode {
    Lexical,
    Hybrid,
}

#[derive(Debug)]
pub struct ResearchOutcome {
    pub sources: Vec<SourcePassages>,
    pub unfetched: Vec<UnfetchedSource>,
    /// `Hybrid` iff an embedder was configured AND the query embedded OK.
    pub ranking: RankMode,
    /// `Some(reason)` iff a configured semantic lane fell back to lexical for the
    /// whole call (query embed failed) or for at least one page (first reason wins).
    pub embed_note: Option<String>,
}
```

- [ ] **Step 4: Replace `gather_source` + `research`** — swap the `ranker: &R` threading for embedding-aware ranking. New `gather_source` returns `(SourcePassages, Option<String>)` (the page's degrade note):

```rust
use crate::embed::Embedder;
use crate::rank::{bm25, cosine, rrf_fuse};

/// Rank one page's passages. Lexical always; if `query_emb` is live, add the
/// semantic lane and RRF-fuse. Returns the ranked passages and an optional
/// degrade reason (the page fell back to lexical because its passage embed failed).
fn rank_page(
    embedder: Option<&dyn Embedder>,
    query_emb: Option<&[f32]>,
    query: &str,
    passages: &[String],
) -> (Vec<ScoredPassage>, Option<String>) {
    let lexical = bm25(query, passages);
    match (embedder, query_emb) {
        (Some(e), Some(qe)) => match e.embed(passages) {
            Ok(pe) if pe.len() == passages.len() => {
                let semantic = cosine(qe, passages, &pe);
                (rrf_fuse(&lexical, &semantic), None)
            }
            Ok(pe) => (
                lexical,
                Some(format!(
                    "embed: passage vector count mismatch (got {}, want {})",
                    pe.len(),
                    passages.len()
                )),
            ),
            Err(err) => (lexical, Some(format!("embed: passage embedding failed: {err}"))),
        },
        _ => (lexical, None),
    }
}

fn gather_source<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    embedder: Option<&dyn Embedder>,
    query_emb: Option<&[f32]>,
    query: &str,
    hit: &Hit,
    max_passages: usize,
) -> Result<(SourcePassages, Option<String>), String> {
    let url = Url::parse(&hit.url).map_err(|e| format!("fetch-failed: bad url: {e}"))?;
    let outcome = drive(transport, allowlist, url).map_err(|e| short_fetch_reason(&e))?;
    if !(200..300).contains(&outcome.status) {
        return Err(format!("fetch-failed: status {}", outcome.status));
    }
    let extracted = extract(&outcome.content_type, &outcome.body)
        .map_err(|e| format!("fetch-failed: extraction: {e}"))?;
    let passages = chunk_passages(&extracted.text);
    let (mut ranked, note) = rank_page(embedder, query_emb, query, &passages);
    ranked.truncate(max_passages);
    if ranked.is_empty() {
        return Err("no-relevant-passages".to_string());
    }
    Ok((
        SourcePassages {
            url: outcome.final_url,
            title: hit.title.clone(),
            snippet: hit.snippet.clone(),
            passages: ranked,
        },
        note,
    ))
}

pub fn research<T: HttpGet>(
    transport: &T,
    endpoint: &Url,
    allowlist: &HostAllowlist,
    embedder: Option<&dyn Embedder>,
    query: &str,
    max_sources: usize,
    max_passages: usize,
) -> Result<ResearchOutcome, ResearchError> {
    if query.trim().is_empty() {
        return Err(ResearchError::EmptyQuery);
    }
    let max_sources = max_sources.clamp(1, MAX_MAX_SOURCES);
    let max_passages = max_passages.clamp(1, MAX_MAX_PASSAGES);

    // Embed the query once up front. On failure, degrade the WHOLE call to
    // lexical and drop the embedder (fail-fast: a dead endpoint is not re-hit
    // once per page). This is the dominant failure mode (endpoint down).
    let mut embed_note: Option<String> = None;
    let query_emb: Option<Vec<f32>> = match embedder {
        Some(e) => match e.embed(&[query.to_string()]) {
            Ok(mut v) if v.len() == 1 => Some(v.remove(0)),
            Ok(v) => {
                embed_note = Some(format!(
                    "embed: query vector count {} (expected 1); ranking lexical",
                    v.len()
                ));
                None
            }
            Err(err) => {
                embed_note = Some(format!("embed: query embedding failed: {err}; ranking lexical"));
                None
            }
        },
        None => None,
    };
    // Effective embedder: only present when the query embedded successfully.
    let eff_embedder = query_emb.as_ref().and(embedder);
    let ranking = if query_emb.is_some() { RankMode::Hybrid } else { RankMode::Lexical };

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
        match gather_source(
            transport, allowlist, eff_embedder, query_emb.as_deref(),
            query, hit, max_passages,
        ) {
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
    Ok(ResearchOutcome { sources, unfetched, ranking, embed_note })
}
```

Add `use crate::rank::ScoredPassage;` if not already imported (it is, via `use crate::rank::{PassageRanker, ScoredPassage};` — change that line to `use crate::rank::ScoredPassage;`).

- [ ] **Step 5: Retire the trait/struct in `rank.rs`** — delete the `PassageRanker` trait and the `LexicalRanker` struct + its `impl PassageRanker`. Keep `bm25`, `cosine`, `rrf_fuse`, `ScoredPassage`, `RRF_K`, `tokenize`, `unique`, `K1`, `B`. Update the module doc's first paragraph to describe the primitives, not the retired seam. Re-point the 5 legacy tests from `LexicalRanker.rank(q, &p)` to `bm25(q, &p)`.

- [ ] **Step 6: Fix the `handler.rs` call site (minimal)** — in `handler.rs`:
  - remove `use crate::rank::LexicalRanker;`
  - remove the `ranker: LexicalRanker` field from `WebResearchHandler` and both constructors (`from_env`, `with_parts`)
  - change the `research(...)` call to pass `None` in the embedder position:
    ```rust
    let out = research(
        &self.transport, &self.endpoint, &self.allowlist, None,
        &p.query, max_sources, max_passages,
    ).map_err(research_err_to_rpc)?;
    ```
  (The env-driven embedder + JSON surface land in Task 5; handler tests still pass because `None` → lexical, same output shape as today.)

- [ ] **Step 7: Run, expect PASS**:

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research
cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings
```
Expected: rank + research + handler + chunk tests all PASS; clippy clean (no dead `FakeEmbedder`).

- [ ] **Step 8: Commit**

```sh
git add workers/web-research/src/research.rs workers/web-research/src/rank.rs workers/web-research/src/handler.rs
git commit -m "feat(web-research): hybrid RRF ranking in research(); retire PassageRanker

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Handler — read the embed endpoint, build the embedder, surface the signal

**Files:**
- Modify: `workers/web-research/src/handler.rs`

**Interfaces:**
- Consumes: `HttpEmbedder`, `Embedder`, `FakeEmbedder` (Task 3); `RankMode`/`embed_note` on `ResearchOutcome` (Task 4); `make_get` + `validate_endpoint` (existing).
- Produces: JSON result gains `"ranking": "hybrid"|"lexical"` and (when set) `"embed_note": "<reason>"`.

- [ ] **Step 1: Write the failing tests** — in `handler.rs` tests, add a constructor that injects an embedder + a FakeGet, and assert the JSON surface:

```rust
    fn handler_with_embedder(
        responses: Vec<RawResponse>,
        embedder: Option<Box<dyn crate::embed::Embedder>>,
    ) -> WebResearchHandler<FakeGet> {
        let mut h = WebResearchHandler::with_parts(
            Url::parse("https://searx.example.org/search").unwrap(),
            al(&["searx.example.org", "docs.example.org"]),
            FakeGet::new(responses),
        );
        h.embedder = embedder;
        h
    }

    #[test]
    fn lexical_result_reports_ranking_lexical() {
        let page = "bwrap creates user namespaces.";
        let mut h = handler(vec![
            json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
            RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                body: page.as_bytes().to_vec() },
        ]);
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["ranking"], "lexical");
        assert!(out.get("embed_note").is_none() || out["embed_note"].is_null());
    }

    #[test]
    fn hybrid_result_reports_ranking_hybrid() {
        use crate::embed::FakeEmbedder;
        let page = "bwrap creates user namespaces.";
        let emb = FakeEmbedder::new(&[
            ("bwrap user namespaces", vec![1.0_f32, 0.0]),
            ("bwrap creates user namespaces.", vec![1.0_f32, 0.0]),
        ]);
        let mut h = handler_with_embedder(
            vec![
                json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
                RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                    body: page.as_bytes().to_vec() },
            ],
            Some(Box::new(emb)),
        );
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["ranking"], "hybrid");
    }

    #[test]
    fn degraded_result_carries_embed_note() {
        use crate::embed::FakeEmbedder;
        let page = "bwrap creates user namespaces.";
        let mut h = handler_with_embedder(
            vec![
                json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
                RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                    body: page.as_bytes().to_vec() },
            ],
            Some(Box::new(FakeEmbedder::failing())),
        );
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["ranking"], "lexical");
        assert!(out["embed_note"].as_str().unwrap().contains("embed"));
    }
```

- [ ] **Step 2: Run, expect FAIL** (`embedder` field missing, JSON keys absent):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research --lib handler -- --nocapture
```
Expected: compile error — no `embedder` field.

- [ ] **Step 3: Add the `embedder` field + env wiring** — in `handler.rs`:
  - imports: `use crate::embed::{Embedder, HttpEmbedder};`
  - struct:
    ```rust
    pub struct WebResearchHandler<T: HttpGet> {
        endpoint: Url,
        allowlist: HostAllowlist,
        transport: T,
        embedder: Option<Box<dyn Embedder>>,
    }
    ```
  - `with_parts` (test ctor): set `embedder: None`.
  - `from_env`: after building `transport`, read the embed endpoint and build the embedder:
    ```rust
    let embedder = match std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT") {
        Ok(raw) if !raw.trim().is_empty() => {
            // The embed endpoint host must be on the same allowlist (fail closed
            // if the operator forgot to allow it).
            let embed_endpoint = validate_endpoint(&raw, &allowlist)
                .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
            let model = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_MODEL")
                .unwrap_or_else(|_| "embeddinggemma".to_string());
            let embed_transport = make_get("kastellan-web-research/0")?;
            let e: Box<dyn Embedder> =
                Box::new(HttpEmbedder::new(embed_transport, embed_endpoint, model));
            Some(e)
        }
        _ => None,
    };
    Ok(Self { endpoint, allowlist, transport, embedder })
    ```

- [ ] **Step 4: Pass the embedder into `research` + surface the signal** — in `call`:
  ```rust
  let out = research(
      &self.transport, &self.endpoint, &self.allowlist,
      self.embedder.as_deref(), &p.query, max_sources, max_passages,
  ).map_err(research_err_to_rpc)?;
  Ok(outcome_to_json(&p.query, out))
  ```
  and extend `outcome_to_json` to include the new fields (build the base object mutably so `embed_note` is only added when present):
  ```rust
  let ranking = match out.ranking {
      crate::research::RankMode::Hybrid => "hybrid",
      crate::research::RankMode::Lexical => "lexical",
  };
  let mut obj = json!({
      "query": query,
      "sources": sources,
      "unfetched": unfetched,
      "sources_fetched": out.sources.len(),
      "passage_count": passage_count,
      "ranking": ranking,
  });
  if let Some(note) = &out.embed_note {
      obj["embed_note"] = json!(note);
  }
  obj
  ```

- [ ] **Step 5: Run, expect PASS**:

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-worker-web-research
cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings
```
Expected: all web-research tests PASS (existing handler tests still green — they get `"ranking":"lexical"` and no `embed_note`); clippy clean.

- [ ] **Step 6: Commit**

```sh
git add workers/web-research/src/handler.rs
git commit -m "feat(web-research): env-driven embedder + ranking/embed_note in the result

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Manifest — allowlist + env injection for the embed endpoint

**Files:**
- Modify: `core/src/workers/web_research.rs`

**Interfaces:**
- Consumes: existing `endpoint_net_entry`, `net_entries`, `web_research_entry`, `ENDPOINT_ENV`, `WebResearchManifest`.
- Produces: `EMBED_ENDPOINT_ENV`, `EMBED_MODEL_ENV`; a new `web_research_entry_with_embed(...)` (the 3-arg `web_research_entry` is kept as a thin `None,None` wrapper so its two `core/tests/web_research_e2e.rs` callers are untouched); the embed endpoint host:port unioned into `Net::Allowlist` and both env vars injected when the embed endpoint is set.

- [ ] **Step 1: Write the failing tests** — in `core/src/workers/web_research.rs`'s `#[cfg(test)] mod tests` (which already has `ctx(&get_env, &exists, &allowlist)` and `WebResearchManifest.resolve(&c)`), add the embed-set case and pin the unset case. Add to the existing `resolve_registers_union_net_and_injects_env` test (embed unset): `assert_eq!(entry.policy.env.len(), 2, "no embed env when endpoint unset");`. Then add:

```rust
    #[test]
    fn resolve_unions_embed_endpoint_into_net_and_injects_env() {
        let get_env = |k: &str| match k {
            BIN_ENV => Some("/opt/web-research".to_string()),
            ENDPOINT_ENV => Some("https://searx.example.org/search".to_string()),
            EMBED_ENDPOINT_ENV => Some("http://embed.example.org:11434/v1/embeddings".to_string()),
            _ => None, // EMBED_MODEL_ENV unset -> default model
        };
        let exists = |_p: &Path| true;
        let allowlist = |_t: &str| vec![
            "searx.example.org".to_string(),
            "embed.example.org".to_string(),
            ".docs.example.org".to_string(),
        ];
        let c = ctx(&get_env, &exists, &allowlist);
        match WebResearchManifest.resolve(&c) {
            Resolution::Register(entry) => {
                match &entry.policy.net {
                    Net::Allowlist(hosts) => {
                        assert!(hosts.iter().any(|h| h == "embed.example.org:11434"),
                            "embed host:port missing from net: {hosts:?}");
                    }
                    other => panic!("expected Net::Allowlist, got {other:?}"),
                }
                let has = |k: &str, v: &str| entry.policy.env.iter().any(|(ek, ev)| ek == k && ev == v);
                assert!(has(EMBED_ENDPOINT_ENV, "http://embed.example.org:11434/v1/embeddings"));
                assert!(has(EMBED_MODEL_ENV, "embeddinggemma"), "default model injected");
            }
            other => panic!("expected Register, got {}", outcome_label(&other)),
        }
    }
```

- [ ] **Step 2: Run, expect FAIL** (`EMBED_ENDPOINT_ENV` undefined / embed host absent / env.len() != 2):

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib workers::web_research -- --nocapture
```
Expected: FAIL.

- [ ] **Step 3: Implement** — in `core/src/workers/web_research.rs`:
  - add consts after `ENDPOINT_ENV`:
    ```rust
    const EMBED_ENDPOINT_ENV: &str = "KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT";
    const EMBED_MODEL_ENV: &str = "KASTELLAN_WEB_RESEARCH_EMBED_MODEL";
    const DEFAULT_EMBED_MODEL: &str = "embeddinggemma";
    ```
  - generalize `net_entries` to also union the embed host:port (endpoint first, embed second, then content, de-duped):
    ```rust
    fn net_entries(endpoint: &str, embed_endpoint: Option<&str>, allowlist: &[String]) -> Vec<String> {
        let mut entries = endpoint_net_entry(endpoint);
        if let Some(embed) = embed_endpoint {
            for e in endpoint_net_entry(embed) {
                if !entries.contains(&e) { entries.push(e); }
            }
        }
        for e in crate::workers::web_fetch::allowlist_to_net_entries(allowlist) {
            if !entries.contains(&e) { entries.push(e); }
        }
        entries
    }
    ```
  - add `web_research_entry_with_embed` (the real builder) and keep `web_research_entry` as a wrapper:
    ```rust
    pub fn web_research_entry(binary: PathBuf, endpoint: &str, allowlist: &[String]) -> ToolEntry {
        web_research_entry_with_embed(binary, endpoint, None, None, allowlist)
    }

    pub fn web_research_entry_with_embed(
        binary: PathBuf,
        endpoint: &str,
        embed_endpoint: Option<&str>,
        embed_model: Option<&str>,
        allowlist: &[String],
    ) -> ToolEntry {
        let allow_json = serde_json::to_string(allowlist).expect("serializing Vec<String> never fails");
        let mut env = vec![
            (ENDPOINT_ENV.to_string(), endpoint.to_string()),
            ("KASTELLAN_WEB_RESEARCH_ALLOWLIST".to_string(), allow_json),
        ];
        if let Some(embed) = embed_endpoint {
            env.push((EMBED_ENDPOINT_ENV.to_string(), embed.to_string()));
            env.push((EMBED_MODEL_ENV.to_string(),
                embed_model.unwrap_or(DEFAULT_EMBED_MODEL).to_string()));
        }
        let policy = SandboxPolicy {
            // ... unchanged fs_read/fs_write/cpu_ms/mem_mb/profile/quota/... ...
            net: Net::Allowlist(net_entries(endpoint, embed_endpoint, allowlist)),
            env,
            // ... rest unchanged ...
        };
        ToolEntry { /* unchanged */ }
    }
    ```
    (Move the existing `SandboxPolicy`/`ToolEntry` construction into `_with_embed`, swapping the `env`/`net` lines for the above; `web_research_entry` becomes the two-line wrapper.)
  - in `resolve`, read the embed env and pass it through:
    ```rust
    let embed_endpoint = (ctx.get_env)(EMBED_ENDPOINT_ENV).filter(|s| !s.trim().is_empty());
    let embed_model = (ctx.get_env)(EMBED_MODEL_ENV).filter(|s| !s.trim().is_empty());
    Resolution::Register(web_research_entry_with_embed(
        binary, &endpoint, embed_endpoint.as_deref(), embed_model.as_deref(), &allowlist,
    ))
    ```

- [ ] **Step 4: Run, expect PASS**:

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib workers::web_research
cargo clippy -p kastellan-core --lib --all-targets -- -D warnings
```
Expected: manifest tests PASS (incl. the unchanged "unset" test, now pinning `env.len() == 2`); clippy clean. The two `core/tests/web_research_e2e.rs` callers of `web_research_entry` still compile (unchanged 3-arg wrapper).

- [ ] **Step 5: Commit**

```sh
git add core/src/workers/web_research.rs
git commit -m "feat(core): web-research manifest unions the embed endpoint into egress + env

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: ROADMAP tick + full verification

**Files:**
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Tick the Slice-4 deferral** — in `docs/devel/ROADMAP.md`, find the `web-research` line's "**Deferred (Slice 4):** EmbeddingRanker+HybridRanker(RRF) …" and mark that item done (strike or move to done), e.g. append: "EmbeddingRanker+HybridRanker(RRF) DONE 2026-07-08 (opt-in `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT`, hybrid default, degrade-to-lexical-with-signal); live embed e2e still deferred."

- [ ] **Step 2: Full build + clippy + targeted tests** (foreground):

```sh
source "$HOME/.cargo/env"
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p kastellan-worker-web-common
cargo test -p kastellan-worker-web-research
cargo test -p kastellan-worker-web-search
cargo test -p kastellan-worker-web-fetch
cargo test -p kastellan-core --lib workers::web_research
```
Expected: workspace builds; clippy clean; all listed suites green. (`kastellan-worker-web-research` binary present under `target/debug/` so manifest sibling-discovery resolves it.)

- [ ] **Step 3: Confirm the no-endpoint path is unchanged** — the pre-existing web-research handler/research tests all pass unmodified in shape (only the `research(...)` call gained a `None` arg and new lexical-mode assertions). This is the backward-compat gate.

- [ ] **Step 4: Commit**

```sh
git add docs/devel/ROADMAP.md
git commit -m "docs(roadmap): tick web-research EmbeddingRanker+HybridRanker (Slice 4)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Verification (whole feature)

- Mac (Seatbelt, rustc 1.96): `cargo build --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` clean; web-common / web-research / web-search / web-fetch / core-lib `workers::web_research` suites green.
- No PG / DGX / sandbox-behaviour surface is touched, so the DGX `2270/0/34` baseline carries forward.
- **Deferred:** a live `#[ignore]` e2e against a real embedding endpoint (needs a running embed server) — file as the next follow-up, mirroring how the SearxNG live e2e was staged after the composite worker.

## Post-implementation

- `/review` (or `superpowers:requesting-code-review`) on the branch, `/fixall` any findings.
- Update `docs/devel/handovers/HANDOVER.md` (header + a "Recently completed" entry) and open a PR linking the spec.
