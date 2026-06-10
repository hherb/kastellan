# Egress proxy — slice #2: unbypassable force-routing + live worker transport

**Date:** 2026-06-10
**Status:** Design approved; implementation plan pending.
**ROADMAP:** 141 (Egress proxy — per-worker host allowlist, TLS pinning, audit logging).
**Scope:** Slice **#2 of four**. Slice #1 (boundary host-allowlist + SSRF/IP defense — the
proxy *mechanism*) shipped (PR #240). This slice makes the proxy **live**: it routes the real
`web-fetch`/`web-search` workers through the sidecar and makes that routing **unbypassable** by
a compromised worker. Slices #3 (TLS-intercept + co-located credential-leak scanner) and #4
(TLS pinning for the frontier path) get their own spec → plan → implementation cycles and are
**out of scope here**.

---

## Problem

Slice #1 built a sandboxed per-worker CONNECT proxy that enforces the host allowlist + SSRF/IP
defense, but **nothing routes through it**. Net workers still run under bwrap `--share-net` /
Seatbelt `(allow network*)` with the **full host network namespace** and merely *self-enforce*
their allowlist in worker code. A genuinely *compromised* worker (malicious dep, injected
fetched content, LLM-driven) can ignore that self-check and reach any endpoint the host can.

So the threat-model invariant — "worst-case compromise of one tool reaches at most that tool's
explicitly allowlisted endpoints" — is **still not true at the network layer**. Slice #2 makes
it true: after this slice, a net worker has **no network-namespace route to any IP**; its only
egress is a bind-mounted UNIX socket that terminates at the allowlist+SSRF-enforcing proxy.

## Goals (slice #2)

1. **Worker-side transport** — a CONNECT-over-UDS `HttpGet` implementation in `web-common`,
   selected automatically when the proxy env is present; both net workers inherit it with no
   logic change.
2. **Spawn-time hookup** — bringing up a `Net::Allowlist` worker auto-spawns its sidecar
   (slice-#1 `spawn_sidecar`), injects the UDS env, bind-mounts the socket, ties sidecar
   teardown to worker teardown, and ingests the sidecar's decision stream into `audit_log`.
3. **Unbypassable force-routing** — the worker's OS sandbox physically denies all direct egress
   so the proxy UDS is the *only* path out. Kernel-enforced on Linux (private netns); equal
   guarantee on macOS (Seatbelt outbound-UDS-only filter, with the `container` backend as a
   parity fallback).
4. **Port-scoped allowlist (folds in #241)** — the proxy constrains `host:port`, not just host,
   so an allowlisted web host is reachable only on its declared port.
5. **Fail-closed** — a net worker is never spawned without a live proxy.

## Non-goals

- TLS interception / credential-leak scanning (slice #3). TLS stays **end-to-end worker↔origin**
  in this slice; the proxy tunnels ciphertext via CONNECT and never sees plaintext.
- TLS pinning for the frontier/LLM path (slice #4).
- New worker features (categories, pagination, etc.).
- Tunnel idle/resolve-timeout tuning against the live workload — deferred to #242.

---

## Locked-in design decisions (from brainstorming)

- **Deliverable:** this spec only; implementation is a follow-on via writing-plans.
- **macOS bar:** *equal guarantee, container fallback.* Commit to the Seatbelt
  outbound-UDS-only filter, gated by a real on-host probe; if the probe can't prove AF_INET is
  denied, net-egress workers on darwin run under the `MacosContainer` backend (real VM netns).
- **Transport:** *two impls + env-selected factory.* Keep `ReqwestGet` for the dev/no-proxy
  path; add `ProxyConnectGet` (hyper + tokio-rustls over the UDS) for the proxied path. Confines
  new bespoke HTTP code to the one trust-boundary transport; keeps audited reqwest (incl.
  gzip/brotli decompression) for the common path. (Single-hyper was weighed and rejected: it
  would lose reqwest's transparent decompression — a correctness risk for `web-fetch` on
  arbitrary pages — and enlarge the bespoke surface on the dev path, while its "fewer deps"
  benefit is nil because `reqwest`/`tokio` stay in the lock graph via `llm-router`.)
- **Force-routing mechanism:** Linux private netns (no veth) + bind-mounted proxy UDS, **primary**;
  an AF_INET/AF_INET6 `socket(2)` domain-deny in the worker seccomp profile is **optional belt**,
  not required (the netns already has no route).
- **#241 port-scoping** is folded into this slice (real traffic now flows — natural moment).

---

## Architecture & spawn-time data flow

The net-worker bring-up becomes a **coupled pair** (worker + its proxy) instead of a lone
process. When a worker's policy is `Net::Allowlist(endpoints)`:

```
   build_tool_registry  ──►  resolved allowlist (DB tool_allowlists, already prefetched)
   (single source of truth)        │ host:port endpoints
                                    ▼
   spawn net worker  ──►  1. core::egress::spawn_sidecar(backend, proxy_bin, allowlist,
   (Net::Allowlist)            shared_dir, worker_name)
                               → proxy in its OWN sandbox: Net::ProxyEgress (real netns),
                                 binds <shared>/egress.sock; bounded-wait until it exists
                                 (already built in slice #1)
                         ──►  2. rewrite the worker policy:
                               · inject  KASTELLAN_EGRESS_PROXY_UDS = <shared>/egress.sock
                               · bind-mount <shared>/egress.sock into the worker jail
                                 (IDENTICAL host↔jail path — #243)
                               · Net::Allowlist  →  PRIVATE netns (bwrap --unshare-net,
                                 drop --share-net)
                               · drop /etc/resolv.conf from fs_read (worker no longer resolves)
                         ──►  3. spawn the worker under the rewritten policy
                         ──►  4. decision-ingest task: sidecar.stdout → decision_to_audit
                                 → audit_log (core DB pool; proxy never touches PG)
                         ──►  5. bundle SidecarHandle into SupervisedWorker so worker-terminal
                                 teardown also kills the sidecar + removes the socket
                                 (1:1 lifecycle coupling)
```

Worker side: `web-common`'s factory sees `KASTELLAN_EGRESS_PROXY_UDS` and returns
`ProxyConnectGet`. Every `web.fetch`/`web.search` GET dials the UDS, sends `CONNECT host:port`,
reads `200`, then does its own rustls handshake (https) or raw HTTP (loopback SearxNG) over the
tunneled stream. Redirects stay disabled — the worker drives them and re-checks its allowlist
per hop, each hop a fresh CONNECT through the proxy.

**Invariant delivered:** a net worker cannot be spawned without its proxy, and once spawned it
has **no** network-namespace route to any IP — its only egress is the UDS, which terminates at
the allowlist+SSRF-enforcing proxy. Worker compromise can no longer reach an off-allowlist (or
off-port) endpoint. This is the threat-model win slice #1 set up but could not deliver.

---

## Component 1 — `web-common` CONNECT-over-UDS connector

New module `workers/web-common/src/proxy_connect.rs`, exposing `ProxyConnectGet` implementing
the existing `HttpGet` seam. Owns a small current-thread tokio runtime (mirrors what
`reqwest::blocking` already does — tokio is already in the worker lock graph, so **no new seccomp
surface**). Per `get(url)`:

1. **Dial** the UDS at `KASTELLAN_EGRESS_PROXY_UDS` (`tokio::net::UnixStream::connect`).
2. **Write** `CONNECT <host>:<port> HTTP/1.1\r\nHost: <host>:<port>\r\n\r\n`. `host` is the
   URL host **verbatim** (name, not a resolved IP — the proxy resolves + range-checks). `port`
   from the URL (443 https default; explicit for loopback http). IPv6 literals arrive bracketed
   from `Url::host_str()` (`[2606:4700::1111]`) — the form both the CONNECT line and the proxy's
   bracketed-IPv6 parser want; pass it through, do not re-bracket.
3. **Read** the proxy's status line; require exactly `200`. Any non-200 (the `403`/`502`/`400`
   the proxy already emits) → `Err` surfaced to the worker as a fetch failure. Cap the
   response-head read (mirror the proxy's 8 KiB head cap) so a misbehaving peer can't grow the heap.
4. **Layer transport over the tunneled stream:**
   - `https` → `tokio-rustls` client handshake (SNI + cert verification against the worker's root
     store — unchanged; the proxy never sees plaintext). Build the rustls `ServerName` from
     `url.host()` (domain → `try_from(name)`, IP literal → `ServerName::IpAddress`), **not** from
     the bracketed `host_str()` string — brackets are a URL-authority artifact, not an SNI identity.
   - `http` (loopback SearxNG only) → use the stream raw.
5. **Issue the GET** (hyper client, HTTP/1.1), enforce the same `TIMEOUT_SECS` (per-phase) and
   `MAX_BODY_BYTES` cap-while-reading, decode into the existing
   `RawResponse { status, location, content_type, body }`.

**Decompression (Option-2 risk made explicit):** this hand-rolled path requests
`Accept-Encoding: identity` for v1 (simplest correct) so the extractor still gets decoded bytes.
Transparent gzip/brotli is a follow-up only if a real origin refuses `identity`.

**Factory:** `web_common::http::make_get(user_agent) -> Box<dyn HttpGet>` returns
`ProxyConnectGet` iff `KASTELLAN_EGRESS_PROXY_UDS` is set, else `ReqwestGet`. `web-fetch` and
`web-search` swap their direct `ReqwestGet::new(...)` for `make_get(...)` — the **only** change
in either worker crate.

**AGPL check:** `hyper`, `tokio`, `tokio-rustls`, `http-body-util` are MIT/Apache-2.0 —
compatible; all already in the lock graph from the slice-#1 work.

---

## Component 2 — `tool_host` hookup, lifecycle coupling & Linux force-routing

**Where it hooks.** `spawn_worker` is the structural chokepoint, but it is generic over backend
and policy-agnostic today. Add a coupling helper so a `Net::Allowlist` worker **cannot** be
spawned without its proxy:

- New `core::egress::spawn_net_worker(backend, proxy_bin, spec, allowlist, shared_dir)` runs the
  5-step flow above and returns a `SupervisedWorker` whose teardown also fells the sidecar.
  Plain (`Net::Deny`) workers keep the existing `spawn_worker` path untouched.
- `SupervisedWorker` gains one additive field — `egress: Option<EgressSidecar>` bundling the
  `SidecarHandle` + the decision-ingest task handle — dropped / awaited on `close()`; `None` for
  plain workers. **No wrapper type** (a parallel `NetWorker` would force every net-worker call
  site to special-case a second type; extending `SupervisedWorker` keeps one uniform teardown
  path). Field drop order: worker client/pipes → sidecar (proxy dies) → ingest task (sees EOF,
  finishes its last audit rows).
- **Fail-closed:** if `spawn_sidecar` errors or times out, the net worker is **never** spawned
  (slice-#1 `spawn_sidecar` already bounded-waits and returns `Err`). No
  "spawn-without-proxy" path — same posture as the no-unsandboxed-escape-hatch invariant.

**Linux enforcement (the bwrap change).** Today `build_argv` emits `--share-net` for both
`Allowlist` and `ProxyEgress`. The change:

- `Net::ProxyEgress` (the proxy itself) → `--share-net` (unchanged; needs the real netns).
- `Net::Allowlist` (the worker) → **drop `--share-net`** so `--unshare-all` leaves it in a
  private netns with only a down loopback. No veth, no default route, no DNS.
- The proxy UDS is bind-mounted into the worker jail at a path **identical inside and out**
  (#243 host↔jail path-identity). The shared dir is created on the host, passed as `fs_write` to
  the proxy (binds the socket) and bind-mounted into the worker (connects). Connecting a UNIX
  socket needs write access to the socket inode, so the worker gets a `--bind` (rw) of the path.
- *(Optional belt:)* add an AF_INET/AF_INET6 `socket(2)` domain-deny to the worker seccomp
  profile. Hardening, not required for correctness — the netns already has no route.

This is the new cross-platform-divergent bwrap path the HANDOVER flagged. The #243 DGX checks
(proxy seccomp permits `bind`/`listen`/`accept`; worker seccomp permits AF_UNIX `connect`; host↔jail
UDS path identity) become **gating acceptance criteria**, runnable natively on the DGX over the
operator's WireGuard SSH (native-Linux verification is reachable from the dev Mac — see
"Verification").

---

## Component 3 — macOS enforcement & the allowlist source / port-scoping

**macOS force-routing (equal guarantee, container fallback).**
- *Primary:* the worker's Seatbelt profile (`Net::Allowlist` arm) flips from `(allow network*)`
  to **deny-all-outbound except the proxy UDS**:
  `(deny network-outbound)` + `(allow network-outbound (remote unix-socket (path-literal "<uds>")))`.
  The proxy process keeps `Net::ProxyEgress` → `(allow network*)`.
- *Gating probe:* a tiny program under that profile must (a) **fail** to `connect()` any AF_INET
  address and (b) **succeed** connecting the proxy UDS. The probe outcome decides primary vs fallback.
- *Fallback (if the probe can't prove (a)):* run net-egress workers on darwin under the existing
  `MacosContainer` backend — its Linux micro-VM gets a real private netns, so the identical
  netns mechanism applies inside the VM. Parity at the cost of requiring `container` for egress
  workers on macOS. The spec documents both and which the probe selects.

**Allowlist source & port-scoping (resolves #241).**
The proxy's allowlist is the **same** `tool_allowlists` row the worker already uses
(`build_tool_registry` prefetches it) — single source of truth, no second config. Today the
proxy matches **host-only**; the worker's `Net::Allowlist` entries are `host:port`. This slice
threads the full `host:port` endpoints to the proxy and tightens `proxy::decide` to also
constrain the port, so an allowlisted web host is reachable **only** on its declared port (e.g.
`:443`), closing the "allowlisted host → SSH on :22" gap. A small, well-contained tightening of
the already-tested `decide` + `HostAllowlist`. A **bare-host entry** (no `:port`) stays
port-unconstrained for the literal-IP carve-out + legacy back-compat; force-routed worker
allowlists are always `host:port`, so the weaker form is unreachable for them, and when a
bare-host entry *does* match, `decide` flags it in the audit reason (`allowed:host-only-entry`)
so the port-unconstrained grant is visible rather than silent.

**Decision-ingest (closing the slice-#1 loop).**
A core-side async task per sidecar reads `SidecarHandle::stdout()` line-by-line, maps each via
the already-built pure `decision_to_audit`, and inserts into `audit_log` through the core DB
pool (proxy never touches PG — invariant intact). Bounded by worker lifetime; on teardown the
proxy dies, stdout hits EOF, the task drains and exits.

---

## Testing & acceptance gates

**Unit (hermetic, both OSes):**
- `web-common::proxy_connect` — CONNECT request-line shape; status-line parse (200 vs
  403/502); head-cap; `identity` encoding pinned; https-vs-loopback-http transport selection.
  Drive against an in-test UDS CONNECT stub (mirrors the proxy's `handle_conn` test rig).
- `core::egress` policy-rewrite — `Net::Allowlist` worker rewrite yields private-netns argv
  (no `--share-net`), injects `KASTELLAN_EGRESS_PROXY_UDS`, binds the UDS path identically, drops
  `/etc/resolv.conf`; fail-closed when `spawn_sidecar` errors.
- `sandbox` builder — new `Net::Allowlist` → `--unshare-net` arm on bwrap; the Seatbelt
  deny-outbound-except-UDS arm; cross-clippy the bwrap arm for `aarch64-unknown-linux-gnu`.
- `decide` port-scoping (#241) — allowlisted host on declared port allowed; same host on
  another port blocked.

**Integration / acceptance gates:**
- **macOS Seatbelt probe (gating):** the deny-AF_INET / allow-proxy-UDS profile actually denies
  a real inet `connect` and permits the UDS — decides primary vs container fallback.
- **Linux force-routing e2e (gating, run natively on the DGX):** a real `web.fetch` round-trips
  through the spawned sidecar to an allowlisted host **and** an off-allowlist host (or off-port)
  is refused **even when the worker tries a direct `connect`** (assert `ENETUNREACH`/no route
  from inside the private netns) — proving the kernel barrier, not the env. Plus the #243 checks
  (proxy seccomp `bind`/`listen`/`accept`; worker seccomp AF_UNIX `connect`; host↔jail UDS path
  identity).
- Decision-stream → `audit_log` persistence (PG-gated; skip-as-pass on the Mac).

**Verification reachability.** Native-Linux runs (private-netns force-routing, the proxy
`accept`/UDS-path under seccomp) are reachable from the dev Mac via the operator's WireGuard
SSH into the DGX, so the Linux acceptance gate is **in-band**, not a parked follow-up.

---

## Staged build order

Each stage compiles + tests green on its own.

1. **Connector** — `proxy_connect.rs` + factory + worker swap to `make_get(...)`. Inert (no env
   set yet) but fully unit-tested.
2. **Sandbox enforcement** — bwrap `--unshare-net` arm + Seatbelt deny-outbound-except-UDS arm
   + the macOS probe + (optional) seccomp domain-deny belt. Builder tests + probe.
3. **Port-scoping (#241)** — tighten `decide` + thread `host:port` to the proxy.
4. **Hookup + lifecycle** — `spawn_net_worker` coupling, env injection, UDS bind-mount,
   decision-ingest task, teardown coupling. The DGX force-routing e2e lands here.

---

## Explicitly deferred

- **#242** tunnel idle/resolve-timeout tuning against the live workload (follow-up; not blocking).
- **Slice #3** — TLS interception + co-located credential-leak scanner (needs a per-instance CA
  the workers trust).
- **Slice #4** — TLS pinning for the frontier/LLM egress path.
- **Transparent gzip/brotli decompression** in `ProxyConnectGet` (only if an origin refuses
  `Accept-Encoding: identity`).
- **Slice 3 lifecycle** (operator surface + SIGTERM grace) from the worker-lifecycle line —
  unrelated, unchanged.

## Open risks

1. **Seatbelt `remote unix-socket` filter fidelity** — if Seatbelt silently ignores the
   outbound-UDS allow rule or fails to deny AF_INET, the probe fails and we take the container
   fallback. The probe is the guard; design doesn't trust the filter unverified.
2. **bwrap private-netns + AF_UNIX bind-mount** — the UDS must remain connectable from inside a
   netns-isolated jail. AF_UNIX is mount-namespace-scoped, not netns-scoped, so this is expected
   to hold; the DGX e2e proves it. If a kernel/bwrap quirk breaks it, fall back to the
   seccomp-domain-deny mechanism (worker stays in host netns, AF_INET `socket()` denied).
3. **Decision-ingest backpressure** — a chatty proxy could outrun the audit insert. Bounded by
   the worker's request rate (one decision per CONNECT) and SingleUse lifetime; revisit only if
   a long-lived IdleTimeout net worker appears.
