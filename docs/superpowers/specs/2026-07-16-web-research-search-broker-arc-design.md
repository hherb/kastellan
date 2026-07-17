# web-research × search-broker arc (follow-up feature — NOT this session)

> **✅ SHIPPED (2026-07-17):** Slice 1 (single-broker search-broker for
> web-research) implemented on branch `feat/web-research-search-broker` (#464),
> plan `docs/superpowers/plans/2026-07-17-web-research-search-broker-xor.md`.
> DGX-verified (VM search-broker live e2e GREEN, zero direct SearxNG egress);
> see the HANDOVER header entry. The multi-broker follow-up below stays deferred.

**Date:** 2026-07-16 (brainstormed alongside the loopback-endpoint-guard spec;
implementation deferred to its own session/PR).
**Motivation (revised 2026-07-16 after the carve-out finding):** web-research's
SearxNG endpoint has **no broker escape hatch** — its only broker is the
embed-broker (`BrokerSpec::embed`). In a force-routed mode (micro-VM always;
host with `KASTELLAN_EGRESS_FORCE_ROUTING=1`, the supervised DGX default) the
egress proxy range-denies what a **hostname** resolves to, so a
`localhost`-name (or any local-name) SearxNG endpoint is dead; a **literal**
loopback endpoint works via the proxy's allowlisted-literal carve-out (see the
guard spec's Revision section). The value of a web-research search-broker is
therefore: (a) serve name-form / non-literal local endpoints, and (b) the
**stronger containment posture** — SearxNG leaves worker egress entirely (zero
direct egress), the same reason web-search's broker exists beyond mere
reachability. The guard spec
(`2026-07-16-vm-loopback-endpoint-guard-design.md`) makes the dead name-form
config an explicit `Resolution::Misconfigured`; **this arc gives web-research
the broker option web-search already has** (#440 host search-broker, #451 VM ×
search-broker).

## Slice 1 (the arc's deliverable): single-broker search-broker for web-research

One worker binds at most one broker socket today (`BrokerKind` is "a plain enum,
not a bitset"; `SandboxPolicy.broker_uds` is a single `Option<PathBuf>`; one vsock
port 1026). Within that model, give web-research a **search-broker** option —
**search XOR embed**, operator-chosen:

| Config (force-routed)                  | Search path      | Embed path                     |
|----------------------------------------|------------------|--------------------------------|
| no broker                              | direct (must be routable, else Misconfigured) | direct (routable) or lexical |
| `USE_EMBED_BROKER=1` (existing)        | direct (must be routable, else Misconfigured) | brokered — loopback OK |
| `USE_SEARCH_BROKER=1` (**new**)        | brokered — loopback OK | direct (routable) or degrades to lexical (warned) |
| both flags                             | `Misconfigured` (single-broker model; see Multi-broker below) |

### Work items

1. **Lift the brokered-search client into `web-common`.** `SearchProvider`,
   `choose_search_provider`, `DirectSearchProvider`, `BrokeredSearchProvider`
   currently live in `workers/web-search/src/handler.rs` only; web-research calls
   `web_common::search::search()` directly and has no provider seam. Move the
   provider seam + brokered client to `web-common` (feature `search`), re-point
   web-search byte-preserved (the established web-common consolidation pattern,
   2026-07-07).
2. **Rework web-research's search step** (`research.rs` + `handler.rs`) to run
   over the lifted `SearchProvider` seam; select `BrokeredSearchProvider` when
   `KASTELLAN_SEARCH_BROKER_UDS` is set. Hermetic tests via a fake provider.
3. **Core manifest entries**: `web_research_search_broker_entry` (host) +
   `web_research_firecracker_search_broker_entry` (VM), carrying
   `BrokerSpec::search(endpoint)`; SearxNG host dropped from `Net::Allowlist`;
   new gate env `KASTELLAN_WEB_RESEARCH_USE_SEARCH_BROKER`. `resolve()` grows a
   three-way broker choice (none / embed / search) × (host / VM); both flags set
   → `Misconfigured` naming the single-broker limitation.
4. **Reuse, no new mechanism**: the `kastellan-worker-search-broker` binary, the
   kind-agnostic spawn chokepoint, and the single vsock-1026 channel are used
   as-is (search XOR embed means no port collision).
5. **DGX gates**: Linux resolve() tests + a manager-level live e2e mirroring
   #451's `web_search_firecracker_egress_e2e` (VM boot → `web.research` against a
   loopback SearxNG via the broker → results with zero direct search egress).
   Rebuild `web-research.ext4` (worker binary changes). Expect #451-shaped
   effort: ~6 subagent tasks + a DGX session.

### Why XOR, not both

The typical fully-local deployment (loopback SearxNG **and** loopback embed)
would want both brokers, but the single-broker model can't express that (one
`broker_uds`, one vsock port, one `BrokerSpec`). Under XOR, choosing the
search-broker costs hybrid ranking when the embed endpoint is also loopback
(degrades to lexical, warned per #429) — an accepted trade-off until/unless
multi-broker lands.

## Multi-broker per worker (deferred; risk analysis)

Full hybrid-in-VM with both backends loopback requires N brokers per worker.
Documented drawbacks (operator-reviewed 2026-07-16) beyond the effort:

- **Doubled trusted-sidecar surface** per worker instance — two host-side
  processes forwarding worker-influenced input to backends.
- **Harder-to-prove containment invariant** — must prove both backend hosts left
  `Net::Allowlist` and the worker can't cross-wire sockets; the "zero embed
  egress" e2e becomes "zero embed AND zero search egress simultaneously".
- **Partial-spawn teardown** — broker A up + broker B fails ⇒ fail-closed
  rollback; the #251 sweep must reap both scratch prefixes.
- **VM vsock port multiplexing** (the riskiest piece) — a second broker port
  (1027), `microvm-init` relaying N channels, the FC plan rewriting N UDS envs;
  port assignment becomes stateful; a mis-wire presents as a silent hang (the
  classic "looks like a boot bug" failure class).
- **`SandboxPolicy.broker_uds` widening to N** — touches bwrap + Seatbelt + FC
  and every construction site of a struct all workers share, for a need only
  web-research has.
- **Config-matrix growth** — {searxng local|routable} × {embed local|routable} ×
  {VM|host} × two gate envs; adds misconfiguration surface while #452/#429 exist
  to reduce it.
- **Narrow marginal benefit** — only binds when *neither* backend can be made
  routable *and* full hybrid ranking in a force-routed mode is required.

**Decision:** implement Slice 1 (XOR) first; revisit multi-broker only if the
both-loopback + force-routed + hybrid combination becomes a real requirement.

## Prerequisites / interactions

- Lands **after** the loopback-endpoint guard PR; the guard's web-research
  `Misconfigured` detail then gains the `USE_SEARCH_BROKER=1` remedy (mirroring
  web-search's message) instead of "make the endpoint routable".
- Issue to file when this arc starts: cite this spec + #452's guard as the
  interim behaviour.
