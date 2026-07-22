# Design — localmail read-only mail worker (`kastellan-worker-mail`)

**Date:** 2026-07-22
**Status:** Approved (design); implementation plan pending.
**Scope:** Give the kastellan agent read-only access to the localmail mail
archive, so it can search and retrieve mail (and attachment text) as a tool.

## Goal

Enable agent tasks of the shape *"in my email, find all Qantas flight bookings
between date X and date Y and return a CSV of date / from / to / passengers /
cost."* The tool layer supplies **search + retrieval**; the agent (LLM) does the
**extraction and formatting**. No mail-provider-specific or booking-specific code.

## Decisions (settled during brainstorming)

- **Integration mechanism — worker → localmail HTTP (not worker → Postgres, not
  core-as-MCP-client).** localmail's "refined semantic search" is a *model-bearing
  runtime* — a `Searcher` built from a `FastEmbedBackend` (in-process ONNX query
  embedding that must match the stored document vectors), an optional
  `FastEmbedReranker` (a second ONNX cross-encoder), an optional LLM query
  rewriter (`smart=True`, an outbound HTTP call), and an in-process `PageCache`.
  Reaching Postgres directly would force those models into the worker sandbox
  (pinned to localmail's `embedding_model`, or relevance silently degrades) and
  couple the worker to localmail's schema (currently at migration 0031). Talking
  to localmail's HTTP surface keeps the models, config, and caches where they
  already live, at full fidelity, behind a stable ACL'd API.
- **Uplink surface — REST API + bearer token (not MCP-over-OAuth).** localmail's
  REST API authenticates a headless client with a simple
  `Authorization: Bearer <token>` (`api_tokens` table: token sha256, user_id,
  expires_at; ACL enforced server-side via `verify_token` → `allowed_account_ids`).
  Its MCP-over-HTTP surface requires the full OAuth2 dance (dynamic registration /
  authorize / token, RFC 8707 resource indicators, consent) — built for
  *interactive* clients, heavyweight for a headless worker, and no extra
  capability over REST for our use.
- **Worker language — Rust (not Python).** We are not importing localmail; we are
  calling it over HTTP+JSON. A Rust worker (like `web-fetch`) needs no Python
  runtime, no venv/rootfs, and no models in the sandbox. This also honours the
  project's "Rust core, Python only inside sandboxed workers" rule by not even
  needing Python here.
- **Topology — one config value.** Mostly co-located with the agent, sometimes
  remote over LAN/VPN. The endpoint (`http://127.0.0.1:PORT` or
  `https://mailhost.vpn:PORT`) is a single config value; the same worker binary
  and code path serve both.

## Architecture

```
agent core ──stdio JSON-RPC── [sandbox: kastellan-worker-mail (Rust)]
                                    │ reqwest + Bearer token
                                    ▼ (egress proxy, allowlisted host:port)
                              localmail /v1 REST ──▶ Postgres + warm models
```

### Components

- **`workers/mail`** — new crate `kastellan-worker-mail` (fills the reserved,
  currently-`.gitkeep` slot). Layout modelled on `web-fetch`:
  - `src/main.rs` — the `kastellan-worker-prelude` stdio JSON-RPC loop.
  - `src/handler.rs` — method dispatch (`mail.*`), param validation, REST-response
    → JSON-RPC-result mapping, error mapping.
  - `src/client.rs` — a thin `reqwest` wrapper over localmail `/v1`, reads the
    endpoint from `KASTELLAN_MAIL_ENDPOINT` and the token from
    `KASTELLAN_MAIL_TOKEN`, sets the `Authorization` header.
  - Dependencies: `kastellan-protocol`, `kastellan-worker-prelude`, `reqwest`,
    `serde`, `serde_json`, `url`. **No** `web-common` (that crate is HTML/PDF
    extraction — localmail extracts server-side).
- **`core/src/workers/mail.rs`** — host-side manifest. Builds a `ToolEntry` +
  `SandboxPolicy`, sources the endpoint + allowlist from config / the
  `tool_allowlists` DB table (keyed `"mail"`) and the bearer token from the
  secret vault, and injects them into `policy.env` at spawn. Registered in
  `core/src/workers/mod.rs` and wired through `registry_build.rs` /
  `worker_manifest.rs` like the other workers.

### Lifecycle

`Lifecycle::SingleUse` (like `web-fetch`): each tool call is a fresh sandboxed
process making one stateless HTTP request. localmail's search pagination is a
server-side-encoded cursor token returned to the agent and passed back on the
next call, and its rerank `PageCache` lives in localmail's `serve` process — so
SingleUse composes cleanly with pagination (no worker-side state to preserve).

## Tool surface (JSON-RPC → localmail REST)

Five read-only tools, namespaced `mail.*`. All are ACL-scoped server-side by the
token's localmail user.

| kastellan tool | localmail REST | purpose |
|---|---|---|
| `mail.search` | `POST /v1/search` (`run_search`) | Hybrid vector+FTS+RRF search with optional cross-encoder rerank. `POST` carries the query/filters in a JSON body; it performs no mutation. Params: `query`, `filters { date_from, date_to, from, to, subject, has_attachment, account_ids, folder_ids, lang }`, `sort` (`rank`\|`date`), `limit`, `cursor`. Returns ranked hits + `next_cursor`. `smart` (LLM rewrite) is forced off — see Error handling. |
| `mail.get_message` | `GET /v1/messages/{id}` | One message: headers, plaintext body, attachment list `[{ filename, sha256 }]`. |
| `mail.list_messages` | `GET /v1/messages` | Keyset date-ordered browse page; `account_ids` / `folder_ids` filters, `limit`, `cursor`. |
| `mail.list_accounts` | `GET /v1/accounts` | The accounts this token may read. |
| `mail.get_attachment_text` | `GET /v1/attachments/{sha256}/text` | Server-extracted text of an attachment (docling / pypdf server-side). How the agent reads a booking PDF. |

**Attachment text only, not raw bytes.** The raw-blob endpoint
(`GET /v1/attachments/{sha256}`) is deliberately not exposed: the use case wants
LLM-readable text, and raw-byte download is YAGNI. Revisit if a real need for the
original bytes appears.

## Containment / sandbox policy

Mirrors `web-fetch`, minus the DNS resolver files when force-routed (the egress
proxy resolves host-side).

- `net: Net::Allowlist([<endpoint host:port>])` — **only** localmail's endpoint.
  Operator-controlled via the `tool_allowlists` table keyed `"mail"`; the
  LLM-supplied `step.parameters` cannot widen it (mapped through
  `allowlist_to_net_entries`, preserving any wildcard/port semantics).
- **Co-located loopback** relies on the egress proxy's allowlisted-IP-literal
  carve-out: a literal `127.0.0.1:PORT` is dialed through the proxy under
  force-routing (the corrected institutional fact — force-routing blocks local
  *hostnames*, not operator-allowlisted IP literals; the same mechanism
  `web-search` uses to reach a loopback SearxNG). **Remote** is a normal
  allowlisted egress host:port.
- `profile: Profile::WorkerNetClient`; `mem_mb: 256` (JSON only, lighter than
  web-fetch's 512 — no HTML/PDF parsing); `cpu_ms: 10_000`;
  `wall_clock_ms: Some(30_000)`.
- **Bearer token** injected as `KASTELLAN_MAIL_TOKEN` in `policy.env` from the
  secret vault at spawn (the pattern the Matrix worker uses for its bot secret).
  Never in params, never LLM-visible, never logged.
- **Cross-platform:** a pure Rust HTTP client runs identically under bwrap
  (Linux) and Seatbelt (macOS) through the existing `SandboxBackend` — no
  platform-specific code, no asymmetry.
- **Firecracker micro-VM entry is deferred.** Not needed for a same-host / LAN
  tool; can be added later exactly as the web workers did (an opt-in
  `*_firecracker_entry` + a rootfs), without reworking the tool layer.

**Threat-model fit.** Worst-case compromise of this worker reaches only
localmail's REST endpoint, scoped to that one API token's ACL — no Postgres role,
no keyring, no other host, no other endpoint. Tighter than a worker→Postgres
read-only role, which would see the whole archive DB.

## Config & provisioning (operator, one-time)

1. **localmail:** create a dedicated `agent` API user; grant it the accounts /
   folders the agent may read (this ACL *is* the agent's mail scope); mint a
   bearer token via the localmail CLI.
2. **kastellan:** `kastellan-cli secret put localmail-agent-token` (paste the
   token); the manifest redeems it from the vault at spawn.
3. Set `KASTELLAN_MAIL_ENDPOINT`; add the endpoint to `tool_allowlists` keyed
   `"mail"`.
4. Ensure localmail `serve` is running and reachable from the agent host.

Scope is entirely operator-driven: granting the `agent` user only a personal
Gmail account (and not a work account) restricts the agent to exactly that.

## Data flow — the Qantas-CSV task

1. `mail.search { query: "Qantas flight booking confirmation",
   filters: { date_from: X, date_to: Y, has_attachment: true } }` → ranked hits.
2. For each hit: `mail.get_message { id }` (itinerary often in the body) and/or
   `mail.get_attachment_text { sha256 }` for the booking PDF.
3. The **LLM** extracts date / from / to / passengers / cost and formats the CSV.

The worker supplies retrieval; the agent does extraction and formatting. No
flight-specific code anywhere in the worker.

## Error handling & degradation

- **`smart`-rewrite off:** workers do not call the LLM (kastellan invariant; the
  LLM is core-only via `llm_router`). The agent's planner already decomposes /
  rewrites queries, and base hybrid+rerank is full-fidelity without it. A future
  option is to route rewrite through `llm_router` core-side — noted, not built.
- **Read-only:** only read-only endpoints are wired — the four GETs plus
  `POST /v1/search` (a POST purely to carry the query body; it mutates nothing).
  The worker has no send / delete / modify path at all.
- **Endpoint down / token expired / 401 / 403 / 5xx:** mapped to clean
  `RpcError`s the agent sees as actionable messages (not stack traces), matching
  web-fetch's error mapping. A 401/403 surfaces as a distinct "auth/permission"
  message so the operator can re-provision the token.
- **ACL:** enforced server-side by localmail per token; a not-found and a
  not-permitted message are indistinguishable by design (localmail already does
  this).

## Testing strategy

- **Unit** (both hosts, hermetic): `handler.rs` dispatch — unknown method →
  `MethodNotFound`; param validation; REST-response → JSON-RPC-result shape
  mapping; the token is never echoed into a result or error. Uses a fake HTTP
  client (the `web-fetch` `FakeGet` pattern), no network.
- **Manifest / containment** (both hosts): assert `Net::Allowlist` contains *only*
  the configured endpoint; LLM params cannot widen it; the token is sourced from
  the vault, not params.
- **Integration** (`#[ignore]`, opt-in): against a live `localmail serve` with a
  seeded test archive — real `search` / `get_message` / `get_attachment_text`
  round-trips, including a paginated search using `next_cursor`. Runs on macOS
  and the DGX.

## Explicitly out of scope (YAGNI)

- MCP-over-OAuth uplink.
- Python runtime or embedding/reranker models in the worker sandbox.
- worker → Postgres direct access.
- Any write path (send / delete / modify) — the archive is read-only upstream and
  the worker mirrors that.
- Flight / booking-specific extraction logic.
- Raw attachment-byte download (`GET /v1/attachments/{sha256}`).
- Firecracker micro-VM entry.
- Reusing kastellan's embed-broker for query embedding (localmail owns embedding;
  its stored vectors must be matched by its own model).

## Open questions

- **Exact localmail REST search response shape** (`POST /v1/search`) — the
  request-body schema and the response field names for hits, snippet, and the
  cursor token — to be pinned against the running service when the plan is
  written (`serve/routes/search.py`).
- **localmail CLI command to mint an API-user token** — confirm the exact
  subcommand for the provisioning doc.
- **HTTPS vs plain HTTP for remote endpoints** — assume HTTPS for LAN/VPN, plain
  HTTP acceptable only for `127.0.0.1`; confirm during planning.
