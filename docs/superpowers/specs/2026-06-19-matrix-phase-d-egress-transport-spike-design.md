# Matrix Phase D — egress-transport spike + `matrix-sdk` dependency landing (design)

**Date:** 2026-06-19
**Status:** approved (brainstorm), pending implementation plan
**Slice of:** comms slice #2 Phase D (live `matrix-rust-sdk` integration). See
`docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md` and
`docs/superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md` (Task 8, Step 0 + Step 1).

## Why this slice

Phase D is the only remaining un-built part of the Matrix channel: the real
`matrix-rust-sdk` network code. Phases A–C + E (the worker JSON-RPC surface +
buffer, the core `MatrixChannel` + driver thread, the sandbox/egress spawn
policy builders, config-gated daemon wiring) are already on `main` and verified
hermetically.

The Phase D plan makes **Step 0 a spike-first gate**: confirm `matrix-rust-sdk`'s
HTTP client can be routed through our egress proxy *before* writing the sync
loop, because the answer forks the downstream design. This slice executes that
gate and lands the dependency cleanly. It does **not** write the live SDK
integration (`sdk_live.rs`), the live worker wiring, or the live e2e — those are
the next slice, verified on the DGX.

## Key facts established during brainstorming

`matrix-sdk` 0.17 `ClientBuilder` exposes (non-wasm builds):

- `.add_root_certificates(Vec<Certificate>)` — trusts custom root CAs (wraps
  `reqwest::ClientBuilder::add_root_certificate`). Unlike a browser, matrix-sdk
  **can** be made to trust our per-instance egress CA cleanly. (Not used by this
  slice — see the posture decision below — but it is what makes MITM *feasible*
  for matrix, where it was not for the browser.)
- `.proxy("http://…")` — but reqwest proxies are **TCP HTTP(S) URLs**; reqwest
  cannot dial a **Unix-domain-socket** proxy. Our egress sidecar is UDS-only.
- `.http_client(reqwest::Client)` — a fully custom client, but mutually exclusive
  with `.proxy()`/`.add_root_certificates()`, and reqwest still won't do
  CONNECT-over-UDS without a custom connector. Not a clean path.

**Consequence:** the UDS gap forces an in-worker **TCP↔UDS bridge** regardless of
posture — the Rust analogue of browser-driver's `shim.py ProxyShim`. matrix-sdk
points `.proxy()` at a loopback TCP port; the bridge relays that connection to
the sidecar UDS. This is a DGX-proven, threat-model-reviewed pattern; the spike
confirms it empirically, it does not invent anything new.

## Decisions locked in

1. **Scope = spike-first slice (plan Step 0 + Step 1).** Land the dependency +
   license pass + the transport proof + the recorded decision. Live SDK
   integration is the next slice.
2. **Egress posture = transparent-tunnel (MITM-bypass pin).** The sidecar runs
   `disable_mitm` keyed on the matrix worker name (reuse browser-driver's exact
   mechanism). The proxy still enforces allowlist + SSRF + IP-pin; it does *not*
   TLS-intercept the homeserver. matrix-sdk keeps native end-to-end TLS
   validation against the trusted, self-hosted, federation-off homeserver.
   **Rationale:** Matrix room content is E2E-encrypted *before* it hits HTTP, so
   a MITM leak-scan would only ever see opaque ciphertext — MITM buys almost
   nothing here while enlarging sidecar blast radius and discarding the SDK's own
   homeserver cert validation. (MITM is feasible via `.add_root_certificates()`
   if a future need ever justifies it; it is explicitly declined now.)
3. **Verification split:** macOS hermetic this slice; DGX live deferred to the
   next slice.

## What this slice delivers

All of the following are committable on the macOS dev box and keep the default
build/CI byte-identical (feature off → no SDK compiled):

1. **`matrix-sdk` dependency** added to `workers/matrix/Cargo.toml` behind the
   existing `[features] live-matrix` flag, configured for a **SQLite state store**
   and **rustls** TLS (no native-tls/OpenSSL). Default features stay light; the
   heavy crate compiles only under `--features live-matrix`.
2. **AGPL license pass** on the `live-matrix`-enabled dependency subtree —
   recorded in this spec (or a sibling note) before any further work. Hard gate:
   if any transitive dep carries a non-AGPL-compatible license (CDDL, BUSL, SSPL,
   Elastic, or any "source-available"), **stop and report** — do not proceed to
   the bridge/spike. Permissive (Apache-2.0 / MIT / BSD / MPL / LGPL / (A)GPL) is
   fine. matrix-rust-sdk itself is Apache-2.0; the gate must scan the transitive
   set (e.g. vodozemac, ruma).
3. **`ProxyBridge`** — a small Rust loopback-TCP↔UDS relay in `workers/matrix`
   (e.g. `src/bridge.rs`): bind `127.0.0.1:0`, accept, connect the sidecar UDS,
   `copy_bidirectional`. One bridge listener per worker; the bound port is handed
   to the SDK as the `.proxy()` target. Kept small and unit-testable; pure helpers
   (address parsing, the relay loop seam) separated from the I/O where practical.
4. **Hermetic spike test** (gated on `live-matrix`): stand up a **stub UDS proxy**
   that records the request line, start a `ProxyBridge` in front of it, build a
   `matrix_sdk::Client` with `homeserver_url(<fake host>)` + `.proxy(bridge_addr)`,
   trigger the SDK's first network call (the `/_matrix/client/versions` probe on
   `.build()`), and assert the stub observed a **`CONNECT <fake-host>:443`**. This
   proves matrix-sdk routes through our egress transport without any homeserver.
5. **Recorded outcome** in this spec + HANDOVER: transport = transparent-tunnel
   via `disable_mitm` (worker name) + the `ProxyBridge`; sync loop unblocked.

## What this slice explicitly does NOT do (next slice, DGX)

- `workers/matrix/src/sdk_live.rs` — the `LiveSdk` impl of the `MatrixSdk` seam
  (tokio runtime, `block_on` login, persistent encrypted store, sync task →
  bounded `VecDeque`, `poll`/`send`).
- Worker `main.rs` live wiring (build `LiveSdk` → `prelude::lock_down` →
  `serve_stdio`), mirroring the egress proxy's "network-init then lock_down" order.
- The `disable_mitm`-by-worker-name wiring in the core spawn path for the matrix
  worker (the mechanism exists for browser-driver; matrix adoption rides the
  live-wiring slice). **#286 macOS-loopback caveat (carry forward):** the
  `ProxyBridge` binds `127.0.0.1:0` inside the worker — the same loopback pattern
  as browser-driver's `shim.py`, which `docs/threat-model.md` records as having a
  macOS-only containment divergence (no netns ⇒ the worker's loopback is the
  host's). It is latent in this spike slice (no Seatbelt loopback grant is added
  here), but the live-wiring slice that grants the matrix worker loopback on
  macOS must pair it with the #286 mitigation (scope the grant to the bridge's
  bound port, or use a UDS-only transport / the `MacosContainer` VM-netns backend).
- `core/tests/matrix_live_e2e.rs` `#[ignore]` live round-trip against conduwuit
  (`scripts/matrix/setup-conduwuit.sh` already exists from slice #6).

## Components & boundaries

- **`workers/matrix/src/bridge.rs` — `ProxyBridge`.** *What:* relays a loopback
  TCP connection to the sidecar UDS. *Interface:* `bind(uds_path) -> ProxyBridge`
  exposing `proxy_addr() -> SocketAddr`; spawns/owns the accept loop; dropped on
  worker shutdown. *Depends on:* tokio net only. Testable against a stub UDS
  listener with no SDK.
- **Spike test (`workers/matrix/tests/` or `#[cfg(feature="live-matrix")]`
  in-crate).** *What:* the empirical proof matrix-sdk uses the bridge. *Depends
  on:* `matrix-sdk` (feature-gated), `ProxyBridge`, a local stub UDS proxy. No
  homeserver, no real sidecar binary, no PG.
- **`workers/matrix/Cargo.toml`.** *What:* the feature-gated dependency surface.
  *Constraint:* default build unaffected.

## Error handling

- Bridge: a failed UDS connect or a closed peer ends that relayed connection;
  the accept loop continues. Bind failure is fatal to the worker (surfaced at
  startup) — fail-closed, mirroring egress/browser-driver.
- License gate: any incompatible transitive license is a hard stop with a report,
  never a silent proceed.

## Testing

- **TDD order:** bridge unit tests (relay round-trip, peer-close, bind) →
  bridge impl → dependency add (makes the spike test compile) → spike test.
- **macOS green gate (this slice):**
  - `cargo build -p kastellan-worker-matrix --features live-matrix` compiles.
  - the hermetic spike test passes (CONNECT reaches the stub via the bridge).
  - `cargo test -p kastellan-worker-matrix` (default features) green.
  - `cargo clippy --workspace --all-targets -- -D warnings` (default features,
    `live-matrix` off) clean. The heavy SDK is not in the default-feature clippy
    surface; a `--features live-matrix` clippy pass on the matrix crate is run
    locally on macOS as an additional check.
- **DGX (deferred):** the live login/E2E/send-recv round-trip — next slice.

## Open risks

- **SDK `.build()` network behaviour.** The spike assumes `Client::builder()
  .homeserver_url(url).build()` (or the first call after) issues a network
  request that traverses the proxy. If `.build()` is lazy, the spike triggers the
  probe explicitly (a `versions()`/whoami-style call) — the assertion is on the
  stub seeing a CONNECT, so any first network call suffices. Confirm during
  implementation; adjust the trigger, not the design.
- **License surprise in the transitive tree.** Low (matrix-rust-sdk ecosystem is
  permissively licensed) but the gate is mandatory and abortive.

## License pass (2026-06-19)

**matrix-sdk version:** 0.8.0
**Resolved feature set:** e2e-encryption, sqlite, bundled-sqlite, rustls-tls
**New crate count (unique names added by `live-matrix` feature):** 225
**Decision:** PASS — all AGPL-compatible

### Method

Enumerated the full dependency tree of `kastellan-worker-matrix` with the
`live-matrix` feature on via `cargo tree -p kastellan-worker-matrix --features
live-matrix -e normal --prefix none | sort -u` (359 lines including dedup markers
`(*)`), then cross-referenced against the baseline tree without the feature (40
lines). 225 unique crate names are new. Workspace-wide license map obtained via
`cargo-license --all-features`.

### Non-obvious licenses investigated

| Crate | License ID | Actual license | Compatible? |
|---|---|---|---|
| `xxhash-rust` | `BSL-1.0` | **Boost Software License 1.0** (permissive) — confirmed by reading LICENSE file | YES — permissive, AGPL-compatible |
| `webpki-roots` | `CDLA-Permissive-2.0` | Community Data License Agreement – Permissive 2.0 — a **data license** for the bundled TLS root certificates; Section 3.1 explicitly places no restriction on use of results | YES — permissive data license, AGPL-compatible |
| `ryu` | `Apache-2.0 OR BSL-1.0` | Dual Apache-2.0 / Boost; licensor chose whichever the user prefers | YES |
| `blake3` | `Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR CC0-1.0` | All three variants are permissive | YES |
| Matrix/ruma/vodozemac crates | `Apache-2.0` | Pure Apache-2.0 | YES |
| `ring` | `Apache-2.0 AND ISC` | Conjunctive Apache-2.0 + ISC | YES |
| `curve25519-dalek`, `ed25519-dalek`, `x25519-dalek` | `BSD-3-Clause` | Permissive | YES |
| `MPL-2.0` family (`eyeball`, `imbl`, `as_variant`, …) | MPL-2.0 / MPL-2.0+ | File-copyleft only; compatible as a dependency in an AGPL project | YES |
| ICU crates | `Unicode-3.0` | Unicode License v3 — permissive | YES |

No `CDDL`, `BUSL` (Business Source), `SSPL`, `Elastic License`, `Commons Clause`,
or any other source-available / non-free license detected in the subtree.

## Spike outcome (2026-06-19)

**Branch:** `feat/matrix-phase-d-egress-spike`
**Status:** CONFIRMED — transport decision locked, live integration unblocked.

### matrix-sdk version and resolved feature set

- **matrix-sdk = 0.8.0** (Cargo.toml `workers/matrix/Cargo.toml`, optional dep gated
  by `live-matrix = ["dep:matrix-sdk"]`)
- **Features used:** `e2e-encryption, sqlite, bundled-sqlite, rustls-tls`
  (rustls, no native-tls; bundled SQLite so the jail needs no system libsqlite)
- **default-features = false** — default build excludes matrix-sdk entirely; only
  `--features live-matrix` pulls it in. Default CI/clippy surface is unchanged.

### Transport decision — CONFIRMED

**Transparent tunnel via `disable_mitm` (worker name) + in-worker `ProxyBridge`.**

- `matrix-sdk 0.8.0` routes its first HTTPS request as a **`CONNECT` tunnel** when
  given a proxy URL via the SDK builder's `.proxy()` method.
- `ProxyBridge` (added in `workers/matrix/src/bridge.rs`) binds a loopback-TCP
  listener, accepts the SDK's CONNECT, and byte-relays it to the sidecar UDS. This
  is the Rust analogue of browser-driver's `shim.py ProxyShim`.
- The egress sidecar runs in **`disable_mitm`** mode keyed on the matrix worker
  name (the same mechanism already used for browser-driver). The proxy enforces
  allowlist + SSRF + IP-pin but does NOT TLS-intercept. matrix-sdk keeps native
  end-to-end TLS validation against the self-hosted homeserver.
- **No custom CA is injected.** MITM is feasible via `.add_root_certificates()` if
  a future need arises, but is explicitly declined now (Matrix room content is
  E2E-encrypted before it hits HTTP, so a MITM leak-scan would only see opaque
  ciphertext and enlarges sidecar blast radius with no gain).

### Exact SDK builder and trigger method names (for the next slice's LiveSdk impl)

The hermetic spike test (`workers/matrix/src/egress_spike.rs`, gated on
`#[cfg(all(test, feature="live-matrix"))]`) confirmed the following API surface
against matrix-sdk 0.8.0:

```rust
// Builder:
Client::builder()
    .homeserver_url("<url>")              // set the homeserver URL
    .sqlite_store("<path>", None)         // persistent encrypted SQLite store (path, passphrase)
    .proxy("<http://127.0.0.1:<port>")    // proxy URL; here: the ProxyBridge loopback address
    .build()                              // consumes the builder; performs the first network probe
    .await?;

// First network trigger (the one that traverses the proxy):
client.whoami().await                     // causes the SDK to issue CONNECT <host>:443 through the proxy
```

The spike asserted that the stub UDS observer received `CONNECT fake-homeserver.invalid:443`
immediately after the `whoami()` call, proving the routing is end-to-end through the bridge.
`build()` itself may or may not issue a network probe depending on the SDK version — `whoami()`
is the reliable trigger.

### What was confirmed green (macOS, hermetic, no homeserver)

| Check | Result |
|---|---|
| `cargo build -p kastellan-worker-matrix --features live-matrix` | PASS |
| `cargo test -p kastellan-worker-matrix` (default features, 7 tests) | PASS |
| `cargo test -p kastellan-worker-matrix --features live-matrix` (8 tests, +1 spike) | PASS |
| `cargo clippy --workspace --all-targets -- -D warnings` (default features) | PASS |

### What is deferred to the next slice (live `LiveSdk` integration)

Per plan `docs/superpowers/plans/2026-06-12-matrix-inbound-sandboxed-worker.md`
Task 8 Steps 2–5:

1. **`workers/matrix/src/sdk_live.rs`** — `LiveSdk` impl of the `MatrixSdk` seam:
   tokio runtime, `block_on` login, persistent encrypted SQLite store, sync task →
   bounded `VecDeque`, `poll`/`send`. Reuses `ProxyBridge` for transport (exact
   builder sequence above).
2. **Restore `main.rs` live serving wiring** — build `LiveSdk` → `prelude::lock_down`
   → `serve_stdio` — and narrow the crate-wide `#![allow(dead_code)]` back to the
   specific items that remain in-progress.
3. **Wire `disable_mitm`-by-worker-name** for the matrix worker in the core spawn
   path (the mechanism exists for browser-driver; matrix adoption rides this slice).
4. **`core/tests/matrix_live_e2e.rs`** `#[ignore]` live round-trip against
   conduwuit — DGX-verified (`scripts/matrix/setup-conduwuit.sh` exists from
   slice #6).
