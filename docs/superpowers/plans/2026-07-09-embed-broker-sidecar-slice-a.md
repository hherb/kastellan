# Embedding Broker Sidecar — Slice A Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the two ends of a trusted embedding-broker pipe — a new sandboxed `embed-broker` sidecar that forwards JSON-RPC `embed` requests to the operator's embedding backend, and a `BrokeredEmbedder` in `web-research` that reaches it over a UDS — fully hermetic, no sandbox/core plumbing yet.

**Architecture:** The broker is a `workers/` sidecar (like `egress-proxy`, NOT a `tool_host` stdio worker). It serves a line-delimited JSON-RPC `embed{model,input}` method over a Unix socket by reusing the transport-generic `kastellan_protocol::serve`, and forwards each request as an OpenAI-compatible POST to the backend via `kastellan_worker_web_common::http::HttpGet` (fake-able in tests). `web-research` gains a `BrokeredEmbedder` behind its existing `Embedder` seam, selected by a new pure `choose_embedder` function when `KASTELLAN_EMBED_BROKER_UDS` is set.

**Tech Stack:** Rust (edition/rust-version from workspace, rustc 1.96), `kastellan-protocol` (JSON-RPC codec), `kastellan-worker-web-common` (`HttpGet`/`make_get`/`FakeGet`), `kastellan-worker-prelude` (`lock_down`), `std::os::unix::net::UnixStream/UnixListener`, `tempfile` (dev).

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible deps only** (Apache-2.0/MIT/BSD/MPL/LGPL/(A)GPL). No new non-compatible deps. `tempfile` (MIT OR Apache-2.0) is the only new dev-dep.
- **Cross-platform Linux + macOS.** `UnixStream`/`UnixListener` are unix (both targets). No Linux-only code in Slice A.
- **Rust core, Python only inside sandboxed workers.** No PyO3.
- **Files under ~500 LOC**; prefer pure functions in reusable modules (project rule 1).
- **TDD**: write the failing test first, watch it fail, implement minimally, watch it pass, commit (project rule 2 + 6: all tests pass before committing).
- **Inline docs understandable to a junior contributor are mandatory** (project rule 3) — every new public item gets a doc comment.
- **Build/test invocation:** `source "$HOME/.cargo/env"` first (cargo is not on the non-interactive PATH). Run cargo in the FOREGROUND (no background jobs).
- **Verification gate (Mac, Seatbelt, rustc 1.96):** `cargo build --workspace` exit 0; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test -p kastellan-worker-embed-broker` and `cargo test -p kastellan-worker-web-research` green. No PG/sandbox/DGX surface → the DGX 2369/0/39 baseline carries forward.
- **Commit hygiene:** `git add <specific files>` only — never `git add -A` (untracked scratch files must stay out). End commit messages with the `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` trailer.

---

### Task 1: Scaffold the `embed-broker` crate + wire request/result types

**Files:**
- Create: `workers/embed-broker/Cargo.toml`
- Create: `workers/embed-broker/src/lib.rs`
- Create: `workers/embed-broker/src/main.rs` (temporary stub; fleshed out in Task 5)
- Modify: `Cargo.toml` (workspace root — add the member)

**Interfaces:**
- Produces:
  - `pub const MAX_INPUTS: usize = 256;`
  - `pub const MAX_REQUEST_BYTES: usize = 1_000_000;`
  - `pub struct EmbedParams { pub model: String, pub input: Vec<String> }` (Deserialize)
  - `pub struct EmbedData { pub index: usize, pub embedding: Vec<f32> }` (Serialize)
  - `pub struct EmbedResult { pub data: Vec<EmbedData> }` (Serialize)

- [ ] **Step 1: Register the crate in the workspace**

Edit the root `Cargo.toml` `members = [ ... ]` list — add `"workers/embed-broker",` immediately after the `"workers/egress-proxy",` line (keep the workers grouped).

- [ ] **Step 2: Write `workers/embed-broker/Cargo.toml`**

```toml
[package]
name        = "kastellan-worker-embed-broker"
description = "Trusted embedding broker sidecar: bridges a jailed worker's UDS to the operator's embedding backend (text -> vector), so the worker needs no embed egress."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../../README.md"

[[bin]]
name = "kastellan-worker-embed-broker"
path = "src/main.rs"

[dependencies]
kastellan-protocol          = { path = "../../protocol", version = "0.1.0" }
kastellan-worker-prelude    = { path = "../prelude", version = "0.1.0" }
kastellan-worker-web-common = { path = "../web-common", version = "0.1.0" }
serde      = { workspace = true }
serde_json = { workspace = true }
anyhow     = { workspace = true }
url        = { workspace = true }

[dev-dependencies]
kastellan-worker-web-common = { path = "../web-common", features = ["testing"] }
tempfile = "3"
```

- [ ] **Step 3: Write the failing test in `workers/embed-broker/src/lib.rs`**

Create the file with the module doc, the type declarations' *test* first will not compile without the types — so add the types AND the test together but keep the test asserting behaviour. Write ONLY this (types + test) for now:

```rust
//! Trusted embedding broker sidecar.
//!
//! A jailed worker cannot reach the operator's embedding backend directly (no
//! egress). Instead it talks JSON-RPC `embed{model,input}` to this broker over a
//! Unix socket that core bind-mounts into its jail; the broker forwards the
//! request to the backend as an OpenAI-compatible POST and returns the vectors.
//! All OpenAI-compat coupling lives here, in one place.

use serde::{Deserialize, Serialize};

/// Max passages accepted in one `embed` batch (defense-in-depth; bounds the
/// backend POST). Web-research caps per-page embeds at 128, so 256 leaves margin.
pub const MAX_INPUTS: usize = 256;

/// Max total input-text bytes accepted in one `embed` batch (fail-closed above).
pub const MAX_REQUEST_BYTES: usize = 1_000_000;

/// The `embed` method params, parsed from the JSON-RPC request.
#[derive(Debug, Deserialize)]
pub struct EmbedParams {
    /// The embedding model name (forwarded verbatim to the backend).
    pub model: String,
    /// The texts to embed, one vector returned per input.
    pub input: Vec<String>,
}

/// One row of the `embed` result: a vector at its input position.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EmbedData {
    /// Position in the request's `input` array (0-based).
    pub index: usize,
    /// The embedding vector.
    pub embedding: Vec<f32>,
}

/// The `embed` method result: one [`EmbedData`] per input, in input order.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EmbedResult {
    pub data: Vec<EmbedData>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_params_parse_from_json() {
        let v = serde_json::json!({ "model": "m", "input": ["a", "b"] });
        let p: EmbedParams = serde_json::from_value(v).unwrap();
        assert_eq!(p.model, "m");
        assert_eq!(p.input, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn embed_result_serializes_index_and_embedding() {
        let r = EmbedResult { data: vec![EmbedData { index: 0, embedding: vec![1.0, 2.0] }] };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v, serde_json::json!({ "data": [{ "index": 0, "embedding": [1.0, 2.0] }] }));
    }
}
```

- [ ] **Step 4: Write the temporary `main.rs` stub so the crate builds**

```rust
//! Binary entry point — fleshed out in Task 5.
fn main() -> anyhow::Result<()> {
    Ok(())
}
```

- [ ] **Step 5: Run the tests — verify they pass (types now exist)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker`
Expected: PASS (2 tests). (There is nothing to "fail-first" here beyond compilation; the meaningful red/green cycles start in Task 2.)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml workers/embed-broker/Cargo.toml workers/embed-broker/src/lib.rs workers/embed-broker/src/main.rs
git commit -m "feat(embed-broker): scaffold crate + embed wire types

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `forward_embed` — OpenAI-compat forwarding (reorder + count-check + error mapping)

**Files:**
- Modify: `workers/embed-broker/src/lib.rs`

**Interfaces:**
- Consumes: `EmbedParams`, `EmbedResult`, `EmbedData` (Task 1); `kastellan_worker_web_common::http::{HttpGet, RawResponse}`; `kastellan_protocol::{RpcError, codes}`; `url::Url`.
- Produces: `pub fn forward_embed<T: HttpGet>(transport: &T, endpoint: &Url, params: &EmbedParams) -> Result<EmbedResult, kastellan_protocol::RpcError>`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `workers/embed-broker/src/lib.rs`:

```rust
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::FakeGet;
    use url::Url;

    fn endpoint() -> Url { Url::parse("http://127.0.0.1:11434/v1/embeddings").unwrap() }

    fn ok_body(rows: &[(usize, &[f32])]) -> Vec<u8> {
        let data: Vec<String> = rows.iter().map(|(i, v)| {
            let nums: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            format!(r#"{{"index":{i},"embedding":[{}]}}"#, nums.join(","))
        }).collect();
        format!(r#"{{"data":[{}]}}"#, data.join(",")).into_bytes()
    }

    fn resp(status: u16, body: Vec<u8>) -> RawResponse {
        RawResponse { status, location: None, content_type: "application/json".into(), body }
    }

    fn params(model: &str, input: &[&str]) -> EmbedParams {
        EmbedParams { model: model.into(), input: input.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn forward_returns_vectors_in_input_order() {
        let t = FakeGet::new(vec![resp(200, ok_body(&[(0, &[1.0, 2.0]), (1, &[3.0, 4.0])]))]);
        let out = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap();
        assert_eq!(out, EmbedResult { data: vec![
            EmbedData { index: 0, embedding: vec![1.0, 2.0] },
            EmbedData { index: 1, embedding: vec![3.0, 4.0] },
        ]});
    }

    #[test]
    fn forward_reorders_out_of_order_backend_rows() {
        // Backend returns index:1 first, index:0 second — result must be input-ordered.
        let t = FakeGet::new(vec![resp(200, ok_body(&[(1, &[3.0, 4.0]), (0, &[1.0, 2.0])]))]);
        let out = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap();
        assert_eq!(out.data[0].embedding, vec![1.0, 2.0]);
        assert_eq!(out.data[1].embedding, vec![3.0, 4.0]);
    }

    #[test]
    fn forward_count_mismatch_is_error() {
        let t = FakeGet::new(vec![resp(200, ok_body(&[(0, &[1.0])]))]); // 1 row for 2 inputs
        let err = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_non_2xx_is_error() {
        let t = FakeGet::new(vec![resp(503, b"upstream down".to_vec())]);
        let err = forward_embed(&t, &endpoint(), &params("m", &["a"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_transport_failure_is_error() {
        let t = FakeGet::new(vec![]); // empty queue -> post() errors "no more canned responses"
        let err = forward_embed(&t, &endpoint(), &params("m", &["a"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_empty_input_makes_no_call() {
        let t = FakeGet::new(vec![]); // would error if called
        let out = forward_embed(&t, &endpoint(), &params("m", &[])).unwrap();
        assert!(out.data.is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker`
Expected: FAIL to compile — `forward_embed` not found.

- [ ] **Step 3: Implement `forward_embed`**

Add above the `tests` module in `workers/embed-broker/src/lib.rs`:

```rust
use kastellan_protocol::{codes, RpcError};
use kastellan_worker_web_common::http::HttpGet;
use url::Url;

/// The OpenAI-compatible request body sent to the backend.
#[derive(Serialize)]
struct BackendReq<'a> {
    model: &'a str,
    input: &'a [String],
}

/// One row of the backend's OpenAI-compatible response.
#[derive(Deserialize)]
struct BackendRow {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// The backend's OpenAI-compatible response envelope.
#[derive(Deserialize)]
struct BackendResp {
    data: Vec<BackendRow>,
}

/// Forward one `embed` request to the backend and normalise the response.
///
/// POSTs `{model, input}` (OpenAI-compatible) to `endpoint` over `transport`,
/// decodes `{data:[{index,embedding}]}`, reorders rows by `index` so each vector
/// pairs with its input position, and count-checks (one vector per input). Any
/// transport error, non-2xx status, decode failure, or count mismatch becomes an
/// `OPERATION_FAILED` [`RpcError`] — the broker never partially succeeds.
pub fn forward_embed<T: HttpGet>(
    transport: &T,
    endpoint: &Url,
    params: &EmbedParams,
) -> Result<EmbedResult, RpcError> {
    if params.input.is_empty() {
        return Ok(EmbedResult { data: Vec::new() });
    }
    let body = serde_json::to_vec(&BackendReq { model: &params.model, input: &params.input })
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("request encode: {e}")))?;
    let resp = transport
        .post(endpoint, "application/json", &body)
        .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("backend transport: {e}")))?;
    if !(200..300).contains(&resp.status) {
        return Err(RpcError::new(
            codes::OPERATION_FAILED,
            format!("backend status {}", resp.status),
        ));
    }
    let decoded: BackendResp = serde_json::from_slice(&resp.body)
        .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("backend decode: {e}")))?;
    if decoded.data.len() != params.input.len() {
        return Err(RpcError::new(
            codes::OPERATION_FAILED,
            format!(
                "vector count mismatch: requested {}, returned {}",
                params.input.len(),
                decoded.data.len()
            ),
        ));
    }
    let mut rows = decoded.data;
    rows.sort_by_key(|d| d.index);
    let data = rows
        .into_iter()
        .enumerate()
        .map(|(i, d)| EmbedData { index: i, embedding: d.embedding })
        .collect();
    Ok(EmbedResult { data })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker`
Expected: PASS (all forward_* tests + Task 1 tests).

- [ ] **Step 5: Commit**

```bash
git add workers/embed-broker/src/lib.rs
git commit -m "feat(embed-broker): forward_embed OpenAI-compat forwarding

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `EmbedHandler` implementing `protocol::Handler` + input caps

**Files:**
- Modify: `workers/embed-broker/src/lib.rs`

**Interfaces:**
- Consumes: `forward_embed`, `EmbedParams`, `MAX_INPUTS`, `MAX_REQUEST_BYTES` (Tasks 1-2); `kastellan_protocol::{Handler, RpcError, codes}`.
- Produces: `pub struct EmbedHandler<T: HttpGet>` with `pub fn new(transport: T, endpoint: Url) -> Self`, `impl<T: HttpGet> kastellan_protocol::Handler for EmbedHandler<T>`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module:

```rust
    use kastellan_protocol::Handler;

    fn handler(responses: Vec<RawResponse>) -> EmbedHandler<FakeGet> {
        EmbedHandler::new(FakeGet::new(responses), endpoint())
    }

    #[test]
    fn call_embed_returns_result_value() {
        let mut h = handler(vec![resp(200, ok_body(&[(0, &[1.0, 2.0])]))]);
        let out = h.call("embed", serde_json::json!({ "model": "m", "input": ["a"] })).unwrap();
        assert_eq!(out, serde_json::json!({ "data": [{ "index": 0, "embedding": [1.0, 2.0] }] }));
    }

    #[test]
    fn call_unknown_method_is_method_not_found() {
        let mut h = handler(vec![]);
        let err = h.call("bogus", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn call_bad_params_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("embed", serde_json::json!({ "model": 5 })).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::INVALID_PARAMS);
    }

    #[test]
    fn call_too_many_inputs_rejected_before_backend() {
        // Empty response queue: if the cap did NOT fire first, forward_embed would
        // error OPERATION_FAILED. Asserting INVALID_PARAMS proves the cap fired
        // before any backend call.
        let big: Vec<String> = (0..(MAX_INPUTS + 1)).map(|i| i.to_string()).collect();
        let mut h = handler(vec![]);
        let err = h.call("embed", serde_json::json!({ "model": "m", "input": big })).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::INVALID_PARAMS);
    }

    #[test]
    fn call_oversized_request_rejected_before_backend() {
        let huge = "x".repeat(MAX_REQUEST_BYTES + 1);
        let mut h = handler(vec![]);
        let err = h.call("embed", serde_json::json!({ "model": "m", "input": [huge] })).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::INVALID_PARAMS);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker`
Expected: FAIL to compile — `EmbedHandler` not found.

- [ ] **Step 3: Implement `EmbedHandler`**

Add above the `tests` module (after `forward_embed`):

```rust
use kastellan_protocol::Handler;

/// JSON-RPC handler for the broker's single `embed` method.
///
/// Enforces the batch caps ([`MAX_INPUTS`], [`MAX_REQUEST_BYTES`]) fail-closed
/// *before* any backend call, then delegates to [`forward_embed`]. Generic over
/// the transport so tests inject a `FakeGet`.
pub struct EmbedHandler<T: HttpGet> {
    transport: T,
    endpoint: Url,
}

impl<T: HttpGet> EmbedHandler<T> {
    /// Build a handler that forwards `embed` calls to `endpoint` over `transport`.
    pub fn new(transport: T, endpoint: Url) -> Self {
        Self { transport, endpoint }
    }
}

impl<T: HttpGet> Handler for EmbedHandler<T> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if method != "embed" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method: {method}"),
            ));
        }
        let p: EmbedParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("params: {e}")))?;
        if p.input.len() > MAX_INPUTS {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!("too many inputs: {} > {}", p.input.len(), MAX_INPUTS),
            ));
        }
        let total: usize = p.input.iter().map(|s| s.len()).sum();
        if total > MAX_REQUEST_BYTES {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!("request too large: {total} > {MAX_REQUEST_BYTES} bytes"),
            ));
        }
        let result = forward_embed(&self.transport, &self.endpoint, &p)?;
        serde_json::to_value(result)
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("result encode: {e}")))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add workers/embed-broker/src/lib.rs
git commit -m "feat(embed-broker): EmbedHandler + fail-closed input caps

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `serve_connection` over a UnixStream + UDS round-trip test

**Files:**
- Modify: `workers/embed-broker/src/lib.rs`

**Interfaces:**
- Consumes: `EmbedHandler` (Task 3); `kastellan_protocol::server::serve` (NOT re-exported at the crate root — `Handler`/`serve` live in the `server` module, as `web-research`/`web-fetch` import them); `std::os::unix::net::UnixStream`.
- Produces: `pub fn serve_connection<T: HttpGet>(handler: &mut EmbedHandler<T>, stream: std::os::unix::net::UnixStream) -> std::io::Result<()>`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
    use std::io::{BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Drive the broker's serve loop over a real UDS with a fake backend, from a
    /// client that speaks the JSON-RPC `embed` protocol. Proves the on-wire path
    /// end to end (framing + dispatch + response), not just the in-process call.
    #[test]
    fn uds_round_trip_embeds_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("embed.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Broker side: accept ONE connection, serve it with a fake backend.
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut h = EmbedHandler::new(
                FakeGet::new(vec![resp(200, ok_body(&[(0, &[7.0, 8.0])]))]),
                endpoint(),
            );
            serve_connection(&mut h, conn).unwrap();
        });

        // Client side: send one embed request, read the response.
        let mut client = UnixStream::connect(&sock).unwrap();
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "embed",
            "params": { "model": "m", "input": ["hello"] }
        });
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        client.write_all(&line).unwrap();
        client.flush().unwrap();

        let mut br = BufReader::new(&client);
        let rec = kastellan_protocol::read_capped_record(&mut br, kastellan_protocol::MAX_RECORD_BYTES).unwrap();
        let buf = match rec {
            kastellan_protocol::Record::Line(b) => b,
            other => panic!("expected a response line, got {other:?}"),
        };
        let resp: kastellan_protocol::Response = serde_json::from_slice(&buf).unwrap();
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(
            resp.result.unwrap(),
            serde_json::json!({ "data": [{ "index": 0, "embedding": [7.0, 8.0] }] })
        );

        drop(client); // let the serve loop see EOF and return
        server.join().unwrap();
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker uds_round_trip`
Expected: FAIL to compile — `serve_connection` not found.

- [ ] **Step 3: Implement `serve_connection`**

Add above the `tests` module:

```rust
use std::os::unix::net::UnixStream;

/// Serve one accepted UDS connection: run the JSON-RPC loop until the peer
/// closes the socket (EOF). Reuses the transport-generic
/// [`kastellan_protocol::serve`] over the two cloned halves of the stream.
///
/// A client connects, sends one or more `embed` requests, and reads each
/// response; when it drops the socket the loop returns `Ok`.
pub fn serve_connection<T: HttpGet>(
    handler: &mut EmbedHandler<T>,
    stream: UnixStream,
) -> std::io::Result<()> {
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    kastellan_protocol::server::serve(handler, &mut reader, &mut writer)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker`
Expected: PASS (all broker tests).

- [ ] **Step 5: Commit**

```bash
git add workers/embed-broker/src/lib.rs
git commit -m "feat(embed-broker): serve_connection over a UDS (protocol::serve)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `main.rs` — bind UDS + lock_down + accept loop

**Files:**
- Modify: `workers/embed-broker/src/main.rs`

**Interfaces:**
- Consumes: `kastellan_worker_embed_broker::{EmbedHandler, serve_connection}` (the crate's own lib); `kastellan_worker_web_common::http::make_get`; `kastellan_worker_prelude::lock_down`; env `KASTELLAN_EMBED_BROKER_UDS`, `KASTELLAN_EMBED_BROKER_ENDPOINT`.
- Produces: the runnable `kastellan-worker-embed-broker` binary (no unit test; exercised by the Slice C e2e — build + clippy are the Slice A gate).

- [ ] **Step 1: Replace the stub `main.rs`**

```rust
//! Embedding broker sidecar binary.
//!
//! Spawned by core (Slice B) like the egress proxy: it binds its UDS, applies
//! the worker-prelude lockdown, then serves JSON-RPC `embed` requests over the
//! socket, forwarding each to the operator's embedding backend. Two env vars:
//! `KASTELLAN_EMBED_BROKER_UDS` (socket path) and `KASTELLAN_EMBED_BROKER_ENDPOINT`
//! (the backend's OpenAI-compatible embeddings URL).

use std::os::unix::net::UnixListener;

use kastellan_worker_embed_broker::{serve_connection, EmbedHandler};

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("KASTELLAN_EMBED_BROKER_UDS")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EMBED_BROKER_UDS unset"))?;
    let endpoint_raw = std::env::var("KASTELLAN_EMBED_BROKER_ENDPOINT")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EMBED_BROKER_ENDPOINT unset"))?;
    let endpoint = url::Url::parse(&endpoint_raw)
        .map_err(|e| anyhow::anyhow!("KASTELLAN_EMBED_BROKER_ENDPOINT is not a URL: {e}"))?;

    // The backend transport: direct (loopback Ollama/vLLM) in v1. `make_get`
    // returns a proxy-connect transport only if KASTELLAN_EGRESS_PROXY_UDS is set
    // (a remote backend force-routed through the egress proxy — out of scope here).
    let transport = kastellan_worker_web_common::http::make_get("kastellan-embed-broker/0")?;

    // Bind the UDS BEFORE lock-down (Landlock forbids fs mutation after) — the
    // same ordering the egress proxy uses.
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS, where
    // the parent Seatbelt profile contains us). The net_client profile must permit
    // AF_UNIX accept + AF_INET connect (serve + dial) — verified on the DGX in Slice B.
    let _report = kastellan_worker_prelude::lock_down()?;

    let mut handler = EmbedHandler::new(transport, endpoint);
    // Connections are handled serially: one web-research worker per broker, and
    // its embeds are sequential. Each connection runs to EOF, then the next is
    // accepted. (Thread-per-connection can come with a second consumer.)
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if let Err(e) = serve_connection(&mut handler, conn) {
            eprintln!("embed-broker: connection error: {e}");
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Build + clippy (the gate for this task)**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-embed-broker && cargo clippy -p kastellan-worker-embed-broker --all-targets -- -D warnings`
Expected: both exit 0, no warnings. (`make_get` returns `Box<dyn HttpGet>`, which implements `HttpGet` via the blanket impl, so `EmbedHandler<Box<dyn HttpGet>>` is valid.)

- [ ] **Step 3: Commit**

```bash
git add workers/embed-broker/src/main.rs
git commit -m "feat(embed-broker): binary entry point (bind UDS + lock_down + serve)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: `BrokeredEmbedder` in web-research + UDS round-trip test

**Files:**
- Modify: `workers/web-research/src/embed.rs`
- Modify: `workers/web-research/Cargo.toml` (add `tempfile` dev-dep)

**Interfaces:**
- Consumes: `Embedder`, `EmbedError` (existing seam in `embed.rs`); `kastellan_protocol::{Request, Response, Record, read_capped_record, MAX_RECORD_BYTES}`; `std::os::unix::net::UnixStream`.
- Produces: `pub struct BrokeredEmbedder` with `pub fn new(uds: std::path::PathBuf, model: String) -> Self`, `impl Embedder for BrokeredEmbedder`.

- [ ] **Step 1: Add the `tempfile` dev-dep**

In `workers/web-research/Cargo.toml`, under `[dev-dependencies]`, add:

```toml
tempfile = "3"
```

- [ ] **Step 2: Write the failing tests**

Add to the `tests` module in `workers/web-research/src/embed.rs` (a stub broker that speaks the JSON-RPC `embed` protocol, so the test exercises the CLIENT independently of the broker crate):

```rust
    use std::io::{BufReader as StdBufReader, Write as StdWrite};
    use std::os::unix::net::UnixListener;

    /// Spawn a one-shot stub broker on `sock` that reads one request line and
    /// writes `response_json` back. Returns the join handle.
    fn stub_broker(sock: std::path::PathBuf, response_json: String) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            // Drain the request line (we don't assert on it here).
            let mut br = StdBufReader::new(conn.try_clone().unwrap());
            let _ = kastellan_protocol::read_capped_record(&mut br, 1_000_000).unwrap();
            conn.write_all(response_json.as_bytes()).unwrap();
            conn.write_all(b"\n").unwrap();
            conn.flush().unwrap();
        })
    }

    #[test]
    fn brokered_embedder_round_trip_returns_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("embed.sock");
        // Single line: the JSON-RPC framing is line-delimited (`read_capped_record`
        // reads to the first `\n`), so the response must NOT contain embedded newlines.
        let h = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"data":[{"index":1,"embedding":[3.0,4.0]},{"index":0,"embedding":[1.0,2.0]}]}}"#.to_string(),
        );
        let e = BrokeredEmbedder::new(sock, "m".into());
        let out = e.embed(&["a".into(), "b".into()]).unwrap();
        // Reordered by index back to input order.
        assert_eq!(out, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        h.join().unwrap();
    }

    #[test]
    fn brokered_embedder_maps_broker_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("embed.sock");
        let h = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"backend down"}}"#.to_string(),
        );
        let e = BrokeredEmbedder::new(sock, "m".into());
        let err = e.embed(&["a".into()]).unwrap_err();
        assert!(matches!(err, EmbedError::Transport(_)), "got {err:?}");
        h.join().unwrap();
    }

    #[test]
    fn brokered_embedder_absent_socket_is_transport_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nope.sock"); // never bound
        let e = BrokeredEmbedder::new(sock, "m".into());
        let err = e.embed(&["a".into()]).unwrap_err();
        assert!(matches!(err, EmbedError::Transport(_)), "got {err:?}");
    }

    #[test]
    fn brokered_embedder_empty_input_makes_no_connection() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nope.sock"); // never bound; must not be dialed
        let e = BrokeredEmbedder::new(sock, "m".into());
        assert!(e.embed(&[]).unwrap().is_empty());
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-research brokered_embedder`
Expected: FAIL to compile — `BrokeredEmbedder` not found.

- [ ] **Step 4: Implement `BrokeredEmbedder`**

Add to `workers/web-research/src/embed.rs` (after `HttpEmbedder`'s impl, before the `#[cfg(test)]` items). Add the needed imports at the top of the file (`use std::io::{BufReader, Write};`, `use std::os::unix::net::UnixStream;`, `use std::path::PathBuf;`):

```rust
/// The broker's `embed` result envelope (mirrors `kastellan-worker-embed-broker`
/// `EmbedResult`). Kept local so web-research does not depend on the broker crate.
#[derive(serde::Deserialize)]
struct BrokerEmbedRow {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

#[derive(serde::Deserialize)]
struct BrokerEmbedResult {
    data: Vec<BrokerEmbedRow>,
}

/// Embed via the trusted embedding-broker sidecar over a Unix socket.
///
/// Sends a JSON-RPC `embed{model,input}` request to the broker (whose UDS core
/// bind-mounts into this worker's jail) and decodes the returned vectors. The
/// worker needs no embed egress — the broker holds the only route to the backend.
/// Selected by [`WebResearchHandler::from_env`] when `KASTELLAN_EMBED_BROKER_UDS`
/// is set.
pub struct BrokeredEmbedder {
    uds: PathBuf,
    model: String,
}

impl BrokeredEmbedder {
    /// Build an embedder that talks to the broker at `uds`, requesting `model`.
    pub fn new(uds: PathBuf, model: String) -> Self {
        Self { uds, model }
    }
}

impl Embedder for BrokeredEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut stream = UnixStream::connect(&self.uds)
            .map_err(|e| EmbedError::Transport(format!("connect broker {:?}: {e}", self.uds)))?;

        let req = kastellan_protocol::Request {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(1),
            method: "embed".into(),
            params: serde_json::json!({ "model": self.model, "input": texts }),
        };
        let mut line = serde_json::to_vec(&req)
            .map_err(|e| EmbedError::Decode(format!("request encode: {e}")))?;
        line.push(b'\n');
        stream
            .write_all(&line)
            .map_err(|e| EmbedError::Transport(format!("write broker request: {e}")))?;
        stream.flush().ok();

        let mut br = BufReader::new(&stream);
        let buf = match kastellan_protocol::read_capped_record(&mut br, kastellan_protocol::MAX_RECORD_BYTES)
            .map_err(|e| EmbedError::Transport(format!("read broker response: {e}")))?
        {
            kastellan_protocol::Record::Line(b) => b,
            kastellan_protocol::Record::Eof => {
                return Err(EmbedError::Transport("broker closed without responding".into()))
            }
            kastellan_protocol::Record::TooLarge => {
                return Err(EmbedError::Decode("broker response exceeded record cap".into()))
            }
        };
        let resp: kastellan_protocol::Response = serde_json::from_slice(&buf)
            .map_err(|e| EmbedError::Decode(format!("broker response: {e}")))?;
        if let Some(err) = resp.error {
            return Err(EmbedError::Transport(format!(
                "broker error {}: {}",
                err.code, err.message
            )));
        }
        let result = resp
            .result
            .ok_or_else(|| EmbedError::Decode("broker response missing result".into()))?;
        let decoded: BrokerEmbedResult = serde_json::from_value(result)
            .map_err(|e| EmbedError::Decode(format!("result decode: {e}")))?;
        if decoded.data.len() != texts.len() {
            return Err(EmbedError::CountMismatch {
                requested: texts.len(),
                returned: decoded.data.len(),
            });
        }
        let mut rows = decoded.data;
        rows.sort_by_key(|d| d.index);
        Ok(rows.into_iter().map(|d| d.embedding).collect())
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-research brokered_embedder`
Expected: PASS (4 brokered_embedder tests).

- [ ] **Step 6: Commit**

```bash
git add workers/web-research/Cargo.toml workers/web-research/src/embed.rs
git commit -m "feat(web-research): BrokeredEmbedder over the embed-broker UDS

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: `choose_embedder` selection (broker UDS precedence) + wire into `from_env`

**Files:**
- Modify: `workers/web-research/src/embed.rs` (add the pure `choose_embedder` + `EmbedderChoice`)
- Modify: `workers/web-research/src/handler.rs` (use it in `from_env`)

**Interfaces:**
- Consumes: `BrokeredEmbedder`, `HttpEmbedder` (Task 6 + existing); `validate_endpoint`, `make_get`, `search_err_to_rpc` (existing in handler.rs).
- Produces: `pub enum EmbedderChoice<'a> { None, Broker { uds: &'a str }, Endpoint { endpoint: &'a str } }` and `pub fn choose_embedder<'a>(broker_uds: Option<&'a str>, embed_endpoint: Option<&'a str>) -> EmbedderChoice<'a>`.

- [ ] **Step 1: Write the failing tests for the pure selector**

Add to the `tests` module in `workers/web-research/src/embed.rs`:

```rust
    #[test]
    fn choose_broker_wins_when_both_set() {
        match choose_embedder(Some("/run/embed.sock"), Some("http://x/embed")) {
            EmbedderChoice::Broker { uds } => assert_eq!(uds, "/run/embed.sock"),
            other => panic!("expected Broker, got {other:?}"),
        }
    }

    #[test]
    fn choose_endpoint_when_only_endpoint_set() {
        match choose_embedder(None, Some("http://x/embed")) {
            EmbedderChoice::Endpoint { endpoint } => assert_eq!(endpoint, "http://x/embed"),
            other => panic!("expected Endpoint, got {other:?}"),
        }
    }

    #[test]
    fn choose_none_when_neither_set() {
        assert!(matches!(choose_embedder(None, None), EmbedderChoice::None));
    }

    #[test]
    fn choose_treats_blank_as_unset() {
        assert!(matches!(choose_embedder(Some("  "), Some("  ")), EmbedderChoice::None));
        match choose_embedder(Some("   "), Some("http://x/embed")) {
            EmbedderChoice::Endpoint { .. } => {}
            other => panic!("blank broker uds must fall through to endpoint, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-research choose_`
Expected: FAIL to compile — `choose_embedder` / `EmbedderChoice` not found.

- [ ] **Step 3: Implement the pure selector**

Add to `workers/web-research/src/embed.rs` (near the top, after the `Embedder` trait):

```rust
/// Which embedder `from_env` should build, decided purely from two env values.
/// Kept separate from the (I/O-bound) construction so the precedence rule is
/// unit-testable without touching env or sockets.
#[derive(Debug, PartialEq)]
pub enum EmbedderChoice<'a> {
    /// No embedder configured → lexical-only ranking.
    None,
    /// Use the broker sidecar at this UDS path (takes precedence).
    Broker { uds: &'a str },
    /// Use a direct embedding endpoint (validated + built by the caller).
    Endpoint { endpoint: &'a str },
}

/// Pick the embedder source. The broker UDS wins over a direct endpoint when both
/// are set; blank/whitespace values count as unset.
pub fn choose_embedder<'a>(
    broker_uds: Option<&'a str>,
    embed_endpoint: Option<&'a str>,
) -> EmbedderChoice<'a> {
    let broker = broker_uds.map(str::trim).filter(|s| !s.is_empty());
    let endpoint = embed_endpoint.map(str::trim).filter(|s| !s.is_empty());
    match (broker, endpoint) {
        (Some(uds), _) => EmbedderChoice::Broker { uds },
        (None, Some(endpoint)) => EmbedderChoice::Endpoint { endpoint },
        (None, None) => EmbedderChoice::None,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-research choose_`
Expected: PASS.

- [ ] **Step 5: Wire `choose_embedder` into `from_env`**

In `workers/web-research/src/handler.rs`, replace the `let embedder = match std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT") { ... };` block (the current lines building the embedder) with:

```rust
        // Embedder selection: the broker UDS (KASTELLAN_EMBED_BROKER_UDS) wins
        // over a direct endpoint (KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT). The
        // model is shared by both paths.
        let broker_uds = std::env::var("KASTELLAN_EMBED_BROKER_UDS").ok();
        let embed_endpoint_raw = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT").ok();
        let model = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_MODEL")
            .unwrap_or_else(|_| "embeddinggemma".to_string());
        let embedder: Option<Box<dyn Embedder>> =
            match choose_embedder(broker_uds.as_deref(), embed_endpoint_raw.as_deref()) {
                EmbedderChoice::Broker { uds } => {
                    // No allowlist check: the broker path has no worker egress.
                    Some(Box::new(BrokeredEmbedder::new(std::path::PathBuf::from(uds), model)))
                }
                EmbedderChoice::Endpoint { endpoint } => {
                    // The embed endpoint host must be on the same allowlist (fail
                    // closed if the operator forgot to allow it).
                    let embed_endpoint = validate_endpoint(endpoint, &allowlist)
                        .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
                    let embed_transport = make_get("kastellan-web-research/0")?;
                    Some(Box::new(HttpEmbedder::new(embed_transport, embed_endpoint, model)))
                }
                EmbedderChoice::None => None,
            };
```

Then update the `use crate::embed::...` import at the top of `handler.rs` to bring in the new items — change it to:

```rust
use crate::embed::{choose_embedder, BrokeredEmbedder, Embedder, EmbedderChoice, HttpEmbedder};
```

- [ ] **Step 6: Build, clippy, and run the full crate suites**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-web-research && cargo clippy -p kastellan-worker-web-research --all-targets -- -D warnings && cargo test -p kastellan-worker-web-research`
Expected: build exit 0, clippy clean, all tests PASS (existing embed/handler tests + the new choose_/brokered_ tests). The unset path (`from_env` with neither env set) still yields `embedder: None` — byte-identical behaviour.

- [ ] **Step 7: Commit**

```bash
git add workers/web-research/src/embed.rs workers/web-research/src/handler.rs
git commit -m "feat(web-research): select BrokeredEmbedder via KASTELLAN_EMBED_BROKER_UDS

Pure choose_embedder decides broker-vs-endpoint-vs-none; broker wins.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Workspace verification gate

**Files:** none (verification only).

- [ ] **Step 1: Full-workspace build**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Expected: exit 0 (the new crate compiles as a workspace member; nothing else changed).

- [ ] **Step 2: Full-workspace clippy**

Run: `source "$HOME/.cargo/env" && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean, exit 0. Zero `#[allow(dead_code)]` in the new code — `from_env` references `BrokeredEmbedder`/`choose_embedder`, the broker bin references its lib, so nothing is dead.

- [ ] **Step 3: Targeted test suites**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-embed-broker && cargo test -p kastellan-worker-web-research`
Expected: both green.

- [ ] **Step 4: Confirm no unintended files staged / no scratch tracked**

Run: `git status --porcelain`
Expected: clean (all changes already committed in Tasks 1-7). If anything unexpected appears, do NOT `git add -A` — inspect and stage only the intended files.

---

## Self-Review

**Spec coverage:**
- *embed-broker crate: JSON-RPC serve-loop over UDS + OpenAI-compat forwarding behind a seam + input caps* → Tasks 1-5. ✓
- *BrokeredEmbedder behind the Embedder seam* → Task 6. ✓
- *from_env selection on KASTELLAN_EMBED_BROKER_UDS (broker precedence)* → Task 7. ✓
- *Fully hermetic (fake backend, in-test UDS)* → Tasks 2-4 (FakeGet), Task 6 (stub-broker UDS). ✓
- *HttpEmbedder + unset path byte-identical* → Task 7 Step 6 note; `HttpEmbedder` untouched, unset → `None`. ✓
- *Protocol: reuse kastellan-protocol codec if transport-generic* → confirmed `serve<H,R,W>` is generic; Task 4 uses it (the "local framing fallback" is unneeded). ✓
- *Input caps fail-closed before backend* → Task 3 (`too_many_inputs`/`oversized` tests assert the cap fires first). ✓
- *Threat model: worker has zero embed egress* → the broker path adds no worker net entry (Task 7 Broker arm skips `validate_endpoint`/`make_get`); full removal of the embed host from `Net::Allowlist` is Slice B (manifest), correctly out of scope. ✓

**Placeholder scan:** none — every code step contains complete code; every command has an expected result.

**Type consistency:** `EmbedParams`/`EmbedResult`/`EmbedData`, `forward_embed`, `EmbedHandler::new`, `serve_connection`, `BrokeredEmbedder::new`, `choose_embedder`/`EmbedderChoice` are used consistently across tasks and match the exact upstream signatures (`Handler::call`, `serve<H,R,W>`, `read_capped_record<BufRead>`, `Record::{Line,Eof,TooLarge}`, `RpcError::new`, `HttpGet::post`, `RawResponse{status,location,content_type,body}`). Broker error codes reuse `protocol::codes::{METHOD_NOT_FOUND, INVALID_PARAMS, OPERATION_FAILED, INTERNAL_ERROR}`.

**Notes for Slice B (not in scope):** `SandboxPolicy.embed_broker_uds` + bwrap/Seatbelt bind; `spawn_embed_broker` coupling; web-research manifest change so the embed host leaves `Net::Allowlist` and `KASTELLAN_EMBED_BROKER_ENDPOINT`/`_UDS` are injected at spawn; the broker's `Profile::WorkerNetClient` seccomp (AF_UNIX accept + AF_INET connect — DGX-verify). Slice C: DGX e2e + the #427 content-fetch concurrency e2e.
