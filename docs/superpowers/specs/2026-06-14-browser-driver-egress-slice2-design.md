# browser-driver egress slice #2 — egress-proxy routable (transparent tunnel)

**Date:** 2026-06-14
**Issues:** closes [#280](https://github.com/hherb/kastellan/issues/280) (the production fix) and [#263](https://github.com/hherb/kastellan/issues/263) (the force-routing collision).
**Predecessor:** `docs/superpowers/specs/2026-06-12-browser-driver-worker-design.md` (§2 egress decision, §6 subresource/allowlist), browser-driver Phase 2 (PR #282).

---

## 1. Problem

Phase 2 made `browser-driver` render under the real OS jail on both platforms, but **only for
development**. It runs on the legacy direct-net `Net::Allowlist` path (no `proxy_uds`), explicitly
**exempt** from egress force-routing, with a hard production lockout
(`ForceRouteUnconfined` → `POLICY_DENIED`) unless `KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET=1`
(dev-only). In that posture the browser's egress is enforced **only in-process** (Playwright request
interception), not at the OS boundary — a real weakening of the threat-model invariant for a large
attack surface.

The blocker is mechanical: the egress sidecar speaks `CONNECT`-over-UDS, and a headless Chromium
**cannot** speak `CONNECT`-over-UDS. The other net workers (web-fetch/web-search) use the in-process
Rust `ProxyConnectGet` client for that; the browser is a separate process that only understands an
HTTP proxy on a TCP socket.

## 2. Goal & acceptance

Run `browser-driver` in the **default force-routed deployment** (`KASTELLAN_EGRESS_FORCE_ROUTING=1`,
on by default) with egress enforced at the **netns boundary**: private netns → per-worker egress
sidecar → host:port allowlist + SSRF rejection at `CONNECT` time. No in-process-only allowlist as
the sole boundary. Remove the dev-only escape hatch and the production lockout; close #263 and #280.

**Acceptance:** `browser-driver` renders a reachable allowlisted page in the default force-routed
deployment with egress enforced at the netns boundary, and an off-allowlist navigation **fails closed
at the sidecar** (not only via in-process interception). Verified on macOS Seatbelt + DGX bwrap.

## 3. Key design decision — transparent tunnel, not MITM

The sidecar enforces the host:port allowlist + SSRF rejection **at `CONNECT`/dial time**, before any
TLS. That enforcement does **not** depend on MITM. MITM (slices #3a/#3b) exists for content / credential-leak
inspection, and **#3b is not wired for any worker today** (all callers pass `&[]`).

We therefore **transparently tunnel** the browser's TLS rather than MITM it:

- The browser does normal **end-to-end TLS** to the origin using Chromium's own trust store. This
  **preserves Chromium-grade certificate validation** (Certificate Transparency, CRLSets, HSTS
  preload, built-in key pinning, active distrust lists) — strictly stronger than the sidecar's
  webpki check. MITM would *downgrade* origin validation to webpki-grade.
- No per-instance CA is injected into Chromium, so a compromised sidecar **cannot impersonate an
  HTTPS origin** to the browser (smaller blast radius). With MITM, any browser-trusted CA whose key
  lives in the sidecar would let a compromised sidecar forge any site silently.
- No `certutil`/NSS dependency, no `--ignore-certificate-errors-*` error-suppression flag, no
  per-host leaf-minting compatibility risk in Chromium.

The egress-containment guarantee (private netns + allowlist + SSRF) is **identical** to a MITM
posture; transparent tunnel only forgoes the (currently inert) inspection capability. MITM of the
browser is deferred to a later additive slice — to be done **with a proper NSS trust-store import**,
once the leak-scanner is actually wired and the origin-validation downgrade is justified by a concrete
inspection benefit.

## 4. Architecture / data flow

### Production (force-routing ON)
```
Chromium  --proxy-server=127.0.0.1:NNNN
   │ TCP (HTTP CONNECT host:port)
   ▼
in-jail asyncio shim (127.0.0.1:NNNN)          ← lives in the browser-driver Python worker
   │ AF_UNIX (forwards the same CONNECT bytes verbatim)
   ▼
egress sidecar (per-worker UDS, DISABLE_MITM=1)
   │  allowlist(host:port) + SSRF range reject + self-resolve DNS + dial
   ▼  transparent tunnel (no MITM; end-to-end TLS passes through)
origin
```

Key properties:
- The shim is a **dumb byte-pipe**. Chromium's `CONNECT host:port HTTP/1.1` request is exactly what
  the sidecar's UDS protocol already expects, so the shim does **no HTTP parsing** — accept TCP →
  open UDS → splice both directions until either side closes.
- **DNS:** with an HTTP proxy, Chromium sends the hostname in `CONNECT` and lets the proxy resolve.
  The browser needs no in-jail DNS; the current `/etc/resolv.conf` removal in `rewrite_worker_policy`
  stays correct.
- **No-MITM mode** is required: if the sidecar MITM'd, it would present a forged leaf Chromium does
  not trust → TLS handshake failure. The `DISABLE_MITM` flag makes the sidecar always tunnel.

### Development (force-routing OFF) — unchanged
`force_route_action` returns `Direct` for **all** workers when force-routing is inactive, so the
browser runs on the host netns with no sidecar and no shim, exactly as in Phase 2. The worker starts
the shim **only when `KASTELLAN_EGRESS_PROXY_UDS` is present** — symmetric with how `make_get` selects
`ProxyConnectGet` vs `ReqwestGet` by the same env var.

### Sandbox-layer feasibility (verified)
- **Linux/bwrap:** the `Net::Allowlist + proxy_uds` arm keeps `--unshare-all`'s private netns. bwrap
  brings the loopback interface **up** when it unshares the network namespace, so Chromium↔shim over
  `127.0.0.1` works with **no bwrap change**. (Proven by the force-routed e2e on the DGX.)
- **macOS/Seatbelt:** the `proxy_uds` arm emits `(deny network-outbound)` + an allow for *only* the
  UDS, which also denies loopback TCP. The shim (bind/accept on `127.0.0.1`) and Chromium (connect to
  `127.0.0.1`) need explicit loopback-TCP allows. These are added **only** for
  `Profile::WorkerBrowserClient` + `proxy_uds` so the in-process-client UDS workers are not widened.

## 5. Components & changes

### 5.1 `workers/egress-proxy` — no-MITM mode
- Read `KASTELLAN_EGRESS_PROXY_DISABLE_MITM` once at startup (before `lock_down`), thread the bool
  into the connection handler.
- When set, `handle_conn` always transparently tunnels — it skips the first-byte peek → MITM branch
  entirely. Allowlist, SSRF, DNS-self-resolution, and the `200`/`403` decision are unchanged.
- The per-instance CA is still generated and `ca.pem` still exported (keeps `spawn_sidecar`'s
  "wait for sock + ca.pem" unchanged); in no-MITM mode it is simply never used. (Deliberate
  simplicity choice — a public ephemeral cert, harmless.)
- **Tests:** with the flag set, a connection whose first tunnel byte is `0x16` (TLS) is tunneled
  transparently (no `tls_intercepted`); without the flag, behavior is byte-identical to today.

### 5.2 `core/src/egress` — thread the flag to the sidecar
- Add `disable_mitm: bool` to the `NetWorkerSpawn<'a>` params struct.
- `proxy_policy` / `spawn_sidecar` push `KASTELLAN_EGRESS_PROXY_DISABLE_MITM=1` into the sidecar env
  **only when** `disable_mitm` is true → the no-flag path is byte-identical for existing callers.
- **Tests:** `proxy_policy` includes the env when `disable_mitm`, omits it otherwise.

### 5.3 `core/src/worker_lifecycle/force_route.rs` — remove the exemption
- Delete the `DirectInsecureDevExempt` and `RefuseProductionUnconfined` `ForceRouteAction` variants,
  the `browser_insecure_direct_net` field on `ForceRoutingConfig`, its env read
  (`ENV_BROWSER_INSECURE_DIRECT_NET`), and the two browser-specific branches in `force_route_action`
  + `spawn_worker_maybe_forced`.
- `browser-driver` now flows through the generic `Sidecar` arm like every other `Net::Allowlist`
  worker.
- Retain a single `BROWSER_DRIVER_TOOL` const, used **only** to set `disable_mitm = true` on the
  `NetWorkerSpawn` for the browser's sidecar (the browser is intrinsically MITM-incompatible — it
  cannot trust our CA). This replaces a large exemption with a one-line MITM opt-out.
- **Tests:** `force_route_action(browser, force_routing=ON)` → `Sidecar`; the Sidecar spawn for the
  browser sets `disable_mitm = true`, and for a non-browser worker sets it `false`.

### 5.4 `core/src/tool_host.rs` — drop the dead error
- Remove the now-unreachable `ForceRouteUnconfined` error variant and its message.

### 5.5 `sandbox/src/macos_seatbelt.rs` — loopback TCP for the browser
- When `policy.profile == Profile::WorkerBrowserClient` **and** `proxy_uds.is_some()`, emit, in
  addition to the UDS allow:
  - `(allow network-bind (local ip "localhost:*"))`
  - `(allow network-inbound (local ip "localhost:*"))`
  - `(allow network-outbound (remote ip "localhost:*"))`
- All other `proxy_uds` workers keep the strict UDS-only outbound rule.
- bwrap needs no change.
- **Tests:** the browser + proxy_uds profile contains the three loopback rules; a non-browser
  proxy_uds profile contains none of them; the deny-then-UDS-allow structure is preserved.

### 5.6 `workers/browser-driver` (Python) — the shim + wiring
- **New `shim.py`** — a pure asyncio loopback-TCP↔UDS relay:
  - `async start() -> int`: bind `127.0.0.1:0`, return the assigned port; serve in the background.
  - Per accepted TCP connection: `open_unix_connection(uds_path)`, then two splice coroutines
    copying TCP→UDS and UDS→TCP until EOF on either side; close both halves.
  - `async stop()`: shut the listener + drain.
  - Reads the UDS path from `KASTELLAN_EGRESS_PROXY_UDS`.
- **Wire into the worker startup**: if `KASTELLAN_EGRESS_PROXY_UDS` is set, `await shim.start()` and
  pass Chromium launch args `--proxy-server=127.0.0.1:<port>` plus an empty
  `--proxy-bypass-list` (force *all* navigations through the proxy); otherwise launch as today
  (direct). Stop the shim on teardown.
- **Keep** the in-process Playwright per-navigation + per-subresource allowlist interception as
  **defense-in-depth** (the OS boundary is now primary; the in-process check stays as belt-and-braces
  and for clean error messages).
- **Tests (pytest):** relay copies bytes both directions; concurrent connections; close on either
  side propagates; the Chromium launch-args builder includes `--proxy-server` when the UDS env is
  present and omits it when absent. The relay test uses a fake UDS echo/relay server — no sidecar,
  no Chromium.

### 5.7 `core/src/workers/browser_driver.rs` — manifest
- `proxy_uds` stays `None` in the manifest; force-routing's `rewrite_worker_policy` sets it at spawn
  (same as web-fetch). Update the doc comments to describe the slice-#2 force-routed posture and drop
  the dev-only legacy-direct-net language.

### 5.8 Cleanup — remove the escape hatch
- Remove every `KASTELLAN_BROWSER_DRIVER_INSECURE_DIRECT_NET` reference: `core/src/tool_host.rs`,
  `core/src/worker_lifecycle/force_route.rs`, `scripts/workers/browser-driver/install.sh` (help
  text), and the docs (`ROADMAP.md`, `HANDOVER.md`).

## 6. Error handling / fail-closed

- **No sidecar binary under force-routing** → the worker does not spawn (existing fail-closed
  behavior in `spawn_forced_net_worker`, now applied to the browser too).
- **Off-allowlist / SSRF host** → sidecar returns `CONNECT 403` → shim relays it → Chromium
  navigation fails. The in-process interception is a redundant second line.
- **Shim bind/connect failure** → the worker errors before navigation; no fallback to direct net.
- **No direct route exists** in the private netns, so a bug that bypasses the shim cannot reach the
  network (Linux: private netns; macOS: deny-outbound-except-loopback+UDS).

## 7. Testing strategy (TDD)

| Layer | Test | Where |
| --- | --- | --- |
| shim relay | bidirectional copy, concurrent conns, close propagation | pytest, `workers/browser-driver` |
| launch args | `--proxy-server` present iff UDS env set | pytest |
| egress proxy | DISABLE_MITM → TLS first byte tunneled transparently | `egress-proxy` unit |
| core egress | `disable_mitm` → sidecar env present/absent | `core` unit (`egress::spawn`) |
| force-route | browser+force-routing → `Sidecar` w/ `disable_mitm`; non-browser → no flag | `core` unit (`force_route`) |
| seatbelt | browser+proxy_uds → loopback rules; non-browser → none | `sandbox` unit (macos) |
| **acceptance** | force-routed render of a loopback page **through the sidecar**; off-allowlist nav fails closed at the sidecar | `core/tests/browser_driver_e2e.rs`, gated, macOS + DGX |

Gating mirrors the existing `browser_driver_e2e` (`--ignored`, PG + staged Chromium + sandbox +
proxy binary; skip-as-pass otherwise).

## 8. Out of scope (deferred)

- **MITM of the browser** + in-Chromium CA trust (a later additive slice, via NSS import, once
  leak-scanning is wired).
- **Per-spawn scratch on macOS** (#283) and the narrowing of the Seatbelt
  `mach-lookup`/`sysctl-write`/`system-socket` grants — independent Phase-2 hardening.
- **Linux seccomp/Landlock for the pure-Python worker** (#281) — independent.
- Screenshot output, warm-keep lifecycle.

## 9. File-change inventory (for the plan)

- `workers/egress-proxy/src/{main.rs,proxy.rs}` — DISABLE_MITM read + transparent-tunnel branch.
- `core/src/egress/{spawn.rs,net_worker.rs}` — `disable_mitm` on `NetWorkerSpawn`, sidecar env.
- `core/src/worker_lifecycle/force_route.rs` — remove exemption; one-line MITM opt-out.
- `core/src/tool_host.rs` — drop `ForceRouteUnconfined`.
- `sandbox/src/macos_seatbelt.rs` — loopback-TCP allows for browser+proxy_uds.
- `workers/browser-driver/.../shim.py` (new) + worker startup wiring + launch-args builder.
- `core/src/workers/browser_driver.rs` — doc/comment updates (`proxy_uds` stays `None`).
- `scripts/workers/browser-driver/install.sh` — drop escape-hatch help.
- `core/tests/browser_driver_e2e.rs` — force-routed render + off-allowlist-fail-closed.
- `docs/devel/{ROADMAP.md,handovers/HANDOVER.md}` — close #263/#280, record the slice.
