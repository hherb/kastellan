# Egress Proxy Slice #2 — Unbypassable Force-Routing + Live Transport — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route the real `web-fetch`/`web-search` workers through the slice-#1 egress proxy and make that routing unbypassable by a compromised worker (no network-namespace route except the proxy UDS).

**Architecture:** A net worker is spawned in a coupled pair with its own sandboxed proxy sidecar. The worker's OS sandbox denies all direct egress (Linux private netns; macOS Seatbelt outbound-UDS-only filter, with the `container` backend as a parity fallback). The worker's only egress is a bind-mounted UNIX socket; it speaks `CONNECT host:port` to the proxy, which enforces the host:port allowlist + SSRF/IP defense and tunnels. TLS stays end-to-end worker↔origin.

**Tech Stack:** Rust; `hyper` + `tokio` + `tokio-rustls` (worker-side CONNECT-over-UDS transport, already in lock graph); `bwrap` (Linux netns); macOS `sandbox-exec` (Seatbelt); existing `kastellan-sandbox`, `kastellan-core::egress`, `kastellan-worker-web-common`, `kastellan-worker-egress-proxy` crates.

**Spec:** `docs/superpowers/specs/2026-06-10-egress-proxy-slice2-force-routing-design.md`

**Prerequisite reading for the implementer:**
- The spec above (all of it).
- `workers/egress-proxy/src/proxy.rs` — the slice-#1 proxy `decide`/`handle_conn` you extend in Stage 3.
- `workers/web-common/src/http.rs` — the `HttpGet` seam + `ReqwestGet` you add a sibling to in Stage 1.
- `core/src/egress/spawn.rs` — `spawn_sidecar`/`SidecarHandle`/`proxy_policy` (already built) you call in Stage 4.
- `sandbox/src/linux_bwrap.rs` `build_argv` (line ~158, the `Net::Allowlist | Net::ProxyEgress` → `--share-net` arm) and `sandbox/src/macos_seatbelt.rs` (line ~321) you change in Stage 2.
- `sandbox/src/lib.rs` — the `Net` enum (lines ~36–49) whose doc already anticipates this divergence.

**Build/test env (every task):**
```sh
source "$HOME/.cargo/env"
```
Linux acceptance gates (Stage 2 macОS-probe excepted) run natively on the DGX over the operator's WireGuard SSH — these are in-band, not deferred.

---

## Cross-platform invariant (do not violate)

Every enforcement change needs a counterpart of equivalent guarantee on the other OS. Stage 2 changes bwrap **and** Seatbelt together; do not land one without the other.

---

## File Structure

**Stage 1 — connector (worker-side transport):**
- Create `workers/web-common/src/proxy_connect.rs` — `ProxyConnectGet` (`HttpGet` over CONNECT-over-UDS, hyper+tokio-rustls). One responsibility: speak the proxy protocol + do end-to-end TLS, return a `RawResponse`.
- Modify `workers/web-common/src/http.rs` — add a `make_get(user_agent) -> anyhow::Result<Box<dyn HttpGet>>` factory (env-selected). Keep `ReqwestGet` unchanged.
- Modify `workers/web-common/src/lib.rs` — `pub mod proxy_connect;`.
- Modify `workers/web-common/Cargo.toml` — add `hyper`, `hyper-util`, `http-body-util`, `tokio`, `tokio-rustls`, `rustls-pki-types`, `webpki-roots` (versions from the workspace lock; see Task 1.1).
- Modify `workers/web-fetch/src/handler.rs` — swap the `ReqwestGet::new(...)` construction site for `make_get(...)`.
- Modify `workers/web-search/src/handler.rs:78` — same swap.

**Stage 2 — OS force-routing:**
- Modify `sandbox/src/linux_bwrap.rs` — split the `Net::Allowlist` (→ `--unshare-net`, no `--share-net`) vs `Net::ProxyEgress` (→ `--share-net`) arms in `build_argv`.
- Modify `sandbox/src/macos_seatbelt.rs` — `Net::Allowlist` emits deny-all-outbound-except the proxy UDS; `Net::ProxyEgress` keeps `(allow network*)`.
- Create `sandbox/tests/seatbelt_uds_probe.rs` — gating macOS probe (deny AF_INET / allow proxy UDS).
- Modify `sandbox/src/lib.rs` — `SandboxPolicy` gains a `proxy_uds: Option<PathBuf>` field (the UDS path the `Net::Allowlist` Seatbelt arm allows + the bwrap bind target). Additive, `#[serde(default)]`.
- *(Optional belt)* Modify `workers/prelude/src/seccomp_lock.rs` — AF_INET/AF_INET6 `socket(2)` domain-deny for the worker profile. Documented as optional; gate behind a follow-up if time-boxed.

**Stage 3 — port-scoping (#241):**
- Modify `workers/egress-proxy/src/proxy.rs` `decide` — constrain `port`, not just host.
- Modify `workers/web-common/src/allowlist.rs` — add a port-aware match (`is_allowed_endpoint(host, port)`) alongside the existing host-only `is_allowed` (kept for back-compat / literal-IP).
- Modify `workers/egress-proxy/src/main.rs` — parse `host:port` allowlist entries.

**Stage 4 — host-side hookup + lifecycle:**
- Modify `core/src/egress/spawn.rs` — `proxy_policy` already exists; no change needed there.
- Create `core/src/egress/net_worker.rs` — `spawn_net_worker(...)`: spawn sidecar, rewrite worker policy (inject `KASTELLAN_EGRESS_PROXY_UDS`, bind UDS, private netns, drop resolv.conf), spawn worker, bundle handles.
- Modify `core/src/egress/mod.rs` — `pub mod net_worker;`.
- Modify `core/src/egress/audit.rs` — already has `decision_to_audit`; add the async ingest loop `ingest_decisions(stdout, pool, ...)`.
- Create `core/tests/egress_force_routing_e2e.rs` — the DGX gating e2e.
- Modify the net-worker bring-up call site (wherever `Net::Allowlist` workers are spawned in the scheduler/registry path — Task 4.5 locates it).

---

## STAGE 1 — Worker-side CONNECT-over-UDS connector

Inert until Stage 4 sets the env, but fully unit-tested here.

### Task 1.1: Add transport dependencies to web-common

**Files:**
- Modify: `workers/web-common/Cargo.toml`

- [ ] **Step 1: Pin exact versions from the workspace lock**

Run: `grep -A1 -E '^name = "(hyper|hyper-util|http-body-util|tokio|tokio-rustls|webpki-roots|rustls-pki-types)"' Cargo.lock`
Expected: prints the resolved versions already present (slice #1 pulled them transitively). Record each `version`.

- [ ] **Step 2: Add the deps under `[dependencies]`** (use the versions from Step 1; example shapes):

```toml
hyper = { version = "1", features = ["client", "http1"] }
hyper-util = { version = "0.1", features = ["tokio"] }
http-body-util = "0.1"
tokio = { version = "1", features = ["rt", "net", "io-util", "time"] }
tokio-rustls = "0.26"
rustls-pki-types = "1"
webpki-roots = "0.26"
```

Pin to the **exact** lock versions so no new resolution happens. `tokio` features: `rt` (current-thread runtime), `net` (UnixStream), `io-util`, `time` (timeouts). No `rt-multi-thread`.

- [ ] **Step 3: Verify it builds, no lock churn**

Run: `cargo build -p kastellan-worker-web-common --locked`
Expected: compiles; `--locked` proves no Cargo.lock change.

- [ ] **Step 4: AGPL license check**

Run: `cargo tree -p kastellan-worker-web-common -e no-dev --format '{p} {l}' | grep -iE 'hyper|tokio|rustls|webpki' | sort -u`
Expected: every line MIT / Apache-2.0 / ISC (webpki-roots is MPL-2.0/ISC for the data — all AGPL-compatible). If any shows a non-compatible license, STOP and report.

- [ ] **Step 5: Commit**

```bash
git add workers/web-common/Cargo.toml Cargo.lock
git commit -m "build(web-common): add hyper/tokio-rustls for the CONNECT-over-UDS connector (ROADMAP:141)"
```

### Task 1.2: Pure CONNECT request-line builder + status-line parser

These are the two pure, exhaustively-testable pieces of the connector. Build them first, TDD.

**Files:**
- Create: `workers/web-common/src/proxy_connect.rs`
- Modify: `workers/web-common/src/lib.rs`

- [ ] **Step 1: Register the module**

In `workers/web-common/src/lib.rs`, after `pub mod http;`, add:
```rust
pub mod proxy_connect;
```

- [ ] **Step 2: Write the failing tests** (append to a `#[cfg(test)] mod tests` in `proxy_connect.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_line_has_host_port_and_host_header() {
        let line = build_connect_request("example.com", 443);
        assert_eq!(
            line,
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"
        );
    }

    #[test]
    fn parse_status_accepts_200() {
        assert_eq!(parse_status_line("HTTP/1.1 200 Connection Established\r\n").unwrap(), 200);
    }

    #[test]
    fn parse_status_rejects_403() {
        assert_eq!(parse_status_line("HTTP/1.1 403 Forbidden\r\n").unwrap(), 403);
    }

    #[test]
    fn parse_status_errors_on_garbage() {
        assert!(parse_status_line("garbage").is_err());
    }
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p kastellan-worker-web-common proxy_connect::tests -- --nocapture`
Expected: FAIL — `build_connect_request`/`parse_status_line` not found.

- [ ] **Step 4: Implement the two pure functions** (top of `proxy_connect.rs`, above the test module)

```rust
//! `ProxyConnectGet`: an `HttpGet` that reaches origins **only** through the
//! per-worker egress proxy's UDS via HTTP CONNECT. Used when force-routing is
//! active (`KASTELLAN_EGRESS_PROXY_UDS` set) — the worker has no other route
//! out. TLS stays end-to-end worker↔origin (the proxy tunnels ciphertext).

/// Build the CONNECT request head for `host:port`. Host is passed verbatim
/// (a name, never a resolved IP — the proxy resolves + range-checks).
fn build_connect_request(host: &str, port: u16) -> String {
    format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n")
}

/// Parse the proxy's status line, returning the numeric status code.
fn parse_status_line(line: &str) -> Result<u16, String> {
    let code = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("malformed status line: {line:?}"))?;
    code.parse::<u16>().map_err(|e| format!("bad status code: {e}"))
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p kastellan-worker-web-common proxy_connect::tests -- --nocapture`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add workers/web-common/src/proxy_connect.rs workers/web-common/src/lib.rs
git commit -m "feat(web-common): CONNECT request builder + status parser (ROADMAP:141)"
```

### Task 1.3: `ProxyConnectGet` end-to-end against an in-test CONNECT stub

Drives the full transport over a real UDS, with an in-test stub playing the proxy (mirrors the proxy's own `handle_conn` test rig). Covers: dial → CONNECT → 200 → raw-http GET → `RawResponse`. (Loopback-http path; the https/TLS layering is exercised by the Stage-4 real e2e, since a hermetic TLS origin needs a test CA — out of scope until slice #3's CA exists, noted in the spec.)

**Files:**
- Modify: `workers/web-common/src/proxy_connect.rs`

- [ ] **Step 1: Write the failing integration-style test** (add to the `tests` module)

```rust
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use url::Url;

/// Minimal in-test proxy: accept one conn, read the CONNECT head to the blank
/// line, reply `200`, then serve a fixed HTTP/1.1 response as the "origin".
fn spawn_stub_proxy(path: std::path::PathBuf, origin_response: &'static [u8]) {
    let listener = UnixListener::bind(&path).unwrap();
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        // Drain CONNECT head up to blank line.
        let mut buf = [0u8; 1024];
        let mut acc = Vec::new();
        loop {
            let n = conn.read(&mut buf).unwrap();
            acc.extend_from_slice(&buf[..n]);
            if acc.windows(4).any(|w| w == b"\r\n\r\n") || n == 0 { break; }
        }
        conn.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
        // Now act as the raw-HTTP origin.
        let mut req = [0u8; 1024];
        let _ = conn.read(&mut req).unwrap();
        conn.write_all(origin_response).unwrap();
    });
}

#[test]
fn proxy_connect_get_round_trips_loopback_http() {
    let dir = std::env::temp_dir().join(format!("kastellan-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let uds = dir.join("egress.sock");
    let _ = std::fs::remove_file(&uds);
    spawn_stub_proxy(
        uds.clone(),
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
    );
    // Give the listener a moment to bind.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let get = ProxyConnectGet::new("kastellan-test/0", uds.clone());
    let url = Url::parse("http://127.0.0.1:8888/search").unwrap();
    let resp = get.get(&url).expect("round trip");

    assert_eq!(resp.status, 200);
    assert_eq!(resp.content_type, "application/json");
    assert_eq!(resp.body, b"{}");
    let _ = std::fs::remove_file(&uds);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-worker-web-common proxy_connect_get_round_trips_loopback_http -- --nocapture`
Expected: FAIL — `ProxyConnectGet` not defined.

- [ ] **Step 3: Implement `ProxyConnectGet`** (in `proxy_connect.rs`)

Implementation notes for the engineer:
- It implements `crate::http::HttpGet` and returns `crate::http::RawResponse`.
- Owns a `tokio` current-thread runtime built once; each `get` does `rt.block_on(async { ... })`.
- Async flow: `tokio::net::UnixStream::connect(uds)` → write `build_connect_request(host, port)` → read+parse the proxy status line (require `200`, cap the head read at 8 KiB) → then:
  - scheme `https` → `tokio_rustls::TlsConnector` (roots = `webpki_roots::TLS_SERVER_ROOTS`) handshake with `ServerName::try_from(host)`; run hyper HTTP/1.1 over the TLS stream.
  - scheme `http` → run hyper HTTP/1.1 over the raw stream.
- Request: `GET <path?query>` with headers `Host: <host>`, `User-Agent: <ua>`, `Accept-Encoding: identity`, `Connection: close`.
- Response: read status, `Location`, `Content-Type`; body via `http_body_util::BodyExt::collect` with a running cap — abort if it exceeds `crate::http::MAX_BODY_BYTES` (return `Err`), mirroring `ReqwestGet`.
- Reuse `crate::http::{RawResponse, MAX_BODY_BYTES, TIMEOUT_SECS}`; wrap the whole `block_on` future in `tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), …)`.

```rust
use std::path::PathBuf;
use std::time::Duration;

use url::Url;

use crate::http::{HttpGet, RawResponse, MAX_BODY_BYTES, TIMEOUT_SECS};

/// Read cap for the proxy's CONNECT response head (mirrors the proxy's 8 KiB).
const MAX_PROXY_HEAD_BYTES: usize = 8 * 1024;

/// `HttpGet` that reaches origins only via the egress-proxy UDS (HTTP CONNECT).
pub struct ProxyConnectGet {
    user_agent: String,
    uds: PathBuf,
    rt: tokio::runtime::Runtime,
}

impl ProxyConnectGet {
    /// Build the transport. `uds` is the proxy socket path
    /// (`KASTELLAN_EGRESS_PROXY_UDS`).
    pub fn new(user_agent: &str, uds: PathBuf) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        Self { user_agent: user_agent.to_string(), uds, rt }
    }

    async fn get_async(&self, url: &Url) -> Result<RawResponse, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let host = url.host_str().ok_or("url has no host")?;
        let port = url
            .port_or_known_default()
            .ok_or("url has no port and no known default")?;

        // 1. Dial the proxy UDS and issue CONNECT.
        let mut stream = tokio::net::UnixStream::connect(&self.uds)
            .await
            .map_err(|e| format!("connect proxy uds: {e}"))?;
        stream
            .write_all(build_connect_request(host, port).as_bytes())
            .await
            .map_err(|e| format!("write CONNECT: {e}"))?;

        // 2. Read the proxy status head (bounded), require 200.
        let head = read_proxy_head(&mut stream).await?;
        let status = parse_status_line(&head)?;
        if status != 200 {
            return Err(format!("proxy refused CONNECT: {status}"));
        }

        // 3. Layer transport and run one GET. (Engineer: factor `run_get` over
        //    a generic `AsyncRead + AsyncWrite` so https/http share it; for
        //    https wrap `stream` in tokio_rustls first.)
        match url.scheme() {
            "https" => {
                let tls = tls_connect(stream, host).await?;
                run_get(tls, url, host, &self.user_agent).await
            }
            "http" => run_get(stream, url, host, &self.user_agent).await,
            other => Err(format!("unsupported scheme: {other}")),
        }
    }
}

impl HttpGet for ProxyConnectGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        self.rt.block_on(async {
            match tokio::time::timeout(Duration::from_secs(TIMEOUT_SECS), self.get_async(url)).await {
                Ok(r) => r,
                Err(_) => Err(format!("request exceeded {TIMEOUT_SECS}s")),
            }
        })
    }
}
```

Plus the helpers `read_proxy_head` (loop `read` into a `Vec`, stop at `\r\n\r\n`, error past `MAX_PROXY_HEAD_BYTES`, return the status line slice), `tls_connect` (build a `tokio_rustls::TlsConnector` from `webpki_roots`, handshake with `rustls_pki_types::ServerName::try_from(host.to_owned())`), and `run_get` (hyper `http1::handshake`, build the `GET` request with the headers above, drive `conn`, collect the body with the `MAX_BODY_BYTES` cap, return `RawResponse`). Keep each helper small; if `proxy_connect.rs` approaches 400 LOC, split TLS into `proxy_connect/tls.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p kastellan-worker-web-common -- --nocapture`
Expected: all green, including `proxy_connect_get_round_trips_loopback_http`.

- [ ] **Step 5: Clippy**

Run: `cargo clippy -p kastellan-worker-web-common --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add workers/web-common/src/proxy_connect.rs
git commit -m "feat(web-common): ProxyConnectGet — CONNECT-over-UDS HttpGet (ROADMAP:141)"
```

### Task 1.4: Env-selected `make_get` factory

**Files:**
- Modify: `workers/web-common/src/http.rs`

- [ ] **Step 1: Write the failing tests** (in `http.rs`'s test module)

```rust
#[test]
fn make_get_returns_reqwest_when_no_proxy_env() {
    // Ensure the env is unset for this test.
    std::env::remove_var("KASTELLAN_EGRESS_PROXY_UDS");
    let g = make_get("kastellan-test/0").unwrap();
    // ReqwestGet has no UDS; assert via a marker method (see Step 3).
    assert_eq!(g.transport_kind(), "reqwest");
}

#[test]
fn make_get_returns_proxy_connect_when_env_set() {
    std::env::set_var("KASTELLAN_EGRESS_PROXY_UDS", "/tmp/does-not-need-to-exist.sock");
    let g = make_get("kastellan-test/0").unwrap();
    assert_eq!(g.transport_kind(), "proxy-connect");
    std::env::remove_var("KASTELLAN_EGRESS_PROXY_UDS");
}
```

> Note: these two tests mutate process env; mark them `#[serial_test::serial]` if the crate already uses `serial_test`, otherwise keep them in a dedicated `#[cfg(test)] mod make_get_tests` and run single-threaded in CI is unnecessary because they set/remove their own keys — but the two must not interleave. Add `serial_test` only if it is already a dev-dep; if not, gate them behind one test function that runs both sequentially.

- [ ] **Step 2: Add a `transport_kind` marker to the `HttpGet` trait** (test-support, cheap, also useful in audit/debug)

In `http.rs`, extend the trait:
```rust
pub trait HttpGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String>;
    /// Stable identifier of the concrete transport (for tests + diagnostics).
    fn transport_kind(&self) -> &'static str;
}
```
Implement on `ReqwestGet` → `"reqwest"`; on `ProxyConnectGet` (in `proxy_connect.rs`) → `"proxy-connect"`.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p kastellan-worker-web-common make_get -- --nocapture`
Expected: FAIL — `make_get` not found / trait method missing.

- [ ] **Step 4: Implement `make_get`** (in `http.rs`)

```rust
use std::path::PathBuf;

/// Build the appropriate `HttpGet` for the current environment. When
/// `KASTELLAN_EGRESS_PROXY_UDS` is set (force-routing active), egress MUST go
/// through the proxy, so return [`crate::proxy_connect::ProxyConnectGet`];
/// otherwise the direct [`ReqwestGet`] for dev/no-proxy runs.
pub fn make_get(user_agent: &str) -> anyhow::Result<Box<dyn HttpGet>> {
    match std::env::var("KASTELLAN_EGRESS_PROXY_UDS") {
        Ok(uds) if !uds.is_empty() => Ok(Box::new(
            crate::proxy_connect::ProxyConnectGet::new(user_agent, PathBuf::from(uds)),
        )),
        _ => Ok(Box::new(ReqwestGet::new(user_agent)?)),
    }
}
```

- [ ] **Step 5: Run to verify pass + clippy**

Run: `cargo test -p kastellan-worker-web-common -- --nocapture && cargo clippy -p kastellan-worker-web-common --all-targets --locked -- -D warnings`
Expected: green, exit 0.

- [ ] **Step 6: Commit**

```bash
git add workers/web-common/src/http.rs workers/web-common/src/proxy_connect.rs
git commit -m "feat(web-common): env-selected make_get factory (proxy vs reqwest) (ROADMAP:141)"
```

### Task 1.5: Swap both workers onto `make_get`

**Files:**
- Modify: `workers/web-fetch/src/handler.rs`
- Modify: `workers/web-search/src/handler.rs` (line ~78)

- [ ] **Step 1: web-search** — replace the construction site

In `workers/web-search/src/handler.rs`, the `impl WebSearchHandler<ReqwestGet>` constructor builds `ReqwestGet::new("kastellan-web-search/0")?`. Change it to return a boxed handler over `Box<dyn HttpGet>`:
- Change the field to `transport: Box<dyn HttpGet>` (the struct is already generic `WebSearchHandler<T: HttpGet>`; keep the generic for tests, but add a `from_env()` that uses `make_get`).
- Add:
```rust
impl WebSearchHandler<Box<dyn HttpGet>> {
    pub fn from_env() -> anyhow::Result<Self> {
        let transport = kastellan_worker_web_common::http::make_get("kastellan-web-search/0")?;
        // ... existing endpoint/allowlist resolution unchanged ...
        Ok(Self { transport, /* … */ })
    }
}
```
Ensure `impl<T: HttpGet> HttpGet for Box<T>`-style usage works: `Box<dyn HttpGet>` must itself implement `HttpGet`. Add a blanket impl in `http.rs` if missing:
```rust
impl HttpGet for Box<dyn HttpGet> {
    fn get(&self, url: &Url) -> Result<RawResponse, String> { (**self).get(url) }
    fn transport_kind(&self) -> &'static str { (**self).transport_kind() }
}
```

- [ ] **Step 2: web-fetch** — same swap at its `ReqwestGet::new(...)` site in `handler.rs`; route construction through `make_get("kastellan-web-fetch/0")`.

- [ ] **Step 3: Build + existing tests still pass (no behaviour change with env unset)**

Run: `cargo test -p kastellan-worker-web-fetch -p kastellan-worker-web-search --locked -- --nocapture`
Expected: all existing unit tests pass (env unset → `make_get` returns `ReqwestGet`, byte-identical behaviour).

- [ ] **Step 4: Clippy both**

Run: `cargo clippy -p kastellan-worker-web-fetch -p kastellan-worker-web-search --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add workers/web-fetch/src/handler.rs workers/web-search/src/handler.rs workers/web-common/src/http.rs
git commit -m "feat(web-fetch,web-search): route transport through make_get factory (ROADMAP:141)"
```

---

## STAGE 2 — OS force-routing enforcement

Land bwrap + Seatbelt together (cross-platform invariant). Adds `SandboxPolicy.proxy_uds`.

### Task 2.1: `SandboxPolicy.proxy_uds` field

**Files:**
- Modify: `sandbox/src/lib.rs`

- [ ] **Step 1: Write the failing test** (in `lib.rs` tests)

```rust
#[test]
fn proxy_uds_defaults_none_and_is_settable() {
    let mut p = SandboxPolicy::default();
    assert!(p.proxy_uds.is_none());
    p.proxy_uds = Some(std::path::PathBuf::from("/scratch/egress.sock"));
    assert_eq!(p.proxy_uds.as_deref(), Some(std::path::Path::new("/scratch/egress.sock")));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-sandbox proxy_uds_defaults_none -- --nocapture`
Expected: FAIL — no field `proxy_uds`.

- [ ] **Step 3: Add the field** to `SandboxPolicy` (after `env`), with rustdoc:

```rust
/// When `Net::Allowlist` and this is `Some(path)`, the worker's only egress
/// is the egress-proxy UDS at `path`: Linux puts the worker in a private
/// netns (no route out) with the socket bind-mounted; macOS Seatbelt denies
/// all outbound except this UDS. `None` keeps the legacy `--share-net`
/// behaviour (slice #1 posture). Additive.
#[serde(default)]
pub proxy_uds: Option<PathBuf>,
```
Update the `..Default::default()`-style literal constructors and any exhaustive `SandboxPolicy { .. }` builders in the tree to set `proxy_uds: None` (grep `SandboxPolicy {` to find them; `core/src/egress/spawn.rs::proxy_policy` and `tool_host` lockdown derivation are the likely ones).

- [ ] **Step 4: Run to verify pass + workspace build**

Run: `cargo test -p kastellan-sandbox proxy_uds_defaults_none -- --nocapture && cargo build --workspace --locked`
Expected: green; the field addition compiles everywhere.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/lib.rs core/src/egress/spawn.rs
git commit -m "feat(sandbox): SandboxPolicy.proxy_uds (force-routing target) (ROADMAP:141)"
```

### Task 2.2: bwrap — private netns for `Net::Allowlist` + UDS bind

**Files:**
- Modify: `sandbox/src/linux_bwrap.rs` (`build_argv`, ~line 158)

- [ ] **Step 1: Write the failing builder tests** (in `linux_bwrap.rs` tests)

```rust
#[test]
fn allowlist_with_proxy_uds_uses_private_netns_and_binds_socket() {
    let mut p = SandboxPolicy::default();
    p.net = Net::Allowlist(vec!["api.example.com:443".into()]);
    p.proxy_uds = Some(std::path::PathBuf::from("/scratch/egress.sock"));
    let argv = build_argv(&p, "/bin/worker", &[]);
    // No host-net sharing — private netns only.
    assert!(!argv.contains(&"--share-net".to_string()),
        "Net::Allowlist with proxy_uds must NOT --share-net; got: {argv:?}");
    // The proxy UDS is bind-mounted in (rw) at an identical path.
    let bind_idx = argv.iter().position(|a| a == "--bind").expect("a --bind");
    assert_eq!(argv[bind_idx + 1], "/scratch/egress.sock");
    assert_eq!(argv[bind_idx + 2], "/scratch/egress.sock");
}

#[test]
fn allowlist_without_proxy_uds_keeps_legacy_share_net() {
    let mut p = SandboxPolicy::default();
    p.net = Net::Allowlist(vec!["api.example.com:443".into()]);
    // proxy_uds = None
    let argv = build_argv(&p, "/bin/worker", &[]);
    assert!(argv.contains(&"--share-net".to_string()),
        "legacy Allowlist (no proxy_uds) keeps --share-net; got: {argv:?}");
}

#[test]
fn proxy_egress_still_shares_net() {
    let mut p = SandboxPolicy::default();
    p.net = Net::ProxyEgress;
    let argv = build_argv(&p, "/bin/proxy", &[]);
    assert!(argv.contains(&"--share-net".to_string()));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-sandbox allowlist_with_proxy_uds -- --nocapture`
Expected: FAIL — current code `--share-net`s for all `Allowlist`.

- [ ] **Step 3: Implement the split** in `build_argv`

Replace the existing arm:
```rust
if matches!(policy.net, Net::Allowlist(_) | Net::ProxyEgress) {
    argv.push("--share-net".into());
}
```
with:
```rust
match (&policy.net, &policy.proxy_uds) {
    // Force-routed worker: private netns (no route out); only the bound
    // proxy UDS reaches the host. AF_UNIX is mount-ns-scoped, not net-ns.
    (Net::Allowlist(_), Some(_uds)) => { /* no --share-net: keep --unshare-all's private netns */ }
    // The proxy itself, or legacy Allowlist without a proxy: real netns.
    (Net::ProxyEgress, _) | (Net::Allowlist(_), None) => argv.push("--share-net".into()),
    (Net::Deny, _) => { /* no net */ }
}
```
Then, after the `fs_write` bind loop, bind the proxy socket rw at an identical path:
```rust
if let Some(uds) = &policy.proxy_uds {
    push_bind(&mut argv, "--bind", uds); // rw: connecting AF_UNIX needs write on the inode
}
```
(`push_bind` already emits `flag src dst` with identical src/dst — Task asserts `/scratch/egress.sock` twice.)

- [ ] **Step 4: Run to verify pass; cross-clippy the Linux arm**

Run:
```sh
cargo test -p kastellan-sandbox -- --nocapture
cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -- -D warnings
```
Expected: builder tests green; cross-clippy exit 0 (per the project's Mac-side Linux check; pure-Rust crate, no linker needed).

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/linux_bwrap.rs
git commit -m "feat(sandbox/linux): private netns + UDS bind for force-routed Net::Allowlist (ROADMAP:141)"
```

### Task 2.3: Seatbelt — deny outbound except the proxy UDS

**Files:**
- Modify: `sandbox/src/macos_seatbelt.rs` (~line 321)

- [ ] **Step 1: Write the failing profile-builder tests** (in `macos_seatbelt/tests.rs`)

```rust
#[test]
fn allowlist_with_proxy_uds_denies_outbound_except_uds() {
    let mut p = SandboxPolicy::default();
    p.net = crate::Net::Allowlist(vec!["api.example.com:443".into()]);
    p.proxy_uds = Some(std::path::PathBuf::from("/scratch/egress.sock"));
    let prof = build_profile(&p); // the profile-builder fn name in this file
    assert!(prof.contains("(deny network-outbound)"),
        "force-routed worker must deny outbound; got:\n{prof}");
    assert!(prof.contains("(allow network-outbound (remote unix-socket (path-literal \"/scratch/egress.sock\")))"),
        "must allow only the proxy UDS; got:\n{prof}");
    assert!(!prof.contains("(allow network*)"),
        "must NOT broadly allow network; got:\n{prof}");
}

#[test]
fn allowlist_without_proxy_uds_keeps_legacy_allow_network() {
    let mut p = SandboxPolicy::default();
    p.net = crate::Net::Allowlist(vec!["api.example.com:443".into()]);
    let prof = build_profile(&p);
    assert!(prof.contains("(allow network*)"));
}
```
(Use the real builder function name from the file — likely `build_profile`/`render_profile`; grep it.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-sandbox allowlist_with_proxy_uds_denies_outbound -- --nocapture`
Expected: FAIL.

- [ ] **Step 3: Implement** — in the net arm (~line 321), branch on `proxy_uds`:

```rust
match (&policy.net, &policy.proxy_uds) {
    (crate::Net::Allowlist(_), Some(uds)) => {
        // Force-routed: deny all outbound, then re-allow ONLY the proxy UDS.
        out.push_str("(deny network-outbound)\n");
        out.push_str(&format!(
            "(allow network-outbound (remote unix-socket (path-literal {:?})))\n",
            uds.display().to_string()
        ));
    }
    (crate::Net::Allowlist(_), None) | (crate::Net::ProxyEgress, _) => {
        out.push_str("(allow network*)\n");
    }
    (crate::Net::Deny, _) => { /* no network rules */ }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p kastellan-sandbox -- --nocapture`
Expected: green (macOS host) — both Seatbelt arms covered.

- [ ] **Step 5: Commit**

```bash
git add sandbox/src/macos_seatbelt.rs sandbox/src/macos_seatbelt/tests.rs
git commit -m "feat(sandbox/macos): Seatbelt deny-outbound-except-proxy-UDS for force-routed Net::Allowlist (ROADMAP:141)"
```

### Task 2.4: macOS gating probe (deny AF_INET / allow UDS)

Decides primary (Seatbelt) vs fallback (`container`). Real on-host sandbox-exec test; skip-as-pass on non-macOS.

**Files:**
- Create: `sandbox/tests/seatbelt_uds_probe.rs`

- [ ] **Step 1: Write the probe test**

```rust
//! Gating probe for egress slice #2 (macOS): a process under the force-routed
//! `Net::Allowlist` Seatbelt profile must (a) FAIL to connect any AF_INET
//! address and (b) SUCCEED connecting the proxy UDS. If (a) fails, the design
//! falls back to the `container` backend for net workers on darwin.
#![cfg(target_os = "macos")]

use std::os::unix::net::{UnixListener, UnixStream};

#[test]
fn force_routed_profile_denies_inet_allows_uds() {
    // Build the profile via the sandbox crate's public surface for a policy
    // with Net::Allowlist + proxy_uds set to a real bound UDS, run a tiny
    // helper under sandbox-exec that:
    //   1. attempts TcpStream::connect("1.1.1.1:443") -> expect Err (denied)
    //   2. attempts UnixStream::connect(<uds>)        -> expect Ok (allowed)
    // and asserts the exit code encodes (deny, allow).
    //
    // Engineer: mirror the existing macos_smoke.rs harness for invoking
    // sandbox-exec with a generated .sb profile + a child helper binary.
    // If the project lacks a child-helper pattern, use a `bash -c` payload
    // that runs `nc`/`/dev/tcp` for the inet probe and `nc -U` for the UDS.
    todo_replace_with_real_harness();
}
```
> This is the ONE place a literal scaffold is unavoidable in the plan because the harness shape depends on the existing `macos_smoke.rs` helper convention. The implementer MUST read `sandbox/tests/macos_smoke.rs` and reuse its sandbox-exec invocation + child-process pattern, then delete the `todo_replace_with_real_harness()` marker. Do not commit the marker.

- [ ] **Step 2: Implement using the `macos_smoke.rs` pattern**

Read `sandbox/tests/macos_smoke.rs`; copy its "generate profile → `sandbox-exec -p` → run child → assert" scaffolding. The inet-deny assertion is the gating one.

- [ ] **Step 3: Run on the macOS host**

Run: `cargo test -p kastellan-sandbox --test seatbelt_uds_probe -- --nocapture`
Expected (primary path): PASS — inet denied, UDS allowed.
**If inet is NOT denied:** STOP. Record the failure in the spec's "Open risks" §1 and switch Stage 4's macOS bring-up to select the `MacosContainer` backend for net workers (the field already exists per `SandboxBackendKind`). Do not proceed assuming Seatbelt enforces.

- [ ] **Step 4: Commit**

```bash
git add sandbox/tests/seatbelt_uds_probe.rs
git commit -m "test(sandbox/macos): gating probe — force-routed profile denies AF_INET, allows proxy UDS (ROADMAP:141)"
```

---

## STAGE 3 — Port-scoped allowlist (#241)

The proxy currently matches host-only; tighten to `host:port`.

### Task 3.1: Port-aware matcher in `web-common::allowlist`

**Files:**
- Modify: `workers/web-common/src/allowlist.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn endpoint_match_requires_port() {
    let al = HostAllowlist::from_endpoints(&["api.example.com:443".into()]);
    assert!(al.is_allowed_endpoint("api.example.com", 443));
    assert!(!al.is_allowed_endpoint("api.example.com", 22),
        "same host, wrong port must be denied");
}

#[test]
fn endpoint_match_wildcard_host_any_declared_port() {
    let al = HostAllowlist::from_endpoints(&[".example.com:443".into()]);
    assert!(al.is_allowed_endpoint("docs.example.com", 443));
    assert!(!al.is_allowed_endpoint("docs.example.com", 80));
}

#[test]
fn endpoint_without_port_in_entry_allows_any_port_back_compat() {
    // A bare host entry (no :port) keeps host-only semantics.
    let al = HostAllowlist::from_endpoints(&["legacy.example.com".into()]);
    assert!(al.is_allowed_endpoint("legacy.example.com", 8443));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-worker-web-common endpoint_match -- --nocapture`
Expected: FAIL — `from_endpoints`/`is_allowed_endpoint` not found.

- [ ] **Step 3: Implement** — parse entries into `(host_matcher, Option<u16>)`; `is_allowed_endpoint(host, port)` matches the host via the existing exact/`.domain` logic AND (`port_entry.is_none()` OR `port_entry == Some(port)`). Keep the existing `is_allowed(host)` for the literal-IP carve-out and back-compat. Reuse the existing host-matching internals — do not duplicate the wildcard logic.

- [ ] **Step 4: Run to verify pass + clippy**

Run: `cargo test -p kastellan-worker-web-common -- --nocapture && cargo clippy -p kastellan-worker-web-common --all-targets --locked -- -D warnings`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add workers/web-common/src/allowlist.rs
git commit -m "feat(web-common): port-aware allowlist matcher is_allowed_endpoint (#241, ROADMAP:141)"
```

### Task 3.2: Proxy `decide` constrains port

**Files:**
- Modify: `workers/egress-proxy/src/proxy.rs` (`decide`)
- Modify: `workers/egress-proxy/src/main.rs` (parse `host:port` entries via `from_endpoints`)

- [ ] **Step 1: Write the failing tests** (in `workers/egress-proxy/src/proxy/tests.rs`)

```rust
#[test]
fn decide_blocks_allowed_host_on_wrong_port() {
    let al = HostAllowlist::from_endpoints(&["example.com:443".into()]);
    let r = decide("example.com", 22, &al, &StubResolve::to(&["93.184.216.34"]));
    assert!(matches!(r, Target::Block(Verdict::BlockedAllowlist, _)),
        "allowed host on undeclared port must be blocked");
}

#[test]
fn decide_allows_host_on_declared_port() {
    let al = HostAllowlist::from_endpoints(&["example.com:443".into()]);
    let r = decide("example.com", 443, &al, &StubResolve::to(&["93.184.216.34"]));
    assert!(matches!(r, Target::Dial(_)));
}
```
(Use the existing stub-resolver helper from `proxy/tests.rs`; match its name.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-worker-egress-proxy decide_blocks_allowed_host_on_wrong_port -- --nocapture`
Expected: FAIL — `decide` still host-only.

- [ ] **Step 3: Implement** — change `decide`'s first guard from `allow.is_allowed(host)` to `allow.is_allowed_endpoint(host, port)`; keep the literal-IP carve-out using `is_allowed_endpoint` too (so a literal `127.0.0.1:8888` entry pins both). Update `main.rs` to build the allowlist with `HostAllowlist::from_endpoints(...)`. Update the `proxy.rs` rustdoc that currently says "host-only — port unconstrained" to describe port-scoping; remove the slice-#1 "#241 deferred" note.

- [ ] **Step 4: Run + clippy + cross-clippy**

Run:
```sh
cargo test -p kastellan-worker-egress-proxy -- --nocapture
cargo clippy -p kastellan-worker-egress-proxy --all-targets --locked -- -D warnings
```
Expected: green (23+ existing + new tests).

- [ ] **Step 5: Commit**

```bash
git add workers/egress-proxy/src/proxy.rs workers/egress-proxy/src/main.rs workers/egress-proxy/src/proxy/tests.rs
git commit -m "feat(egress-proxy): port-scope the allowlist in decide (closes #241, ROADMAP:141)"
```

---

## STAGE 4 — Host-side hookup + lifecycle coupling

Makes it live. The DGX e2e is the gating acceptance test.

### Task 4.1: Decision-stream → `audit_log` ingest loop

**Files:**
- Modify: `core/src/egress/audit.rs` (add `ingest_decisions`; `decision_to_audit` already exists)

- [ ] **Step 1: Write the failing test** (hermetic — fake reader + capturing sink)

```rust
#[tokio::test]
async fn ingest_maps_each_line_to_an_audit_row() {
    // Two decision JSON lines (allowed + blocked) over an in-memory reader.
    let input = b"{\"worker\":\"web-fetch\",\"host\":\"a.com\",\"port\":443,\"resolved_ip\":\"1.2.3.4\",\"verdict\":\"allowed\",\"reason\":\"ok\"}\n\
                  {\"worker\":\"web-fetch\",\"host\":\"b.com\",\"port\":443,\"resolved_ip\":null,\"verdict\":\"blocked_allowlist\",\"reason\":\"x\"}\n";
    let reader = tokio::io::BufReader::new(&input[..]);
    let mut rows: Vec<crate::egress::audit::AuditRow> = Vec::new();
    ingest_decisions_into(reader, |row| rows.push(row)).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].action, "egress.allowed");
    assert_eq!(rows[1].action, "egress.blocked.allowlist");
}
```
(`AuditRow`/action strings are whatever `decision_to_audit` already produces — match them exactly; grep `egress.allowed` in `audit.rs`.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-core ingest_maps_each_line -- --nocapture`
Expected: FAIL — `ingest_decisions_into` not found.

- [ ] **Step 3: Implement** a pure-ish `ingest_decisions_into<R, F>(reader, on_row)` (reads lines, parses each via the existing `Decision` deserialize, maps via `decision_to_audit`, calls `on_row`) plus a thin `ingest_decisions(stdout, pool)` wrapper that supplies an `on_row` doing the real `db::audit::insert`. The split keeps the loop unit-testable without PG.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p kastellan-core egress::audit -- --nocapture`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add core/src/egress/audit.rs
git commit -m "feat(core/egress): ingest_decisions — proxy stdout -> audit_log (ROADMAP:141)"
```

### Task 4.2: `spawn_net_worker` policy rewrite (unit-tested, no spawn)

**Files:**
- Create: `core/src/egress/net_worker.rs`
- Modify: `core/src/egress/mod.rs` (`pub mod net_worker;`)

- [ ] **Step 1: Write the failing test for the pure rewrite**

```rust
#[test]
fn rewrite_worker_policy_forces_routing() {
    let base = SandboxPolicy {
        net: Net::Allowlist(vec!["api.example.com:443".into()]),
        fs_read: vec!["/etc/resolv.conf".into(), "/bin/worker".into()],
        env: vec![],
        ..SandboxPolicy::default()
    };
    let uds = std::path::PathBuf::from("/scratch/egress.sock");
    let out = rewrite_worker_policy(base, &uds);
    // proxy_uds set -> bwrap/Seatbelt force-route.
    assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
    // resolv.conf removed (worker no longer resolves).
    assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
    // env carries the UDS path.
    assert!(out.env.iter().any(|(k, v)| k == "KASTELLAN_EGRESS_PROXY_UDS" && v == "/scratch/egress.sock"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-core rewrite_worker_policy_forces_routing -- --nocapture`
Expected: FAIL — function missing.

- [ ] **Step 3: Implement `rewrite_worker_policy`** (pure)

```rust
//! Couple a Net::Allowlist worker with its egress-proxy sidecar so the worker
//! cannot be spawned without a live proxy and has no egress except the UDS.

use std::path::{Path, PathBuf};
use kastellan_sandbox::SandboxPolicy;

const ENV_UDS: &str = "KASTELLAN_EGRESS_PROXY_UDS";

/// Rewrite a net worker's policy for force-routing: point it at the proxy UDS,
/// drop direct DNS, and inject the UDS env. Pure — no spawn, fully testable.
pub fn rewrite_worker_policy(mut policy: SandboxPolicy, uds: &Path) -> SandboxPolicy {
    policy.proxy_uds = Some(uds.to_path_buf());
    // The worker no longer resolves DNS (the proxy does); revoke the file.
    policy.fs_read.retain(|p| p != Path::new("/etc/resolv.conf"));
    // Inject the UDS env (overwrite any stale entry).
    policy.env.retain(|(k, _)| k != ENV_UDS);
    policy.env.push((ENV_UDS.to_string(), uds.to_string_lossy().into_owned()));
    policy
}
```

- [ ] **Step 4: Run to verify pass + clippy**

Run: `cargo test -p kastellan-core rewrite_worker_policy_forces_routing -- --nocapture && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings`
Expected: green. (Cross-`cargo test` of `core` for Linux is the #144 wall; this is host-tested + DGX-verified later.)

- [ ] **Step 5: Commit**

```bash
git add core/src/egress/net_worker.rs core/src/egress/mod.rs
git commit -m "feat(core/egress): rewrite_worker_policy — force-route a Net::Allowlist worker (ROADMAP:141)"
```

### Task 4.3: `spawn_net_worker` — coupled spawn + teardown

**Files:**
- Modify: `core/src/egress/net_worker.rs`

- [ ] **Step 1: Write the failing test** (fail-closed when the sidecar can't start)

```rust
#[test]
fn spawn_net_worker_fails_closed_when_sidecar_unavailable() {
    // Point at a non-existent proxy binary so spawn_sidecar errs; assert the
    // worker is NEVER spawned (Err returned, no SupervisedWorker).
    let backend = test_backend(); // existing sandbox test factory
    let spec = /* a WorkerSpec for a dummy worker */;
    let res = spawn_net_worker(
        &backend,
        std::path::Path::new("/nonexistent/egress-proxy"),
        &spec,
        &["api.example.com:443".to_string()],
        &scratch_dir(),
        "web-fetch",
    );
    assert!(res.is_err(), "no proxy => no net worker (fail-closed)");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p kastellan-core spawn_net_worker_fails_closed -- --nocapture`
Expected: FAIL — `spawn_net_worker` missing.

- [ ] **Step 3: Implement `spawn_net_worker`**

Signature + flow (uses the already-built `spawn_sidecar` from `spawn.rs`):
```rust
pub struct NetWorker {
    pub worker: kastellan_core::tool_host::SupervisedWorker, // adjust path
    _sidecar: super::spawn::SidecarHandle,
    _ingest: tokio::task::JoinHandle<()>,
}

pub fn spawn_net_worker(
    backend: &dyn SandboxBackend,
    proxy_bin: &Path,
    spec: &WorkerSpec<'_>,
    allowlist: &[String],
    scratch: &Path,
    worker_name: &str,
) -> Result<NetWorker, ToolHostError> {
    // 1. Sidecar first; fail-closed on its Err (no worker without a proxy).
    let mut sidecar = super::spawn::spawn_sidecar(backend, proxy_bin, allowlist, scratch, worker_name)
        .map_err(|e| ToolHostError::from(/* wrap */ e))?;
    // 2. Rewrite the worker policy onto the sidecar UDS.
    let uds = sidecar.uds_path.clone();
    let forced = rewrite_worker_policy(spec.policy.clone(), &uds);
    let forced_spec = WorkerSpec { policy: &forced, ..*spec };
    // 3. Spawn the worker under the forced policy.
    let worker = kastellan_core::tool_host::spawn_worker(backend, &forced_spec)?;
    // 4. Decision-ingest task on the sidecar stdout.
    let stdout = sidecar.stdout();
    let ingest = spawn_ingest(stdout /*, pool */);
    Ok(NetWorker { worker, _sidecar: sidecar, _ingest: ingest })
}
```
Drop order in `NetWorker`: `worker` (closes pipes) → `_sidecar` (kills proxy) → `_ingest` (sees EOF, finishes). Document it on the struct.

- [ ] **Step 4: Run to verify pass + clippy**

Run: `cargo test -p kastellan-core spawn_net_worker_fails_closed -- --nocapture && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add core/src/egress/net_worker.rs
git commit -m "feat(core/egress): spawn_net_worker — coupled sidecar+worker, fail-closed (ROADMAP:141)"
```

### Task 4.4: Wire net-worker bring-up to `spawn_net_worker`

**Files:**
- Modify: the net-worker spawn site in the scheduler/registry dispatch path.

- [ ] **Step 1: Locate the spawn site**

Run: `grep -rn "spawn_worker\|Net::Allowlist\|WorkerNetClient" core/src/scheduler core/src/tool_host.rs core/src/workers`
Find where a `Net::Allowlist` worker's `WorkerSpec` is built + `spawn_worker` is called (web-fetch/web-search dispatch). Document the exact file:line in this task before editing.

- [ ] **Step 2: Branch on `Net::Allowlist` + a configured proxy binary**

Where the worker is spawned: if `spec.policy.net` is `Net::Allowlist(_)` and a proxy binary is resolvable (reuse the worker-binary discovery from `worker_manifest`/`registry_build` for `egress-proxy`), call `spawn_net_worker(...)` with the worker's already-resolved allowlist (the `tool_allowlists` row `build_tool_registry` prefetched) and the per-task scratch dir; else keep the existing `spawn_worker` path (legacy / `Net::Deny`).

- [ ] **Step 3: Add the egress-proxy binary to worker discovery**

Ensure `egress-proxy` is discoverable as a sibling binary (mirror `shell_exec`/`web_fetch` manifest discovery). If a manifest entry is the cleanest path, add a minimal `EgressProxyManifest` returning its binary path; it is not a JSON-RPC tool (no registry `ToolEntry`), only a spawnable sidecar — so discover the path without registering it as a callable tool.

- [ ] **Step 4: Build the workspace + run the host-side suite**

Run: `cargo build --workspace --locked && cargo test -p kastellan-core --locked -- --nocapture`
Expected: green on macOS skip-as-pass (no PG). The real force-routing assertion is the DGX e2e (Task 4.6).

- [ ] **Step 5: Commit**

```bash
git add core/src/scheduler core/src/workers core/src/tool_host.rs
git commit -m "feat(core): route Net::Allowlist workers through spawn_net_worker (ROADMAP:141)"
```

### Task 4.5: Tie ingest to the core DB pool

**Files:**
- Modify: `core/src/egress/net_worker.rs` + the call site (thread the pool through)

- [ ] **Step 1:** Thread the runtime pool (or an `AuditSink`) into `spawn_net_worker` so `spawn_ingest` inserts via `db::audit::insert`. Keep the pure `ingest_decisions_into` (Task 4.1) untouched; only the wrapper gains the pool.
- [ ] **Step 2:** Build + clippy.

Run: `cargo build --workspace --locked && cargo clippy -p kastellan-core --all-targets --locked -- -D warnings`
Expected: green.
- [ ] **Step 3: Commit**

```bash
git add core/src/egress/net_worker.rs core/src/scheduler
git commit -m "feat(core/egress): persist egress decisions via the runtime pool (ROADMAP:141)"
```

### Task 4.6: DGX force-routing e2e (gating, Linux-native)

**Files:**
- Create: `core/tests/egress_force_routing_e2e.rs`

- [ ] **Step 1: Write the e2e** (`#![cfg(target_os = "linux")]`, skip-as-pass without bwrap/userns; PG-gated for the audit assertion)

The test must prove the **kernel barrier**, not just the env:
1. Spawn a `web-fetch`-shaped worker via `spawn_net_worker` against a sidecar whose allowlist is a literal loopback origin you stand up in-test (a tiny TCP listener acting as the "origin" on `127.0.0.1:<port>`, allowlisted as `127.0.0.1:<port>`).
2. **Allowed path:** a `web.fetch`-style request to the allowlisted loopback origin round-trips through the sidecar (returns the origin's bytes).
3. **Force-routing proof:** from *inside the worker's netns* a direct `TcpStream::connect` to an off-allowlist address returns `ENETUNREACH`/no-route (assert the worker cannot reach the network without the proxy). Implement via a worker test-hook or a one-shot helper run under the same forced policy.
4. **Off-allowlist/off-port:** a CONNECT for a non-allowlisted host (or the allowlisted host on a different port) gets `403` from the proxy.
5. **Audit (PG-gated):** the decision rows land in `audit_log` via the ingest task (skip-as-pass without PG).

Reuse `core/tests/egress_proxy_e2e.rs` (slice #1) as the harness template — it already drives the real sandboxed sidecar; extend it with the worker side + the netns no-route assertion.

- [ ] **Step 2: Run on the DGX over WireGuard SSH** (native aarch64, real bwrap + PG)

Run (on the DGX):
```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test egress_force_routing_e2e -- --nocapture
cargo test --workspace --locked    # full native baseline
cargo clippy --workspace --all-targets --locked -- -D warnings
```
Expected: the force-routing e2e passes with **real** containment (no `[SKIP]`); full workspace green; clippy exit 0. **If the netns no-route assertion fails** (worker can still reach the network), the force-routing is not effective — STOP and debug the bwrap netns/bind before claiming the slice done. Also confirm #243: the proxy's seccomp permits `bind`/`listen`/`accept` and the worker's permits AF_UNIX `connect`; widen `workers/prelude/src/seccomp_lock.rs` if `accept`/`connect` is killed.

- [ ] **Step 3: Commit**

```bash
git add core/tests/egress_force_routing_e2e.rs
git commit -m "test(core): DGX e2e — force-routing kernel barrier + off-allowlist refusal (ROADMAP:141)"
```

---

## Final verification (whole slice)

- [ ] **macOS host:** `cargo test --workspace --locked` green (skip-as-pass, no PG); `cargo clippy --workspace --all-targets --locked -- -D warnings` exit 0; the Seatbelt UDS probe (Task 2.4) passes (or the container fallback is selected + documented).
- [ ] **DGX (native Linux):** full `cargo test --workspace` green incl. `egress_force_routing_e2e` with real containment; `cargo clippy --workspace -D warnings` exit 0; #243 seccomp checks confirmed.
- [ ] **Security review:** request a review focused on bypass — can a compromised worker reach off-allowlist/off-port? Is there any spawn path that skips the sidecar? Does dropping `/etc/resolv.conf` + private netns actually remove all direct resolution/connection? Is the literal-IP carve-out still port-scoped?
- [ ] **Threat-model doc:** update `docs/threat-model.md` "Network egress" — the SSRF/allowlist gap is now closed at the network layer (force-routed); note the macOS mechanism (Seatbelt filter or container fallback).
- [ ] All new/changed files < 500 LOC; split per the file-structure map if any approaches the cap.

---

## Self-review notes (author)

- **Spec coverage:** Stage 1 = spec Component 1 (connector + factory + worker swap); Stage 2 = spec Component 2 Linux enforcement + Component 3 macOS enforcement + probe; Stage 3 = spec Component 3 port-scoping (#241); Stage 4 = spec Component 2 hookup/lifecycle + Component 3 decision-ingest + the DGX/macOS acceptance gates. Deferrals (#242, slice #3/#4, transparent decompression, optional seccomp belt) carried verbatim.
- **One scaffold marker** (`todo_replace_with_real_harness()` in Task 2.4) is intentional and explicitly flagged because the macOS probe harness must be copied from the existing `macos_smoke.rs` convention the implementer reads at that step — the implementer MUST replace + not commit it. Every other step carries real code.
- **Type consistency:** `make_get -> anyhow::Result<Box<dyn HttpGet>>`, `HttpGet::transport_kind`, `is_allowed_endpoint`/`from_endpoints`, `rewrite_worker_policy`, `spawn_net_worker`/`NetWorker`, `ingest_decisions_into`/`ingest_decisions`, `SandboxPolicy.proxy_uds` — names used consistently across tasks.
