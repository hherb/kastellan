# Egress proxy SLICE #4 — TLS pinning for the frontier/LLM egress path

**Date:** 2026-06-13
**Status:** Design approved, pre-implementation
**ROADMAP:** 142 (egress proxy slice #4)
**Predecessors:** slice #1 boundary/SSRF (PR #240), slice #2 force-routing (PR #256),
slice #3a TLS-intercept MITM (PR #259), slice #3b credential-leak scanner (PR #269).
**Companion plan:** `docs/superpowers/plans/2026-06-13-egress-proxy-slice4-tls-pinning.md`.

---

## 1. Problem & goal

The per-worker egress proxy MITM-terminates each worker's TLS and **re-originates** a
fresh, validated TLS session to the pinned origin (slice #3a). Today that
re-origination leg validates the real origin against the full public root store
(`webpki-roots`, `egress-proxy::main` `upstream_tls`). For the **high-value frontier /
LLM egress** (the Phase-5 path — Claude / OpenAI), trusting *any* of ~150 public CAs
means a single mis-issued or compromised public CA could silently MITM kastellan's own
outbound frontier traffic — exfiltrating prompts, secrets, and responses.

**Goal:** pin the **SubjectPublicKeyInfo (SPKI)** of the specific frontier origin(s) so
the re-origination leg refuses a substituted certificate even when it chains to a
publicly-trusted CA. This is **additive defense-in-depth** layered on top of (never
replacing) standard webpki chain validation.

### Scope decisions (locked during brainstorming)

1. **Seam = the egress-proxy upstream re-origination leg.** Not the in-core
   `llm-router` rustls client. Rationale: the handover scopes #4 to
   `egress-proxy::main upstream_tls`; the proxy already owns the upstream validation
   seam. Pinning the in-core router (where frontier *will* originate from core today,
   since the core is not itself a sandboxed worker behind a proxy) is a **separate
   Phase-5 router-seam decision**, explicitly out of scope.
2. **Pin type = a SET of `SHA-256(SPKI)` hashes per host** (HPKP / RFC 7469 model).
   A match is "some certificate in the validated chain presents a pinned SPKI." This
   survives leaf renewal when the key is reused and lets a **backup / next-key pin** be
   pre-staged so rotation never bricks egress. (Rejected: leaf-cert fingerprint — breaks
   on every ~90-day renewal; CA-SPKI pin — weaker, a CA compromise still MITMs.)
3. **Provisioning = an env var read once at startup** (`KASTELLAN_EGRESS_PROXY_PINS`).
   Pins are *static operator config* (the same shape as the existing
   `KASTELLAN_EGRESS_PROXY_ALLOWLIST`), not runtime per-worker state like #3b's
   materialized secrets — so they need no scratch file and no per-connection re-read.
   The verifier is built **once** before `lock_down`. (Rejected: a `cert_pins.json` in
   the sidecar scratch — maximally consistent with #3b, but adds a per-connection
   `ClientConfig` rebuild and file plumbing for config that never changes during the
   proxy's life.)

### Forward-looking, like #3b

No frontier egress worker exists yet (`Backend::Frontier` is Phase-5-gated to
`PolicyDeniedFrontier` in `llm-router`). So this slice ships the **mechanism + spawn
wiring** with **today's callers passing empty pins** (no `PINS` env ⇒ no pinning ⇒
byte-identical to current behaviour), demonstrated hermetically against a test origin.
The wiring that reads the operator's real frontier pin config and routes frontier
egress through a pinned sidecar lands with the first frontier worker / Phase-5 routing.

---

## 2. Non-goals

- Pinning the in-core `llm-router` reqwest/rustls client (Phase-5 router seam).
- Routing real frontier LLM traffic through a per-worker egress proxy (Phase-5 routing).
- Reading/persisting the operator's live frontier pin configuration on the daemon
  (lands with the first frontier worker; this slice's host callers pass empty pins).
- Certificate Transparency / OCSP / CRL checks (orthogonal; webpki path unchanged).
- Wildcard-host or per-port pin keys (frontier origins are specific hosts; exact-host
  match only).
- Replacing webpki chain validation. Pinning is **strictly additive** — a chain that
  fails webpki is rejected regardless of pins.

---

## 3. Architecture

### 3.1 Components

**Proxy-local, pure — `workers/egress-proxy/src/pins.rs` (+ `pins/tests.rs`).**
No shared crate: unlike #3b (where *both* `core` and the proxy ran the matcher, forcing
the `kastellan-leak-scan` crate), here **only the proxy** computes and matches SPKI. The
host passes an opaque JSON env string through; the operator computes the pin hashes
offline. So all pin logic lives in the proxy.

- `pub fn spki_sha256(cert_der: &[u8]) -> Result<[u8; 32], PinError>` — parse the
  certificate via `x509-cert` (RustCrypto), take its
  `tbs_certificate.subject_public_key_info`, `.to_der()` it, and `SHA-256` the result.
  This is the RFC 7469 pin pre-image (the full SPKI structure: algorithm + public key
  bits). Note: `to_der()` *re-encodes* the SPKI; for canonical DER (every CA-issued
  cert) that is byte-identical to the original SPKI bytes, so the hash matches — pinned
  by the fixture test in §5.
- `pub struct PinSet { map: HashMap<String, HashSet<[u8; 32]>> }`
  - `pub fn parse(json: &str) -> Result<PinSet, PinError>` — JSON shape
    `{ "host": ["sha256/<base64>", ...], ... }`. Pin strings use the RFC 7469
    `sha256/<base64-standard>` form. Host keys are lowercased on parse. **Lenient on
    content but strict on structure**: a value that is not a JSON object of
    string→array-of-pin-strings is a hard `Err` (so a typo fails loudly at startup).
    An entry whose pin list is **empty** is a hard `Err` (it names the offending host):
    the operator clearly meant to pin that host but supplied no pins, so we fail loud at
    startup rather than silently degrade it to webpki-only — and rather than enforce an
    unsatisfiable set that permanently blocks it. (An empty top-level `{}` is fine and
    means "no hosts pinned".)
  - `pub fn is_empty(&self) -> bool`
  - `pub fn pins_for(&self, host: &str) -> Option<&HashSet<[u8; 32]>>` — exact,
    case-insensitive host lookup.
- `pub fn chain_has_pin(pins: &HashSet<[u8; 32]>, chain_ders: &[&[u8]]) -> bool` —
  true iff any cert in `chain_ders` hashes to a pin in `pins`. (Errors from
  `spki_sha256` on a malformed chain cert are treated as "no match" for that cert, not
  fatal — webpki has already validated the chain by the time this runs.)
- `pub enum PinError { Json(String), Pin(String), X509(String) }` (display-only).

**Proxy, impure — the rustls verifier (same `pins.rs`).**

```rust
#[derive(Debug)]
pub struct PinningVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>, // standard chain validation
    pins: PinSet,
}
impl rustls::client::danger::ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(&self, end_entity, intermediates, server_name, ocsp, now)
        -> Result<ServerCertVerified, rustls::Error>
    {
        // 1. ALWAYS: standard webpki chain validation. Fail-closed if it fails.
        self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)?;
        // 2. Pin overlay — only for hosts the operator pinned.
        if let Some(pins) = self.pins.pins_for(&server_name_host(server_name)) {
            let chain: Vec<&[u8]> = once(end_entity).chain(intermediates).map(|c| c.as_ref()).collect();
            if !chain_has_pin(pins, &chain) {
                return Err(rustls::Error::General("certificate pin mismatch".into()));
            }
        }
        Ok(ServerCertVerified::assertion())
    }
    // verify_tls12_signature / verify_tls13_signature / supported_verify_schemes
    //   -> delegate to self.inner
}
```

`server_name_host` renders the `ServerName` to the lookup string (DNS name verbatim;
IP literal in its canonical text form) so the key matches the CONNECT authority host
used to populate the pin map.

### 3.2 Data flow

**Startup (`egress-proxy::main`, before `lock_down`):**

1. Read `KASTELLAN_EGRESS_PROXY_PINS` (optional).
2. **Unset or empty ⇒ today's path exactly:** build the plain webpki `ClientConfig`
   (`with_root_certificates(webpki_roots).with_no_client_auth()`). Byte-identical, zero
   behaviour change, zero added cost.
3. **Present ⇒ `PinSet::parse`**, which is **fail-closed**: a set-but-unparseable env
   aborts startup with a clear error (never silently degrades to no-pinning). Build a
   `WebPkiServerVerifier` from the same `webpki-roots`, wrap it in `PinningVerifier`,
   and build the `ClientConfig` via
   `.dangerous().with_custom_certificate_verifier(Arc::new(verifier))`.
4. The resulting `Arc<ClientConfig>` flows into `MitmCtx.upstream_tls` **unchanged** —
   `mitm::intercept` needs **no signature change**; pinning rides inside the verifier it
   already receives.

**Per CONNECT (control flow unchanged):** `intercept` re-originates with `upstream_tls`;
rustls invokes `PinningVerifier::verify_server_cert(...)` mid-handshake → webpki chain
check → pin check for `server_name`. A mismatch fails the handshake (the verifier returns
`rustls::Error::General(PIN_MISMATCH_MARKER)`), surfacing through the existing `intercept`
error path. `run_mitm` distinguishes it from a generic transport failure by matching the
marker string and emits a **new `Verdict::BlockedTlsPin`** decision with reason
`pin_mismatch` (not an `Allowed`-with-failed-reason record). This mirrors slice #3b's
`BlockedCredentialLeak`: a pinned connection that fails the pin is a security *block*,
queryable as such in the audit log — the host maps it to `egress.blocked.tls_pin`. As
with #3b, the allow decision was already emitted pre-intercept (the CONNECT *was*
allowed; the pin was caught mid-handshake), so a pin rejection emits TWO decisions
(`Allowed (tls_intercepted)` then `BlockedTlsPin`) — coherent for the per-line audit
consumer, correlate by worker/host/port.

### 3.3 Enforcement semantics & fail posture

| Case | Result |
|------|--------|
| Host **not** in pin set | webpki chain validation only (today's behaviour) |
| Host pinned, chain valid, **some chain SPKI matches** | **allow** |
| Host pinned, chain valid, **no SPKI matches** | **reject — fail-CLOSED** (`pin_mismatch`) |
| Host pinned, chain invalid | reject (webpki fails first) |
| `KASTELLAN_EGRESS_PROXY_PINS` set-but-unparseable | **abort proxy startup** (fail loud) |

A pin rejection surfaces as `Verdict::BlockedTlsPin` (reason `pin_mismatch`), mapped
host-side to the `egress.blocked.tls_pin` audit action — a new block verdict alongside
`BlockedAllowlist`/`BlockedSsrf`/`BlockedCredentialLeak`.

Deliberately the **inverse of #3b's fail-*open* leak scanner**: pinning's entire purpose
is to *refuse* a substituted certificate, so a configured-but-unmatched pin must be
fail-closed. Absent config means "unpinned host → webpki-only" (graceful degrade, not a
bypass). The OS netns + allowlist + SSRF barrier remains the real containment boundary
regardless; pinning is additive defense for the high-value frontier leg, and (like the
leak scanner) catches only what a network adversary can be made to present — it is not
the containment boundary and is documented as such.

---

## 4. Host wiring + the params-struct refactor

The host **passes pins through**; it does not compute SPKI.

- `core/src/egress/spawn.rs`: add `const ENV_PINS = "KASTELLAN_EGRESS_PROXY_PINS"`.
  `proxy_policy` / `spawn_sidecar` take a `cert_pins_json: Option<&str>` (or `&str`,
  empty = none). When non-empty, push `(ENV_PINS, json)` into the sidecar `env`. When
  empty, **omit the env var entirely** so the no-pin path is byte-identical.
- `core/src/egress/net_worker.rs`: rather than grow `spawn_net_worker` /
  `spawn_forced_net_worker` to **9 positional args**, bundle into a `NetWorkerSpawn<'a>`
  params struct:

  ```rust
  pub struct NetWorkerSpawn<'a> {
      pub backend: &'a dyn SandboxBackend,
      pub proxy_bin: &'a Path,
      pub spec: &'a WorkerSpec<'a>,
      pub allowlist: &'a [String],
      pub scratch: &'a Path,
      pub worker_name: &'a str,
      pub secret_fingerprints: &'a [SecretFingerprint],
      pub cert_pins_json: Option<&'a str>,
  }
  ```

  `spawn_net_worker(params, on_decision)` / `spawn_forced_net_worker(params, ..., on_decision)`
  and **drop both `#[allow(clippy::too_many_arguments)]`**. This is exactly the refactor
  the handover flagged for "the first slice that adds a spawn arg" (alongside #268).
  Update the existing call sites (force-route cold-spawn path + tests) to construct the
  struct.
- **Optional guard:** the host may `serde_json::from_str::<PinSet-shaped>(...)` /
  reuse a tiny validator to confirm the pin JSON parses *before* spawn, so an operator
  typo fails loudly at spawn rather than at the proxy's startup. (The proxy is still the
  fail-closed authority; this is a nicety.)
- **Forward-looking:** today's callers pass `cert_pins_json: None`. No `PINS` env, no
  behaviour change. Reading the operator's real frontier pin config + routing frontier
  egress through a pinned sidecar is the standing deferral.

---

## 5. Testing (TDD)

**`pins.rs` units (pure):**
- `spki_sha256` against a known fixture cert (pin computed independently, e.g. via
  `openssl x509 -pubkey | openssl pkey -pubin -outform der | sha256` documented in the
  test) — pins the pre-image definition so the algorithm can't drift, **and guards the
  `x509-cert` `to_der()` re-encode**: if re-encoding ever diverged from the original
  SPKI bytes for a real cert, this test fails loudly.
- `PinSet::parse`: valid single pin; multiple pins per host; multiple hosts;
  case-insensitive host key; `sha256/<base64>` decode; **malformed → Err** (not object,
  wrong value type, bad base64, wrong digest length, **empty pin list → Err**); empty JSON
  object → empty set.
- `chain_has_pin`: end-entity match; intermediate match; no match → false; backup-pin
  (two hashes, the second matches) → true.

**Verifier unit (hermetic, reuse the `mitm/tests` test-CA harness):**
- unpinned host → Ok (webpki passes);
- pinned host + matching SPKI → Ok;
- pinned host + non-matching SPKI → Err.

**Proxy e2e (extend `mitm/tests`):** two-leg TLS round-trip where the test origin's SPKI
**is** pinned → succeeds; where a *different* SPKI is pinned → handshake fails
(MITM-rejected). Asserts the new `pin_mismatch` audit reason on the reject leg.

**Host units:**
- `proxy_policy` **omits** `ENV_PINS` when pins are `None`/empty; **includes** the exact
  JSON when set.
- the `NetWorkerSpawn` refactor preserves `spawn_net_worker` behaviour — existing
  `core/tests/egress_force_routing_e2e.rs` + `egress_proxy_e2e.rs` stay green unchanged.

**Cross-cutting:** cross-clippy `egress-proxy` for `aarch64-unknown-linux-gnu`
(pure-Rust crate, no linker needed); full `cargo test --workspace` green on Mac
(skip-as-pass); clippy `--workspace --all-targets -D warnings` clean. DGX real-bwrap run
not required this slice — the change is the existing MITM relay path plus a verifier the
DGX already exercises; flag if a Linux-specific concern surfaces.

---

## 6. Dependencies & risks

- **One new dependency:** `x509-cert` (RustCrypto; Apache-2.0 / MIT —
  **AGPL-compatible**) in `egress-proxy` only, for SPKI extraction. Chosen over
  `x509-parser` because its ASN.1 base (`der`/`spki`/`const-oid`) is **already in the
  lockfile** (transitive) and it shares the `sha2` digest family we already use, so it
  adds essentially one crate rather than a parallel `asn1-rs`/`nom` stack in the egress
  **security-boundary** crate. Trade-off: `to_der()` re-encodes (guarded by the §5
  fixture test). `x509-parser` would hash the verbatim SPKI slice but pulls four new
  crates — rejected for the larger audit/compile surface.
- **No new crate, no shared-crate churn** — pin logic is proxy-local.
- **`mitm::intercept` signature unchanged** — pinning rides inside `upstream_tls`.
- **rustls `dangerous()` API:** the custom verifier uses
  `ClientConfig::builder().dangerous().with_custom_certificate_verifier(...)`. The
  "dangerous" name reflects that a custom verifier *can* weaken validation; here it
  **strengthens** it (webpki delegate + pin overlay). Documented inline so a future
  reader doesn't mistake it for a validation bypass.
- **File-size budget:** `pins.rs` is a fresh small module (well under the 500-LOC cap);
  `net_worker.rs` is 520 LOC today — the `NetWorkerSpawn` struct adds a few lines but
  removes positional-arg noise at call sites; if it pushes materially over, lift the
  struct + its tests to a sibling (`net_worker/spawn_params.rs`) per the standing policy.

---

## 7. Acceptance

1. With **no** `KASTELLAN_EGRESS_PROXY_PINS`: behaviour byte-identical to slice #3b
   (existing egress e2e suites green, no env var emitted by `proxy_policy`).
2. With a pin set whose SPKI **matches** the origin: the MITM re-origination succeeds.
3. With a pin set whose SPKI does **not** match: the re-origination **fails closed** and
   emits a `pin_mismatch` audit decision.
4. A malformed `KASTELLAN_EGRESS_PROXY_PINS` **aborts proxy startup** with a clear error.
5. `spawn_net_worker` / `spawn_forced_net_worker` carry the params struct, no
   `#[allow(clippy::too_many_arguments)]`, all existing callers/tests updated and green.
6. New dep is AGPL-compatible; workspace clippy `-D warnings` clean; cross-clippy Linux
   clean.
