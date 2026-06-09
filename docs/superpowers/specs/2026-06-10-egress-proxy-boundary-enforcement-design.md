# Egress proxy — slice #1: boundary host-allowlist enforcement + SSRF/IP defense

**Date:** 2026-06-10
**Status:** Design approved; implementation plan pending.
**ROADMAP:** 141 (Egress proxy — per-worker host allowlist, TLS pinning, audit logging).
**Scope:** This is **slice #1 of four** independent subsystems the ROADMAP line bundles. The
other three get their own spec → plan → implementation cycles and are **out of scope here**
(see "Decomposition" and "Explicitly deferred").

---

## Problem

Today a net-worker (`web-fetch`, `web-search`) runs under bwrap `--share-net` /
`Profile::WorkerNetClient` with the **full host network namespace** and merely
*self-enforces* its host allowlist in worker code. A genuinely *compromised* worker (not
just a buggy one) can therefore ignore that allowlist and reach any endpoint the host can —
so the threat-model invariant ("worst-case compromise reaches at most the explicitly
allowlisted endpoints for the one compromised tool") is **not yet true at the network
layer**. Separately, because DNS resolution happens *inside* the jail, an allowlisted public
hostname that resolves (or is rebound) to a private/internal address is still connected to:
the documented SSRF / DNS-rebinding gap (`docs/threat-model.md` §"Network egress").

The egress proxy is the component that closes both gaps. This slice delivers the
boundary-enforcement and SSRF/IP-defense half.

## Goals (slice #1)

1. A proxy process, **one per net-worker**, that every outbound request from that worker is
   routed through — over a **per-worker UDS** bind-mounted into the worker's scratch (see
   "Non-goals" for the bypassability caveat: slice #1 doesn't yet *force* the worker to use it).
2. The proxy enforces the worker's **host allowlist at the boundary** (defense-in-depth layer 2
   behind the worker's existing self-enforcement), reusing the *same* matcher the workers use.
3. The proxy **resolves DNS itself**, rejects private/loopback/link-local/etc. resolved IPs
   (allowlist-aware — see SSRF policy), and **pins** the surviving resolved IP across the
   connection, closing the SSRF/DNS-rebinding gap.
4. Every proxy decision is **audited** (allowed / blocked-allowlist / blocked-ssrf), without
   violating the core-only-Postgres invariant.
5. The proxy process is **itself sandboxed** (net-permissive lockdown, `Net::ProxyEgress`),
   because it parses untrusted bytes from both sides and will hold plaintext secrets in slice #3.
6. **Both existing net-workers keep working**, with the worker→proxy transport change confined
   to the shared `web-common` crate (zero change in either worker crate): web-fetch (public
   HTTPS hosts) and web-search (operator's local SearxNG on `http://127.0.0.1:PORT`).

## Non-goals (this slice)

- **Unbypassable force-routing.** Slice #1 hands the worker the proxy UDS but does not yet
  *force* its use — the worker still has `--share-net` and a *compromised* worker can open its
  own raw socket and ignore the proxy. So slice #1 is defense-in-depth + SSRF closure, **not**
  hard containment. Making the route unbypassable (private netns + the bind-mounted UDS as the
  only route out on Linux; pf/Seatbelt story on macOS) is **slice #2** — and the UDS transport
  chosen here is exactly the channel #2 force-routes over.
- **TLS interception / credential-leak scanning** (ROADMAP:142) — needs MITM with a per-instance
  CA the workers trust. **Slice #3.** Slice #1 leaves TLS end-to-end worker↔origin and never
  sees request/response bodies.
- **TLS pinning** for the frontier/LLM egress path (Phase 5 consumer). **Slice #4.**

## Decomposition (the four subsystems)

The ROADMAP:141 line bundles four genuinely independent subsystems. Slice #1 is the
foundation the others build on:

| # | Subsystem | Depends on | Status |
|---|-----------|-----------|--------|
| 1 | Boundary allowlist enforcement + SSRF/IP defense | — | **this spec** |
| 2 | Unbypassable force-routing (netns containment) | routes *to* #1 | future spec |
| 3 | TLS interception + credential-leak scanner (ROADMAP:142) | MITM *at* #1 | future spec |
| 4 | TLS pinning (frontier/LLM egress) | #1 | future spec |

---

## Architecture

### Topology — one proxy per net-worker

`tool_host`, when bringing up a worker whose policy carries `Net::Allowlist`, first spawns a
**dedicated sidecar proxy** for that worker, with that worker's allowlist passed in at spawn.
Worker identity is therefore trivially **1:1** — no per-connection authentication, no
worker→allowlist registry. This 1:1 identity is what slice #3's leak scanner needs (the proxy
knows *exactly* whose secrets to scan for), and it matches the codebase's "one process + one
identity per worker" ethos. Net workers are `SingleUse` and few, so "an extra process per
net-worker spawn" is an acceptable cost.

### New crate `workers/egress-proxy` (`hhagent-worker-egress-proxy`)

A binary. Reuses `web-common::allowlist::HostAllowlist` (the security-critical matcher,
single source of truth — do **not** re-implement). Pure, exhaustively-unit-tested modules
plus a thin I/O drive, following the `web-fetch` decomposition and the project's
"pure functions in reusable modules" rule:

- **`ssrf.rs`** *(pure, security-critical)* —
  `fn ssrf_verdict(resolved: IpAddr, entry: &AllowlistEntry) -> Verdict`.
  Classifies `resolved` against the denied ranges and **allows it only if the matching
  allowlist entry was itself a literal IP in that same denied range** (operator intent).
  Denied ranges (must all be covered by tests, IPv4 **and** IPv6):
  - loopback `127.0.0.0/8`, `::1`
  - RFC1918 private `10/8`, `172.16/12`, `192.168/16`
  - link-local `169.254.0.0/16`, `fe80::/10`
  - unique-local `fc00::/7`
  - CGNAT `100.64.0.0/10`
  - multicast `224/4`, `ff00::/8`
  - unspecified `0.0.0.0`, `::`
  - IPv4-mapped IPv6 `::ffff:0:0/96` (unwrap to the embedded v4 and re-classify, so a
    mapped private address can't slip through)
- **`request_line.rs`** *(pure)* — extract host:port from a `CONNECT host:port` line. The
  `web-common` connector issues `CONNECT` for **both** schemes (https tunnels, and the
  loopback-`http` SearxNG case tunnels too — see "Worker→proxy transport"), so the proxy only
  ever parses `CONNECT`. Rejects malformed input.
- **`proxy.rs`** *(thin I/O)* — the drive loop: accept (on the UDS) → `request_line` →
  `HostAllowlist` check → resolve (getaddrinfo via `std::net`) → `ssrf_verdict` filter over all
  resolved IPs → dial the **first surviving (pinned) IP** → reply `200` + bidi-copy the tunnel.
  Any failure path closes fail-closed.
- **`report.rs`** *(pure record + line writer)* — emit one decision record per request as a
  JSON line on **stdout**: `{worker, host, port, resolved_ip, verdict, reason}`.

### Worker→proxy transport (shared `web-common` change)

The proxy listens on a **UDS** at a deterministic per-worker path (`<scratch>/egress.sock`,
under the worker's writable scratch, bind-mounted so the worker — and only that worker — can
reach it). A UDS can't be expressed as an `HTTP(S)_PROXY` URL, so reqwest's built-in proxy-env
routing does not apply; instead `web-common::http` gains a **custom CONNECT-over-UDS
connector**, enabled when an env var (`HHAGENT_EGRESS_PROXY_UDS`) points at the socket. The
connector dials the UDS, sends `CONNECT host:port`, reads the `200`, and hands the stream to
rustls (https) or uses it directly (http). Because **both** net-workers build their client
through `web-common::http::ReqwestGet`, this is **one change in the shared transport** and
**zero change in either worker crate**. Per-redirect re-checking is preserved: each new origin
opens a new connection → a new `CONNECT` → a fresh proxy allowlist/SSRF check.

### Host side — `core/src/egress/` (host-side spawn + wiring + audit ingest)

When `tool_host` resolves a worker with `Net::Allowlist`:

1. **Spawn the sidecar** under a net-permissive lockdown `SandboxPolicy` (see below), passing
   the worker's allowlist (env, mirroring how `web-fetch` receives
   `HHAGENT_WEB_FETCH_ALLOWLIST`) and the UDS path to bind (`<scratch>/egress.sock`). The path
   is **deterministic** — no port handshake; core, proxy, and worker all derive it from the
   per-task scratch dir. Core waits for the socket to exist (bounded) before step 2.
2. **Point the worker at the UDS.** Set `HHAGENT_EGRESS_PROXY_UDS=<scratch>/egress.sock` in the
   *worker's* `policy.env`; the shared `web-common` connector picks it up (see "Worker→proxy
   transport"). The socket is bind-mounted into the worker's scratch so only that worker reaches it.
3. **Tie lifecycle.** The sidecar is killed when the worker reaches a terminal state
   (reuse/extend the existing worker-lifecycle teardown). If the sidecar fails to spawn, the
   net-worker is **not started** (fail-closed bring-up).
4. **Ingest audit.** Core reads the sidecar's stdout decision stream and writes one
   `audit_log` row per decision — `actor='egress_proxy'`,
   `action='egress.allowed' | 'egress.blocked.allowlist' | 'egress.blocked.ssrf'`, payload
   carrying `{worker, host, port, resolved_ip, reason}`. **The proxy never touches Postgres**
   (core-only-DB invariant); decisions flow proxy→core→PG, consistent with the existing
   stdio-between-core-and-workers and `audit_mirror` JSONL conventions.

### Sidecar sandbox policy

The proxy is the security boundary and the first component to touch plaintext secrets
(slice #3), so it is **sandboxed**, not a plain trusted child:

- `Profile`: a **net-permissive lockdown** — real outbound network + DNS allowed (it *is* the
  egress point, so it is **not** itself routed through a proxy: no recursion), `fs_write`
  denied, syscall surface restricted via the worker prelude's Landlock+seccomp `lock_down`.
- `Net`: a **new `Net::ProxyEgress` variant** (introduced this slice) meaning "this process is
  the egress point: real outbound + DNS, self-enforcing." Maps to `--share-net` (bwrap) /
  `(allow network*)` (Seatbelt) / `--network default` (container) — behaviorally identical to
  `Net::Allowlist` *today*, but it explicitly distinguishes the proxy from a worker in any
  policy audit, and is the hook slice #2 needs (ProxyEgress keeps real netns; `Net::Allowlist`
  workers get a private netns). Additive enum change — `Net` is not `#[non_exhaustive]` and is
  rebuilt from manifests per spawn (never persisted), so there's **no migration**; each of the
  three sandbox backends gains one arm (`matches!(…, Allowlist(_) | ProxyEgress)` for
  bwrap/Seatbelt; an explicit arm for container) plus a builder-shape test.
- `fs_read`: `/etc/{resolv.conf,hosts,nsswitch.conf}` for DNS (same set `web-fetch` needs),
  plus whatever Landlock-RO the prelude derives.
- Cross-platform via the existing `SandboxBackend`/prelude seam (bwrap / Seatbelt /
  `container`) — no OS-specific proxy code.

---

## Data flow (one HTTPS request)

```
worker (web-common CONNECT-over-UDS connector, HHAGENT_EGRESS_PROXY_UDS set)
  │  CONNECT api.example.com:443   (over <scratch>/egress.sock)
  ▼
sidecar proxy
  │  1. request_line  → host="api.example.com" port=443
  │  2. HostAllowlist::matches("api.example.com:443")? ── no ─► 403, report blocked.allowlist
  │  3. resolve("api.example.com") → [203.0.113.5, ...]
  │  4. ssrf_verdict(ip, entry) for each ── all denied ─► 403, report blocked.ssrf
  │  5. dial pinned 203.0.113.5:443
  │  6. reply 200, bidi-copy (TLS end-to-end worker↔origin; proxy sees only ciphertext)
  │  7. report allowed
  ▼
core ingests stdout line → audit_log row
```

For the loopback-`http` SearxNG case the connector also issues `CONNECT 127.0.0.1:8888`;
`HostAllowlist` matches the literal entry; `ssrf_verdict` sees loopback **but the allowlist
entry is the literal `127.0.0.1`** → allowed; the proxy dials and tunnels the plain HTTP
request through. Both existing workers keep working.

## Error handling — fail-closed everywhere

| Condition | Behaviour |
|-----------|-----------|
| Malformed request line | close connection, report `blocked.allowlist` (parse-reject) |
| Host not on allowlist | `403` / close, report `blocked.allowlist` |
| Host unresolvable | `403` / close, report `blocked.ssrf` (no target) |
| All resolved IPs SSRF-denied | `403` / close, report `blocked.ssrf` |
| Dial to pinned IP fails | `502` / close. This is a **transport** failure, not a policy verdict: the allowlist+SSRF decision was `allowed`, so the row is `egress.allowed` with a `connect_failed` note — the worker just sees a `502`. |
| Sidecar fails to spawn | net-worker **not started** (fail-closed bring-up) |

The proxy never falls open: any uncertainty blocks.

## Testing (mirrors `web-fetch`)

- **`ssrf.rs` unit** — every denied range, IPv4 and IPv6, the literal-IP carve-out (loopback
  literal allowed, public-name→loopback blocked), IPv4-mapped-IPv6 unwrap. This is the
  security core → exhaustive.
- **`request_line.rs` unit** — CONNECT form, default ports, malformed reject.
- **`web-common` connector unit** — CONNECT-over-UDS handshake (sends `CONNECT host:port`,
  parses `200` vs non-`200`); env-gated activation (`HHAGENT_EGRESS_PROXY_UDS` set vs unset =
  direct, behaviour-preserving for non-proxied callers).
- **Allowlist reuse** — a thin test confirming `web-common::HostAllowlist` is the matcher (no
  re-implementation drift).
- **`proxy.rs` hermetic** — drive the loop against a localhost origin: allowed round-trip,
  off-allowlist block, SSRF block (a hostname stubbed to resolve to a private IP).
- **Decision-report** unit — JSON line shape for each verdict.
- **`core` integration (`egress_proxy_e2e`)** — hermetic: spawning a net-worker brings up the
  sidecar, worker traffic routes through it, an off-allowlist host is blocked at the boundary,
  and an `audit_log` row is written. Plus one **`#[ignore]`** real-network test: a real
  `web.fetch` round-trips *through* the spawned sidecar (validates DNS+pinning+tunnel end to
  end), and a rebinding-style host is blocked.

## Cross-platform

Pure Rust `std::net` + getaddrinfo behaves identically on Linux and macOS. The sidecar's
containment wraps via the existing `SandboxBackend`/prelude seam (bwrap on Linux,
Seatbelt/`container` on macOS). No Linux-only or macOS-only proxy code — satisfies the
hard cross-platform constraint.

## Open implementation points (for the plan, not blocking the design)

- **Socket-readiness wait.** The UDS path is deterministic (no port handshake), but core must
  wait for the sidecar to actually `bind()`+`listen()` before pointing the worker at it. Bounded
  poll for the socket file vs a one-byte ready signal on the sidecar's stdout — plan picks one
  (lean: bounded poll, simplest and fail-closed on timeout).
- **Bind-mount plumbing.** `<scratch>/egress.sock` lives in the per-task scratch; confirm the
  sidecar and worker see the *same* path under their respective mount namespaces (the scratch
  is already bind-mounted into the worker; the sidecar needs the same scratch mounted writable
  to create the socket). Pin the exact `fs_write` entry during planning.
- **Where host-side spawn lives.** `core/src/egress/` new module vs `core/src/tool_host/
  egress_proxy.rs`. Lean new module to keep `tool_host` under cap.
- **Lifecycle hook.** Reuse `worker_lifecycle` teardown to kill the sidecar; confirm the exact
  seam during planning.
- **Audit-stream backpressure.** A chatty worker could emit many decision lines; bound the
  ingest (drop-with-`warn!` past a cap, like the handoff backstop) rather than unbounded.

## What this slice deliberately leaves true

After slice #1, a *compromised* worker can still bypass the proxy (it keeps `--share-net` and
can open its own raw socket instead of dialing the UDS). That is acknowledged and is **slice
#2's** job — and since the worker-facing value (hard containment) only fully arrives with #2,
shipping slice #1 as defense-in-depth first is deliberate. Slice #1's concrete wins: the
SSRF/DNS-rebinding gap is closed, the allowlist is enforced at a second boundary, every egress
decision is audited, the `Net::ProxyEgress` policy distinction and the UDS transport that #2
force-routes over are both in place — all without TLS interception and without breaking either
existing net-worker.
