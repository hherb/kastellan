# Egress Proxy Slice #3b — Co-located Credential-Leak Scanner

**Status:** design approved, ready for implementation plan
**Date:** 2026-06-12
**ROADMAP:** 142 (egress proxy)
**Builds on:** slice #3a TLS-intercept (PR #259, `e2a7b2b`), whose
[design doc](2026-06-11-egress-proxy-slice3-tls-intercept-design.md) "Follow-up — slice #3b"
section scopes this work.

## 1. Summary

Slice #3a MITM-terminates each worker's TLS inside its per-worker egress-proxy sidecar, so the
proxy now sees **plaintext** request/response bodies. Slice #3b co-locates a **credential-leak
scanner** on that plaintext: each worker's outbound request / inbound response is scanned for the
verbatim bytes of any secret currently materialized for that worker. A hit **kills the connection
and is audited** carrying only the secret's value-hash + byte offset + direction — never the
plaintext, never the secret.

This is **defense-in-depth that raises the cost of naive or accidental secret leakage**, not a
perfect exfiltration barrier (see §8 Limitations). Its payoff is partly forward-looking: **no
current egress worker (web-fetch / web-search) carries secrets**, so the scanner activates only
when a secret-bearing egress worker lands. With no secrets provisioned, the scanner is never built
and egress behaves exactly as it does today (fast path).

References for prior art: IronClaw `safety::leak_detector`, ZeroClaw `security/leak_detector.rs`.

## 2. Locked decisions

These four decisions, made during brainstorming, frame the whole design:

1. **Detection = hashes-only (rolling-window).** The proxy holds only one-way hashes of each
   secret value (a SHA-256 + a 64-bit Rabin fingerprint + the length), never the value. A proxy
   compromise leaks only the length (+ a 64-bit fingerprint), never the secret. This matches 3a's
   "plaintext surface at zero" posture.
2. **Provisioning = scratch-file, lazily re-read.** The host writes
   `<scratch>/secret_hashes.json` into the sidecar's scratch dir (which the host already owns —
   it is where 3a's `ca.pem` lives, RAII-cleaned). The proxy re-reads it at each incoming
   connection. This inherently supports live updates and needs no new IPC.
3. **Block semantics = best-effort streaming + carry-over.** A sliding byte cursor with an
   `(maxSecretLen − 1)`-byte carry-over catches secrets straddling read boundaries. On a confirmed
   hit the connection is killed immediately and the hit is audited. The already-relayed in-body
   prefix is not recalled, but the kill denies request completion and the response round-trip.
4. **Wiring scope = mechanism + spawn-wire, dispatch-append deferred.** Build and fully
   (hermetically) test the whole mechanism; provision at sidecar spawn from the worker's known
   secret set (empty for today's workers); **file the dispatch-chokepoint live-append as a tracked
   follow-up** that lands with the first secret-bearing egress worker.

## 3. Why the timing problem shapes this slice

Secrets are **not known at sidecar-spawn time.** The egress sidecar is spawned first, then the
worker (`core/src/egress/net_worker.rs` `spawn_net_worker`). A secret only materializes at
**dispatch time** — `core/src/tool_host.rs::dispatch` calls
`secrets::substitute::substitute_refs_in_params`, which walks the planner's params and redeems
each `secret://…` ref into plaintext immediately before `worker.call`. There is no pre-declared
"this worker will receive secrets X, Y, Z" manifest; refs are discovered dynamically per call.

Consequently a one-shot spawn-time hand-off (e.g. a `KASTELLAN_EGRESS_PROXY_SECRET_HASHES` env)
would freeze the hash set at spawn and **miss exactly the secrets the worker actually receives.**
The scratch-file channel (decision §2.2) solves this: the host can append a hash the moment a
secret materializes, and the proxy's per-connection lazy re-read picks it up on the next
connection. The full dispatch-time append is deferred (§2.4); the mechanism is built so wiring it
later is additive.

## 4. Data flow

```
HOST (core, daemon)                          SIDECAR (egress-proxy, sandboxed)
─────────────────────────────────           ─────────────────────────────────────
Vault holds secret plaintext
  │ value_fingerprint(ref)  ── pure ──►  SecretFingerprint{ len, fp64, sha256 }
  │   (read-lock; never returns/logs plaintext)
  │
  │ write_secret_hashes(scratch,&[fp])  ─atomic write─►  <scratch>/secret_hashes.json
  │ + emit policy/egress.secret_hash.provisioned                                │
  │   { worker, name, value_sha256 }                                           │ per incoming CONNECT:
  │                                                                            │   lazy-read secret_hashes.json
  │                                                                            │   build LeakScanner(patterns)
  │                                            MITM intercept (3a) terminates TLS, then:
  │                                            request  client→upstream ─► scanner.feed()
  │                                            response upstream→client ─► scanner.feed()
  │                                                     │ on confirmed hit: kill conn + Decision
  │   audit_log ◄─ ingest_decisions_into ◄──── Verdict::BlockedCredentialLeak
  │   action: egress.blocked.credential_leak    { leaked_sha256, leak_offset, leak_direction }
  │   payload: hash + offset + direction (NEVER plaintext)
```

With no `secret_hashes.json` (or an empty list) the scanner is never built; the MITM relay is the
unchanged 3a `copy_bidirectional` path.

## 5. Components

### 5.1 Pure `LeakScanner` (`workers/egress-proxy/src/leak_scanner.rs`) — the heart

Self-contained, I/O-free, fully unit-testable — the `linux_bwrap::build_argv` precedent (pure
logic separable from the spawn/relay).

```rust
pub struct SecretFingerprint { pub len: usize, pub fp64: u64, pub sha256: [u8; 32] }

pub struct LeakHit { pub sha256_hex: String, pub offset: u64 } // offset = stream pos of window start

pub struct LeakScanner { /* patterns grouped by len; per-len rolling state; ring buffer; byte counter */ }

impl LeakScanner {
    pub fn new(patterns: Vec<SecretFingerprint>) -> Self;
    /// Feed a chunk; return the first confirmed hit. Stateful across calls (carry-over).
    pub fn feed(&mut self, chunk: &[u8]) -> Option<LeakHit>;
}
```

**Algorithm — Rabin-Karp rolling hash + SHA-256 confirm:**

- Patterns are grouped by length `L`. For each distinct `L`, maintain one polynomial rolling hash
  over a sliding `L`-byte window of the stream — O(1) per byte, with `B^(L−1)` precomputed per
  length.
- A ring buffer retains the last `maxLen` bytes. This is the **carry-over**: a secret straddling a
  read boundary (`…AB | CD…`) matches on the same pass because state persists across `feed()`
  calls. Chunk boundaries are transparent.
- On a rolling-hash match for length `L`, extract the exact `L`-byte window from the ring buffer,
  compute its SHA-256, and compare to the provisioned `sha256` for that length. **The SHA-256
  confirm eliminates Rabin-Karp collisions** → no false positives from the cheap pre-filter.
- O(1) memory beyond the small ring buffer ⇒ the scanner streams the **entire** connection with
  **no body cap** — a property buffer-then-forward could not afford.
- `MIN_SECRET_LEN = 8`: the host never provisions shorter secrets (trivial values have high false-
  positive rates); the scanner also defensively ignores any pattern under the floor.

`fp64` is provisioned alongside `sha256` because the proxy needs a value-derived rolling
fingerprint to pre-filter cheaply. Both `fp64` and `sha256` are one-way for a high-entropy secret,
so this stays strictly hashes-only.

### 5.2 Scanning relay (`workers/egress-proxy/src/mitm/relay.rs`)

A small sibling of `mitm.rs` (keeps `mitm.rs` under the 500-LOC cap) that replaces 3a's
`copy_bidirectional` with a **scanning bidirectional relay**: each half (`client→upstream` =
request, `upstream→client` = response) feeds its bytes through its own `LeakScanner` before
forwarding. A confirmed hit aborts the relay and surfaces a leak error carrying
`{ sha256_hex, offset, direction }`. When no patterns are provisioned, `intercept` keeps using the
plain `copy_bidirectional` path (no scanner allocated).

### 5.3 Proxy wiring (`main.rs`, `proxy.rs`, `mitm.rs`)

- `main.rs`: derive the `secret_hashes.json` path as a sibling of the UDS (same pattern as
  `ca.pem`); thread it into `MitmCtx`.
- `proxy.rs` `run_mitm`: lazy-read the file per MITM connection; map a leak error from the relay
  into a `Decision { verdict: BlockedCredentialLeak, leaked_sha256, leak_offset, leak_direction }`
  reported via the existing `LineReporter`.
- `mitm.rs` `intercept`: thread the (optional) patterns and call the scanning relay when present.

### 5.4 Proxy decision (`workers/egress-proxy/src/report.rs`)

- Add `Verdict::BlockedCredentialLeak`.
- `Decision` gains three additive optional fields (`#[serde(default)]`, so 3a serialization is
  unaffected): `leaked_sha256: Option<String>`, `leak_offset: Option<u64>`,
  `leak_direction: Option<String>` (`"request"` | `"response"`).

### 5.5 Host-side provisioning (`core/src/egress/leak_provision.rs`)

```rust
pub struct SecretFingerprint { pub len: usize, pub fp64: u64, pub sha256: [u8; 32] }

/// Pure. None if value.len() < MIN_SECRET_LEN. Computes fp64 (Rabin) + SHA-256.
pub fn fingerprint_value(value: &[u8]) -> Option<SecretFingerprint>;

/// Atomic write (temp + rename) so the proxy's lazy read never sees a torn file.
/// Empty slice writes an empty list (proxy treats as "no scanning").
pub fn write_secret_hashes(scratch: &Path, fps: &[SecretFingerprint]) -> io::Result<()>;
```

File format `<scratch>/secret_hashes.json`:

```json
{ "version": 1,
  "secrets": [ { "len": 40, "fp64": "<16 hex>", "sha256": "<64 hex>" } ] }
```

`fp64` and `sha256` are serialized as hex strings (avoids JSON `u64`-precision pitfalls). The proxy
parses this same shape in `leak_scanner.rs`.

### 5.6 Minimal Vault introspection (`core/src/secrets/vault.rs`)

The one new method the 3a follow-up flagged as missing:

```rust
/// Compute a one-way fingerprint of the secret's value without exposing it.
/// None if not present/expired or below MIN_SECRET_LEN.
pub fn value_fingerprint(&self, r: &SecretRef) -> Option<SecretFingerprint>;
```

Takes the read-lock, computes the fingerprint from the `Zeroizing<Vec<u8>>` plaintext, returns the
fingerprint. **Never returns or logs the plaintext.** This is the only widening of the Vault
surface and it exposes only one-way hashes. (Note: today's audit uses `SecretRef::ref_hash()` =
SHA-256 of the opaque *ref string*; this slice introduces the distinct SHA-256 of the secret
*value*. They are deliberately different hashes.)

### 5.7 Spawn-wiring (`core/src/egress/net_worker.rs`, `spawn.rs`)

`spawn_net_worker` / `spawn_forced_net_worker` gain an optional
`secret_fingerprints: &[SecretFingerprint]` argument; when non-empty they call
`write_secret_hashes` and emit one `policy / egress.secret_hash.provisioned { worker, name,
value_sha256 }` audit row per secret. **Today's callers pass an empty slice** (no egress worker
carries secrets). The audit row is what makes a later leak hash correlatable to a secret *name*
(the leak decision carries only `value_sha256`); the value hash is one-way, so storing
`name + value_sha256` in the audit log is safe.

### 5.8 Host audit mapping (`core/src/egress/audit.rs`)

`DecisionLine` gains the same three optional fields. `decision_to_audit` maps verdict
`"blocked_credential_leak"` → action `egress.blocked.credential_leak`, payload carries the existing
fields (worker, host, port, resolved_ip, reason, tls_intercepted) **plus** `leaked_sha256`,
`leak_offset`, `leak_direction` — **never plaintext**, mirroring the `cassandra::injection_guard`
redacted-audit pattern (`tool_host.rs` injection.blocked row carries `body_sha256`, never the body).

## 6. Testing (TDD — tests precede implementation)

| Test | Where | Verifies |
| ---- | ----- | -------- |
| `leak_scanner` unit | egress-proxy | exact match; no match; **secret split across two `feed()` calls** (boundary pin); two secrets same length; two different lengths; `< MIN_SECRET_LEN` ignored; offset correctness; empty-patterns no-op; **fp64 collision → SHA-256 rejects the false positive** |
| `leak_provision` unit | core | `fingerprint_value` known-vector + min-length `None`; `write_secret_hashes` round-trip + atomic-replace |
| `Vault::value_fingerprint` unit | core | materialize → fingerprint matches expected SHA-256; `None` for short/absent |
| `report` unit | egress-proxy | new verdict + fields JSON round-trip; 3a lines still deserialize (additive `#[serde(default)]`) |
| `egress::audit` unit | core | credential-leak line → correct action + payload (hash/offset/direction, **no plaintext**); garbage tolerance |
| `egress_leak_scan_e2e` integration | core | **real sandbox**, extends the `egress_force_routing_e2e` harness: provision a synthetic secret, drive a MITM connection whose body contains it → `BlockedCredentialLeak` + connection killed + audit row (hash+offset, no plaintext); a clean body passes. Cross-platform (Seatbelt + bwrap); skip-as-pass without sandbox/proxy-bin/PG |

## 7. Files & LOC discipline

**New:** `workers/egress-proxy/src/leak_scanner.rs`, `workers/egress-proxy/src/mitm/relay.rs`,
`core/src/egress/leak_provision.rs`, `core/tests/egress_leak_scan_e2e.rs`.

**Edited (kept < 500 LOC):** `workers/egress-proxy/src/{mitm.rs, proxy.rs, main.rs, report.rs}`,
`core/src/egress/{audit.rs, net_worker.rs, spawn.rs, mod.rs}`, `core/src/secrets/vault.rs`.

Pure logic (scanner, fingerprint) is isolated in its own modules, separately testable from I/O.

## 8. Limitations (stated plainly)

- **Exact-contiguous-byte detection only.** Defeated by encoding (base64 / hex / url-encode the
  secret) or by splitting a secret across separate requests/connections — each body is then
  individually clean. This is inherent to value-hash matching and is shared by **any** block mode
  (buffering does not help). 3b raises the cost of naive/accidental leakage; it is **not** a perfect
  exfiltration barrier. Encoding-aware / canary-token DLP is explicitly out of scope.
- **Best-effort block.** An already-relayed in-body prefix is not recalled; the kill denies request
  completion and the response round-trip.
- **Length + 64-bit fingerprint leak** to the sidecar via the provisioned file. The sidecar holds
  no plaintext and is already inside the worker's trust domain.
- **Forward-looking.** Spawn-wiring provisions an empty set today; the scanner activates with the
  first secret-bearing egress worker.

## 9. Deferrals / follow-up issues to file

- **Dispatch-chokepoint live-append** (the §2.4 deferral): thread the worker's sidecar scratch path
  so `tool_host::dispatch` appends a value-hash the moment `substitute_refs_in_params` materializes
  a secret. Lands with — and is shaped by — the first secret-bearing egress worker.
- **Encoding-aware detection / canary tokens** — out of scope for 3b; note as the natural ceiling of
  value-hash matching.
- Then **slice #4** (TLS pinning for the frontier/LLM path) — its own spec.
