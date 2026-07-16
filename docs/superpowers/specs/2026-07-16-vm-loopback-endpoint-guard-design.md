# Resolve-time loopback-endpoint guard for force-routed net workers (#452 + #429)

**Date:** 2026-07-16
**Issues:** [#452](https://github.com/hherb/kastellan/issues/452) (Misconfigured guard, primary), [#429](https://github.com/hherb/kastellan/issues/429) (embed-endpoint warning)
**Status:** design approved; implementation this session.
**Scope decision (operator, 2026-07-16):** the guard covers **any force-routed
mode** — micro-VM (always force-routed) ∪ host mode with
`KASTELLAN_EGRESS_FORCE_ROUTING` enabled — not just the VM mode #452 was filed
against. The mechanism is identical in both, and the supervised DGX deployment
runs host workers force-routed.

## ⚠️ Revision after final review (operator decision, 2026-07-16)

The final whole-branch review found the original premise of this spec — "a
force-routed loopback endpoint is unreachable; the proxy denies the loopback
CONNECT" — **false for IP literals**. The egress proxy has an ORIGINAL
(slice #1, commit `4f6f5857`), test-pinned
(`decide_allows_literal_loopback_when_allowlisted`), live-relied-upon
(`web_research_vm_force_route_daemon_e2e`, #448 arc) **allowlisted-literal
carve-out**: `decide()` dials an operator-allowlisted literal IP with the SSRF
range check skipped ("operator intent is explicit" — a literal cannot be
DNS-rebound). Both guarded workers derive `Net::Allowlist` from the endpoint,
so a literal loopback endpoint (`http://127.0.0.1:8888/…`) **works**
force-routed. Only RFC 6761 `localhost` / `*.localhost` **names** take the
hostname path (resolve → loopback → range-denied) and are genuinely dead.

**Operator decision: Option A — keep the carve-out, narrow the guard.** The
shipped design therefore flags **`localhost`-name endpoints only**:

- `endpoint_host_is_local` became **`endpoint_is_localhost_name`** (Domain arm
  only; literal IPs are never flagged — they are reachable via the carve-out).
- The `Misconfigured` details and the #429 warning now name the literal-IP form
  (`http://127.0.0.1:<port>`) as a remedy alongside routable hosts / brokers.
- Core's dependency on `kastellan-net-classify` was dropped (the narrowed
  predicate is pure name logic); the crate extraction itself **stands** —
  egress-proxy consumes it, and the range list keeps its single pure home.
- Test expectations for literal endpoints flipped to `Register` (pinned as the
  Option-A policy), with `localhost`-name variants pinning the guard.
- The pre-existing docstrings claiming "loopback is unreachable in VM mode"
  (#428/#451 era) were corrected in both workers; the
  `dgx-force-routing-deploy-facts` memory note was corrected likewise.

Sections below describe the original (pre-revision) reasoning where they speak
of loopback/private literals being blocked — read them through this revision.

## Problem

When a net worker's egress **force-routes** through the host MITM egress proxy,
the proxy SSRF-blocks loopback / RFC1918 / CGNAT (the DNS-rebinding defense).
This happens in two modes: **micro-VM mode** (a `Net::Allowlist` VM worker always
force-routes — `linux_firecracker/plan.rs` refuses to boot one without a
`proxy_uds`) and **host mode with `KASTELLAN_EGRESS_FORCE_ROUTING=1`** (the
supervised-deployment default). So if an operator configures a **loopback
endpoint** (e.g. `http://127.0.0.1:8888/search`) for a force-routed worker, the
worker boots fine but **every request fails** — the proxy denies the loopback
CONNECT. The tool registers and looks healthy; the failure only surfaces at
request time and reads like a transport/CA/boot bug. This is a silent operational
footgun (and exactly the pre-#440 failure the DGX deployment hit live — see the
`dgx-force-routing-deploy-facts` memory note).

Two endpoints are affected:

1. **SearxNG endpoint** (`web-search`, `web-research`) — *required*. A loopback
   value in a force-routed mode means the worker reaches **nothing** → a hard
   misconfiguration (#452).
2. **Embed endpoint** (`web-research` only) — *optional*. A loopback value in a
   force-routed mode without the embed-broker is unreachable → the worker
   **degrades hybrid→lexical ranking** but still works → a soft downgrade (#429),
   warn-only.

### Per-worker asymmetry (important)

- **web-search** has a dedicated **search-broker** escape hatch
  (`BrokerSpec::search`, #440/#451): with `KASTELLAN_WEB_SEARCH_USE_BROKER=1` a
  force-routed worker (host or VM) reaches a loopback SearxNG through the
  host-side broker. So the guard only fires when the broker is **not** enabled.
- **web-research** has only an **embed-broker** (`BrokerSpec::embed`). Its SearxNG
  endpoint always force-routes directly through the egress proxy in **every**
  force-routed mode, so a loopback SearxNG has **no escape hatch** today (a real
  `web-research × search-broker` feature is a separate arc — see the companion
  spec). The guard therefore fires in any force-routed mode.

The guard is **not** a substitute for the broker — web-search keeps *both* (the
broker is the escape hatch; the guard is the "you forgot to enable it" safety net).

## Non-goals

- **No DNS at resolve time.** A real hostname that later rebinds to loopback is
  caught by the authoritative **connect-time** proxy SSRF check. This guard only
  catches the operator-typed **literal** footgun (`127.0.0.1`, `[::1]`,
  RFC1918/CGNAT literals, `localhost`). Resolve-time DNS would be flaky and
  redundant with the connect-time boundary.
- **No change to `net_entries`.** A loopback entry stays correct for
  **non-force-routed host mode** (which can reach a loopback backend); it is
  merely inert when force-routed. We do not filter it out.
- **Not a containment change.** Current behaviour is already fail-closed (the
  worker reaches nothing it shouldn't). This is a usability / observability fix.

## Design

### 1. New shared pure crate `kastellan-net-classify`

Mirrors the `kastellan-leak-scan` precedent (a pure crate shared by core +
egress-proxy so a security-critical predicate has one home and cannot drift).

- **Move** `is_denied_range(ip: IpAddr) -> bool` and its private helpers
  (`is_denied_v4/v6`, `is_cgnat_v4`, `is_reserved_v4`, `is_unique_local_v6`,
  `is_link_local_v6`, `embedded_transition_v4`) plus **all 13 unit tests** out of
  `workers/egress-proxy/src/ssrf.rs` into the new crate. Pure `std::net` only — no
  new deps, cross-platform, Mac-buildable.
- **egress-proxy** depends on the crate; `proxy.rs` re-points
  `use crate::ssrf::is_denied_range` → `use kastellan_net_classify::is_denied_range`.
  `ssrf.rs` is deleted (its doc-comment about the literal-IP carve-out moves to the
  crate). Behaviour byte-identical.
- **core** depends on the crate (runtime).

Crate `description`: "Pure IP-range classifier: the SSRF / DNS-rebinding deny
predicate shared by the egress proxy (connect-time containment) and core
(resolve-time endpoint sanity checks)."

### 2. Core helper module `core/src/workers/endpoint_guard.rs`

Cross-platform pure logic (Mac-testable) — called from both the host paths (all
platforms) and the Linux VM branches:

```rust
/// True iff the URL's host is a loopback/private literal or a `localhost` name —
/// i.e. unreachable through the force-routed egress proxy (which SSRF-blocks these).
/// A real remote hostname returns false: resolve-time cannot know its address
/// without DNS, and the connect-time proxy SSRF check is the authoritative guard.
pub(crate) fn endpoint_host_is_local(endpoint: &str) -> bool {
    match Url::parse(endpoint).ok().and_then(|u| u.host().map(|h| h.to_owned())) {
        Some(Host::Ipv4(a)) => is_denied_range(IpAddr::V4(a)),
        Some(Host::Ipv6(a)) => is_denied_range(IpAddr::V6(a)),
        Some(Host::Domain(d)) => is_local_domain(&d),   // "localhost" / "*.localhost" (RFC 6761)
        None => false,                                  // parse failure / no host → fail elsewhere
    }
}
```

Uses the typed `url::Host` enum (not `host_str()`) so an IPv6 literal is matched
directly, avoiding the `[::1]` bracket pitfall. `is_local_domain` is a private
ASCII-case-insensitive check (`== "localhost"` or `ends_with(".localhost")`).

**The force-routing question** is answered by one shared predicate so the two
workers can't drift from `force_route`'s own semantics:

```rust
/// True iff a Net::Allowlist worker's egress will force-route through the host
/// egress proxy: always in micro-VM mode (plan.rs refuses a NIC), and in host
/// mode iff the operator enabled KASTELLAN_EGRESS_FORCE_ROUTING. Mirrors
/// force_route::env_flag_enabled — widened to pub(crate) with ENV_ENABLE so the
/// flag name + truthiness (1|true|yes|on) have one home.
pub(crate) fn egress_will_force_route(is_microvm: bool, get_env: &dyn Fn(&str) -> Option<String>) -> bool {
    is_microvm || force_route::env_flag_enabled((get_env)(force_route::ENV_ENABLE))
}
```

### 3. #452 — Misconfigured guard (both workers, host + VM paths)

Evaluated right after the mode envs are read, **before** binary discovery and
entry construction, so a dead config never registers:

- **web-search** `resolve()`: `if !use_broker && egress_will_force_route(…) &&
  endpoint_host_is_local(&endpoint)` → `Resolution::Misconfigured { detail }`
  naming `KASTELLAN_WEB_SEARCH_ENDPOINT`, explaining the force-routed egress proxy
  SSRF-blocks loopback, and pointing at `KASTELLAN_WEB_SEARCH_USE_BROKER=1`.
- **web-research** `resolve()`: `if egress_will_force_route(…) &&
  endpoint_host_is_local(&endpoint)` (its broker is embed-only — no search escape
  hatch) → `Resolution::Misconfigured { detail }` naming
  `KASTELLAN_WEB_RESEARCH_ENDPOINT`: the endpoint must be routable in a
  force-routed deployment (until the search-broker arc lands).

An unset/unparseable endpoint never fires the guard (`endpoint_host_is_local` →
false) — the worker keeps today's fail-closed startup behaviour.

### 4. #429 — embed-endpoint warning (web-research only)

Pure decision function (Mac-testable):

```rust
/// Some(message) iff a loopback/private embed endpoint is configured where it is
/// unreachable (force-routed egress, no embed-broker) → silent hybrid→lexical
/// downgrade. None when brokered (reachable via UDS), not force-routed (host can
/// reach loopback), or the embed endpoint is routable/unset.
fn embed_local_warning(force_routed: bool, use_broker: bool, embed_endpoint: Option<&str>) -> Option<String>;
```

`resolve()` logs it once, after the #452 SearxNG guard passes and before entry
construction: `if let Some(w) = embed_local_warning(…) {
tracing::warn!(target: "web_research.resolve", "{w}"); }`. This is the first
`tracing::warn!` in a manifest `resolve()` — deliberately minimal, one call site.
It **warns, not Misconfigured**: the worker still functions (lexical ranking); the
fix is either a routable embed endpoint or `KASTELLAN_WEB_RESEARCH_USE_EMBED_BROKER=1`.

## Testing (TDD — failing tests first)

- **Mac (dev box), cross-platform:**
  - the 13 moved `is_denied_range` tests (now in `kastellan-net-classify`);
  - new `endpoint_host_is_local` unit tests: loopback v4/v6 literal, bracketed
    `[::1]`, RFC1918, CGNAT, link-local, unspecified, `localhost`, `sub.localhost`,
    public host (`searx.example.org`) → not local, public IP → not local, the
    rebinding lookalike `http://127.0.0.1.attacker.com` → not local (it is a
    Domain, not a literal), unparseable → not local;
  - new `egress_will_force_route` unit tests (VM → true regardless of flag; host +
    each truthy form `1|true|yes|on` → true; host + unset/`0`/garbage → false);
  - new `embed_local_warning` unit tests (not force-routed → None; broker on →
    None; force-routed + loopback embed + no broker → Some; routable embed →
    None; unset → None);
  - new **host-path resolve() guard tests** (cross-platform! host mode exists on
    both OSes): web-search host + flag on + loopback endpoint + no broker →
    `Misconfigured`; + broker on → `Register`; + flag off → `Register`
    (byte-identical entry to today); web-research host + flag on + loopback
    SearxNG → `Misconfigured`; + flag off → `Register` unchanged;
  - full `cargo build --workspace` + `clippy --workspace --all-targets -D warnings`
    (egress-proxy re-point compiles on Mac).
- **DGX gate (over `ssh dgx`), Linux-only resolve() integration tests:**
  - web-search: direct-VM + loopback SearxNG → `Misconfigured` (flag irrelevant);
    broker-VM + loopback SearxNG → `Register` (allowed, unchanged); direct-VM +
    **routable** SearxNG → `Register` (no false positive);
  - web-research: VM + loopback SearxNG → `Misconfigured`; VM + routable SearxNG +
    loopback embed (direct) → `Register` (warn path exercised, still registers);
    VM + routable SearxNG + embed-broker + loopback embed → `Register`;
  - full `cargo test --workspace` + `clippy --workspace --all-targets -D warnings`,
    0 `[SKIP]` regressions. **No VM boot / new e2e** — resolve-time logic only.

## Deploy consequence (DGX, note for the operator)

On the live force-routed DGX daemon, any worker currently configured with a
loopback endpoint and no broker is **already dead at request time**; after this
change it becomes **explicitly `Misconfigured`** (unregistered, with a clear
detail message) at daemon startup. If the live web-research rides the loopback
SearxNG, it will unregister until its endpoint is routable or the search-broker
arc lands. Honest, self-healing, and acceptable per the eval-only stance — but
check `journalctl` for the `registry.loaded` delta after the next deploy.

## Files touched

- **new** `net-classify/{Cargo.toml,src/lib.rs}` (+ moved tests); workspace
  `Cargo.toml` members.
- `workers/egress-proxy/{Cargo.toml, src/proxy.rs}`; **delete** `src/ssrf.rs`.
- `core/Cargo.toml` (+dep); **new** `core/src/workers/endpoint_guard.rs`;
  `core/src/workers/mod.rs` (module decl).
- `core/src/worker_lifecycle/force_route.rs` (`env_flag_enabled` + `ENV_ENABLE`
  → `pub(crate)`, no behaviour change).
- `core/src/workers/web_search.rs` + `web_search/tests.rs` (guard + tests).
- `core/src/workers/web_research.rs` (guard + warning + tests) — note this file
  is 813 LOC; the guard adds a few lines. A test-lift split is a *separate*
  backlog item, not folded here.
- Docstring tightening in both fc entries to reference the guard.

## Deferred (companion spec)

The real fix for web-research's missing SearxNG escape hatch — a
`web-research × search-broker` feature (single-broker: search XOR embed) — is
specced separately (`2026-07-16-web-research-search-broker-arc-design.md`). Full
hybrid-in-VM with *both* backends loopback needs multi-broker-per-worker, an
architectural extension deferred with a documented risk analysis in that spec.
