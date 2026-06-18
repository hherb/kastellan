# Matrix Phase D — egress-transport spike + `matrix-sdk` landing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the `matrix-sdk` dependency behind the `live-matrix` feature with an AGPL license pass, and hermetically prove on macOS that matrix-sdk routes its HTTP through our UDS egress proxy via an in-worker TCP↔UDS bridge — the spike-first gate that unblocks the live SDK integration.

**Architecture:** matrix-sdk's reqwest client cannot dial a Unix-domain-socket proxy, so the worker runs a loopback-TCP↔UDS bridge (`ProxyBridge`, the Rust analogue of browser-driver's `shim.py`) and points the SDK at `.proxy("http://127.0.0.1:<port>")`. The egress posture is transparent-tunnel (no TLS interception of the trusted homeserver), so no custom CA is injected. A feature-gated spike test stands up a stub UDS proxy behind the bridge, builds a `matrix_sdk::Client` through it, triggers the SDK's first network call, and asserts the stub observed a `CONNECT <host>:443`.

**Tech Stack:** Rust, `matrix-sdk` (optional dep, SQLite store + rustls TLS), tokio (workspace, full), `kastellan-protocol`, `kastellan-worker-prelude`.

**Spec:** `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md`

## Global Constraints

- **AGPL-3.0 project; AGPL-compatible dependencies only.** Apache-2.0 / MIT / BSD / MPL / LGPL / (A)GPL are fine. **Block** any CDDL, BUSL, SSPL, Elastic License, or "source-available" dependency. This gate (Task 2) is **abortive**: if a blocked license appears in the `live-matrix` subtree, stop and report — do not work around it.
- **Cross-platform: Linux + macOS first-class.** This slice is verified on macOS (dev box); the live homeserver round-trip is a DGX job in the *next* slice. No macOS-only code without a Linux counterpart of equivalent guarantee — the `ProxyBridge` is plain tokio net, portable.
- **No system OpenSSL / native-tls.** Use rustls for TLS and a bundled SQLite (the project does not link system OpenSSL; see workspace `reqwest` config).
- **Default build & CI stay byte-identical.** `matrix-sdk` compiles only under `--features live-matrix`; the default `cargo build`/`cargo test -p kastellan-worker-matrix` and `cargo clippy --workspace --all-targets -- -D warnings` (default features) are unaffected.
- **Egress UDS env var:** `KASTELLAN_EGRESS_PROXY_UDS` (the sidecar UDS path the bridge connects to).
- **File-size cap:** keep files under 500 LOC where feasible.
- **TDD:** test first, watch it fail, minimal implementation, watch it pass, commit.

---

## File Structure

- `workers/matrix/Cargo.toml` — add `matrix-sdk` as an optional dep; `live-matrix = ["dep:matrix-sdk"]`; add `[dev-dependencies]` if needed (none expected — temp paths use `std`).
- `workers/matrix/src/main.rs` — make the `live-matrix` build compile: drop the dangling `mod sdk_live;`, turn the live `main()` block into an honest `bail!` (live serving is the next slice), broaden the dead-code allow for this mid-construction slice.
- `workers/matrix/src/bridge.rs` — **new.** `ProxyBridge`: loopback-TCP listener that relays each accepted connection to the sidecar UDS. Always compiled; `#[allow(dead_code)]` (consumed by the spike test + the next slice's `LiveSdk`). In-crate `#[cfg(test)] mod tests` (no matrix-sdk — runs in the default test pass).
- `workers/matrix/src/egress_spike.rs` — **new, `#[cfg(all(test, feature = "live-matrix"))]`.** The matrix-sdk transport spike test + a small stub-UDS-proxy test helper.
- `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md` — append the license-pass result + the recorded transport decision (Task 2, Task 5).

---

## Task 1: Land `matrix-sdk` behind `live-matrix` + make the feature build compile

**Files:**
- Modify: `workers/matrix/Cargo.toml`
- Modify: `workers/matrix/src/main.rs`

**Interfaces:**
- Consumes: the existing `[features] live-matrix = []` and the bin's `mod handler; mod sdk;`.
- Produces: a `--features live-matrix` build that **compiles** (it does not run — `main()` bails). `matrix_sdk` is on the dependency path only under the feature.

- [ ] **Step 1: Add `matrix-sdk` as an optional dependency.**

Run (resolves + pins the latest compatible version; records it in `Cargo.toml`):

```bash
source "$HOME/.cargo/env"
cd /Users/hherb/src/kastellan
cargo add matrix-sdk -p kastellan-worker-matrix \
  --optional --no-default-features \
  --features e2e-encryption,sqlite,bundled-sqlite,rustls-tls
```

Then edit `workers/matrix/Cargo.toml` so the feature enables the optional dep:

```toml
[features]
# The heavy matrix-rust-sdk integration is opt-in so the default build (and CI)
# stays light + hermetic. Phase D (DGX) builds with `--features live-matrix`.
live-matrix = ["dep:matrix-sdk"]
```

If `cargo add` reports that `bundled-sqlite` is not a feature of the resolved
`matrix-sdk` version, drop it from the `--features` list (keep
`e2e-encryption,sqlite,rustls-tls`) and re-run; record which feature set
resolved. Do **not** enable any `native-tls`/OpenSSL feature.

- [ ] **Step 2: Make `main.rs` compile under the feature without the deferred `LiveSdk`.**

Replace the whole of `workers/matrix/src/main.rs` with (note: `mod sdk_live;` is removed and the live arm now bails — `LiveSdk` + real serving land in the next slice):

```rust
//! kastellan-worker-matrix: the sandboxed Matrix channel worker. Wraps
//! matrix-rust-sdk (login + E2E sync loop + buffered inbound) behind a JSON-RPC
//! stdio surface (`matrix.init` / `matrix.poll` / `matrix.send`), served via the
//! prelude's `serve_stdio` after `lock_down`. Design:
//! docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md
//!
//! The real matrix-rust-sdk integration is gated behind the `live-matrix`
//! feature (Phase D, verified on the DGX). This slice lands the `matrix-sdk`
//! dependency and proves the egress transport (see `bridge.rs` + the
//! `egress_spike` test); the live serving path (`sdk_live::LiveSdk` + the
//! `serve_stdio` wiring) is the NEXT slice.

// This bin crate is mid-construction: the handler + SDK seam + the egress
// `ProxyBridge` are exercised by unit/spike tests and by the next slice's live
// wiring, but `main()` does not serve yet (it bails under both cfgs). Allow the
// resulting dead code crate-wide for now; the live-wiring slice narrows this
// back to `#![cfg_attr(not(feature = "live-matrix"), allow(dead_code))]`.
#![allow(dead_code)]

mod bridge;
mod handler;
mod sdk;

#[cfg(all(test, feature = "live-matrix"))]
mod egress_spike;

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "live-matrix")]
    {
        anyhow::bail!(
            "kastellan-worker-matrix `live-matrix` build: the matrix-sdk egress \
             transport is proven (see bridge.rs + the egress_spike test) but the \
             live serving path (LiveSdk: login + sync loop + poll/send) is wired \
             in the next slice"
        )
    }
    #[cfg(not(feature = "live-matrix"))]
    {
        anyhow::bail!(
            "kastellan-worker-matrix was built without the `live-matrix` feature; \
             rebuild with `--features live-matrix` to run the real Matrix client"
        )
    }
}
```

(`mod bridge;` and `mod egress_spike;` are added now so later tasks only create
the files. `bridge.rs` is created in Task 3 and `egress_spike.rs` in Task 4 — if
you implement strictly in order, temporarily comment those two `mod` lines until
their files exist, or create empty stubs. Simplest: create `bridge.rs` and
`egress_spike.rs` as empty files now, filled in Tasks 3–4.)

- [ ] **Step 3: Create empty module files so Task 1 compiles standalone.**

```bash
: > workers/matrix/src/bridge.rs
: > workers/matrix/src/egress_spike.rs
```

- [ ] **Step 4: Verify the default build is unaffected and the feature build compiles.**

```bash
cargo build -p kastellan-worker-matrix
cargo build -p kastellan-worker-matrix --features live-matrix
```

Expected: both succeed. The first does **not** pull in `matrix-sdk` (confirm
with `cargo tree -p kastellan-worker-matrix -i matrix-sdk` → "package ID
specification ... did not match any packages"); the second compiles it (slow
first build — matrix-sdk is heavy).

- [ ] **Step 5: Verify default tests + clippy still green.**

```bash
cargo test -p kastellan-worker-matrix
cargo clippy -p kastellan-worker-matrix --all-targets -- -D warnings
```

Expected: PASS (the existing handler/sdk unit tests still run; no new warnings).

- [ ] **Step 6: Commit.**

```bash
git add workers/matrix/Cargo.toml workers/matrix/src/main.rs \
        workers/matrix/src/bridge.rs workers/matrix/src/egress_spike.rs Cargo.lock
git commit -m "feat(matrix-worker): land matrix-sdk behind live-matrix; compile the feature build

Adds matrix-sdk as an optional dep gated by \`live-matrix = [\"dep:matrix-sdk\"]\`
(SQLite store + rustls, no native-tls). Removes the dangling \`mod sdk_live;\`
and turns the live main() into an honest bail! — LiveSdk + serving are the next
slice. Default build/tests/clippy unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: AGPL license pass on the `live-matrix` subtree (abortive gate)

**Files:**
- Modify: `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md` (append the result)

**Interfaces:**
- Consumes: the `live-matrix` dependency tree from Task 1.
- Produces: a recorded PASS/ABORT decision. On ABORT the whole slice stops.

- [ ] **Step 1: Install the license scanner (one-time).**

```bash
cargo install cargo-license --locked 2>/dev/null || true
cargo-license --version
```

- [ ] **Step 2: Enumerate the licenses of the matrix-sdk subtree.**

```bash
# All deps reachable from the matrix worker WITH the feature on, normal edges:
cargo tree -p kastellan-worker-matrix --features live-matrix -e normal --prefix none | sort -u > /tmp/matrix-live-deps.txt
wc -l /tmp/matrix-live-deps.txt
# Workspace-wide license map (cargo-license has no per-package feature scoping;
# cross-reference against the dep list above):
cargo-license --all-features 2>/dev/null | sort -u > /tmp/all-licenses.txt
# Surface anything that even looks blocked:
grep -iE "CDDL|BUSL|Business Source|SSPL|Server Side Public|Elastic|source-available|Commons Clause" /tmp/all-licenses.txt || echo "no obviously-blocked licenses found"
```

- [ ] **Step 3: Manually confirm each new crate's license is AGPL-compatible.**

Cross-reference the crates in `/tmp/matrix-live-deps.txt` against
`/tmp/all-licenses.txt`. Every new crate's license must be one of: Apache-2.0,
MIT, BSD-2/3-Clause, MPL-2.0, ISC, Unicode-DFS, Zlib, LGPL-*, (A)GPL-*, or a
dual permissive (`MIT OR Apache-2.0`). matrix-rust-sdk + ruma + vodozemac are
expected Apache-2.0/MIT. Note any crate whose license you cannot positively
classify.

- [ ] **Step 4: Decide — PASS or ABORT.**

- If **all** new crates are AGPL-compatible: append to the spec a
  `## License pass (2026-06-19)` section listing the matrix-sdk version, the
  resolved feature set, the crate count, and "PASS — all AGPL-compatible". Then
  continue to Task 3.
- If **any** crate carries a blocked license: append the same section recording
  "ABORT — `<crate>` is `<license>`", **stop here**, and report to the operator.
  Do not proceed.

- [ ] **Step 5: Commit the recorded result.**

```bash
git add docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md
git commit -m "docs(matrix): record AGPL license pass for the matrix-sdk subtree

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `ProxyBridge` — loopback-TCP↔UDS relay (TDD)

**Files:**
- Modify: `workers/matrix/src/bridge.rs` (created empty in Task 1)
- Test: in-crate `#[cfg(test)] mod tests` inside `bridge.rs`

**Interfaces:**
- Consumes: `KASTELLAN_EGRESS_PROXY_UDS` semantics (a filesystem path to the sidecar UDS).
- Produces:
  - `pub struct ProxyBridge` with:
    - `pub async fn bind(uds_path: std::path::PathBuf) -> std::io::Result<ProxyBridge>` — binds `127.0.0.1:0`, spawns the accept loop, returns immediately.
    - `pub fn proxy_addr(&self) -> std::net::SocketAddr` — the bound loopback address to hand to matrix-sdk's `.proxy()`.
  - Dropping the `ProxyBridge` aborts the accept loop (the spawned task is stored as a `tokio::task::JoinHandle` and aborted on `Drop`).

- [ ] **Step 1: Write the failing tests.**

Append to `workers/matrix/src/bridge.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UnixListener};

    // A short, unique UDS path under /tmp (stays well under the 108-byte
    // sun_path limit; /tmp is the macOS egress scratch root).
    fn uds_path(tag: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/tmp/km-bridge-{}-{}.sock", tag, std::process::id()))
    }

    #[tokio::test]
    async fn relays_tcp_bytes_to_uds_and_back() {
        let path = uds_path("relay");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind uds");

        // Echo server on the UDS side: read one chunk, write it back uppercased.
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept uds");
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).await.expect("read");
            let upper: Vec<u8> = buf[..n].iter().map(|b| b.to_ascii_uppercase()).collect();
            s.write_all(&upper).await.expect("write back");
        });

        let bridge = ProxyBridge::bind(path.clone()).await.expect("bind bridge");
        let mut client = TcpStream::connect(bridge.proxy_addr()).await.expect("connect tcp");
        client.write_all(b"hello").await.expect("write");
        let mut resp = [0u8; 5];
        client.read_exact(&mut resp).await.expect("read");
        assert_eq!(&resp, b"HELLO");

        server.await.expect("server task");
        drop(bridge);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn proxy_addr_is_loopback() {
        let path = uds_path("addr");
        let _ = std::fs::remove_file(&path);
        let _listener = UnixListener::bind(&path).expect("bind uds");
        let bridge = ProxyBridge::bind(path.clone()).await.expect("bind bridge");
        assert!(bridge.proxy_addr().ip().is_loopback());
        drop(bridge);
        let _ = std::fs::remove_file(&path);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

```bash
cargo test -p kastellan-worker-matrix bridge -- --nocapture
```

Expected: FAIL to compile — `ProxyBridge` not found.

- [ ] **Step 3: Implement `ProxyBridge`.**

Prepend to `workers/matrix/src/bridge.rs` (above the test module):

```rust
//! `ProxyBridge`: matrix-sdk's reqwest client speaks HTTP-proxy CONNECT over
//! TCP, but our egress sidecar listens on a Unix-domain socket. This bridge
//! binds a loopback TCP port, and for each accepted connection opens the sidecar
//! UDS and copies bytes both ways — the Rust analogue of browser-driver's
//! `shim.py ProxyShim`. The SDK is pointed at `proxy_addr()` via `.proxy()`.

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::JoinHandle;

/// A loopback-TCP↔UDS relay. Constructed by the next slice's `LiveSdk` and
/// exercised now by the `egress_spike` test.
#[allow(dead_code)]
pub struct ProxyBridge {
    addr: SocketAddr,
    accept_task: JoinHandle<()>,
}

#[allow(dead_code)]
impl ProxyBridge {
    /// Bind `127.0.0.1:0`, spawn the accept loop relaying to `uds_path`, and
    /// return immediately. The accept loop runs until the `ProxyBridge` is
    /// dropped.
    pub async fn bind(uds_path: PathBuf) -> std::io::Result<ProxyBridge> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let accept_task = tokio::spawn(async move {
            loop {
                let (tcp, _peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let path = uds_path.clone();
                tokio::spawn(async move { relay(tcp, path).await });
            }
        });
        Ok(ProxyBridge { addr, accept_task })
    }

    /// The bound loopback address to hand to matrix-sdk's `.proxy()`.
    pub fn proxy_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for ProxyBridge {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Relay one accepted TCP connection to the sidecar UDS, both directions.
async fn relay(mut tcp: TcpStream, uds_path: PathBuf) {
    let Ok(mut uds) = UnixStream::connect(&uds_path).await else {
        return; // sidecar gone / not listening: drop this connection
    };
    let _ = copy_bidirectional(&mut tcp, &mut uds).await;
}
```

- [ ] **Step 4: Run the tests to verify they pass.**

```bash
cargo test -p kastellan-worker-matrix bridge -- --nocapture
```

Expected: PASS (both bridge tests).

- [ ] **Step 5: Clippy on the touched crate.**

```bash
cargo clippy -p kastellan-worker-matrix --all-targets -- -D warnings
```

Expected: clean.

- [ ] **Step 6: Commit.**

```bash
git add workers/matrix/src/bridge.rs
git commit -m "feat(matrix-worker): ProxyBridge loopback-TCP to sidecar-UDS relay

The Rust analogue of browser-driver's shim.py: matrix-sdk's reqwest client
speaks proxy CONNECT over TCP, the egress sidecar listens on a UDS, so the
bridge copies bytes both ways. Unit-tested hermetically (no matrix-sdk).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: The egress-transport spike test (TDD, feature-gated)

**Files:**
- Modify: `workers/matrix/src/egress_spike.rs` (created empty in Task 1; declared `#[cfg(all(test, feature = "live-matrix"))] mod egress_spike;` in `main.rs`)

**Interfaces:**
- Consumes: `crate::bridge::ProxyBridge`, `matrix_sdk` (feature-gated).
- Produces: the empirical proof that matrix-sdk routes its first HTTPS request through the bridge as a `CONNECT <host>:443`. This is the slice's central deliverable.

- [ ] **Step 1: Write the failing spike test + the stub-UDS-proxy helper.**

Write `workers/matrix/src/egress_spike.rs` (the whole file is under
`#[cfg(all(test, feature = "live-matrix"))]` via the `mod` declaration in
`main.rs`, so it only compiles in the feature'd test build):

```rust
//! Phase D egress-transport spike: prove that `matrix_sdk`'s HTTP client routes
//! through our egress sidecar via the loopback-TCP↔UDS `ProxyBridge`. Hermetic —
//! no homeserver, no real sidecar binary, no PG. A stub UDS "proxy" records the
//! CONNECT request line; the assertion is that matrix-sdk's first network call
//! reaches it as `CONNECT <host>:443`.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use crate::bridge::ProxyBridge;

fn uds_path() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/km-spike-{}.sock", std::process::id()))
}

/// Stub UDS proxy: accept one connection, read the first request line, record
/// it, reply `200 Connection established`, then drop (the SDK's TLS handshake to
/// the non-existent origin then fails — irrelevant; we only assert the CONNECT).
async fn spawn_stub_proxy(listener: UnixListener, seen: Arc<Mutex<Vec<String>>>) {
    if let Ok((mut s, _)) = listener.accept().await {
        let mut buf = [0u8; 256];
        if let Ok(n) = s.read(&mut buf).await {
            let line = String::from_utf8_lossy(&buf[..n])
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            seen.lock().unwrap().push(line);
        }
        let _ = s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await;
    }
}

#[tokio::test]
async fn matrix_sdk_routes_first_request_through_the_bridge() {
    use matrix_sdk::Client;

    let path = uds_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind stub uds");
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let stub = tokio::spawn(spawn_stub_proxy(listener, seen.clone()));

    let bridge = ProxyBridge::bind(path.clone()).await.expect("bind bridge");
    let proxy_url = format!("http://{}", bridge.proxy_addr());

    // A SQLite store dir for the (encrypted) state store.
    let store = std::path::PathBuf::from(format!("/tmp/km-spike-store-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&store);

    // Build a client pointed at a fake homeserver, routed through the bridge.
    let client = Client::builder()
        .homeserver_url("https://fake-homeserver.invalid")
        .sqlite_store(&store, None)
        .proxy(proxy_url)
        .build()
        .await
        .expect("client builds");

    // Trigger the first network call. It will error (no real origin), but the
    // stub records the CONNECT first. `whoami` hits the homeserver; if the
    // resolved matrix-sdk version names this differently, use any first network
    // call (e.g. `client.server_versions()` or a login attempt) — the assertion
    // is on the CONNECT, not on this call's result.
    let _ = client.whoami().await;

    // Give the stub a moment to record, then assert.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stub).await;
    let lines = seen.lock().unwrap().clone();

    let saw_connect = lines
        .iter()
        .any(|l| l.starts_with("CONNECT") && l.contains("fake-homeserver.invalid"));
    assert!(saw_connect, "expected a CONNECT to the homeserver via the bridge; saw: {lines:?}");

    drop(bridge);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&store);
}
```

- [ ] **Step 2: Run the spike test to verify it fails for the right reason.**

```bash
cargo test -p kastellan-worker-matrix --features live-matrix egress_spike -- --nocapture
```

Expected: the test compiles and runs but **fails the assertion** *only if* the
transport is wrong. If matrix-sdk's `sqlite_store`/`whoami`/`proxy` method names
differ in the resolved version, you'll get a **compile error** first — fix the
method name against the resolved version's docs (the design fork is unchanged;
this is a naming adjustment), then re-run. The goal of this step is to see the
test exercise the real SDK → bridge → stub path.

- [ ] **Step 3: Make it pass.**

The implementation is the `ProxyBridge` (Task 3) + the correct SDK builder
calls. If the assertion fails (no CONNECT seen), debug the transport: confirm
`.proxy(http://127.0.0.1:<port>)` is set, the bridge is bound to that port, and
the stub UDS path matches. If a compile error, correct the matrix-sdk method
names (`sqlite_store` vs `store_config`, `whoami` vs `server_versions`, etc.)
against the resolved version. No production code changes should be needed beyond
Task 3 — this task is the test plus any SDK-API-naming corrections.

- [ ] **Step 4: Run the spike test to verify it passes.**

```bash
cargo test -p kastellan-worker-matrix --features live-matrix egress_spike -- --nocapture
```

Expected: PASS — "expected a CONNECT" assertion holds; the stub saw
`CONNECT fake-homeserver.invalid:443 HTTP/1.1`.

- [ ] **Step 5: Full feature'd test + clippy pass on the matrix crate.**

```bash
cargo test -p kastellan-worker-matrix --features live-matrix -- --nocapture
cargo clippy -p kastellan-worker-matrix --all-targets --features live-matrix -- -D warnings
```

Expected: PASS + clean (bridge tests + the spike test all green under the feature).

- [ ] **Step 6: Commit.**

```bash
git add workers/matrix/src/egress_spike.rs
git commit -m "test(matrix-worker): egress-transport spike — matrix-sdk routes via ProxyBridge

Hermetic proof (no homeserver) that matrix-sdk's first HTTPS request traverses
the loopback-TCP to UDS bridge as a CONNECT to the homeserver. Confirms the
transparent-tunnel egress transport before the live sync loop is written.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Record the outcome — spec decision + HANDOVER + ROADMAP

**Files:**
- Modify: `docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md`
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Interfaces:**
- Consumes: the green results from Tasks 1–4.
- Produces: the recorded go-decision the next slice (live `LiveSdk` integration) reads cold.

- [ ] **Step 1: Append the spike outcome to the spec.**

Add a `## Spike outcome (2026-06-19)` section: matrix-sdk version + feature set
resolved; "transport CONFIRMED — `.proxy()` + `ProxyBridge` carries matrix-sdk's
HTTPS as CONNECT over the sidecar UDS; transparent-tunnel (no CA injection)";
and the exact SDK builder method names used (so the next slice's `LiveSdk`
reuses them).

- [ ] **Step 2: Update HANDOVER.md** per the doc's own "How to update" checklist:
  bump `Last updated`, add a top "Recently completed (this session)" block
  describing the spike slice (files, the transport decision, what's deferred to
  the live slice), reconcile the stale "#310 on branch" header note to
  "merged to main as `83bf95e`" and add #310 to the condensed "Recently merged"
  list, and rewrite "Next TODO" to point at the **live `LiveSdk` integration
  slice** (plan Task 8 Steps 2–5: `sdk_live.rs`, restore `main.rs` serving +
  `disable_mitm`-by-worker-name wiring, `matrix_live_e2e.rs` `#[ignore]` on the
  DGX). Also correct the stale "matrix/matrix-wire not yet folded into this tree"
  caveat (they are in the tree).

- [ ] **Step 3: Tick ROADMAP.md** — mark the Matrix Phase-D egress-transport spike
  done with the branch/date; note the live integration remains.

- [ ] **Step 4: Verify the default-feature workspace gate is green.**

```bash
cargo test -p kastellan-worker-matrix
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: PASS + clean (default features; `live-matrix` off — the heavy SDK is
not in this surface).

- [ ] **Step 5: Commit.**

```bash
git add docs/superpowers/specs/2026-06-19-matrix-phase-d-egress-transport-spike-design.md \
        docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "docs(matrix): record egress-transport spike outcome + handover/roadmap

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-review notes (for the implementer)

- **Spec coverage:** Task 1 = dep landing (spec deliverable 1) + the feature-build-compiles requirement; Task 2 = AGPL gate (deliverable 2); Task 3 = `ProxyBridge` (deliverable 3); Task 4 = the hermetic spike test (deliverable 4); Task 5 = recorded outcome (deliverable 5). The deferred items (`sdk_live.rs`, live wiring, `disable_mitm` wiring, live e2e) are explicitly NOT tasks here — they are the next slice.
- **The one known API risk** (matrix-sdk builder/method names: `sqlite_store`, `proxy`, `whoami`) is handled by Task 4 Step 2/3 as a naming adjustment against the resolved version — the design (bridge + `.proxy()` + assert-on-CONNECT) does not change.
- **Verification is macOS-only this slice.** The DGX live round-trip is the next slice; do not attempt it here.
