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

1. A proxy **binary** that listens on a **per-worker UDS** and, per `CONNECT`, enforces the
   worker's host allowlist + SSRF/IP defense and tunnels to the pinned IP.
2. The proxy enforces the worker's **host allowlist at the boundary** (defense-in-depth layer 2
   behind the worker's existing self-enforcement), reusing the *same* matcher the workers use.
3. The proxy **resolves DNS itself**, rejects private/loopback/link-local/etc. resolved IPs
   (allowlist-aware — see SSRF policy), and **pins** the surviving resolved IP across the
   connection, closing the SSRF/DNS-rebinding gap.
4. Every proxy decision is **audited** (allowed / blocked-allowlist / blocked-ssrf), without
   violating the core-only-Postgres invariant.
5. The proxy process is **itself sandboxed** (net-permissive lockdown, `Net::ProxyEgress`),
   because it parses untrusted bytes from both sides and will hold plaintext secrets in slice #3.
6. A reusable **host-side `core/src/egress` module** that spawns the sandboxed sidecar and maps
   its decision stream to `audit_log` rows — proven end-to-end by an e2e that drives the
   sidecar with a **trivial test `CONNECT` client**. (The hookup that auto-spawns the sidecar
   per real net-worker, and the worker→proxy transport, are **slice #2** — see Non-goals.)

## Non-goals (this slice)

- **Worker→proxy transport + live `tool_host` hookup.** Slice #1 does **not** route the real
  `web-fetch`/`web-search` workers through the proxy, and does **not** modify `tool_host`'s
  spawn path. Reqwest's proxy support is TCP-only (no `unix://` scheme), so routing over the
  UDS needs a hand-rolled CONNECT-over-UDS client (hyper + tokio-rustls — already in the lock
  graph, but a real ~150-line async mini-client since the worker terminates TLS end-to-end).
  That client, plus auto-spawning the sidecar per net-worker, only becomes *load-bearing* with
  force-routing — so both land in **slice #2**. (Rationale: nobody routes real traffic through
  the proxy until #2 makes it unbypassable, so wiring live workers in #1 is premature work in
  the wrong slice. Slice #1's machinery is proven by an e2e test client instead.)
- **Unbypassable force-routing.** The thing #2 adds on top: a private netns with the
  bind-mounted UDS as the only route out (Linux; pf/Seatbelt story on macOS), so a
  *compromised* worker physically cannot bypass the proxy. The UDS the proxy listens on here is
  exactly the channel #2 force-routes over.
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

- **`ssrf.rs`** *(pure, security-critical)* — `fn is_denied_range(ip: IpAddr) -> bool`, the
  range classifier. The literal-IP carve-out lives in `proxy.rs`, not here: a **literal-IP
  CONNECT target** (parses as `IpAddr`) that `HostAllowlist::is_allowed` accepts is connected to
  directly — no DNS, no range-deny — because the operator allowlisted that exact address
  (the local-SearxNG case). A **hostname** target is resolved and **every** resolved IP is run
  through `is_denied_range`; any hit blocks (the DNS-rebinding defense — a public name that
  resolves to a private IP is always blocked, since the operator allowlisted a *name*, not the
  address). Denied ranges (must all be covered by tests, IPv4 **and** IPv6):
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
  (slice-#2) `web-common` connector will issue `CONNECT` for **both** schemes (https tunnels,
  and the loopback-`http` SearxNG case tunnels too), so the proxy only ever parses `CONNECT`.
  Rejects malformed input.
- **`proxy.rs`** *(thin I/O)* — the drive loop: accept (on the UDS) → `request_line` →
  `HostAllowlist` check → resolve (getaddrinfo via `std::net`) → `ssrf_verdict` filter over all
  resolved IPs → dial the **first surviving (pinned) IP** → reply `200` + bidi-copy the tunnel.
  Any failure path closes fail-closed.
- **`report.rs`** *(pure record + line writer)* — emit one decision record per request as a
  JSON line on **stdout**: `{worker, host, port, resolved_ip, verdict, reason}`.

### Worker→proxy transport (deferred to slice #2)

The proxy listens on a **UDS** at a deterministic per-worker path (`<scratch>/egress.sock`,
under the worker's writable scratch, bind-mounted so the worker — and only that worker — can
reach it). A UDS can't be expressed as an `HTTP(S)_PROXY` URL, so reqwest's built-in proxy-env
routing does not apply; routing real workers over the UDS needs a custom CONNECT-over-UDS
client in `web-common::http` (hyper + tokio-rustls). **That client lands in slice #2** alongside
force-routing (it only becomes load-bearing then). Slice #1 proves the proxy with a trivial
test client instead. The UDS contract (deterministic path, `CONNECT` for both schemes,
per-redirect re-check via a fresh `CONNECT`) is fixed here so #2 only adds the client.

### Host side — `core/src/egress/` (reusable spawn + audit-ingest module; not wired into `tool_host` this slice)

Slice #1 ships `core/src/egress` as a **reusable module**, exercised by the e2e but **not yet
called from `tool_host`'s spawn path** (that hookup is slice #2, where it joins force-routing).
It provides:

1. **`spawn_sidecar(allowlist, scratch_dir, backend) -> SidecarHandle`** — builds the
   net-permissive lockdown `SandboxPolicy` (see below), passes the allowlist (env, mirroring
   `HHAGENT_WEB_FETCH_ALLOWLIST`) and the UDS path to bind (`<scratch>/egress.sock`, deterministic
   — no port handshake), spawns the sandboxed proxy via the `SandboxBackend`, and waits (bounded)
   for the socket to exist. Fail-closed: spawn/bind timeout → `Err`.
2. **`decision_to_audit(line: &str) -> Option<AuditRow>`** *(pure)* — maps one stdout decision
   JSON line to an `audit_log` row: `actor='egress_proxy'`,
   `action='egress.allowed' | 'egress.blocked.allowlist' | 'egress.blocked.ssrf'`, payload
   `{worker, host, port, resolved_ip, reason}`. **The proxy never touches Postgres**
   (core-only-DB invariant); decisions flow proxy→core→PG, consistent with the existing
   stdio-between-core-and-workers and `audit_mirror` JSONL conventions. The actual DB insert is
   exercised by a PG-gated test (skip-as-pass on the Mac); the mapping is unit-tested PG-free.
3. **`SidecarHandle::shutdown()`** — kills the sidecar; slice #2's `tool_host` hookup ties this
   to worker-terminal teardown.

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
client over <scratch>/egress.sock
  (slice #1: the e2e test CONNECT client; slice #2: the real worker via the web-common connector)
  │  CONNECT api.example.com:443
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

For the loopback-`http` SearxNG case the (slice-#2) connector also issues `CONNECT
127.0.0.1:8888`; `HostAllowlist` matches the literal entry; `ssrf_verdict` sees loopback **but
the allowlist entry is the literal `127.0.0.1`** → allowed; the proxy dials and tunnels the
plain HTTP request through.

## Error handling — fail-closed everywhere

| Condition | Behaviour |
|-----------|-----------|
| Malformed request line | close connection, report `blocked.allowlist` (parse-reject) |
| Host not on allowlist | `403` / close, report `blocked.allowlist` |
| Host unresolvable | `403` / close, report `blocked.ssrf` (no target) |
| All resolved IPs SSRF-denied | `403` / close, report `blocked.ssrf` |
| Dial to pinned IP fails | `502` / close. This is a **transport** failure, not a policy verdict: the allowlist+SSRF decision was `allowed`, so the row is `egress.allowed` with a `connect_failed` note — the caller just sees a `502`. |
| Sidecar fails to spawn / bind UDS in time | `spawn_sidecar` returns `Err` (fail-closed); slice #2's `tool_host` hookup translates that into "net-worker not started" |

The proxy never falls open: any uncertainty blocks.

## Testing (mirrors `web-fetch`)

- **`ssrf.rs` unit** — every denied range, IPv4 and IPv6, the literal-IP carve-out (loopback
  literal allowed, public-name→loopback blocked), IPv4-mapped-IPv6 unwrap. This is the
  security core → exhaustive.
- **`request_line.rs` unit** — CONNECT form, default ports, malformed reject.
- **Allowlist reuse** — a thin test confirming `web-common::HostAllowlist` is the matcher (no
  re-implementation drift).
- **`proxy.rs` hermetic** — drive the loop over a UDS with a test `CONNECT` client against a
  localhost origin: allowed round-trip, off-allowlist block, SSRF block (resolver stubbed to
  return a private IP for a public-looking name).
- **Decision-report** unit — JSON line shape for each verdict; `decision_to_audit` mapping unit
  (each verdict → the right `action`, PG-free).
- **`core` integration (`egress_proxy_e2e`)** — hermetic: `spawn_sidecar` brings up the
  **sandboxed** proxy (real bwrap/Seatbelt), a test `CONNECT` client over the UDS gets an
  allowed round-trip for a literal-allowlisted localhost origin and a block for an off-allowlist
  host, and `decision_to_audit` produces the expected rows. Plus one **`#[ignore]`**
  real-network test: a test `CONNECT` to a real public host round-trips *through* the spawned
  sidecar (validates DNS + IP-pinning + tunnel end to end). A PG-gated test (skip-as-pass on the
  Mac) asserts the audit row actually lands in `audit_log`.

## Cross-platform

Pure Rust `std::net` + getaddrinfo behaves identically on Linux and macOS. The sidecar's
containment wraps via the existing `SandboxBackend`/prelude seam (bwrap on Linux,
Seatbelt/`container` on macOS). No Linux-only or macOS-only proxy code — satisfies the
hard cross-platform constraint.

## Open implementation points (for the plan, not blocking the design)

- **Socket-readiness wait.** The UDS path is deterministic (no port handshake), but
  `spawn_sidecar` must wait for the sidecar to actually `bind()`+`listen()` before returning the
  handle. Bounded poll for the socket file vs a one-byte ready signal on the sidecar's stdout —
  plan picks one (lean: bounded poll, simplest and fail-closed on timeout).
- **Bind-mount plumbing (for the e2e).** `<scratch>/egress.sock` lives in the per-task scratch;
  the sandboxed sidecar must mount that scratch **writable** to create the socket, and the
  host-side test client connects to the *same* file (bind-mount on Linux → same inode; no
  remapping on macOS). Pin the exact `fs_write` entry + verify host↔sandbox path identity
  during planning.
- **Audit-stream backpressure.** A chatty client could emit many decision lines; bound the
  ingest (drop-with-`warn!` past a cap, like the handoff backstop) rather than unbounded.

## What this slice deliberately leaves true

Slice #1 builds the **mechanism**, not the live wiring: the real workers don't route through
the proxy yet (`tool_host` is untouched), so the SSRF/allowlist/audit machinery is proven by an
e2e test client rather than production traffic. That is deliberate — the worker→proxy transport
and the force-routing that make it load-bearing both land together in **slice #2**, so wiring
live workers now would be premature work in the wrong slice. Slice #1's concrete deliverables: a
sandboxed boundary proxy that closes the SSRF/DNS-rebinding gap and enforces the allowlist at a
second boundary; a pure, exhaustively-tested SSRF classifier; the `Net::ProxyEgress` policy
distinction across all three sandbox backends; a reusable `core/src/egress` spawn + audit-ingest
module; and the fixed UDS contract #2 force-routes over — all without TLS interception and
without touching either existing net-worker.
