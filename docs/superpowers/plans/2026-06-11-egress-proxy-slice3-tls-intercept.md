# Egress Proxy Slice #3a — TLS-Intercept (MITM) Mechanism — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the per-worker egress proxy terminate each worker's TLS (with a per-instance CA the worker trusts) and re-originate a fresh, properly-validated TLS session to the real origin, so a future slice can scan the plaintext — without surfacing any new plaintext in 3a.

**Architecture:** The proxy keeps its sync accept loop, CONNECT parse, and `decide()`. After it connects upstream and writes `200`, it peeks the first tunnel byte: `0x16` (TLS) → MITM on a per-connection current-thread tokio runtime (`tokio-rustls` `TlsAcceptor` presenting a CA-signed leaf for the host, `TlsConnector` re-originating to the pinned IP validating the real origin against webpki, `copy_bidirectional`); anything else → existing sync pass-through tunnel. The proxy generates an ephemeral CA at startup (private key in-process only) and writes the public CA PEM to its scratch dir; the host bind-mounts that into the worker jail and points the worker's rustls trust at *only* that CA.

**Tech Stack:** Rust, `rustls` 0.23 (sync server config + client config), `tokio-rustls` 0.26, `tokio` 1 (current-thread runtime), `rcgen` 0.13 (cert generation), `rustls-pki-types` (PEM parse). Spec: `docs/superpowers/specs/2026-06-11-egress-proxy-slice3-tls-intercept-design.md`.

**Conventions for every task:**
- Build/test prelude: `source "$HOME/.cargo/env"` before any `cargo` command.
- Keep every new/changed file under 500 LOC (project rule 4). The egress-proxy unit-test
  convention is an in-crate `#[cfg(test)] mod tests;` sibling file (e.g. `src/ca/tests.rs`), NOT
  a `tests/` dir (the crate is a `[[bin]]` with no lib target, so external integration tests
  can't see its modules).
- Commit after each green step with the shown message. Stage **only** the files named in the
  step (`git add <files>`), never `git add -A` (the repo has an untracked
  `docs/essay-medium-draft.md` that must stay out).
- Commit-message trailer on every commit:
  `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

## File Structure

**`workers/egress-proxy/` (new modules, each one responsibility):**
- `src/ca.rs` — `CaMaterial` (the per-instance CA cert + `Issuer`) + `generate_ca()` +
  `issue_leaf(&CaMaterial, host) -> LeafCert`. Pure crypto, no I/O. `+ src/ca/tests.rs`.
- `src/leaf_cache.rs` — `LeafCache`: bounded `host -> Arc<rustls leaf chain+key>` cache over
  `issue_leaf`. `+ src/leaf_cache/tests.rs`.
- `src/mitm.rs` — `looks_like_tls(u8) -> bool` (pure) + async `intercept(...)` (the two-leg
  termination + `copy_bidirectional`). `+ src/mitm/tests.rs` (the hermetic round-trip).

**`workers/egress-proxy/` (modified):**
- `src/proxy.rs` — restructure the `Target::Dial` arm of `handle_conn` to connect→200→peek→branch.
- `src/report.rs` — additive `Decision.tls_intercepted: bool`.
- `src/main.rs` — generate the CA at startup, write `<scratch>/ca.pem`, thread CA + leaf-cache +
  upstream TLS config into `handle_conn`.
- `Cargo.toml` — add `rustls`, `tokio-rustls`, `tokio`, `rustls-pki-types`, `rcgen`.

**`workers/web-common/` (modified):**
- `src/proxy_connect.rs` — `ProxyConnectGet::new` reads an optional CA path; only-CA root store.
- `src/http.rs` — `make_get_inner`/`make_get` read `KASTELLAN_EGRESS_PROXY_CA`, pass it through.
- `Cargo.toml` — enable the `pem` feature on `rustls-pki-types`.

**`core/` (modified):**
- `src/egress/spawn.rs` — `spawn_sidecar` also waits for `<scratch>/ca.pem`; export `CA_FILE_NAME`.
- `src/egress/net_worker.rs` — `rewrite_worker_policy` adds the CA path to `fs_read` + sets
  `KASTELLAN_EGRESS_PROXY_CA`.
- `src/egress/audit.rs` — carry `tls_intercepted` through `DecisionLine` → payload.
- `tests/egress_force_routing_e2e.rs` — assert `ca.pem` written + worker got the CA env, under
  the real sandbox; keep the plaintext pass-through round-trip.

---

## Task 0: Add dependencies to the egress-proxy crate

**Files:**
- Modify: `workers/egress-proxy/Cargo.toml`

- [ ] **Step 1: Add the TLS + cert-gen dependencies**

Append to the `[dependencies]` table in `workers/egress-proxy/Cargo.toml` (mirror the exact
version pins web-common already uses so the lockfile stays consistent):

```toml
tokio        = { version = "1",    features = ["rt", "net", "io-util", "time"] }
tokio-rustls = { version = "0.26", default-features = false, features = ["ring", "tls12", "logging"] }
rustls       = { version = "0.23", default-features = false, features = ["ring", "tls12", "logging", "std"] }
rustls-pki-types = { version = "1", features = ["std"] }
rcgen        = { version = "0.13", default-features = false, features = ["pem", "ring"] }
```

Also update the crate `description` line to drop the "Slice #1 (no TLS interception)." clause:

```toml
description = "Per-worker egress proxy: host-allowlist + SSRF/IP-pinning boundary + TLS interception over a UDS."
```

- [ ] **Step 2: Verify it builds and the licenses are AGPL-compatible**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-egress-proxy`
Expected: builds clean (no code uses the new deps yet — this just resolves them).

Run: `cargo tree -p kastellan-worker-egress-proxy -e features -i rcgen` and confirm `rcgen`
resolves to a 0.13.x. Spot-check the new transitive deps for license: rcgen is `MIT OR
Apache-2.0`, ring is ISC/MIT/OpenSSL-style — all AGPL-compatible. If `cargo deny` is configured,
run it; otherwise eyeball `cargo tree` for any new `*-sys`/copyleft-incompatible crate.

- [ ] **Step 3: Commit**

```bash
git add workers/egress-proxy/Cargo.toml Cargo.lock
git commit -m "build(egress-proxy): add rustls/tokio-rustls/rcgen for TLS interception

Slice #3a deps. No code uses them yet; this isolates the dependency +
license review in one commit.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 1: `looks_like_tls` — the pure peek-branch predicate

**Files:**
- Create: `workers/egress-proxy/src/mitm.rs`
- Create: `workers/egress-proxy/src/mitm/tests.rs`
- Modify: `workers/egress-proxy/src/main.rs` (add `mod mitm;`)

- [ ] **Step 1: Write the failing test**

Create `workers/egress-proxy/src/mitm/tests.rs`:

```rust
use super::looks_like_tls;

#[test]
fn tls_handshake_record_byte_is_recognised() {
    // 0x16 == TLS ContentType::Handshake — the first byte of a ClientHello.
    assert!(looks_like_tls(0x16));
}

#[test]
fn plaintext_http_first_bytes_are_not_tls() {
    // 'C' (CONNECT/GET bodies), 'G', 'P' — none are 0x16.
    assert!(!looks_like_tls(b'G'));
    assert!(!looks_like_tls(b'C'));
    assert!(!looks_like_tls(0x00));
    assert!(!looks_like_tls(0x17)); // application-data, not handshake
}
```

Create `workers/egress-proxy/src/mitm.rs` with just the module wiring + a stub so it compiles:

```rust
//! TLS interception: decide whether a tunnel is TLS, and if so terminate the
//! worker's TLS with a per-instance-CA leaf and re-originate a validated TLS
//! session to the pinned origin. The pure peek predicate is split from the
//! async I/O so the branch logic is unit-testable without sockets.

/// True iff `first_byte` is the TLS record ContentType for `handshake` (0x16),
/// i.e. the first byte of a ClientHello. Anything else is treated as an
/// already-plaintext tunnel (plain-HTTP-over-CONNECT) and passed through.
pub fn looks_like_tls(first_byte: u8) -> bool {
    first_byte == 0x16
}

#[cfg(test)]
mod tests;
```

Add `mod mitm;` to `workers/egress-proxy/src/main.rs` next to the other `mod` lines.

- [ ] **Step 2: Run the test, expect pass** (the predicate is trivial; TDD here pins intent)

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy looks_like_tls -- --exact 2>&1 | tail -20`
Actually run by module: `cargo test -p kastellan-worker-egress-proxy mitm::tests`
Expected: both tests PASS.

- [ ] **Step 3: Commit**

```bash
git add workers/egress-proxy/src/mitm.rs workers/egress-proxy/src/mitm/tests.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): looks_like_tls peek predicate (slice #3a)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Per-instance CA + leaf issuance (`ca.rs`)

**Files:**
- Create: `workers/egress-proxy/src/ca.rs`
- Create: `workers/egress-proxy/src/ca/tests.rs`
- Modify: `workers/egress-proxy/src/main.rs` (add `mod ca;`)

- [ ] **Step 1: Write the failing test**

Create `workers/egress-proxy/src/ca/tests.rs`:

```rust
use super::{generate_ca, issue_leaf};

#[test]
fn ca_pem_round_trips_as_a_parseable_certificate() {
    let ca = generate_ca().expect("generate CA");
    let pem = ca.cert_pem();
    assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
    // Parse it back as a DER cert via rustls-pki-types to prove it's well-formed.
    let der: Vec<_> = rustls_pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .expect("CA PEM parses as a certificate");
    assert_eq!(der.len(), 1, "exactly one CA certificate in the PEM");
}

#[test]
fn issued_leaf_carries_the_requested_host_as_san() {
    let ca = generate_ca().expect("generate CA");
    let leaf = issue_leaf(&ca, "api.example.com").expect("issue leaf");
    // The leaf DER + key DER are produced (non-empty) and the SAN is the host.
    assert!(!leaf.cert_der().is_empty());
    assert!(!leaf.key_der().secret_der().is_empty());
    // Decode the leaf and assert the SAN dnsName. Use a tolerant check: the
    // host string must appear in the DER (dnsNames are stored as ASCII).
    let needle = b"api.example.com";
    assert!(
        leaf.cert_der().windows(needle.len()).any(|w| w == needle),
        "leaf DER must encode the requested host as a SAN"
    );
}

#[test]
fn two_generated_cas_differ() {
    // Ephemeral per-instance: every CA is fresh.
    let a = generate_ca().unwrap();
    let b = generate_ca().unwrap();
    assert_ne!(a.cert_pem(), b.cert_pem(), "each CA must be unique");
}
```

- [ ] **Step 2: Run the test, expect a compile failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy ca::tests 2>&1 | tail -20`
Expected: FAIL to compile — `generate_ca`/`issue_leaf` undefined.

- [ ] **Step 3: Implement `ca.rs`**

Create `workers/egress-proxy/src/ca.rs`:

```rust
//! Per-instance ephemeral CA + on-demand leaf issuance for TLS interception.
//!
//! Each proxy process generates ONE CA at startup; its private key lives only
//! here (never written to disk — only the public cert PEM is exported for the
//! host to inject into the worker's trust store). Leaves are signed per-host on
//! demand and presented to the worker, which trusts only this CA. A CA
//! compromise is therefore scoped to one worker's one short-lived proxy.

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// The process-lifetime CA: the public cert (PEM + DER) plus the `Issuer` used
/// to sign leaves. The CA `KeyPair` is held inside `issuer` and never leaves
/// this process.
pub struct CaMaterial {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    issuer: Issuer<'static, KeyPair>,
}

impl CaMaterial {
    /// Public CA certificate, PEM-encoded — the only thing exported off-process.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Public CA certificate, DER — for building a leaf chain if ever needed.
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }
}

/// A signed leaf for one host: the cert DER + its private key DER, ready to be
/// dropped into a rustls `ServerConfig::with_single_cert`.
pub struct LeafCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
}

impl LeafCert {
    pub fn cert_der(&self) -> &CertificateDer<'static> {
        &self.cert_der
    }
    pub fn key_der(&self) -> &PrivateKeyDer<'static> {
        &self.key_der
    }
    /// Consume into the (chain, key) pair rustls' `with_single_cert` wants.
    pub fn into_rustls(self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (vec![self.cert_der], self.key_der)
    }
}

/// Generate a fresh ephemeral CA. Default rcgen validity (a wide fixed window)
/// is fine for an ephemeral per-process CA.
pub fn generate_ca() -> Result<CaMaterial, rcgen::Error> {
    let mut params = CertificateParams::new(Vec::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "kastellan egress-proxy");
    params
        .distinguished_name
        .push(DnType::CommonName, "kastellan ephemeral egress CA");
    params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    let cert_pem = cert.pem();
    let cert_der = cert.der().clone();
    let issuer = Issuer::new(params, key_pair);
    Ok(CaMaterial { cert_pem, cert_der, issuer })
}

/// Issue a leaf for `host`, signed by `ca`. `host` becomes the sole SAN and the
/// CN. Server-auth EKU so rustls accepts it as a TLS server cert.
pub fn issue_leaf(ca: &CaMaterial, host: &str) -> Result<LeafCert, rcgen::Error> {
    let mut params = CertificateParams::new(vec![host.to_string()])?;
    params.distinguished_name.push(DnType::CommonName, host);
    params.use_authority_key_identifier_extension = true;
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);

    let key_pair = KeyPair::generate()?;
    let cert = params.signed_by(&key_pair, &ca.issuer)?;
    let cert_der = cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    Ok(LeafCert { cert_der, key_der })
}

#[cfg(test)]
mod tests;
```

Add `mod ca;` to `src/main.rs`.

> **rcgen API note:** this is written against rcgen 0.13 (`KeyPair::generate`,
> `params.self_signed(&key_pair)`, `Issuer::new(params, key_pair)`,
> `params.signed_by(&key_pair, &issuer)`, `cert.pem()`/`cert.der()`,
> `key_pair.serialize_der()`). If `cargo build` reports a signature mismatch, run
> `cargo doc -p rcgen --open` (or context7 `/rustls/rcgen`) and adjust — do NOT
> stub the crypto.

- [ ] **Step 4: Run the test, expect pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy ca::tests 2>&1 | tail -20`
Expected: all three PASS.

- [ ] **Step 5: Commit**

```bash
git add workers/egress-proxy/src/ca.rs workers/egress-proxy/src/ca/tests.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): per-instance ephemeral CA + leaf issuance (slice #3a)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Bounded per-host leaf cache (`leaf_cache.rs`)

**Files:**
- Create: `workers/egress-proxy/src/leaf_cache.rs`
- Create: `workers/egress-proxy/src/leaf_cache/tests.rs`
- Modify: `workers/egress-proxy/src/main.rs` (add `mod leaf_cache;`)

- [ ] **Step 1: Write the failing test**

Create `workers/egress-proxy/src/leaf_cache/tests.rs`:

```rust
use super::{LeafCache, MAX_CACHED_LEAVES};
use crate::ca::generate_ca;

#[test]
fn same_host_returns_a_cached_arc() {
    let ca = generate_ca().unwrap();
    let mut cache = LeafCache::new();
    let a = cache.get_or_issue(&ca, "api.example.com").expect("issue");
    let b = cache.get_or_issue(&ca, "api.example.com").expect("cached");
    assert!(std::sync::Arc::ptr_eq(&a, &b), "same host must reuse the Arc");
}

#[test]
fn distinct_hosts_get_distinct_leaves() {
    let ca = generate_ca().unwrap();
    let mut cache = LeafCache::new();
    let a = cache.get_or_issue(&ca, "a.example.com").unwrap();
    let b = cache.get_or_issue(&ca, "b.example.com").unwrap();
    assert!(!std::sync::Arc::ptr_eq(&a, &b));
    assert_eq!(cache.len(), 2);
}

#[test]
fn cache_is_bounded() {
    let ca = generate_ca().unwrap();
    let mut cache = LeafCache::new();
    for i in 0..(MAX_CACHED_LEAVES + 10) {
        cache.get_or_issue(&ca, &format!("h{i}.example.com")).unwrap();
    }
    assert!(cache.len() <= MAX_CACHED_LEAVES, "cache must not grow unbounded");
}
```

- [ ] **Step 2: Run, expect compile failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy leaf_cache::tests 2>&1 | tail -20`
Expected: FAIL — `LeafCache` undefined.

- [ ] **Step 3: Implement `leaf_cache.rs`**

Create `workers/egress-proxy/src/leaf_cache.rs`. The cached value is the prebuilt rustls
`ServerConfig` for the host (built once per host), behind an `Arc` so each connection clones
cheaply.

```rust
//! Bounded per-host cache of prebuilt rustls server configs (one CA-signed leaf
//! each). Building a leaf does a keygen + signature, so we cache by host for the
//! life of the (short-lived, SingleUse) proxy. Bounded so a worker that connects
//! to many distinct hosts can't grow the map without limit; on overflow we clear
//! (the simplest bound — re-issue is cheap and the proxy is ephemeral).

use std::collections::HashMap;
use std::sync::Arc;

use rustls::ServerConfig;

use crate::ca::{issue_leaf, CaMaterial};

/// Upper bound on distinct host leaves held at once.
pub const MAX_CACHED_LEAVES: usize = 256;

/// Host → prebuilt server config (CA-signed leaf for that host).
pub struct LeafCache {
    map: HashMap<String, Arc<ServerConfig>>,
}

impl LeafCache {
    pub fn new() -> Self {
        Self { map: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Return the server config for `host`, issuing + caching it on first use.
    /// Clears the cache first if it is at the bound (cheap re-issue afterwards).
    pub fn get_or_issue(
        &mut self,
        ca: &CaMaterial,
        host: &str,
    ) -> Result<Arc<ServerConfig>, String> {
        if let Some(cfg) = self.map.get(host) {
            return Ok(Arc::clone(cfg));
        }
        if self.map.len() >= MAX_CACHED_LEAVES {
            self.map.clear();
        }
        let leaf = issue_leaf(ca, host).map_err(|e| format!("issue leaf for {host}: {e}"))?;
        let (chain, key) = leaf.into_rustls();
        let cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| format!("build server config for {host}: {e}"))?;
        let cfg = Arc::new(cfg);
        self.map.insert(host.to_string(), Arc::clone(&cfg));
        Ok(cfg)
    }
}

impl Default for LeafCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
```

Add `mod leaf_cache;` to `src/main.rs`.

- [ ] **Step 4: Run, expect pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy leaf_cache::tests 2>&1 | tail -20`
Expected: all three PASS.

- [ ] **Step 5: Commit**

```bash
git add workers/egress-proxy/src/leaf_cache.rs workers/egress-proxy/src/leaf_cache/tests.rs workers/egress-proxy/src/main.rs
git commit -m "feat(egress-proxy): bounded per-host leaf/server-config cache (slice #3a)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: The async `intercept()` + hermetic MITM round-trip

**Files:**
- Modify: `workers/egress-proxy/src/mitm.rs` (add `intercept` + `upstream_server_name`)
- Modify: `workers/egress-proxy/src/mitm/tests.rs` (add the round-trip)

- [ ] **Step 1: Write the failing test**

Append to `workers/egress-proxy/src/mitm/tests.rs`. The test stands up a loopback rustls origin
(its own throwaway CA), then calls `intercept` with the upstream trust set to that origin's CA.
A rustls client trusting the per-instance CA drives a tiny HTTP/1.1 exchange through the proxy.

```rust
use std::sync::Arc;

use crate::ca::generate_ca;
use rustls::pki_types::ServerName;

// A self-contained loopback HTTPS origin built with rcgen, returning its own CA
// (as a rustls RootCertStore) so `intercept`'s upstream leg can validate it.
async fn spawn_tls_origin() -> (std::net::SocketAddr, Arc<rustls::RootCertStore>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Origin cert: a self-signed cert for "origin.test".
    let mut params = rcgen::CertificateParams::new(vec!["origin.test".to_string()]).unwrap();
    params
        .extended_key_usages
        .push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    let cert_der = cert.der().clone();
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()),
    );
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der.clone()).unwrap();

    let server_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_cfg));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((tcp, _)) = listener.accept().await {
            if let Ok(mut tls) = acceptor.accept(tcp).await {
                let mut buf = [0u8; 1024];
                let _ = tls.read(&mut buf).await; // read the request line
                let _ = tls
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nPONG")
                    .await;
                let _ = tls.shutdown().await;
            }
        }
    });
    (addr, Arc::new(roots))
}

#[tokio::test]
async fn mitm_terminates_and_reoriginates_a_real_tls_session() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (origin_addr, upstream_roots) = spawn_tls_origin().await;
    let ca = Arc::new(generate_ca().unwrap());

    // The worker side of the UDS: connect, then drive TLS trusting ONLY the CA.
    let (worker_end, proxy_end) = tokio::net::UnixStream::pair().unwrap();

    let upstream_tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates((*upstream_roots).clone())
            .with_no_client_auth(),
    );

    // Run intercept on the proxy end (server side). It dials the origin itself.
    let ca_for_proxy = Arc::clone(&ca);
    let proxy = tokio::spawn(async move {
        let mut cache = crate::leaf_cache::LeafCache::new();
        super::intercept(
            proxy_end,
            origin_addr,
            "origin.test",
            &ca_for_proxy,
            &mut cache,
            upstream_tls,
        )
        .await
    });

    // Worker: TLS-connect through the UDS trusting only the per-instance CA.
    let mut worker_roots = rustls::RootCertStore::empty();
    for der in rustls::pki_types::CertificateDer::pem_slice_iter(ca.cert_pem().as_bytes()) {
        worker_roots.add(der.unwrap()).unwrap();
    }
    let worker_tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(worker_roots)
            .with_no_client_auth(),
    );
    let connector = tokio_rustls::TlsConnector::from(worker_tls);
    let sni = ServerName::try_from("origin.test").unwrap();
    let mut tls = connector.connect(sni, worker_end).await.expect("worker TLS handshake");
    tls.write_all(b"GET / HTTP/1.1\r\nHost: origin.test\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    tls.read_to_end(&mut resp).await.unwrap();
    assert!(
        resp.windows(4).any(|w| w == b"PONG"),
        "expected the origin body through the MITM, got {:?}",
        String::from_utf8_lossy(&resp)
    );
    proxy.await.unwrap().expect("intercept ok");
}
```

- [ ] **Step 2: Run, expect compile failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy mitm::tests::mitm_terminates 2>&1 | tail -20`
Expected: FAIL — `intercept` undefined, and the test needs `tokio`'s `macros`/`rt` test feature.

> If `#[tokio::test]` doesn't resolve, add `tokio = { ..., features = [..., "macros"] }` to
> `workers/egress-proxy/Cargo.toml` (the `macros` feature provides the test attribute). Keep it
> minimal.

- [ ] **Step 3: Implement `intercept` + `upstream_server_name` in `mitm.rs`**

Add to `workers/egress-proxy/src/mitm.rs`:

```rust
use std::net::SocketAddr;
use std::sync::Arc;

use rustls::pki_types::ServerName;
use tokio::net::{TcpStream, UnixStream};

use crate::ca::CaMaterial;
use crate::leaf_cache::LeafCache;

/// Build the SNI `ServerName` for the upstream leg from the CONNECT authority
/// host. Domains go through `try_from`; IP literals (incl. bracketed IPv6) are
/// parsed as `IpAddress` so rustls validates them correctly.
fn upstream_server_name(host: &str) -> Result<ServerName<'static>, String> {
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = unbracketed.parse::<std::net::IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(host.to_string()).map_err(|e| format!("invalid SNI {host:?}: {e}"))
}

/// Terminate the worker's TLS (presenting a CA-signed leaf for `host`) and
/// re-originate a validated TLS session to the already-resolved `upstream_addr`,
/// then copy plaintext both ways until either side closes.
///
/// `upstream_tls` is the trust config for the **real origin** — production wires
/// `webpki-roots`; tests wire a test-origin CA. Taking it as a parameter keeps
/// the round-trip test hermetic without a test-only env var.
pub async fn intercept(
    worker_side: UnixStream,
    upstream_addr: SocketAddr,
    host: &str,
    ca: &CaMaterial,
    leaf_cache: &mut LeafCache,
    upstream_tls: Arc<rustls::ClientConfig>,
) -> Result<(), String> {
    use tokio::io::copy_bidirectional;

    // 1. Server-side: present a leaf for `host`, handshake with the worker.
    let server_cfg = leaf_cache.get_or_issue(ca, host)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(server_cfg);
    let mut client_tls = acceptor
        .accept(worker_side)
        .await
        .map_err(|e| format!("worker TLS handshake: {e}"))?;

    // 2. Client-side: re-originate to the pinned origin, validating its real cert.
    let upstream_tcp = TcpStream::connect(upstream_addr)
        .await
        .map_err(|e| format!("dial origin {upstream_addr}: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(upstream_tls);
    let sni = upstream_server_name(host)?;
    let mut upstream_tls_stream = connector
        .connect(sni, upstream_tcp)
        .await
        .map_err(|e| format!("origin TLS handshake: {e}"))?;

    // 3. Plaintext flows through here. (Slice #3b scans it; 3a only relays.)
    copy_bidirectional(&mut client_tls, &mut upstream_tls_stream)
        .await
        .map_err(|e| format!("tunnel copy: {e}"))?;
    Ok(())
}
```

> The test calls `intercept` with `proxy_end` (a `tokio::net::UnixStream` from `pair()`) — so the
> production caller in Task 5 must also hand `intercept` a `tokio` `UnixStream`. Task 5 converts
> the accepted `std` `UnixStream` via `set_nonblocking(true)` + `tokio::net::UnixStream::from_std`.

- [ ] **Step 4: Run, expect pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy mitm::tests 2>&1 | tail -30`
Expected: `looks_like_tls` tests + `mitm_terminates_and_reoriginates_a_real_tls_session` PASS.

- [ ] **Step 5: Commit**

```bash
git add workers/egress-proxy/src/mitm.rs workers/egress-proxy/src/mitm/tests.rs workers/egress-proxy/Cargo.toml
git commit -m "feat(egress-proxy): async TLS intercept + hermetic MITM round-trip (slice #3a)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Wire `handle_conn` (connect→200→peek→branch) + `Decision.tls_intercepted`

**Files:**
- Modify: `workers/egress-proxy/src/report.rs` (add the field + default)
- Modify: `workers/egress-proxy/src/proxy.rs` (restructure the `Dial` arm; thread CA/cache/tls)
- Modify: `workers/egress-proxy/src/proxy/tests.rs` (existing tests still green + assert flag)
- Modify: `workers/egress-proxy/src/main.rs` (generate CA, write ca.pem, build upstream tls)

- [ ] **Step 1: Add `tls_intercepted` to `Decision` (failing report test)**

In `workers/egress-proxy/src/report.rs`, add the field to `Decision` (after `reason`):

```rust
    pub reason: String,
    /// True when this allowed CONNECT was TLS-terminated + re-originated by the
    /// proxy (MITM). False for blocks and for plaintext pass-through tunnels.
    /// Default false so existing constructors and the host-side audit stay
    /// backward-compatible. (Slice #3a — the only new plaintext-adjacent signal.)
    #[serde(default)]
    pub tls_intercepted: bool,
```

Every existing `Decision { .. }` literal in `proxy.rs` and `report.rs` tests must now set
`tls_intercepted`. Add `tls_intercepted: false,` to each existing literal (the `blocked()` helper,
the two allowed literals in `handle_conn`, and the test literals in `report.rs`/`proxy/tests.rs`).

Add a `report.rs` test:

```rust
    #[test]
    fn tls_intercepted_serializes_and_defaults_false() {
        let mut d = Decision {
            worker: "web-fetch".into(), host: "h".into(), port: 443,
            resolved_ip: None, verdict: Verdict::Allowed, reason: "ok".into(),
            tls_intercepted: true,
        };
        assert!(d.to_line().contains("\"tls_intercepted\":true"));
        d.tls_intercepted = false;
        let v: serde_json::Value = serde_json::from_str(&d.to_line()).unwrap();
        assert_eq!(v["tls_intercepted"], false);
    }
```

- [ ] **Step 2: Run report tests, expect pass; whole crate, expect compile errors elsewhere**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy report:: 2>&1 | tail -20`
Expected: report tests PASS once every `Decision` literal in `report.rs` compiles. The crate as a
whole will still fail to compile until `proxy.rs` literals are updated (next step).

- [ ] **Step 3: Restructure `handle_conn`'s `Dial` arm in `proxy.rs`**

Change `handle_conn`'s signature to receive the CA, leaf cache, and upstream TLS config, and
replace the `Target::Dial(ip)` arm. New signature + arm:

```rust
pub fn handle_conn(
    mut client: UnixStream,
    worker: &str,
    allow: &HostAllowlist,
    resolver: &dyn Resolve,
    reporter: &mut dyn Reporter,
    ca: &crate::ca::CaMaterial,
    leaf_cache: &mut crate::leaf_cache::LeafCache,
    upstream_tls: std::sync::Arc<rustls::ClientConfig>,
) {
    // ... read_request_line + parse_connect + Block arm unchanged ...

        Target::Dial(ip) => {
            // Connect upstream FIRST (preserves the 502-on-connect-fail behaviour
            // and the pinned-IP SSRF guarantee), THEN reply 200, THEN peek.
            let upstream = match TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_TIMEOUT) {
                Ok(s) => s,
                Err(e) => {
                    reporter.report(Decision {
                        worker: worker.into(), host: host.clone(), port,
                        resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                        reason: format!("connect_failed: {e}"), tls_intercepted: false,
                    });
                    let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
                    return;
                }
            };
            if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").is_err() {
                return;
            }
            // Peek the first tunnel byte (non-consuming). The CONNECT round-trip
            // guarantees the worker only sends after the 200, so this is the
            // first tunnel byte. EOF / error → treat as pass-through.
            let mut first = [0u8; 1];
            let is_tls = matches!(client.peek(&mut first), Ok(1))
                && crate::mitm::looks_like_tls(first[0]);
            if is_tls {
                reporter.report(Decision {
                    worker: worker.into(), host: host.clone(), port,
                    resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                    reason: allowed_reason(allow, &host).into(), tls_intercepted: true,
                });
                run_mitm(client, upstream, ip, port, &host, ca, leaf_cache, upstream_tls, worker, reporter);
            } else {
                reporter.report(Decision {
                    worker: worker.into(), host: host.clone(), port,
                    resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                    reason: allowed_reason(allow, &host).into(), tls_intercepted: false,
                });
                tunnel(client, upstream);
            }
        }
```

Add the `run_mitm` bridge (sync → per-connection current-thread runtime → async `intercept`).
It converts the already-connected `std` streams to tokio inside the runtime:

```rust
/// Bridge the sync accept path to the async MITM. Builds a per-connection
/// current-thread runtime (mirrors web-common's ProxyConnectGet) and runs
/// `mitm::intercept`. A handshake/copy error is reported as an allowed-but-failed
/// decision (the policy verdict was Allowed; this is a transport failure).
#[allow(clippy::too_many_arguments)]
fn run_mitm(
    client: UnixStream,
    upstream: TcpStream,
    ip: IpAddr,
    port: u16,
    host: &str,
    ca: &crate::ca::CaMaterial,
    leaf_cache: &mut crate::leaf_cache::LeafCache,
    upstream_tls: std::sync::Arc<rustls::ClientConfig>,
    worker: &str,
    reporter: &mut dyn Reporter,
) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            reporter.report(Decision {
                worker: worker.into(), host: host.into(), port,
                resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
                reason: format!("mitm_runtime_failed: {e}"), tls_intercepted: true,
            });
            return;
        }
    };
    let res = rt.block_on(async move {
        // Convert the already-connected std streams into tokio handles inside the
        // runtime (requires nonblocking + an active reactor).
        client.set_nonblocking(true).map_err(|e| format!("client nonblocking: {e}"))?;
        upstream.set_nonblocking(true).map_err(|e| format!("upstream nonblocking: {e}"))?;
        let client = tokio::net::UnixStream::from_std(client)
            .map_err(|e| format!("client from_std: {e}"))?;
        let _ = upstream; // upstream is re-dialed inside intercept from (ip, port)
        crate::mitm::intercept(
            client,
            SocketAddr::new(ip, port),
            host,
            ca,
            leaf_cache,
            upstream_tls,
        )
        .await
    });
    if let Err(e) = res {
        reporter.report(Decision {
            worker: worker.into(), host: host.into(), port,
            resolved_ip: Some(ip.to_string()), verdict: Verdict::Allowed,
            reason: format!("mitm_failed: {e}"), tls_intercepted: true,
        });
    }
}
```

> **Design note for the implementer:** `intercept` re-dials the origin itself (from `(ip, port)`)
> so it owns a *tokio* `TcpStream`; the sync `upstream` connected above is only used to (a)
> preserve the connect-before-200 + 502 behaviour and (b) prove the pinned IP is reachable. We
> drop it (`let _ = upstream;`) rather than thread a converted handle through, keeping `intercept`
> single-responsibility. The double-connect is one extra loopback/socket setup on the MITM path
> only; acceptable for slice #3a. (A later optimisation can pass the converted stream in.)

Update the two existing UDS-driven tests in `src/proxy/tests.rs` to pass the new args. Add a
shared test helper and update both `handle_conn(...)` call sites:

```rust
fn test_ca() -> crate::ca::CaMaterial {
    crate::ca::generate_ca().unwrap()
}
fn webpki_upstream() -> std::sync::Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    std::sync::Arc::new(
        rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth(),
    )
}
```

Both existing tests send **plaintext** (`ping`), so they now exercise the pass-through branch and
must still pass with `tls_intercepted == false` on the allowed decision. Update their
`handle_conn` calls to:

```rust
    let ca = test_ca();
    let mut cache = crate::leaf_cache::LeafCache::new();
    handle_conn(conn, "web-fetch", &allow, &StdResolve, &mut reporter, &ca, &mut cache, webpki_upstream());
```

and add to `handle_conn_tunnels_allowed_literal_origin`'s assertions:

```rust
    assert!(!decisions[0].tls_intercepted, "plaintext tunnel is pass-through, not MITM");
```

> `webpki_roots` is needed in the proxy tests; add `webpki-roots = "1"` to
> `workers/egress-proxy/Cargo.toml` under `[dev-dependencies]` (production reads webpki in
> `main.rs`, so also ensure it's a normal dep — see Step 5). Use the same major as web-common (1).

- [ ] **Step 4: Run the proxy crate tests, expect pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-egress-proxy 2>&1 | tail -30`
Expected: all proxy unit tests PASS (pure decide tests, both UDS pass-through tests with the new
args, report tests, ca/leaf_cache/mitm tests).

- [ ] **Step 5: Wire `main.rs` — generate CA, write ca.pem, build upstream trust, pass to handle_conn**

In `workers/egress-proxy/src/main.rs`:
- Add `webpki-roots = "1"` as a normal dep (Cargo.toml) for the upstream root store.
- Add a `CA_FILE_NAME` const (`"ca.pem"`) — but keep the host-side copy authoritative; here we
  just need to know where to write. Derive the scratch dir from the UDS path's parent.
- After binding the UDS and BEFORE `lock_down()` (Landlock forbids fs writes after), generate the
  CA and write its PEM next to the UDS:

```rust
    // Generate the per-instance CA and export ONLY its public cert next to the
    // UDS, before lock-down (Landlock will forbid fs writes afterwards). The
    // host waits for this file and injects it into the worker's trust store.
    let ca = std::sync::Arc::new(ca::generate_ca().map_err(|e| anyhow::anyhow!("generate CA: {e}"))?);
    let ca_path = std::path::Path::new(&uds)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("UDS path has no parent dir"))?
        .join("ca.pem");
    std::fs::write(&ca_path, ca.cert_pem())
        .map_err(|e| anyhow::anyhow!("write CA cert {ca_path:?}: {e}"))?;

    // Upstream trust for the re-origination leg: the REAL public roots. The
    // proxy validates the true origin here (the worker only trusts our CA).
    let mut upstream_roots = rustls::RootCertStore::empty();
    upstream_roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let upstream_tls = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(upstream_roots)
            .with_no_client_auth(),
    );
```

- The proxy installs the default crypto provider once (rustls 0.23 needs a process-default
  provider for `ServerConfig`/`ClientConfig` builders). Add at the top of `main`, before any TLS
  config is built:

```rust
    // rustls 0.23: install the ring provider as the process default (needed by
    // both the server-side leaf configs and the upstream client config).
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("install rustls ring provider"))?;
```

- Each connection needs its own `LeafCache` OR a shared one. The accept loop uses
  `std::thread::scope` per connection; give each connection a fresh `LeafCache` (simplest, no
  shared-mutability; the SingleUse proxy rarely handles many connections). Pass `&ca`, a fresh
  `LeafCache`, and `Arc::clone(&upstream_tls)` into `handle_conn`:

```rust
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut reporter = LineReporter { out: std::io::stdout().lock() };
                let mut cache = leaf_cache::LeafCache::new();
                handle_conn(conn, &worker, allow, &resolver, &mut reporter,
                            &ca, &mut cache, std::sync::Arc::clone(&upstream_tls));
            });
        });
```

- [ ] **Step 6: Build the whole crate + run all proxy tests**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-egress-proxy && cargo test -p kastellan-worker-egress-proxy 2>&1 | tail -20`
Expected: builds, all tests PASS.

- [ ] **Step 7: Commit**

```bash
git add workers/egress-proxy/src/report.rs workers/egress-proxy/src/proxy.rs workers/egress-proxy/src/proxy/tests.rs workers/egress-proxy/src/main.rs workers/egress-proxy/Cargo.toml Cargo.lock
git commit -m "feat(egress-proxy): MITM wiring — connect→200→peek→branch + CA at startup (slice #3a)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Worker-side only-CA trust (`web-common`)

**Files:**
- Modify: `workers/web-common/Cargo.toml` (enable `pem` on rustls-pki-types)
- Modify: `workers/web-common/src/proxy_connect.rs` (only-CA root store)
- Modify: `workers/web-common/src/http.rs` (`make_get_inner`/`make_get` read the CA env)

- [ ] **Step 1: Write the failing tests**

In `workers/web-common/src/http.rs`, extend `make_get_inner` tests to assert CA-driven selection.
First we change the signature (Step 3); write the target test now:

```rust
    #[test]
    fn make_get_inner_threads_ca_override_into_proxy_connect() {
        // No UDS → reqwest (CA ignored).
        let g = make_get_inner("kastellan-test/0", None, None).unwrap();
        assert_eq!(g.transport_kind(), "reqwest");
        // UDS + CA path → proxy-connect (construction must not require the file
        // to exist for the no-CA case; with a CA path that doesn't exist it must
        // FAIL CLOSED rather than silently fall back to webpki).
        let g = make_get_inner("kastellan-test/0", Some("/tmp/x.sock"), None).unwrap();
        assert_eq!(g.transport_kind(), "proxy-connect");
        let err = make_get_inner("kastellan-test/0", Some("/tmp/x.sock"), Some("/nonexistent/ca.pem"));
        assert!(err.is_err(), "a set-but-unreadable CA must fail closed, not fall back");
    }
```

In `workers/web-common/src/proxy_connect.rs`, add a unit test that an only-CA config is built
from a real PEM (generate one inline with a tiny self-signed cert via... we have no rcgen in
web-common — instead assert the *fallback* and *failure* shapes which don't need a valid CA):

```rust
    #[test]
    fn new_with_unreadable_ca_fails_closed() {
        let res = ProxyConnectGet::with_trust(
            "kastellan-test/0",
            PathBuf::from("/tmp/x.sock"),
            Some(PathBuf::from("/nonexistent/ca.pem")),
        );
        assert!(res.is_err(), "set-but-unreadable CA must fail closed");
    }

    #[test]
    fn new_without_ca_uses_webpki() {
        // No CA → infallible webpki path (back-compat with slice #1/#2).
        let g = ProxyConnectGet::with_trust("kastellan-test/0", PathBuf::from("/tmp/x.sock"), None);
        assert!(g.is_ok());
    }
```

- [ ] **Step 2: Run, expect compile failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common 2>&1 | tail -20`
Expected: FAIL — `with_trust` undefined, `make_get_inner` arity changed.

- [ ] **Step 3: Implement the only-CA trust in `proxy_connect.rs`**

Enable PEM parsing — in `workers/web-common/Cargo.toml` change:

```toml
rustls-pki-types = { version = "1", features = ["std"] }
```
to
```toml
rustls-pki-types = { version = "1", features = ["std", "alloc"] }
```
and add the `pem` capability. (rustls-pki-types exposes `CertificateDer::pem_slice_iter` under
its default features in 1.x; if the iterator isn't found, add `features = ["std", "alloc"]` is
insufficient — use `rustls-pemfile = "2"` as a fallback parser. Verify which is available with
`cargo doc -p rustls-pki-types`.)

Replace `ProxyConnectGet::new` with a trust-aware constructor, keeping `new` as the webpki
back-compat shim so existing callers/tests are unchanged:

```rust
impl ProxyConnectGet {
    /// Back-compat constructor: webpki public roots, infallible. Used where no
    /// MITM CA is configured (slice #1/#2 posture, dev/no-proxy).
    pub fn new(user_agent: &str, uds: PathBuf) -> Self {
        Self::with_trust(user_agent, uds, None).expect("webpki-only config is infallible")
    }

    /// Build the transport with an explicit trust posture. When `ca_path` is
    /// `Some`, the worker trusts ONLY that CA (the per-instance MITM CA) and
    /// public roots are dropped — egress fails closed unless the proxy
    /// terminates the TLS. A set-but-unreadable/invalid CA is an error (fail
    /// closed; never silently fall back to webpki). When `None`, webpki roots.
    pub fn with_trust(
        user_agent: &str,
        uds: PathBuf,
        ca_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");

        let mut root_store = rustls::RootCertStore::empty();
        match ca_path {
            Some(path) => {
                let pem = std::fs::read(&path)
                    .map_err(|e| anyhow::anyhow!("read MITM CA {path:?}: {e}"))?;
                let mut added = 0usize;
                for der in rustls_pki_types::CertificateDer::pem_slice_iter(&pem) {
                    let der = der.map_err(|e| anyhow::anyhow!("parse MITM CA {path:?}: {e}"))?;
                    root_store
                        .add(der)
                        .map_err(|e| anyhow::anyhow!("add MITM CA {path:?}: {e}"))?;
                    added += 1;
                }
                if added == 0 {
                    anyhow::bail!("MITM CA {path:?} contained no certificates");
                }
            }
            None => {
                root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
        }
        let tls = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        );
        Ok(Self { user_agent: user_agent.to_string(), uds, tls, rt })
    }
}
```

> `pem_slice_iter` import: `use rustls_pki_types::CertificateDer;` already partially present
> (`ServerName` is imported). Add the `CertificateDer` import if needed.

- [ ] **Step 4: Thread the CA env through `http.rs`**

Change `make_get_inner` to take a `ca_override` and pass it through; `make_get` reads the env:

```rust
pub(crate) fn make_get_inner(
    user_agent: &str,
    uds_override: Option<&str>,
    ca_override: Option<&str>,
) -> anyhow::Result<Box<dyn HttpGet>> {
    match uds_override {
        Some(uds) if !uds.is_empty() => {
            let ca = ca_override.filter(|s| !s.is_empty()).map(PathBuf::from);
            Ok(Box::new(crate::proxy_connect::ProxyConnectGet::with_trust(
                user_agent,
                PathBuf::from(uds),
                ca,
            )?))
        }
        _ => Ok(Box::new(ReqwestGet::new(user_agent)?)),
    }
}

pub fn make_get(user_agent: &str) -> anyhow::Result<Box<dyn HttpGet>> {
    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS").ok();
    let ca = std::env::var("KASTELLAN_EGRESS_PROXY_CA").ok();
    make_get_inner(user_agent, uds.as_deref(), ca.as_deref())
}
```

Update the existing `make_get_inner_selects_transport_by_uds` test call sites to pass the extra
`None` argument (or fold into the new test above).

- [ ] **Step 5: Run web-common tests, expect pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common 2>&1 | tail -20`
Expected: all PASS, including the fail-closed CA tests.

- [ ] **Step 6: Commit**

```bash
git add workers/web-common/src/proxy_connect.rs workers/web-common/src/http.rs workers/web-common/Cargo.toml Cargo.lock
git commit -m "feat(web-common): worker trusts only the per-instance MITM CA when set (slice #3a)

KASTELLAN_EGRESS_PROXY_CA → ProxyConnectGet trusts only that CA (fail
closed if unreadable); absent → webpki roots (slice #1/#2 back-compat).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Host wiring — wait for `ca.pem`, inject into the worker policy, audit the flag

**Files:**
- Modify: `core/src/egress/spawn.rs` (export `CA_FILE_NAME`; `spawn_sidecar` waits for ca.pem)
- Modify: `core/src/egress/net_worker.rs` (`rewrite_worker_policy` adds CA fs_read + env)
- Modify: `core/src/egress/audit.rs` (carry `tls_intercepted` into the payload)

- [ ] **Step 1: Write the failing tests**

In `core/src/egress/net_worker.rs` tests, add a CA-injection assertion. First the signature
changes (Step 3) — write the target:

```rust
    #[test]
    fn rewrite_worker_policy_injects_ca_trust() {
        let base = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            fs_read: vec!["/etc/resolv.conf".into(), "/bin/worker".into()],
            env: vec![],
            ..SandboxPolicy::default()
        };
        let uds = std::path::PathBuf::from("/scratch/egress.sock");
        let ca = std::path::PathBuf::from("/scratch/ca.pem");
        let out = rewrite_worker_policy(base, &uds, &ca);
        // CA path is readable in-jail and announced via the env the worker reads.
        assert!(out.fs_read.contains(&ca));
        assert!(out
            .env
            .iter()
            .any(|(k, v)| k == "KASTELLAN_EGRESS_PROXY_CA" && v == "/scratch/ca.pem"));
    }
```

In `core/src/egress/audit.rs` tests, add the flag round-trip:

```rust
    #[test]
    fn allowed_row_carries_tls_intercepted_flag() {
        let line = r#"{"worker":"web-fetch","host":"a.com","port":443,"resolved_ip":"1.2.3.4","verdict":"allowed","reason":"ok","tls_intercepted":true}"#;
        let row = decision_to_audit(line).unwrap();
        assert_eq!(row.payload["tls_intercepted"], true);
    }

    #[test]
    fn missing_tls_intercepted_defaults_false() {
        // Slice #1/#2 lines (no field) must still parse, defaulting to false.
        let line = r#"{"worker":"w","host":"h","port":443,"resolved_ip":null,"verdict":"allowed","reason":"ok"}"#;
        let row = decision_to_audit(line).unwrap();
        assert_eq!(row.payload["tls_intercepted"], false);
    }
```

- [ ] **Step 2: Run, expect compile/test failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core egress:: 2>&1 | tail -20`
Expected: FAIL — `rewrite_worker_policy` arity changed; `DecisionLine` lacks `tls_intercepted`.

- [ ] **Step 3: Implement the host wiring**

In `core/src/egress/spawn.rs`, add the CA filename const next to `UDS_FILE_NAME` and make
`spawn_sidecar` also wait for it:

```rust
/// Basename of the per-worker CA cert the sidecar exports for the host to inject
/// into the worker's trust store (slice #3a). Lives beside the UDS in scratch.
pub(crate) const CA_FILE_NAME: &str = "ca.pem";
```

In `spawn_sidecar`, change the readiness wait so BOTH the UDS and the CA file must exist:

```rust
    let ca_path = scratch.join(CA_FILE_NAME);
    let deadline = Instant::now() + READY_TIMEOUT;
    while !(uds_path.exists() && ca_path.exists()) {
        if Instant::now() >= deadline {
            let mut handle = SidecarHandle { child, uds_path: uds_path.clone() };
            handle.child.kill().ok();
            handle.child.wait().ok();
            anyhow::bail!(
                "egress-proxy sidecar did not bind {uds_path:?} + write {ca_path:?} within {READY_TIMEOUT:?}"
            );
        }
        std::thread::sleep(READY_POLL);
    }
```

In `core/src/egress/net_worker.rs`:
- Add the env const: `const ENV_CA: &str = "KASTELLAN_EGRESS_PROXY_CA";`
- Change `rewrite_worker_policy` to take the CA path and inject it:

```rust
pub fn rewrite_worker_policy(mut policy: SandboxPolicy, uds: &Path, ca: &Path) -> SandboxPolicy {
    policy.proxy_uds = Some(uds.to_path_buf());
    policy.fs_read.retain(|p| p != Path::new("/etc/resolv.conf"));
    // Make the per-instance CA readable in-jail and announce it to the worker.
    if !policy.fs_read.iter().any(|p| p == ca) {
        policy.fs_read.push(ca.to_path_buf());
    }
    policy.env.retain(|(k, _)| k != ENV_UDS && k != ENV_CA);
    policy.env.push((ENV_UDS.to_string(), uds.to_string_lossy().into_owned()));
    policy.env.push((ENV_CA.to_string(), ca.to_string_lossy().into_owned()));
    policy
}
```

- In `spawn_net_worker`, derive the CA path from the sidecar UDS's parent and pass it:

```rust
    let uds = sidecar.uds_path.clone();
    let ca = uds
        .parent()
        .map(|d| d.join(super::spawn::CA_FILE_NAME))
        .unwrap_or_else(|| PathBuf::from(super::spawn::CA_FILE_NAME));
    let forced = rewrite_worker_policy(spec.policy.clone(), &uds, &ca);
```

- Update the two existing `rewrite_worker_policy` unit tests (`rewrite_worker_policy_forces_routing`,
  `rewrite_overwrites_stale_uds_env`) to pass a `ca` path argument
  (`std::path::Path::new("/scratch/ca.pem")`).

In `core/src/egress/audit.rs`, add the field to `DecisionLine` (defaulting false) and into the
payload:

```rust
struct DecisionLine {
    worker: String,
    host: String,
    port: u16,
    resolved_ip: Option<String>,
    verdict: String,
    reason: String,
    #[serde(default)]
    tls_intercepted: bool,
}
```
and in `decision_to_audit`'s payload json add `"tls_intercepted": d.tls_intercepted,`.

- [ ] **Step 4: Run core egress tests, expect pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core egress:: 2>&1 | tail -20`
Expected: all egress unit tests PASS (rewrite incl. CA, audit incl. flag + default).

- [ ] **Step 5: Commit**

```bash
git add core/src/egress/spawn.rs core/src/egress/net_worker.rs core/src/egress/audit.rs
git commit -m "feat(core/egress): inject per-instance CA into worker trust + audit tls_intercepted (slice #3a)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Force-routing e2e — CA wiring under the real sandbox + pass-through still works

**Files:**
- Modify: `core/tests/egress_force_routing_e2e.rs`

- [ ] **Step 1: Add the CA-wiring assertion to the existing coupling test**

In `forced_coupling_enforces_allowlist_and_ingests_decisions`, after `let uds = minted_uds(...)`
and before the round-trip, assert the sidecar exported its CA next to the UDS (proving the
slice-#3a startup path ran under the real sandbox):

```rust
    // Slice #3a: the sidecar must have exported its per-instance CA beside the
    // UDS for the host to inject into the worker's trust store.
    let ca_pem = uds.parent().unwrap().join("ca.pem");
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ca_pem.exists() {
            assert!(Instant::now() < deadline, "sidecar never wrote ca.pem at {ca_pem:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let pem = std::fs::read_to_string(&ca_pem).expect("read exported CA");
    assert!(pem.contains("BEGIN CERTIFICATE"), "exported CA is not a PEM cert");
```

The existing plaintext `ping`/`PONG` round-trip below it now exercises the **pass-through**
branch (the host-side test client sends plaintext, first byte != 0x16) — it must still pass
unchanged, proving back-compat.

- [ ] **Step 2: Add an `#[ignore]` real-net MITM end-to-end**

Append a new test that drives a *real* public HTTPS origin through a real sandboxed sidecar via
`ProxyConnectGet` trusting the exported per-instance CA. `#[ignore]` (needs network); run on
demand. Add `kastellan-worker-web-common` to `core`'s `[dev-dependencies]` if not already present
(check `core/Cargo.toml` — web-fetch/web-search e2e may already pull it; if not, add it).

```rust
/// Real end-to-end MITM: fetch a real HTTPS origin through a real sandboxed
/// sidecar, with the worker transport trusting ONLY the sidecar's per-instance
/// CA. Proves termination + webpki-validated re-origination on the live path.
/// `#[ignore]`: requires outbound network + DNS.
#[test]
#[ignore = "real network: validates live MITM against a public HTTPS origin"]
fn real_mitm_fetch_through_sidecar() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(proxy) = proxy_binary_or_skip() else {
        eprintln!("[SKIP] egress-proxy binary not built");
        return;
    };
    let scratch_root = short_scratch_root(&format!("mitm-{}", unique_suffix()));
    let policy = allowlist_policy(&["example.com:443"]);
    let spec = WorkerSpec {
        policy: &policy,
        program: "/bin/sleep",
        args: &["30"],
        wall_clock_ms: None,
    };
    let backend = backend();
    let worker = spawn_forced_net_worker(
        backend.as_ref(), &proxy, &spec, &["example.com:443".to_string()],
        &scratch_root, "web-fetch", |_row| {},
    )
    .expect("force-routed worker + sidecar");
    let uds = minted_uds(&scratch_root);
    let ca_pem = uds.parent().unwrap().join("ca.pem");

    let get = kastellan_worker_web_common::http::make_get_inner(
        "kastellan-mitm-e2e/0",
        Some(uds.to_str().unwrap()),
        Some(ca_pem.to_str().unwrap()),
    )
    .expect("build only-CA proxy transport");
    let resp = get
        .get(&url::Url::parse("https://example.com/").unwrap())
        .expect("MITM fetch should succeed against a real origin");
    assert_eq!(resp.status, 200, "expected 200 from example.com through MITM");

    drop(worker);
    let _ = std::fs::remove_dir_all(&scratch_root);
}
```

> `make_get_inner` is `pub(crate)` today — promote it to `pub` in `workers/web-common/src/http.rs`
> (it's already the documented DI seam for tests). If you prefer not to widen it, call
> `ProxyConnectGet::with_trust(...)` directly here instead.

- [ ] **Step 3: Run the e2e (skip-as-pass on the Mac without the proxy bin built; build it first)**

Run:
```bash
source "$HOME/.cargo/env"
cargo build -p kastellan-worker-egress-proxy
cargo test -p kastellan-core --test egress_force_routing_e2e -- --nocapture 2>&1 | tail -30
```
Expected: `forced_coupling_enforces_allowlist_and_ingests_decisions` PASSES with the new ca.pem
assertion (real Seatbelt sandbox on the Mac); the `#[ignore]` real-net test is skipped.

- [ ] **Step 4: Commit**

```bash
git add core/tests/egress_force_routing_e2e.rs workers/web-common/src/http.rs core/Cargo.toml
git commit -m "test(core/egress): assert CA export under real sandbox + #[ignore] real MITM fetch (slice #3a)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Workspace verification, clippy, DGX acceptance, docs

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Full Mac workspace build + clippy**

Run:
```bash
source "$HOME/.cargo/env"
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20
```
Expected: clean build, zero clippy warnings. Fix any `-D warnings` hits inline (e.g. the
`too_many_arguments` on `handle_conn`/`run_mitm` — prefer a small `MitmCtx` struct to bundle
`(ca, leaf_cache, upstream_tls)` if clippy objects, rather than blanket `#[allow]`).

- [ ] **Step 2: Full Mac test suite (skip-as-pass posture)**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -25`
Expected: green (skip-as-pass for live-PG suites). Note the passed/failed/ignored counts for the
HANDOVER. The new proxy unit + mitm round-trip tests run here (no sandbox/PG needed).

- [ ] **Step 3: DGX native acceptance (the gate that matters)**

Per memory [[dgx-native-linux-verification-over-ssh]], drive the DGX over WireGuard SSH
(`ssh dgx`). On the DGX:
```bash
cd ~/src/kastellan && git fetch && git checkout <this-branch> && git pull
source "$HOME/.cargo/env"
cargo build -p kastellan-worker-egress-proxy          # build the bin so e2e really runs
cargo test --workspace 2>&1 | tail -30
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
cargo test -p kastellan-core --test egress_force_routing_e2e -- --nocapture 2>&1 | tail -30
```
Expected: the force-routing e2e runs with **real bwrap containment** (no `[SKIP]`), the ca.pem
assertion passes inside the real private-netns sidecar, and the `#[ignore]` real-net MITM can be
run explicitly (`--ignored`) once to confirm a real HTTPS origin round-trips through the live
MITM. **Verify the rcgen `ring` keygen needs no syscall the `WorkerNetClient` seccomp profile
denies** — if the sidecar dies at CA generation, the proxy will fail to write ca.pem and
`spawn_sidecar` will time out: capture the failure, and if it's a seccomp denial, FILE AN ISSUE
(do not blanket-widen the profile) and surface it before merge.

- [ ] **Step 4: Update HANDOVER.md + ROADMAP.md**

- HANDOVER: bump `Last updated`, `Current state` (new commit hash), `Session-end verification`
  (the workspace counts from Step 2/3); move slice #3a into a "Recently completed" block with the
  file map + the rcgen/seccomp verification result; write a fresh "Next TODO" leading with **slice
  #3b (the credential-leak scanner + Vault value-hash provisioning)** per the spec's follow-up
  section, then `browser-driver` and the refactor bucket.
- ROADMAP: tick the slice-#3a portion of line 142 with the commit hash; leave 3b/#4 open.
- Update the `egress-proxy` crate line in HANDOVER's "Working state" tree to note TLS interception
  is live (drop "no TLS interception").

- [ ] **Step 5: Commit docs**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(handover): egress slice #3a TLS-intercept complete; next = slice #3b scanner

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 6: Push + open PR**

```bash
git push -u origin <this-branch>
gh pr create --base main --title "egress slice #3a: TLS-intercept (MITM) mechanism (ROADMAP:142)" \
  --body "$(cat <<'EOF'
## What

Slice #3a of the egress proxy: the per-worker proxy now **terminates each worker's
TLS** (presenting a per-instance-CA-signed leaf the worker trusts) and **re-originates
a webpki-validated TLS session** to the pinned origin, so a future slice can scan the
plaintext. Zero new plaintext is surfaced in 3a — only an additive `tls_intercepted`
audit boolean.

- In-proxy **ephemeral per-instance CA** (`rcgen`); private key never leaves the
  sandboxed proxy, only the public `ca.pem` is exported.
- Worker trusts **only** that CA (`KASTELLAN_EGRESS_PROXY_CA`) — fail-closed.
- MITM path runs async (`tokio-rustls` `copy_bidirectional`) on a per-connection
  current-thread runtime; the accept loop + `decide()` stay sync. Plain-HTTP-over-
  CONNECT passes through unchanged (peek `0x16`).
- Host wiring: sidecar readiness now waits for `ca.pem`; `rewrite_worker_policy`
  binds it into the jail + sets the env.

## Tests

- Proxy units: `looks_like_tls`, CA gen/leaf SAN/PEM round-trip, bounded leaf cache.
- **Hermetic in-crate MITM round-trip** (`intercept` takes upstream trust as a param).
- web-common: only-CA trust, fail-closed on unreadable CA.
- Force-routing e2e: asserts `ca.pem` exported under the real sandbox + plaintext
  pass-through still works; `#[ignore]` real-net MITM fetch.
- DGX: real bwrap containment, clippy `-D warnings` clean. rcgen/seccomp verified.

## Scope

3a (mechanism) only. **Slice #3b** (credential-leak scanner + Vault secret-value-hash
provisioning) is the next spec. Spec:
`docs/superpowers/specs/2026-06-11-egress-proxy-slice3-tls-intercept-design.md`.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

---

## Self-Review (completed during authoring)

- **Spec coverage:** in-proxy ephemeral CA (Task 2), only-CA worker trust (Task 6), async MITM
  path (Tasks 4–5), peek/pass-through (Tasks 1, 5), host wiring + ca.pem readiness (Task 7),
  `tls_intercepted` audit-only privacy posture (Tasks 5, 7), rcgen dep + license (Task 0), tests
  incl. hermetic round-trip + e2e + `#[ignore]` real-net (Tasks 4, 8), DGX + rcgen/seccomp
  verification (Task 9). All spec sections map to a task.
- **Type consistency:** `CaMaterial`/`LeafCert`/`issue_leaf`/`generate_ca` (Task 2) reused
  verbatim in Tasks 3–5; `LeafCache::get_or_issue` (Task 3) used in `intercept` (Task 4) and
  `run_mitm`/`main` (Task 5); `intercept(worker_side, upstream_addr, host, ca, leaf_cache,
  upstream_tls)` signature identical in test (Task 4 Step 1), impl (Task 4 Step 3), and caller
  (Task 5 Step 3); `rewrite_worker_policy(policy, uds, ca)` arity consistent across Task 7 + its
  test updates; `make_get_inner(ua, uds, ca)` arity consistent across Tasks 6 + 8; `CA_FILE_NAME`
  defined in spawn.rs (Task 7) and read in net_worker + e2e.
- **Open verification flags (carried into the steps, not placeholders):** exact rcgen 0.13
  method signatures (Task 2 note), `rustls-pki-types` PEM iterator availability vs a
  `rustls-pemfile` fallback (Task 6 Step 3), tokio `macros` feature for `#[tokio::test]` (Task 4
  Step 2), and the rcgen-`ring`-vs-seccomp question (Task 9 Step 3 — file an issue, don't widen).
```
