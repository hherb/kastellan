# Egress proxy SLICE #3a — TLS-intercept (MITM) mechanism

**Status:** design approved 2026-06-11. Implementation pending.
**ROADMAP:** 142 (egress proxy slice #3). This spec covers **slice #3a only** — the
TLS-interception *mechanism*. The co-located **credential-leak scanner is slice #3b**, a
separate spec/session (scoped at the end of this doc).

## Why

Slice #2 made the per-worker egress proxy *unbypassable*: every `Net::Allowlist` worker is
force-routed through its own sandboxed proxy sidecar over a UDS, in a private netns with no
direct route out. But the proxy tunnels CONNECT **opaquely** — the bytes are end-to-end TLS
between the worker and the origin, so the proxy can enforce only *who* the worker talks to
(host:port allowlist + SSRF/IP-pinning), never *what* it sends.

To scan egress for credential leaks (slice #3b) the proxy must see plaintext. The only way to
do that at the trust boundary — rather than inside the possibly-compromised worker — is to
**terminate the worker's TLS at the proxy and re-originate a fresh TLS session to the real
origin** (a deliberate, productive man-in-the-middle). The proxy is the *trusted* component;
MITM is how the host earns the right to inspect egress without trusting the worker.

Slice #3a builds and proves that mechanism. It surfaces **no** new plaintext to logs/audit
(see "Privacy posture"); the scanner that decides what is safe to surface is 3b.

## Scope

**In scope (3a):**
- Per-instance ephemeral CA generated *inside* the proxy; private key never leaves the sandbox.
- Per-host leaf certs signed by that CA, issued on demand and cached in-process.
- Sync `rustls` server-side termination of the worker's TLS + sync `rustls` client-side
  re-origination to the pinned origin (real origin cert validated against `webpki-roots`).
- Plain-HTTP-over-CONNECT pass-through (peek the first byte; only `0x16` → MITM).
- Host wiring: export the public CA cert, bind it into the worker jail, point the worker's
  rustls trust at *only* that CA.
- One additive audit field: `tls_intercepted: bool`.

**Out of scope (deferred to 3b / later):**
- The credential-leak body scanner and the Vault → proxy secret-value-hash provisioning path.
- Any logging/audit of request method/path/headers/body (privacy posture below).
- TLS pinning for the frontier/LLM path (slice #4).
- HTTP/2 / ALPN negotiation across the MITM (workers use HTTP/1.1 via `hyper` today; keep h1).

## Locked design decisions

1. **CA model — in-proxy, ephemeral, per-instance.** Each spawned proxy sidecar generates a
   fresh CA keypair at startup. The CA *private* key lives only in the proxy process memory
   (zeroized on exit); only the public CA cert (PEM) is written to the scratch dir. A CA
   compromise is scoped to one worker's one short-lived proxy — consistent with the per-worker
   containment invariant. (Rejected: host-generated CA injected into the proxy — puts a signing
   key on the host; host-persisted single CA — one extractable key forges TLS for every worker.)

2. **Worker trust — only the per-instance CA (fail-closed).** When `KASTELLAN_EGRESS_PROXY_CA`
   is set, the worker's `ProxyConnectGet` builds a `RootCertStore` from *only* that CA;
   `webpki-roots` are dropped. The worker is in a private netns whose sole egress is the proxy,
   and the proxy does the *real* origin-cert validation on the re-originated leg — so the worker
   trusting only-the-CA means TLS fails closed if anything but the proxy terminates it.
   (Rejected: additive CA + webpki — only creates a way to *not* fail closed, no benefit here.)

3. **TLS stack — sync `rustls` inside the existing proxy.** The proxy stays synchronous
   (`std::net`, thread-per-connection). `rustls`'s blocking API (`StreamOwned<ServerConnection,
   S>` / `StreamOwned<ClientConnection, S>`) needs no async runtime. (Rejected: rewrite onto
   `tokio-rustls` — bigger change to a working component; `native-tls`/OpenSSL — adds a C dep
   that worsens the `ring`/#144 cross-compile wall.) The worker's `ProxyConnectGet` keeps its
   existing `tokio-rustls` async client — separate process, no need to match.

4. **Always-MITM for TLS targets; pass-through for plaintext.** Every allowlisted CONNECT whose
   first tunnel byte is `0x16` is terminated+re-originated. Non-TLS bytes (plain-HTTP-over-
   CONNECT) are already plaintext and pass through unchanged (3b scans them directly). No
   SNI parsing needed — the CONNECT authority host *is* the cert name.

## Architecture

Per connection, inside the egress-proxy worker (all synchronous):

1. Read CONNECT line → `(host, port)`; run existing `decide()` (allowlist + DNS + SSRF/IP-pin
   in `ssrf::is_denied_range`). **unchanged**
2. On allow, write `HTTP/1.1 200 Connection Established`. **unchanged**
3. **Peek the first byte** of the tunnel. `0x16` → MITM path. Else → pass-through (existing
   `tunnel()` raw copy).
4. **MITM path:**
   a. Fetch-or-issue a leaf cert for `host` (signed by the in-proxy CA), cached by host.
   b. Sync `rustls` **server** handshake over the client `UnixStream` → decrypted client stream.
   c. Dial the pinned origin IP:port (existing SSRF-checked dial) → sync `rustls` **client**
      handshake with SNI=`host`, validating the real origin cert against `webpki-roots` →
      decrypted upstream stream.
   d. Copy plaintext bidirectionally between the two decrypted streams (existing `tunnel()`
      shape, now over TLS streams). **3a surfaces nothing from inside; 3b scans here.**

CA lifecycle: at startup the proxy `generate_ca()`s, holds the private key in-process (zeroized
on exit), and writes only the public CA PEM to `scratch/ca.pem` before the accept loop.

Host wiring (`core/src/egress`): `spawn_sidecar` already waits for the UDS; it additionally
waits for `ca.pem`, then `rewrite_worker_policy` adds that cert path to the worker's `fs_read`
(so bwrap `--ro-bind`s it into the jail / Seatbelt allows the read) and sets
`KASTELLAN_EGRESS_PROXY_CA` in the worker env.

Worker trust (`web-common::proxy_connect`): when `KASTELLAN_EGRESS_PROXY_CA` is set,
`ProxyConnectGet::new` reads that PEM and builds its `RootCertStore` from only those anchors.

## Components

**New, in `workers/egress-proxy/src/`:**

- `ca.rs` — `generate_ca() -> CaMaterial` (CA cert + key via `rcgen`) and
  `issue_leaf(&CaMaterial, host) -> CertifiedKey`. Pure/unit-testable: a generated leaf chains
  to the CA; the leaf's SAN matches `host`; the CA cert PEM round-trips (parse → serialize).
- `mitm.rs` — `looks_like_tls(first_byte: u8) -> bool` (pure; `== 0x16`) and
  `intercept(client, dial_upstream, host, &CaMaterial, &mut LeafCache)`: the sync
  server-handshake + client-handshake + plaintext copy. The byte-peek + branch decision is
  separated from the I/O so the branch logic is unit-testable.
- `leaf_cache.rs` — `LeafCache(HashMap<String, Arc<CertifiedKey>>)`, bounded with the existing
  `MAX_TRACKED_*` idiom; `get_or_issue(host, &CaMaterial)`.

**Touched:**

- `workers/egress-proxy/src/proxy.rs::handle_conn` — after the `200`, peek → MITM-or-pass-through.
- `workers/egress-proxy/src/main.rs` — `generate_ca()`, write `scratch/ca.pem`, before accept loop.
- `workers/egress-proxy/src/report.rs::Decision` — additive `tls_intercepted: bool` (default false;
  set true on the MITM path's allow row).
- `core/src/egress/spawn.rs::spawn_sidecar` — also wait for `ca.pem` alongside the UDS readiness poll.
- `core/src/egress/net_worker.rs::rewrite_worker_policy` — push the CA path into worker `fs_read`
  and set `KASTELLAN_EGRESS_PROXY_CA`.
- `core/src/egress/audit.rs` — thread `tls_intercepted` into the `egress.allowed` payload
  (`decision_to_audit` / `DecisionLine`).
- `workers/web-common/src/proxy_connect.rs::ProxyConnectGet::new` — only-CA `RootCertStore`
  when `KASTELLAN_EGRESS_PROXY_CA` is set; webpki-only otherwise (legacy slice-#1/#2 path).

**New dependency:** `rcgen` (license **MIT OR Apache-2.0** → AGPL-compatible; `ring` backend,
already in-tree). `rustls`'s server side is already pulled in (client use today), just unused.
Confirm the lockfile adds no incompatibly-licensed transitive dep.

## Privacy posture (the one deliberate non-feature of 3a)

The proxy now holds plaintext, but **3a logs nothing new from inside the tunnel** — only an
additive boolean `tls_intercepted: true` on the existing per-CONNECT `egress.allowed` audit
row. Request paths and bodies can carry secrets; the component that decides what is safe to
surface is the 3b scanner. Proof-of-interception for the tests is **structural** (the
round-trip succeeds *only because* the proxy terminated + re-originated TLS), not "we logged
the plaintext." This keeps 3a's plaintext surface at zero and avoids a privacy regression 3b
would have to walk back.

## Test plan (TDD — write the test first)

- **Proxy units:** `looks_like_tls` truth table; `generate_ca`/`issue_leaf` chaining + SAN
  match + PEM round-trip; `LeafCache` hit/issue/eviction; MITM-vs-pass-through branch selection.
- **Worker unit:** `ProxyConnectGet` builds an only-CA `RootCertStore` when the env var is set,
  webpki-only otherwise.
- **Proxy integration (real, cross-platform):** a real sandboxed sidecar + a loopback `rustls`
  test origin; a client routed through the proxy completes a request **only** when trusting the
  per-instance CA; the two-leg termination is exercised; off-allowlist still 403s; plain-HTTP-
  over-CONNECT still passes through. Skip-as-pass without sandbox/proxy-bin.
- **Force-routing e2e extension** (`core/tests/egress_force_routing_e2e.rs`): the existing
  allow-round-trip now traverses a *terminated + re-originated* TLS path end-to-end (Seatbelt on
  the Mac, bwrap on the DGX) and asserts `tls_intercepted: true` reaches the `on_decision` sink.
- **Risks to verify, not assume:**
  1. `rcgen`'s `ring` backend needs no syscall the `WorkerNetClient` seccomp profile denies
     (cert gen = getrandom + CPU; #243 already cleared AF_UNIX). Verify on the DGX; if a syscall
     is missing, file an issue and decide deliberately rather than widening the profile blindly.
  2. macOS Seatbelt unaffected (in-process crypto, no new sockets/files beyond `scratch/ca.pem`,
     which is under the already-writable scratch dir).

## Acceptance (slice #3a done when)

- Workspace builds + `clippy --workspace --all-targets -D warnings` clean (Mac + DGX).
- The proxy integration + force-routing e2e pass with **real containment** (no `[SKIP]`) on the
  DGX: a worker fetch round-trips through the MITM path and `tls_intercepted: true` is audited.
- Mac skip-as-pass posture green; new units pass on both.
- No new plaintext in any audit row beyond the `tls_intercepted` boolean.
- New/changed files under the 500-LOC cap.

## Follow-up — slice #3b (the scanner), next spec

3b co-locates a **credential-leak scanner** on the now-visible plaintext. It needs a path the
current code does **not** have: the Vault (`core/src/secrets/vault.rs`) today exposes no
introspection, and the audit log carries only `SecretRef::ref_hash()` — the SHA-256 of the
opaque `secret://…` *ref string*, **not** any hash of the secret *value*. 3b must add a way for
the host to provision, into the per-worker proxy, the SHA-256 (or a prefix) of each secret
*value* currently materialized for that worker, then scan each outbound request / inbound
response body for those hashes; hits are blocked + audited (carrying only the hash + offset,
never plaintext), mirroring the `cassandra::injection_guard` `screen`/catalogue + redacted-
audit pattern. Note 3b's payoff is partly forward-looking: **no current egress worker
(web-fetch/web-search) carries secrets**, so the scanner pays off when a secret-bearing egress
worker lands. References: IronClaw `safety::leak_detector`, ZeroClaw `security/leak_detector.rs`.

Then slice #4 (TLS pinning for the frontier/LLM path) — its own spec.
