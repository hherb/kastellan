# Design — localmail read-only mail worker (`kastellan-worker-mail`) + Workspace `out/` activation

**Date:** 2026-07-22
**Status:** Approved (design); implementation plan pending.
**Scope:** Give the kastellan agent read-only access to the localmail mail
archive — search, retrieve messages, retrieve attachments as **extracted text**
*and* as **original-format files** (PDF, etc.). Delivering original-format
attachments requires activating kastellan's currently-dormant per-task
`Workspace` `out/` channel, so this design covers **two coordinated
workstreams**:

- **Workstream A — Workspace `out/` activation (core).** Wire the dormant
  per-task `Workspace` into the scheduler so a worker can emit durable file
  artifacts the agent/user can retrieve. A general capability; the mail worker is
  its first consumer.
- **Workstream B — the mail worker (`kastellan-worker-mail`).** A Rust worker
  that calls localmail's `/v1` REST API and exposes read-only `mail.*` tools,
  using A for original-format attachment delivery.

## Goal

Enable agent tasks of the shape *"in my email, find all Qantas flight bookings
between date X and date Y and return a CSV of date / from / to / passengers /
cost, and save the booking PDFs."* The tool layer supplies **search + retrieval
+ file delivery**; the agent (LLM) does the **extraction and formatting**. No
mail-provider-specific or booking-specific code.

## Decisions (settled during brainstorming)

- **Integration mechanism — worker → localmail HTTP** (not worker → Postgres, not
  core-as-MCP-client). localmail's semantic search is a *model-bearing runtime*
  (a `Searcher` = in-process ONNX query embedding that must match the stored
  document vectors + optional ONNX cross-encoder rerank + optional LLM query
  rewrite + in-process `PageCache`). Reaching Postgres directly would force those
  models into the worker sandbox (pinned to localmail's `embedding_model`, or
  relevance silently degrades) and couple the worker to localmail's schema.
  Talking to localmail's HTTP surface keeps the models, config, and caches where
  they already live, at full fidelity, behind a stable ACL'd API.
- **Uplink surface — REST API + bearer token** (not MCP-over-OAuth). localmail's
  REST API authenticates a headless client with `Authorization: Bearer <token>`
  (`api_tokens` table; ACL enforced server-side via `verify_token` →
  `allowed_account_ids`). Its MCP-over-HTTP surface needs the full OAuth2 dance
  (dynamic registration / authorize / token / RFC 8707 resource indicators /
  consent) — built for interactive clients, heavyweight for a headless worker,
  no extra capability over REST for our use.
- **Worker language — Rust** (not Python). We call localmail over HTTP+JSON; we
  do not import it. A Rust worker (like `web-fetch`) needs no Python runtime,
  venv, or models in the sandbox.
- **Topology — one config value.** Mostly co-located with the agent, sometimes
  remote over LAN/VPN. The endpoint (`http://127.0.0.1:PORT` or
  `https://mailhost.vpn:PORT`) is a single config value; the same worker binary
  serves both.
- **Attachments — text *and* original format.** `get_attachment_text` returns
  server-extracted text (for reading/reasoning); `get_attachment` returns the
  original bytes as a **file written to the task `Workspace` `out/`** (for PDFs
  and any other format the user wants to keep). Chosen over base64-inline so
  binary never bloats the agent context and delivery scales to large/many files.
- **localmail is malleable.** The user owns localmail and it has no production
  users; it exists precisely for consumers like kastellan. So API shapes are
  pinned/adjusted against the live service at plan time rather than
  reverse-engineered, and small server-side tweaks that simplify the worker are
  in-scope (see the localmail-side tweak below).
- **Firecracker micro-VM entry deferred.** The worker ships host-sandboxed only
  (bwrap/Seatbelt). A VM entry (and binding `out/` as a VM persistent store) can
  follow later, as the web workers did.

## Architecture

```
agent core ──stdio JSON-RPC── [sandbox: kastellan-worker-mail (Rust)]
   │                               │ reqwest + Bearer token
   │ per-task Workspace            ▼ (egress proxy, allowlisted host:port)
   │ <root>/<task_id>/{in,out,tmp} localmail /v1 REST ──▶ Postgres + warm models
   │            ▲ out/ bound RW into the worker (extend_policy)
   └── harvest out/ at task finalize ──▶ durable artifacts + task result
```

## Workstream A — Workspace `out/` activation (core)

**Current state.** `core/src/workspace.rs` defines a per-task `Workspace` with a
fixed `<root>/<task_id>/{in,out,tmp}` layout: `in/` read-only inputs, `out/`
"worker-produced outputs the host harvests after the call", `tmp/` private
scratch. `extend_policy` wires all three into `SandboxPolicy.fs_write` *and* the
worker-side Landlock filter (via `tool_host::derive_lockdown_env`) in lock-step.
`Drop` recursively wipes `<root>/<task_id>`. It is **dormant** — no production
code constructs a `Workspace` today (only an `inner_loop.rs` doc comment
anticipates it).

**Activation:**

1. **Construct per-task.** `runner::run_inner_loop_for_task` (task entry) creates
   `Workspace::new(&task_id.to_string())` under `$KASTELLAN_WORKSPACE_ROOT`
   (default `~/.kastellan/workspace`) and holds it in / beside `TaskContext`
   (`task_id: i64` → the `[A-Za-z0-9_-]` id the workspace validates). One per
   task, shared across all plan iterations and worker spawns in that task.
2. **Opt-in binding.** A worker that emits artifacts gets `out/` (+ `tmp/`) bound
   RW via `Workspace::extend_policy` at spawn. The mail worker is the first
   opt-in; other workers are unchanged (`fs_write` stays `vec![]`). The bind is
   the same `SingleUse` path web-fetch uses; the Workspace is **task-scoped**,
   so successive `SingleUse` spawns in one task accumulate into the *same* `out/`
   ("save all the Qantas PDFs" = several `get_attachment` calls, all landing in
   one `out/`).
3. **Path contract.** The worker writes under `out/` and returns the absolute
   path. The core→worker `out/` path is stable for the task's lifetime, so the
   agent can reference a written file across later steps in the same task.
4. **Persistence / harvest (the lifecycle crux).** `Workspace::Drop` wipes the
   whole tree, which would destroy deliverables. So at **task finalize**, before
   the Workspace drops, the scheduler **harvests `out/`** — moves (rename if
   same filesystem, else copy) its contents into a durable per-task artifacts
   dir `$KASTELLAN_ARTIFACTS_ROOT/<task_id>/` (default `~/.kastellan/artifacts`),
   and records the harvested paths in the task-finalize result so the agent's
   final answer references *surviving* paths. `in/` + `tmp/` are wiped by `Drop`
   as designed. This keeps the Workspace's single-cleanup-path RAII contract
   intact (the ephemeral tree is always wiped) while deliverables live on in a
   clearly-named artifacts area.
5. **Cross-platform.** `extend_policy` already targets both bwrap (Linux) and
   Seatbelt (macOS); host-mode only in this pass (Firecracker deferred, so no
   VM-side `out/` mount yet).

**Scope line for A.** Harvest itself is in-scope (without it the feature does not
work). **Retention/GC of the artifacts dir is out of scope** — operator cleans it
for now; a retention policy is a future follow-up. The Workspace is bound only
for opt-in workers (the mail worker), not activated fleet-wide, to keep the blast
radius contained.

## Workstream B — the mail worker

### Components

- **`workers/mail`** — new crate `kastellan-worker-mail` (fills the reserved
  `.gitkeep` slot). Layout modelled on `web-fetch`:
  - `src/main.rs` — the `kastellan-worker-prelude` stdio JSON-RPC loop.
  - `src/handler.rs` — `mail.*` dispatch, param validation, REST→JSON-RPC
    mapping, error mapping.
  - `src/client.rs` — a thin `reqwest` wrapper over localmail `/v1`; reads the
    endpoint from `KASTELLAN_MAIL_ENDPOINT`, the token from `KASTELLAN_MAIL_TOKEN`,
    and (for `get_attachment`) the workspace out dir from `KASTELLAN_WORKER_OUT`
    (or the standard workspace env — pinned at plan time).
  - Dependencies: `kastellan-protocol`, `kastellan-worker-prelude`, `reqwest`,
    `serde`, `serde_json`, `url`. **No** `web-common`.
- **`core/src/workers/mail.rs`** — host-side manifest: builds the
  `ToolEntry`/`SandboxPolicy`, sources endpoint + allowlist from config /
  `tool_allowlists` (keyed `"mail"`) and the bearer token from the secret vault,
  calls `Workspace::extend_policy` to bind `out/`, and injects env at spawn.
  Registered in `core/src/workers/mod.rs` via `registry_build.rs` /
  `worker_manifest.rs`.

### Lifecycle

`Lifecycle::SingleUse` (like `web-fetch`): each tool call is a fresh sandboxed
process. localmail's search pagination is a server-side-encoded cursor returned
to the agent and passed back; its rerank `PageCache` lives in localmail's `serve`
process — so SingleUse composes cleanly with pagination. Attachment files
accumulate in the task-scoped `out/` across spawns (see A.2).

### Tool surface (JSON-RPC → localmail REST)

Six read-only tools, namespaced `mail.*`, all ACL-scoped server-side by the
token's localmail user.

| kastellan tool | localmail REST | purpose |
|---|---|---|
| `mail.search` | `POST /v1/search` (`run_search`) | Hybrid vector+FTS+RRF search + optional cross-encoder rerank. Params: `query`, `filters { date_from, date_to, from, to, subject, has_attachment, account_ids, folder_ids, lang }`, `sort` (`rank`\|`date`), `limit`, `cursor`. Returns ranked hits + `next_cursor`. `POST` carries the body; it mutates nothing. `smart` (LLM rewrite) forced off — see Error handling. |
| `mail.get_message` | `GET /v1/messages/{id}` | One message: headers, plaintext body, attachment list `[{ filename, sha256, content_type, size }]` (the last two added server-side — see localmail tweak). |
| `mail.list_messages` | `GET /v1/messages` | Keyset date-ordered browse; `account_ids` / `folder_ids` filters, `limit`, `cursor`. |
| `mail.list_accounts` | `GET /v1/accounts` | Accounts this token may read. |
| `mail.get_attachment_text` | `GET /v1/attachments/{sha256}/text` | Server-extracted text (docling / pypdf) — how the agent *reads* a booking PDF. |
| `mail.get_attachment` | `GET /v1/attachments/{sha256}` | Fetches the **original bytes**, streams them to `out/<safe-filename>`, returns `{ filename, content_type, size, path }`. How the agent *delivers* the file. No bytes in the JSON-RPC result. |

`get_attachment` filename safety: derive a collision- and traversal-safe name
under `out/` (sanitize the per-email filename; prefix/suffix with the sha256 to
avoid collisions across messages that share bytes). Pinned at plan time.

### localmail-side tweak (in-scope, since localmail is ours)

Include `content_type` and `size` in the `get_message` attachment list (today it
records `[{filename, sha256}]`), so the agent chooses text-vs-original and sizes
a download without a probe. Tracked as
[hherb/localmail#196](https://github.com/hherb/localmail/issues/196) (being
implemented on the localmail side). Any other small shape adjustment discovered at
plan time that simplifies the worker is fair game.

## Containment / sandbox policy (mail worker)

Mirrors `web-fetch`, plus the workspace bind:

- `net: Net::Allowlist([<endpoint host:port>])` — **only** localmail's endpoint.
  Operator-controlled via `tool_allowlists` keyed `"mail"`; LLM params can't
  widen it (mapped through `allowlist_to_net_entries`).
- **Co-located loopback** uses the egress proxy's allowlisted-IP-literal carve-out
  (`127.0.0.1:PORT` dialed via the proxy under force-routing — same mechanism
  `web-search` uses for a loopback SearxNG). **Remote** is a normal allowlisted
  egress host:port.
- `fs_write`: the task workspace `out/` + `tmp/` via `Workspace::extend_policy`
  (RW bind + Landlock in lock-step). Nothing else writable.
- `profile: WorkerNetClient`; `mem_mb: 256` (JSON + a streamed file copy, lighter
  than web-fetch's 512 HTML/PDF parsing); `cpu_ms: 10_000`;
  `wall_clock_ms: Some(30_000)`.
- **Bearer token** injected as `KASTELLAN_MAIL_TOKEN` in `policy.env` from the
  secret vault at spawn (Matrix-secret pattern). Never in params, never
  LLM-visible, never logged.
- **Cross-platform:** pure Rust HTTP client + a file write; runs identically under
  bwrap and Seatbelt via the existing `SandboxBackend`.

**Threat-model fit.** Worst-case compromise of this worker reaches only
localmail's REST endpoint (scoped to one API token's ACL) and its own task
workspace `out/`/`tmp/` — no Postgres role, no keyring, no other host, no other
endpoint, no other task's workspace.

## Config & provisioning (operator, one-time)

1. **localmail:** create a dedicated `agent` API user; grant it the accounts /
   folders the agent may read (this ACL *is* the agent's mail scope); mint a
   bearer token via the localmail CLI (exact subcommand pinned at plan time).
2. **kastellan:** `kastellan-cli secret put localmail-agent-token` (paste token);
   the manifest redeems it from the vault at spawn.
3. Set `KASTELLAN_MAIL_ENDPOINT`; add the endpoint to `tool_allowlists` keyed
   `"mail"`.
4. Ensure localmail `serve` is running and reachable from the agent host.

## Data flow — the Qantas task

1. `mail.search { query: "Qantas flight booking confirmation",
   filters: { date_from: X, date_to: Y, has_attachment: true } }` → ranked hits.
2. Per hit: `mail.get_message { id }` (itinerary often in body) and/or
   `mail.get_attachment_text { sha256 }` for the PDF text.
3. `mail.get_attachment { sha256 }` for each booking PDF → files land in the
   task `out/`.
4. The **LLM** extracts date / from / to / passengers / cost → CSV; references the
   harvested PDF paths in its final answer.
5. Task finalize harvests `out/` → `~/.kastellan/artifacts/<task_id>/`; the CSV
   and PDFs survive for the user.

## Error handling & degradation

- **`smart`-rewrite off:** workers don't call the LLM (kastellan invariant); the
  planner already decomposes queries; base hybrid+rerank is full-fidelity without
  it. Routing rewrite through `llm_router` core-side is a possible future — noted,
  not built.
- **Read-only:** only read endpoints are wired (four GETs + `POST /v1/search`, a
  POST purely to carry the query body). No send / delete / modify path exists in
  the worker.
- **Endpoint down / token expired / 401 / 403 / 5xx:** mapped to clean
  `RpcError`s (not stack traces); 401/403 surfaces a distinct "auth/permission"
  message so the operator re-provisions the token.
- **Attachment write failure** (out/ full, IO error): clean `RpcError`; no
  partial file left claimed as complete (write to a `.partial`, rename on
  success).
- **ACL:** enforced server-side by localmail per token; not-found and
  not-permitted are indistinguishable by design.

## Testing strategy

**Workstream A (core):**
- Unit (both hosts): per-task Workspace construction, `extend_policy` adds
  `out/`+`tmp/` to `fs_write` and the Landlock env in lock-step; harvest moves
  `out/` to the artifacts dir and records paths; the ephemeral tree is wiped
  after harvest; a traversal/odd `task_id` is rejected.
- Integration: a worker spawn writes to `out/`, the host harvests it, the file
  survives task finalize while `in/`+`tmp/` are gone. Both hosts (bwrap/Seatbelt).

**Workstream B (mail worker):**
- Unit (both hosts, hermetic, fake HTTP client à la web-fetch `FakeGet`): `mail.*`
  dispatch; unknown method → `MethodNotFound`; param validation; REST→JSON-RPC
  mapping; token never echoed; `get_attachment` filename is traversal-safe and
  collision-safe; bytes go to `out/`, not the result.
- Manifest/containment (both hosts): `Net::Allowlist` contains *only* the
  endpoint; params can't widen it; token sourced from vault; `fs_write` is exactly
  the workspace paths.
- Integration (`#[ignore]`, opt-in): against a live `localmail serve` with a
  seeded archive — real `search` (incl. a paginated `next_cursor`), `get_message`,
  `get_attachment_text`, and `get_attachment` (original PDF lands in `out/`,
  harvested, byte-identical to source). macOS + DGX.

## Explicitly out of scope (YAGNI)

- MCP-over-OAuth uplink; Python/models in the sandbox; worker → Postgres.
- Any write path to mail (send / delete / modify).
- base64-inline attachment bytes (superseded by the `out/` channel).
- Flight/booking-specific extraction logic.
- Firecracker micro-VM entry (and VM-side `out/` persistent-store mount).
- Fleet-wide Workspace activation (only the mail worker opts in this pass).
- Artifacts-dir retention / GC policy (operator cleans for now).
- Reusing kastellan's embed-broker for query embedding (localmail owns embedding).

## Open questions (pinned at plan time against the live localmail)

- **`POST /v1/search` request-body + response shape** (hit fields, snippet, cursor
  token) — `serve/routes/search.py`.
- **localmail CLI subcommand** to create an API user + mint its token.
- **`get_message` attachment-list tweak** — add `content_type` + `size`
  (localmail-side change, [hherb/localmail#196](https://github.com/hherb/localmail/issues/196)).
- **Workspace env contract** — the exact env var(s) the worker reads for its
  `out/` path (align with `tool_host::ENV_WORKER_SCRATCH` conventions).
- **Harvest destination + same-filesystem move vs copy**, and the
  `$KASTELLAN_ARTIFACTS_ROOT` default.
- **HTTPS-vs-HTTP** for remote endpoints (assume HTTPS for LAN/VPN; plain HTTP
  only for `127.0.0.1`).
