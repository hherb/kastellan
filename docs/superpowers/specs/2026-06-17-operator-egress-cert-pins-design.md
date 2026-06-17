# Operator egress cert-pin plumbing — design

**Date:** 2026-06-17
**Status:** Approved (brainstorm), pending implementation plan
**Scope:** egress slice #4 host-side "last mile" — let an operator configure TLS
cert pins that force-routed tool workers enforce against their upstream hosts.

---

## Problem

The egress proxy's TLS-pinning machinery (slice #4) is complete: the
`kastellan-worker-egress-proxy` sidecar parses `KASTELLAN_EGRESS_PROXY_PINS`
(`{host:["sha256/<b64>"]}`) and enforces SPKI pins as a fail-closed overlay on
top of webpki (`workers/egress-proxy/src/pins.rs`). The host-side threading is
also complete: `cert_pins_json: Option<&str>` flows from `NetWorkerSpawn`
through `spawn_net_worker` → `spawn_sidecar` → `proxy_policy`, which pushes the
env var only when `Some(non-blank)`.

The seam dead-ends at one place. Every production spawn converges on
`spawn_worker_maybe_forced` (`core/src/worker_lifecycle/force_route.rs`), which
hard-codes `cert_pins_json: None`. There is no operator-facing way to supply
pins, so no force-routed worker can pin its upstream hosts.

This design adds the missing operator config source and wires it to that
chokepoint. It is **decoupled from the Phase-5 frontier path**: frontier LLM
egress does not exist yet (`Router::send` returns `PolicyDeniedFrontier`, the
router runs in-core via reqwest, not through a sidecar), so "pin the frontier
LLM endpoint" is Phase-5 work. What ships here is the general capability:
*force-routed tool workers (web-fetch, web-search, browser-driver) can pin
their allowlisted upstream hosts*, and the same plumbing later serves a
frontier worker when one exists.

## Non-goals (explicitly out of scope)

- Frontier LLM routing, Router-behind-a-sidecar, or any frontier dispatch path.
- Frontier API-key handling (that is `db::secrets`, not env, and is Phase 5).
- DB-backed pin storage, runtime reload, or a pin-change audit trail.
- Re-implementing the proxy's strict pin validation on the host.

## Key decisions

1. **Config source = env var.** `KASTELLAN_EGRESS_CERT_PINS`, in the *same*
   JSON shape the proxy already enforces. Rationale: cert pins are public
   integrity data (SHA-256 of a server's public key — derivable from any TLS
   handshake), **not secrets**, so the "never put secrets in env" rule (which is
   exactly why the Phase-5 frontier *API key* must live in `db::secrets`) does
   not apply. Env is consistent with the existing env-driven force-routing
   config (`from_env`) and the proxy's own pin env, needs no migration, and adds
   no new attack surface — whoever sets the daemon's env already controls
   `KASTELLAN_EGRESS_FORCE_ROUTING` and the proxy binary path. Accepted
   trade-off vs. a DB table: no runtime reload (restart to change) and no
   built-in change-audit trail. Acceptable for forward-looking infra with no
   consumer yet; the source can be swapped if a frontier consumer later wants
   operationally-managed pins.

2. **Structural host-side validation; the proxy stays the authoritative strict
   validator.** The host parses enough to (a) fail closed at startup on
   obviously-malformed config and (b) select pins per worker. It does *not*
   re-decode base64 / check 32-byte length / compute SPKI — `PinSet::parse` in
   the proxy remains the single source of truth for the strict format. A pin
   with a valid `sha256/` prefix but bad base64 passes the host parse and fails
   closed one layer later, at sidecar startup, with the proxy's own error. This
   avoids duplicating crypto-adjacent parsing and the drift that invites.
   (Considered alternative: extract `pins.rs` parsing into a shared crate — like
   `kastellan-leak-scan` — so host + proxy share one validator with zero drift.
   Heavier refactor; deferred unless drift becomes a real problem.)

3. **Least-privilege per-worker pin selection.** Each sidecar receives only the
   pin entries whose host appears in *that worker's* allowlist. A pin for
   `api.anthropic.com` is never attached to web-fetch's sidecar if web-fetch's
   allowlist doesn't include that host (it can't dial it anyway). This keeps the
   sidecar env minimal and the "a worker only knows pins for hosts it may reach"
   invariant. (Considered alternative: pass the whole global map to every
   sidecar — harmless but leakier and larger; rejected.)

4. **Fail closed at startup.** Malformed `KASTELLAN_EGRESS_CERT_PINS` makes the
   daemon refuse to start, consistent with the proxy's fail-loud parse and the
   fail-closed proxy-binary discovery. A pin typo is a startup error, not a
   silent degrade.

## Components

### New module: `core/src/egress/cert_pins.rs` (pure)

Keeps the already-large `force_route.rs` (583 LOC) from growing past cap.

- **`CertPinMap`** — newtype over `BTreeMap<String, Vec<String>>` mapping a
  lowercased host to its list of `sha256/<b64>` pin strings. Invariant: every
  value vec is non-empty (empty arrays are rejected at parse; an all-empty map
  normalizes to "no pins" at the call site, see `from_env`).

- **`CertPinError`** — `thiserror` enum for the structural failures: not an
  object, a host whose value is not an array of strings, an empty pin array for
  a host, a pin string missing the `sha256/` prefix. Carries enough context
  (the offending host) for an actionable startup error.

- **`parse_cert_pins(json: &str) -> Result<CertPinMap, CertPinError>`** —
  structural parse + validation. Lowercases host keys. `{}` parses to an empty
  `CertPinMap` (caller treats empty as "no pins"). Does **not** validate base64
  or pin length (proxy's job).

- **`select_pins_for_allowlist(map: &CertPinMap, allowlist: &[String]) ->
  Option<String>`** — pure selection. For each allowlist entry (a `host:port`
  string), extract its host via `host_of_endpoint`, lowercase, and if it is a
  key in `map`, include that whole entry. Re-serialize the filtered submap to
  the `{host:[...]}` JSON the proxy expects. Returns `None` when the filtered
  submap is empty, so the no-pin path stays byte-identical (proxy treats
  `None`/blank/`{}` identically — plain webpki).

- **`host_of_endpoint(endpoint: &str) -> &str`** — strips `:port` from a
  `host:port` allowlist entry. IPv6-bracket aware: `[2001:db8::1]:443` →
  `2001:db8::1`; `api.example.com:443` → `api.example.com`; a bare host with no
  port → itself.

### Changed: `core/src/worker_lifecycle/force_route.rs`

- **`ForceRoutingConfig`** gains `pub(crate) cert_pins: Option<CertPinMap>`.
  `Some` implies a non-empty map; an empty parsed map normalizes to `None` in
  `from_env` so downstream code never sees `Some(empty)`.

- **`ForceRoutingConfig::new`** gains the `cert_pins` parameter.

- **`resolve_force_routing`** gains a `cert_pins: Option<CertPinMap>` parameter
  (it receives an already-parsed map and keeps returning `ProxyBinaryNotFound`;
  parsing/validation happens in the env layer, not the pure resolver).

- **`from_env`** reads `KASTELLAN_EGRESS_CERT_PINS`, runs `parse_cert_pins`,
  normalizes an empty map to `None`, and threads the result into
  `resolve_force_routing`. Its return error becomes:

  ```rust
  #[derive(Debug, thiserror::Error)]
  pub enum ForceRoutingError {
      #[error(transparent)]
      ProxyBinaryNotFound(#[from] ProxyBinaryNotFound),
      #[error("invalid KASTELLAN_EGRESS_CERT_PINS: {0}")]
      CertPins(#[from] CertPinError),
  }
  ```

  `ProxyBinaryNotFound` stays a distinct type (still returned by
  `resolve_force_routing`); only `from_env`'s signature widens. `main.rs`
  already maps the error to `anyhow!` so the call-site change is contained.

- **`spawn_worker_maybe_forced`** — the one behavioral line. After building
  `allowlist`:

  ```rust
  let pins_json = cfg.cert_pins.as_ref()
      .and_then(|m| select_pins_for_allowlist(m, &allowlist));
  let params = NetWorkerSpawn {
      // …
      cert_pins_json: pins_json.as_deref(),
      // …
  };
  ```

  Everything downstream (`spawn_forced_net_worker` → `spawn_net_worker` →
  `spawn_sidecar` → `proxy_policy`) already threads `cert_pins_json` to the
  sidecar env; no other change.

## Data flow

```
operator env: KASTELLAN_EGRESS_CERT_PINS = {"api.example.com":["sha256/…"]}
      │
      ▼  (daemon startup)
force_route::from_env
  └─ parse_cert_pins ──► CertPinMap (fail closed on malformed)
      │  empty? → None
      ▼
ForceRoutingConfig { proxy_bin, scratch_root, make_sink, cert_pins }
      │  (per cold spawn of a Net::Allowlist worker)
      ▼
spawn_worker_maybe_forced(worker="web-fetch", allowlist=["api.example.com:443", …])
  └─ select_pins_for_allowlist(cert_pins, allowlist)
        └─ keep hosts in allowlist → {"api.example.com":["sha256/…"]} (or None)
      │
      ▼
NetWorkerSpawn.cert_pins_json ──► spawn_sidecar ──► proxy_policy
      │
      ▼  --setenv KASTELLAN_EGRESS_PROXY_PINS=…  (only when Some/non-blank)
egress-proxy sidecar: PinSet::parse (authoritative strict validation) →
  PinningVerifier (webpki THEN SPKI-pin overlay) → fail closed on mismatch
```

A worker whose allowlist intersects no pinned host gets `None` → the sidecar
sees no pin env → plain webpki, byte-identical to today.

## Error handling

- Malformed pin config at startup → `ForceRoutingError::CertPins` → daemon
  refuses to start (fail closed).
- Valid structure but bad base64/length → passes the host, fails closed at
  sidecar startup with the proxy's `PinError` (one layer later, still fail
  closed).
- Force-routing disabled, or `KASTELLAN_EGRESS_CERT_PINS` unset → `cert_pins`
  is `None`; the selection is never reached; spawn path byte-identical to today.

## Testing (TDD)

Pure functions carry the weight (no live PG / sandbox needed):

- `parse_cert_pins`: valid map; host keys lowercased; empty-array rejected;
  non-object rejected; missing-`sha256/`-prefix rejected; `{}` → empty map.
- `select_pins_for_allowlist`: intersection selects the right hosts; no
  intersection → `None`; multi-host; case-insensitive host match; selected JSON
  round-trips to the proxy's expected shape.
- `host_of_endpoint`: `host:port`, `[::1]:443`, bare host.
- `from_env` / `resolve_force_routing`: pins parsed + stored; empty map → `None`;
  malformed env → fail-closed `ForceRoutingError::CertPins`; unset → `None`
  (byte-identical legacy path); `cert_pins` is `Some(non-empty)` only.

The proxy-side enforcement (`PinSet`, `PinningVerifier`) is already covered by
`workers/egress-proxy` unit tests and the `egress_force_routing_e2e` suite.

**Deferred follow-up (filed, not built this session):** a real end-to-end
pin-enforcement e2e — a force-routed worker dials a host whose served cert does
*not* match the configured pin and is blocked at the sidecar with a
`tls_pin`/`pin_mismatch` decision. Needs a real sandbox + a controllable TLS
origin; no frontier consumer yet to justify the harness. Track as an issue.

## Verification

- `cargo test --workspace` green on macOS (skip-as-pass for live-PG suites).
- `cargo clippy --workspace --all-targets -D warnings` clean.
- No DGX-specific surface changed (no sandbox/seccomp/Landlock); the existing
  Linux baseline carries forward. (The pin env reaches the sidecar through the
  already-tested `proxy_policy` path.)

## Risks & mitigations

- **Drift between host structural parse and proxy strict parse.** Mitigated by
  keeping the host parse deliberately minimal (prefix + shape only) and
  documenting that the proxy is authoritative. If drift bites, extract a shared
  validator crate (the considered alternative).
- **Startup brick on pin typo.** Intended (fail closed). Documented for
  operators; the error names the offending host.
- **`from_env` error-type widening.** Contained to one call site in `main.rs`
  (already `map_err` to `anyhow`).
