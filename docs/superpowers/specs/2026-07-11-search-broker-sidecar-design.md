# Search-broker sidecar — design

**Status:** design (approved in brainstorming; awaiting spec review)
**Date:** 2026-07-11
**Related:** `2026-07-09-embed-broker-sidecar-design.md` (the precedent this
generalizes), PR #437 (`<tools>` block), PR #439 (web-search endpoint-derived
allowlist), memory `dgx-force-routing-deploy-facts`.

## Problem

The live DGX daemon runs with `KASTELLAN_EGRESS_FORCE_ROUTING=1`. Under
force-routing, a `Net::Allowlist` worker gets a private netns and reaches the
network **only** through the egress proxy, whose SSRF guard
(`workers/egress-proxy/src/ssrf.rs`) denies loopback and every RFC1918 /
link-local / CGNAT range. The operator's SearxNG runs on the DGX at
`127.0.0.1:8888`. So a force-routed `web-search` worker **cannot reach it** —
by design. This is why the screenshot bot said it had no way to answer a
web-search question, and why web-search on the DGX is currently non-functional
even after #437 + #439.

We rejected the alternatives during brainstorming:

- **Public SearxNG** (`searx.kastellan.dev` behind Caddy, IP-restricted): works,
  but exposes an operator service to the public internet, depends on a stable
  DGX egress IP, and adds a server to maintain. Recipe was drafted
  (`scripts/web-search/setup-searxng-public.md`) then set aside.
- **Disable force-routing for web-search:** removes a core containment control.
- **Proxy SSRF exemption for the one loopback host:** widens the proxy's trust
  surface; a per-endpoint allow-hole is exactly what SSRF defense forbids.

## Goal

Let a force-routed, jailed `web-search` worker reach a **loopback** SearxNG
without weakening force-routing, by mirroring the merged **embed-broker**
pattern: a trusted, single-purpose **search-broker sidecar** that runs in the
host network namespace (so it reaches loopback directly), bridges to the jailed
worker over a bound Unix socket, and returns already-parsed results. The worker
keeps **zero** direct network egress in broker mode.

The user's explicit direction: **reuse most of the embed-broker infrastructure**
rather than build a parallel stack, and **generalize
`SandboxPolicy.embed_broker_uds` → `broker_uds`** (a worker binds at most one
broker socket).

## Non-goals

- **VM × broker.** The broker runs host-side only; a Firecracker web-search
  worker is out of scope (web-search has no VM entry today anyway). Deferred
  exactly as embed-broker deferred it.
- **Multiple brokers per worker.** No worker needs both an embed- and a
  search-broker at once. `broker_uds` stays a single `Option<PathBuf>`; if that
  ever changes, revisit as a vec (noted, not built).
- **Per-call engine/language tuning** through the broker. The broker forwards
  `query` + `count`; SearxNG engine selection stays in its `settings.yml`, same
  as the direct path.
- **Remote (non-loopback) search backends.** The direct web-search path already
  handles a public HTTPS SearxNG through force-routing; the broker exists
  specifically for the loopback-under-force-routing case.

## Architecture

```
  ┌─────────────────────── DGX host (force-routing on) ───────────────────────┐
  │                                                                            │
  │   jailed web-search worker            trusted search-broker sidecar        │
  │   (private netns, NO egress)          (HOST netns, Net::Allowlist([searx]))│
  │   ┌──────────────────────┐   UDS      ┌──────────────────────────┐        │
  │   │ BrokeredSearchProvider│──────────▶│ SearchHandler            │──┐     │
  │   │  JSON-RPC search{q,n} │  search.   │  web_common::search()    │  │ loopback
  │   └──────────────────────┘  sock      └──────────────────────────┘  ▼ GET │
  │                                                          127.0.0.1:8888    │
  │                                                          (SearxNG, JSON)   │
  └────────────────────────────────────────────────────────────────────────────┘
```

The broker is **trusted, compiled-in Rust** (like the egress proxy and
embed-broker). Its `broker_policy` sets `proxy_uds: None`, so the sandbox layer
gives it the **host** netns (`--share-net`) and it reaches `127.0.0.1:8888`
directly. Core binds the broker's `search.sock` into the worker's jail at an
identical path (`SandboxPolicy.broker_uds`) and injects
`KASTELLAN_SEARCH_BROKER_UDS`. The worker's `Net::Allowlist` in broker mode is
**empty** — its only I/O is the UDS. The SearxNG host never appears in the
worker's policy.

This is byte-for-byte the embed-broker containment story, with `search`
substituted for `embed` and "no egress at all" (web-search has one backend)
substituted for "drop the embed host from a multi-host allowlist."

## The reuse map

The embed-broker's core-side machinery is almost entirely transport-neutral. The
plan **generalizes it in place** so both brokers share one spawn path, one RAII
bundle, one readiness contract, and one chokepoint. What differs between the two
broker kinds is a small set of string constants (binary name, env keys, socket
filename, scratch prefix) plus the worker-side client seam and the broker binary
itself.

### Reused as-is (generalized, not copied)

| Precedent (embed) | Generalization |
| --- | --- |
| `EmbedBrokerSidecar` (RAII: child + uds + scratch) | `BrokerSidecar` — identical; kind-agnostic |
| `wait_for_broker_ready` / `BrokerReady` | unchanged; already generic |
| `broker_allowlist_from_endpoint` (pure) | unchanged; shared |
| `broker_policy` (host-netns, `WorkerNetClient`, resolver `fs_read`, scratch `fs_write`, `proxy_uds:None`) | shared; env keys come from the broker kind |
| `make_broker_scratch_dir` (sun_path guard, `#251` sweep prefix) | takes the kind's scratch prefix |
| spawn sequence: `derive_lockdown_env` → `spawn_under_policy` → drain both pipes → wait-for-bind → fail-closed cleanup | shared `spawn_broker` |
| `EmbedBrokerConfig` discovery (`discover_binary`, scratch root) | `BrokerConfig` per kind |
| chokepoint `spawn_worker_with_optional_broker` + `rewrite_policy_for_broker` | one chokepoint reads `entry.broker`; rewrite sets `broker_uds` + injects the kind's UDS env |
| `protocol::server::serve` / `Handler` framing | unchanged |
| `kastellan_worker_web_common::search::{validate_endpoint, search}` | the broker calls these directly (already exist) |
| worker-side `Embedder` / `choose_embedder` precedence seam | mirrored as `SearchProvider` / `choose_search_provider` |

### New (thin)

- **`workers/search-broker`** — `kastellan-worker-search-broker` binary. Mirrors
  `workers/embed-broker` main.rs + lib.rs: bind UDS → `lock_down()` → serve
  JSON-RPC `search{query, count?}` → forward to loopback SearxNG via
  `web_common::search::search()` with the existing count cap (fail-closed).
- **worker-side `BrokeredSearchProvider`** in `workers/web-search/src/handler.rs`
  — JSON-RPC client over the broker UDS, mirroring `BrokeredEmbedder`.
- **web-search manifest broker mode** — `KASTELLAN_WEB_SEARCH_USE_BROKER=1`
  drops the SearxNG host from the worker's `Net::Allowlist` (empty), omits the
  direct-endpoint env, and declares `entry.broker = Some(BrokerSpec::search(endpoint))`.

## Components

### A. Sandbox: `embed_broker_uds` → `broker_uds` (decided)

Rename the `SandboxPolicy` field (currently `embed_broker_uds: Option<PathBuf>`,
~81 references across ~24 files, almost all `_uds: None` literals). Behaviour is
byte-identical when `None`; the bind logic (bwrap `--bind`, Seatbelt path allow,
absolute-path + no-`..` validation) is unchanged, only renamed. This is a
mechanical rename slice with the existing tests re-pointed. The label in the
validation error message and the two Linux/macOS bind tests move with it.

### B. Core: generalized broker spawn

Introduce a broker **kind** descriptor carrying the per-kind constants, and
route both brokers through one spawn path:

- `BrokerKind` — a small struct of `&'static str`: `broker_bin_default`,
  `bin_env`, `endpoint_env` (what the broker binary reads for its backend URL),
  `uds_env` (what core injects into the worker), `uds_file` (socket basename),
  `scratch_prefix` (for the `#251` sweep). Two consts: `EMBED` and `SEARCH`.
- `BrokerConfig { kind: &'static BrokerKind, broker_bin, scratch_root }` — the
  daemon-level discovered-binary config (replaces `EmbedBrokerConfig`).
- `BrokerSpec { kind: &'static BrokerKind, endpoint }` — the per-worker
  declaration on `ToolEntry.broker` (replaces `ToolEntry.embed_broker`). The
  embed-only `model` moves entirely into the manifest's worker-env construction,
  where it already lives (`spec.model` is never read at spawn today).
- `spawn_broker(cfg, spec, backend) -> (BrokerSidecar, PathBuf)` — the shared,
  generalized `spawn_embed_broker`.
- `rewrite_policy_for_broker(policy, uds, kind)` sets `broker_uds` and injects
  `kind.uds_env`.
- The daemon threads the discovered configs (embed + search) through the three
  lifecycle managers (`composite`, `manager`, `idle_timeout`). See the **open
  decision** below for how — this is the one place the shape is not yet fixed.

`web-research`'s manifest changes from `embed_broker: Some(EmbedBrokerSpec::new(
embed_endpoint, model))` to `broker: Some(BrokerSpec::embed(embed_endpoint))`,
with `model` set in its worker env exactly as today. The embed-broker e2e tests
(`embed_broker_egress_e2e.rs`, `embed_broker_spawn_e2e.rs`) re-point to the
generalized names; their assertions (zero embed egress, hybrid ranking) are
unchanged.

### C. `kastellan-worker-search-broker` crate

`workers/search-broker/{Cargo.toml, src/main.rs, src/lib.rs}`, modelled on
`workers/embed-broker`:

- **main.rs:** read `KASTELLAN_SEARCH_BROKER_UDS` + `KASTELLAN_SEARCH_BROKER_ENDPOINT`;
  install the rustls provider only for an `https` endpoint (loopback `http` skips
  it); build the transport via `web_common::http::make_get`; **bind the UDS
  before `lock_down()`**; serve.
- **lib.rs:** `SearchHandler` implementing `protocol::server::Handler` for the
  single `search` method. Builds a `HostAllowlist` from the endpoint host (single
  endpoint = its own allowlist, mirroring the worker), validates the endpoint
  (`http` allowed for loopback), and forwards `search{query, count?}` to
  `web_common::search::search()` with the existing `MAX_COUNT` cap. Any transport
  / status / parse error becomes an `OPERATION_FAILED` `RpcError`; an empty query
  is `INVALID_PARAMS`. The result envelope is `{results: Vec<Hit>}`.
- The broker reaches SearxNG directly (host netns + `Net::Allowlist([searx host:port])`).

### D. web-search worker: `SearchProvider` seam + broker-mode manifest

Mirror the web-research embedder seam:

- `trait SearchProvider { fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError>; }`
- `DirectSearchProvider` — wraps `transport + endpoint + allowlist`, calls
  `web_common::search::search()` (today's behaviour, extracted behind the trait).
- `BrokeredSearchProvider` — JSON-RPC `search{query,count}` over the broker UDS,
  decodes `{results: Vec<Hit>}`. Mirrors `BrokeredEmbedder` (connect, line-framed
  `read_capped_record`, map a JSON-RPC error to a distinct variant, not a
  transport failure).
- `choose_search_provider(broker_uds, endpoint)` — broker UDS wins when set;
  blank counts as unset. Mirrors `choose_embedder`.
- `WebSearchHandler::from_env` restructures: if `KASTELLAN_SEARCH_BROKER_UDS` is
  set → `BrokeredSearchProvider` (no direct endpoint required); else the current
  path (`KASTELLAN_WEB_SEARCH_ENDPOINT` required → `DirectSearchProvider`). This
  removes the "endpoint always required" precondition **only** in broker mode.
- `web-common::parse::Hit` gains `Deserialize` (it has `Serialize` today) so it
  survives the broker round-trip; the serialize/deserialize field names must stay
  symmetric (note the `content`→`snippet` rename in the output shape).
- **Manifest broker mode:** `KASTELLAN_WEB_SEARCH_USE_BROKER=1` →
  `Net::Allowlist([])` (empty — no direct egress), omit the direct endpoint +
  allowlist env, and `entry.broker = Some(BrokerSpec::search(endpoint))` (the
  endpoint the broker forwards to). When the flag is unset, the entry is
  byte-identical to today's direct entry.

### E. DGX end-to-end + cutover

A Linux/DGX e2e proving: force-routing on, SearxNG on loopback, web-search in
broker mode → a real `web.search` answer with **zero** worker egress to SearxNG
(the worker's private netns has no route; the broker holds the only one). Then
the production cutover (deploy, re-add `KASTELLAN_EGRESS_FORCE_ROUTING=1` to the
regenerated unit + `daemon-reload`, set
`KASTELLAN_WEB_SEARCH_ENDPOINT=http://127.0.0.1:8888/search` +
`KASTELLAN_WEB_SEARCH_USE_BROKER=1`, restart, test over Matrix).

## Data flow

1. Planner emits a `web.search` task (it now knows the tool exists — #437).
2. Core cold-spawns the web-search worker in broker mode: first `spawn_broker`
   (search kind) → `search.sock`; then `rewrite_policy_for_broker` binds it into
   the jail as `broker_uds` and injects `KASTELLAN_SEARCH_BROKER_UDS`; then the
   worker spawns (force-routed as usual — the broker fields survive the
   force-routing policy clone).
3. Worker `from_env` sees the broker UDS → `BrokeredSearchProvider`.
4. On a call, the worker sends JSON-RPC `search{query,count}` over the UDS.
5. The broker validates, GETs `http://127.0.0.1:8888/search?q=…&format=json`
   directly (host netns), parses SearxNG JSON to `Vec<Hit>`, returns
   `{results:[…]}`.
6. Worker returns `{query, results, count}` to core → the agent → Matrix.

## Security properties (must hold)

- **Containment invariant preserved.** A compromised web-search worker in broker
  mode has an empty `Net::Allowlist` and a private netns — it can reach *nothing*
  on the network except the one broker UDS. It cannot reach SearxNG, loopback,
  the LAN, or the internet directly.
- **Broker egress is minimal.** The broker's own `Net::Allowlist` is exactly
  `[searx host:port]`; `WorkerNetClient` profile; lockdown env derived (the
  `e70174b` DNS lesson). It forwards only `query` + `count`; the LLM cannot
  influence the URL beyond the query string (same property the direct path has).
- **Fail-closed.** A worker with `entry.broker = Some(..)` but no daemon
  `BrokerConfig` for that kind (broker binary absent) is **refused** — the
  manifest already dropped the endpoint from egress, so a silent fallback would
  leave the worker unable to search *and* skip the containment intent. An
  unparseable/hostless broker endpoint is rejected before spawn. The broker
  enforces the count cap before any backend GET.
- **Force-routing untouched.** No SSRF exemption, no proxy change; force-routing
  and the egress proxy behave exactly as before.

## Error handling

| Condition | Result |
| --- | --- |
| Broker binary not discovered, but a worker requests it | refuse to spawn (fail-closed `Err`) |
| Broker endpoint unparseable / hostless | reject before minting scratch / spawning |
| Broker never binds its UDS within the deadline | kill + reap, `Err` (timeout) |
| Broker exits before binding | surface its real exit status, not a bind-timeout |
| Worker cannot connect to the broker UDS | `SearchError::Transport` → `OPERATION_FAILED` |
| Broker returns a JSON-RPC error (bad status, parse) | distinct broker-error variant → `OPERATION_FAILED`, not mislabelled transport |
| Empty query at the broker | `INVALID_PARAMS` |
| `count` over `MAX_COUNT` | clamped by the broker (existing `search()` behaviour) |

## Testing strategy

TDD throughout (RED → GREEN). Split by what a Mac can verify vs. what is
DGX-gated:

- **Mac-verifiable (unit / in-process):** the `broker_uds` rename tests; the
  generalized `spawn_broker` pure/hermetic tests (allowlist derivation, policy
  shape, scratch sun_path guard, readiness/early-exit, malformed-endpoint
  rejection) re-pointed from the embed-broker suite; the search-broker
  `SearchHandler` over a `FakeGet`; `choose_search_provider` precedence;
  `BrokeredSearchProvider` round-trip against a stub UDS broker (mirroring the
  embed stub-broker tests); the web-search manifest broker-mode entry
  (empty allowlist, `entry.broker` set, no direct endpoint env); `Hit`
  round-trips serialize→deserialize.
- **DGX-gated (real bwrap + real SearxNG):** slice E — the force-routed
  zero-egress e2e, driven over `ssh dgx`.

## Slicing (→ implementation plan)

- **A.** Sandbox `embed_broker_uds` → `broker_uds` rename (mechanical, all green).
- **B.** Generalize the core broker spawn (`BrokerKind`/`BrokerConfig`/`BrokerSpec`/
  `spawn_broker`/`entry.broker`); re-point web-research + embed e2e.
- **C.** `kastellan-worker-search-broker` crate (binary + handler + lib tests).
- **D.** web-search `SearchProvider` seam (`Direct`/`Brokered`/`choose`) + `Hit`
  `Deserialize` + manifest broker mode.
- **E.** DGX force-routed zero-egress e2e, then the production cutover.

A and B touch merged, tested code; C and D are additive; E is the live gate.

## Open decision (for spec review)

**How far to unify the core broker abstraction.** Two shapes both honour "reuse
most" and the decided `broker_uds` rename:

1. **Full unification (recommended).** One `entry.broker: Option<BrokerSpec>`,
   one `spawn_broker`, one `BrokerKind`-parameterized config threaded through the
   lifecycle managers. Embed-broker is refactored onto these names. **Pro:**
   lowest long-term duplication, single spawn/chokepoint path, matches
   "generalize." **Con:** larger diff touching the merged embed path + its two
   e2e tests (behaviour-preserving, test-covered).
2. **Shared primitives, parallel field.** Keep `entry.embed_broker` and the embed
   config threading as-is; extract only the stateless spawn primitives into a
   shared helper both call; add a parallel `entry.search_broker` +
   `SearchBrokerConfig` threaded as a second `Option<Arc<_>>`. **Pro:** near-zero
   churn to merged code. **Con:** two config types + two Options threaded through
   three managers + two chokepoint branches — the duplication "reuse most" was
   meant to avoid.

Recommendation: **(1)**. The extra churn is mechanical and fully test-guarded,
and it produces the single, clean broker abstraction the "generalize to
`broker_uds`" decision points at. If minimizing risk to the merged embed path
outweighs that, (2) is the fallback.
