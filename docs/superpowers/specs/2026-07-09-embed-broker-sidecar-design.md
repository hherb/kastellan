# Trusted embedding broker sidecar — design

**Date:** 2026-07-09
**Status:** approved (brainstorming) → Slice A to implement this session
**Supersedes:** the deferred *embed-e2e over the force-routed `ProxyConnectGet`
transport* ([#427](https://github.com/hherb/kastellan/issues/427) family) — this
design removes the embed leg from egress entirely, so that e2e stops being
necessary. The #427 *content-fetch* concurrency e2e remains relevant and is
folded into Slice C.

---

## Problem

The `web-research` worker ranks passages with an optional embedding lane
(`KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` → `HttpEmbedder` → OpenAI-compatible
POST over the worker's egress transport). When the worker is **force-routed**
(`KASTELLAN_EGRESS_FORCE_ROUTING=1`) or runs **in a micro-VM**, its only route
out is the general egress proxy, which:

- **SSRF-blocks loopback / private IPs**, so the common embed backend (a local
  Ollama on `127.0.0.1:11434`, plaintext) is unreachable;
- would, for a routable backend under MITM, need the proxy to trust the
  backend's cert on re-origination (the proxy wires **webpki roots only** — see
  `workers/egress-proxy/src/pins.rs::build_upstream_client_config`), so a
  self-signed/loopback backend fails validation.

The net effect: hybrid ranking silently degrades to lexical in exactly the
deployment modes we care about, and closing the gap through the egress proxy
means enlarging that worker's network blast radius and solving a MITM-loopback
trust puzzle.

**Root observation.** Kastellan already *has* a shared embedding service (the
operator's Ollama/vLLM, which core uses for memory recall). The real problem is
narrow: **how does a jailed worker reach that trusted first-party service
without opening general egress and without duplicating the model per worker.**

Rejected alternatives:

- **fastembed baked into each worker** (in-process ONNX): duplicates the model
  across every worker rootfs and needs the weights pre-staged into each jail;
  resource duplication for no isolation benefit.
- **Route embed through the egress proxy to an internal endpoint**: keeps the
  SSRF/MITM-trust problems and conflates general egress with a single trusted
  verb.
- **Move embedding+ranking into core** (worker returns raw passages): purest
  threat-model fit but redraws the worker/core contract and moves compute into
  core; deferred as a possible future, not chosen here.

## Chosen approach — a single-purpose embedding broker sidecar

A tiny **first-party sidecar** — the same *class* of component as
`workers/egress-proxy` — bridges a jailed worker's UDS to the operator's
configured embedding backend. The worker sees exactly one trusted endpoint whose
sole verb is *text → vector*; it needs **zero embed egress**.

```
core ── spawns ──> embed-broker (sandboxed)
                        │  binds  <scratch>/embed.sock
                        │  Net::Allowlist([backend host:port])
                        │
     worker jail: --bind embed.sock  (the ONLY embed capability)
                        │
   BrokeredEmbedder ── JSON-RPC embed{model,input} ──> broker
                        │  OpenAI-compat POST  ──────> Ollama/vLLM
                        v
                   {data:[{index,embedding}]}  ──back──> worker
                        │
                  cosine / rrf_fuse  (unchanged)
```

### Components

1. **`workers/embed-broker` (`kastellan-worker-embed-broker`)** — new crate, a
   sandboxed sidecar (spawned by core like `egress-proxy`, **not** a `tool_host`
   stdio worker).
   - Binds `embed.sock` in its scratch dir; readiness is "socket exists"
     (mirrors the egress proxy exporting `egress.sock`/`ca.pem`).
   - Serves a **line-delimited JSON request/response** protocol over the UDS,
     one method: `embed { model: String, input: [String] }` →
     `{ data: [{ index: usize, embedding: [f32] }] }` (a JSON-RPC-2.0-shaped
     envelope; see "Protocol" below).
   - Forwards each request as an **OpenAI-compatible HTTP POST**
     (`{model, input}`) to `KASTELLAN_EMBED_BROKER_ENDPOINT`, decodes
     `{data:[{index, embedding}]}`, **reorders by `index`**, and **count-checks**
     (one vector per input) — the same contract `HttpEmbedder` enforces today, so
     all OpenAI-compat coupling lives in **one place** (the broker).
   - **Input caps, fail-closed:** reject a batch exceeding `MAX_INPUTS` items or
     `MAX_REQUEST_BYTES` total; a malformed request → a typed error response, not
     a panic or silent drop.
   - The **backend call is behind a seam** (`Backend` trait / an `HttpGet`-style
     poster) so the forwarding logic is unit-testable with a `FakeBackend`
     without a live Ollama.

2. **`BrokeredEmbedder`** — new `Embedder` impl in
   `workers/web-research/src/embed.rs`, beside `HttpEmbedder` (untouched).
   - `embed(&self, texts) -> Result<Vec<Vec<f32>>, EmbedError>` opens/uses the
     bound `embed.sock`, sends the JSON-RPC `embed` request, reads the framed
     response, maps failures onto the existing `EmbedError` variants
     (`Transport` / `Status`→n/a / `Decode` / `CountMismatch`).
   - Selection in `WebResearchHandler::from_env` mirrors `make_get`'s
     `ProxyConnectGet`-vs-`ReqwestGet` switch:
     **`KASTELLAN_EMBED_BROKER_UDS` set → `BrokeredEmbedder`**; else the existing
     `KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT` → `HttpEmbedder` path; else `None`.
     The broker UDS takes precedence when both are set.

3. **Core coupling `spawn_embed_broker`** *(Slice B)* — mirrors
   `core::egress::net_worker::spawn_forced_net_worker`: mint `embed-<pid>-<seq>/`
   under the scratch root, spawn the broker sandboxed with
   `Net::Allowlist([backend host:port])` + its endpoint env, wait for
   `embed.sock`, and bind that UDS into the consuming worker's jail. The broker
   handle is owned by the worker's supervised bundle (like `EgressSidecar`) →
   1:1 teardown.

4. **Sandbox `embed_broker_uds`** *(Slice B)* — a new
   `SandboxPolicy.embed_broker_uds: Option<PathBuf>` that binds one more UDS into
   the jail, mirroring `proxy_uds` exactly in both `linux_bwrap` (`--bind`) and
   `macos_seatbelt`. A dedicated field (not a `Vec` generalization of
   `proxy_uds`) keeps the change to the security-critical policy struct minimal;
   a list can come later if a third bound socket appears.

### Protocol

Line-delimited JSON over the UDS, one request per line, one response per line
(same framing discipline as the `kastellan-protocol` stdio codec, driven over a
`UnixStream`). If `kastellan-protocol`'s reader/writer is cleanly generic over
`Read`/`Write`, reuse it; **if it is hardwired to stdin/stdout, Slice A uses a
small local framing** (the egress-proxy precedent for a bespoke sidecar
protocol) rather than forcing a protocol-crate refactor into this slice.

Request:  `{"jsonrpc":"2.0","id":1,"method":"embed","params":{"model":"…","input":["…","…"]}}`
Response: `{"jsonrpc":"2.0","id":1,"result":{"data":[{"index":0,"embedding":[…]}]}}`
Error:    `{"jsonrpc":"2.0","id":1,"error":{"code":…,"message":"…"}}`

Reordering by `index` is the broker's job; the worker receives vectors already in
request order but still count-checks defensively.

## Threat model delta

- **Before:** a force-routed / VM web-research worker must reach the embed
  endpoint through the general egress proxy — an allowlist entry, an SSRF
  surface, and (for a routable backend) a MITM re-origination trust problem.
- **After:** the worker's jail has **zero embed egress**. Its only embed
  capability is one first-party UDS whose sole verb is *text → vector*. A
  compromised worker can at worst push arbitrary text at the operator's own
  embedding backend (compute abuse / probing), bounded by the broker's
  `MAX_INPUTS` / `MAX_REQUEST_BYTES` caps. The embed host leaves the worker's
  `Net::Allowlist` union entirely. **Strictly smaller blast radius.**
- The broker holds the only egress to the backend and is sandboxed to exactly
  that endpoint. **v1 targets the loopback-Ollama case** (broker on the host
  netns, `Net::Allowlist([backend])`). A *remote* backend would later force-route
  the broker itself through the egress proxy — out of scope here.
- **Invariants preserved:** one process per worker, one sandbox per worker, IPC
  only via line-delimited JSON over a bound socket — consistent with
  `tool_host` / `egress-proxy`. No new in-core model execution.

## Slicing

**Slice A — the two ends of the pipe (THIS session; pure-code, Mac-verifiable,
no DGX):**
- `workers/embed-broker` crate: the JSON-RPC `embed` serve-loop over a UDS +
  OpenAI-compat forwarding behind a `Backend` seam + input caps.
- `BrokeredEmbedder` + `from_env` selection on `KASTELLAN_EMBED_BROKER_UDS`.
- Fully hermetic (fake backend, in-test UDS). `HttpEmbedder` and the
  unset/endpoint paths stay byte-identical. Independently reviewable/mergeable.

**Slice B (next session; needs DGX):** `SandboxPolicy.embed_broker_uds` +
bwrap/Seatbelt bind (+ tests), `spawn_embed_broker` coupling, web-research
manifest/worker wiring so the embed host leaves the worker's `Net::Allowlist`.
DGX for the real bwrap-bind acceptance.

**Slice C (later; needs live Ollama on DGX):** full DGX e2e — real broker + real
Ollama + jailed worker → `ranking=="hybrid"` with zero embed egress; scripts +
docs. The #427 concurrent-GET *content-fetch* e2e lands here too.

## Slice A test plan (TDD — watch each fail first)

`workers/embed-broker`:
1. **Codec** — parse an `embed` request; encode a `data` response; encode an
   error. Round-trip.
2. **Forwarding** (fake backend) — request → backend called with `{model,input}`
   → response reordered by `index`, count-checked; index-scramble reordered
   correctly; count mismatch → typed error; backend transport failure → typed
   error.
3. **Input caps** — a batch over `MAX_INPUTS` or `MAX_REQUEST_BYTES` → fail-closed
   error, backend never called.
4. **UDS round-trip** — serve loop on an in-test UDS + fake backend; a client
   connects, sends `embed`, receives the vectors; a second request on the same
   socket also succeeds (the serve loop is not one-shot).

`workers/web-research`:
5. **`BrokeredEmbedder` round-trip** — against an in-test stub-broker UDS that
   answers the JSON-RPC `embed`: `embed()` returns the vectors; a broker error →
   the mapped `EmbedError`; a closed/absent socket → `EmbedError::Transport`.
6. **`from_env` selection** (env-guarded, serial): `KASTELLAN_EMBED_BROKER_UDS`
   set → a `BrokeredEmbedder` is built (assert via behaviour against a stub
   socket, not type-reflection); unset + endpoint set → `HttpEmbedder`; both
   unset → `None`; both set → broker wins.

**Verification gate (Mac, Seatbelt, rustc 1.96):** `cargo build --workspace`
exit 0; `cargo clippy --workspace --all-targets -- -D warnings` clean (zero
`#[allow(dead_code)]` — `from_env` references `BrokeredEmbedder`, the broker bin
is standalone); `cargo test -p kastellan-worker-embed-broker` and
`cargo test -p kastellan-worker-web-research` green. No PG/sandbox/DGX surface in
Slice A → the DGX 2369/0/39 baseline carries forward.

## Open questions (parked)

- Long-lived shared broker (one per daemon) vs per-worker 1:1 — v1 is per-worker
  to match the egress-sidecar lifecycle; revisit if spawn overhead matters.
- Generalizing the broker to arbitrary model inference (the `llm_router`
  sibling) — the UDS + sidecar shape is deliberately compatible, but out of
  scope.
- `embedding_dim` consistency: web-research ranking is dimension-agnostic
  (cosine over query+passage vectors from the same model), so the broker imposes
  no Matryoshka truncation — unlike the core memory path (`EMBEDDING_DIM=256`).
