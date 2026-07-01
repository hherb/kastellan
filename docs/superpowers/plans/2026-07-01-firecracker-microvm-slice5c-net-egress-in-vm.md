# Firecracker micro-VM slice 5c — network egress in a VM (long-lived, transparent-tunnel) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a long-lived `Net::Allowlist` worker in a Firecracker VM doing its own end-to-end TLS through a transparent-tunnel (no-MITM) egress sidecar that respawns 1:1 with the VM.

**Architecture:** Compose the existing `PersistentWorker` supervisor (5b-1), the egress sidecar (`spawn_sidecar` + `EgressSidecar`, 4a/4b), and the vsock reverse-channel (4a). The novel surface is a net-aware `PersistentTransport` that bundles the protocol `Client` **and** the `EgressSidecar` (so `PersistentWorker`'s existing off-thread drop tears down both on respawn), a factory that spawns the sidecar in `disable_mitm` mode + rewrites the worker policy with no CA, a `mitm: bool` on `rewrite_worker_policy`, a public webpki+extra-CA CONNECT transport in `web-common`, and a minimal long-lived `net-demo` worker + rootfs.

**Tech Stack:** Rust (workspace, edition 2021, rustc 1.96); `kastellan-worker-prelude` (serve_stdio + in-process lockdown), `kastellan-worker-web-common` (CONNECT-over-UDS + rustls), `kastellan-protocol` (JSON-RPC), Firecracker/bwrap/Seatbelt sandbox backends, `webpki-roots`/`tokio-rustls`.

## Global Constraints

- **AGPL-compatible deps only** (Apache-2.0/MIT/BSD/MPL/LGPL/(A)GPL). No new non-compatible dep. This plan adds **no new third-party crate** — it reuses `web-common`'s existing `rustls`/`webpki-roots`/`hyper`/`url` stack.
- **Cross-platform first-class.** No Linux-only code without a macOS counterpart of equivalent guarantee. VM-specific code is `#[cfg(target_os = "linux")]`; every reusable abstraction compiles and unit-tests on both OSes.
- **Rust core, Python only inside workers.** `net-demo` is Rust (a thin execve/serve_stdio worker) — no Python.
- **Every worker sandboxed before it runs.** No unsandboxed spawn. The sidecar is spawned **sidecar-first fail-closed** (no sidecar ⇒ no worker).
- **Additive & byte-identical.** The `None` / MITM (`mitm=true`, `disable_mitm=false`) paths stay byte-identical; every existing caller is unchanged.
- **Files under 500 LOC where feasible.** `net_worker.rs` is already near cap — the net transport goes in a **new** `egress/persistent_net.rs`, not into `net_worker.rs`.
- **TDD; all tests pass before commit.** RED→GREEN→commit each task.
- **Build env:** `source "$HOME/.cargo/env"` before every cargo command. Dev/CI rustc is **1.96**.
- **Linux-cfg verification on the Mac:** `core` cannot cross-`cargo test` for Linux (`ring` C-dep). Verify `#[cfg(target_os="linux")]` touchpoints on the Mac with cross-clippy `cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings`; run `cargo test` + the VM e2e on the DGX.
- **Commit trailer** on every commit: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- **Branch:** all work on `feat/microvm-slice5c-net-egress-in-vm` (already created, off `main`).

## File Structure

- **Create** `workers/net-demo/Cargo.toml` + `workers/net-demo/src/main.rs` — the long-lived `Net::Allowlist` demonstrator (`net.tls_probe` / `net.stats` / `net.crash`). Depends on `web-common` for the CONNECT-over-UDS + TLS probe.
- **Modify** `Cargo.toml` (workspace `members` += `workers/net-demo`).
- **Modify** `workers/web-common/src/proxy_connect.rs` — add `ProxyConnectGet::with_extra_ca` (webpki roots + optional extra CA, fail-closed).
- **Modify** `workers/web-common/src/http.rs` — add public `make_transparent_get(ua, uds, extra_ca)` factory returning `Box<dyn HttpGet>`.
- **Modify** `core/src/egress/net_worker.rs` — `rewrite_worker_policy` gains `mitm: bool` (CA injected only when `true`); expose `pub(crate) EgressSidecar::from_parts` + `pub(crate) spawn_ingest_thread` + `pub(crate) CA_FILE_NAME` re-use; thread `mitm = !disable_mitm` in `spawn_net_worker`.
- **Create** `core/src/egress/persistent_net.rs` — `NetClientTransport` (bundles `Client` + `EgressSidecar`, `Drop` reaps both), pure `forced_transparent_policy(base, uds) -> SandboxPolicy`, and `spawn_net_transport(params) -> Result<NetClientTransport>`.
- **Modify** `core/src/egress/mod.rs` — `pub mod persistent_net;`.
- **Create** `scripts/workers/microvm/build-net-demo-rootfs.sh` — the VM rootfs (binary + ldd closure + init + `/run`; **no** OS CA bundle — webpki roots are compiled in).
- **Create** `core/tests/net_demo_egress_e2e.rs` — hermetic cross-platform (Seatbelt/bwrap) always-on full-TLS + respawn.
- **Create** `core/tests/net_demo_firecracker_egress_e2e.rs` — DGX real-KVM `#[ignore]`.
- **Modify** `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — tick 5c, frame 5b-4.

---

### Task 1: `net-demo` worker crate skeleton (`net.stats` / `net.crash`)

**Files:**
- Create: `workers/net-demo/Cargo.toml`
- Create: `workers/net-demo/src/main.rs`
- Modify: `Cargo.toml` (workspace members)
- Test: inline `#[cfg(test)] mod tests` in `workers/net-demo/src/main.rs`

**Interfaces:**
- Consumes: `kastellan_worker_prelude::serve_stdio`, `kastellan_protocol::{codes, server::Handler, RpcError}` (same as kv-demo).
- Produces: a `kastellan-worker-net-demo` binary with a `NetHandler` serving `net.stats` (`{calls_served, pid}`), `net.crash` (debug-only `process::exit(1)`), and a stub `net.tls_probe` (filled in Task 3). `NetHandler::new(uds: Option<PathBuf>, extra_ca: Option<PathBuf>)`.

- [ ] **Step 1: Add the crate to the workspace members**

Modify `Cargo.toml`, adding the member after `"workers/kv-demo",`:

```toml
    "workers/kv-demo",
    "workers/net-demo",
    "workers/microvm-run",
```

- [ ] **Step 2: Write the crate manifest**

Create `workers/net-demo/Cargo.toml`:

```toml
[package]
name        = "kastellan-worker-net-demo"
description = "Demo long-lived Net::Allowlist worker: does its own end-to-end TLS through a transparent-tunnel egress sidecar. Exercises network egress in a persistent VM (slice 5c)."
version.workspace      = true
edition.workspace      = true
rust-version.workspace = true
license.workspace      = true
authors.workspace      = true
repository.workspace   = true
readme      = "../../README.md"

[[bin]]
name = "kastellan-worker-net-demo"
path = "src/main.rs"

[dependencies]
kastellan-protocol         = { path = "../../protocol", version = "0.1.0" }
kastellan-worker-prelude   = { path = "../prelude", version = "0.1.0" }
kastellan-worker-web-common = { path = "../web-common", version = "0.1.0" }
serde                    = { workspace = true }
serde_json               = { workspace = true }
anyhow                   = { workspace = true }
url                      = { workspace = true }
```

- [ ] **Step 3: Write the failing test for `net.stats` and unknown-method**

Create `workers/net-demo/src/main.rs` with the handler skeleton + tests (the `tls_probe` arm returns a NOT-yet-implemented placeholder that Task 3 replaces):

```rust
//! net-demo: a minimal LONG-LIVED `Net::Allowlist` worker that does its OWN
//! end-to-end TLS to an origin through the per-worker egress proxy's UDS
//! (transparent tunnel — the proxy never terminates the TLS). It exists to
//! exercise slice 5c: network egress inside a persistent VM. `net.stats` proves
//! many-calls-one-boot; `net.tls_probe` proves the transparent-tunnel TLS path.
//!
//! Env: `KASTELLAN_EGRESS_PROXY_UDS` (the proxy socket the worker dials) and the
//! optional test-only `KASTELLAN_NETDEMO_EXTRA_CA` (a self-signed loopback
//! origin's cert, added on top of the compiled-in webpki roots for hermetic e2e).
use std::path::PathBuf;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::serve_stdio;
use serde::Deserialize;

#[derive(Deserialize)]
struct ProbeParams {
    host: String,
    #[serde(default)]
    port: Option<u16>,
}

struct NetHandler {
    uds: Option<PathBuf>,
    extra_ca: Option<PathBuf>,
    calls_served: u64,
}

impl NetHandler {
    fn new(uds: Option<PathBuf>, extra_ca: Option<PathBuf>) -> Self {
        Self { uds, extra_ca, calls_served: 0 }
    }
}

impl Handler for NetHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        self.calls_served += 1;
        match method {
            "net.tls_probe" => {
                let _p: ProbeParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                // Filled in Task 3.
                Err(RpcError::new(codes::OPERATION_FAILED, "net.tls_probe not yet implemented".into()))
            }
            "net.stats" => Ok(serde_json::json!({
                "calls_served": self.calls_served,
                "pid": std::process::id(),
            })),
            // net.crash: deterministic worker-death trigger for lifecycle e2e.
            // Exits without replying so the caller sees an I/O error, which
            // PersistentWorker treats as a death and respawns. Debug-only.
            #[cfg(debug_assertions)]
            "net.crash" => std::process::exit(1),
            other => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {other}"))),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS").ok().map(PathBuf::from);
    let extra_ca = std::env::var("KASTELLAN_NETDEMO_EXTRA_CA").ok().map(PathBuf::from);
    let mut handler = NetHandler::new(uds, extra_ca);
    serve_stdio(&mut handler)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_counts_calls_and_reports_pid() {
        let mut h = NetHandler::new(None, None);
        let s1 = h.call("net.stats", serde_json::json!({})).unwrap();
        assert_eq!(s1["calls_served"], 1);
        assert_eq!(s1["pid"].as_u64(), Some(std::process::id() as u64));
        let s2 = h.call("net.stats", serde_json::json!({})).unwrap();
        assert_eq!(s2["calls_served"], 2);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = NetHandler::new(None, None);
        let err = h.call("net.nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn tls_probe_rejects_bad_params() {
        let mut h = NetHandler::new(None, None);
        // Missing required `host`.
        let err = h.call("net.tls_probe", serde_json::json!({"port": 443})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass (they define behavior that exists)**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-net-demo`
Expected: 3 tests PASS. (If `RpcError` has no public `code` field, use the accessor the other workers use — check `kv-demo`/`shell-exec` tests; kv-demo asserts on `err`'s code via `RpcError::new(codes::…)` round-trips, so mirror whatever field/method `protocol::RpcError` exposes.)

- [ ] **Step 5: Verify the whole workspace still builds**

Run: `source "$HOME/.cargo/env" && cargo build --workspace`
Expected: success (new crate compiles, no other crate affected).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml workers/net-demo/Cargo.toml workers/net-demo/src/main.rs
git commit -m "$(printf 'feat(net-demo): long-lived Net::Allowlist demo worker skeleton (net.stats/net.crash)\n\nSlice 5c demonstrator crate mirroring kv-demo; net.tls_probe stubbed (Task 3).\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 2: `web-common` webpki+extra-CA CONNECT transport

**Files:**
- Modify: `workers/web-common/src/proxy_connect.rs` (add `with_extra_ca`)
- Modify: `workers/web-common/src/http.rs` (add public `make_transparent_get`)
- Test: inline tests in both files

**Interfaces:**
- Consumes: existing `ProxyConnectGet` internals (`rt`, `tls`, `uds`, `user_agent`).
- Produces:
  - `ProxyConnectGet::with_extra_ca(user_agent: &str, uds: PathBuf, extra_ca: Option<PathBuf>) -> anyhow::Result<Self>` — webpki roots **plus** the optional extra CA (fail-closed on unreadable/invalid).
  - `http::make_transparent_get(user_agent: &str, uds: &std::path::Path, extra_ca: Option<&std::path::Path>) -> anyhow::Result<Box<dyn HttpGet>>` — public factory the `net-demo` worker calls.

- [ ] **Step 1: Write the failing tests in `proxy_connect.rs`**

Add to the `#[cfg(test)] mod tests` in `workers/web-common/src/proxy_connect.rs`:

```rust
    #[test]
    fn with_extra_ca_none_is_webpki_and_ok() {
        // No extra CA → webpki roots only, infallible.
        let g = ProxyConnectGet::with_extra_ca(
            "kastellan-test/0", PathBuf::from("/tmp/x.sock"), None,
        );
        assert!(g.is_ok());
    }

    #[test]
    fn with_extra_ca_unreadable_fails_closed() {
        // A set-but-unreadable extra CA must fail closed (never silently drop it).
        let g = ProxyConnectGet::with_extra_ca(
            "kastellan-test/0",
            PathBuf::from("/tmp/x.sock"),
            Some(PathBuf::from("/nonexistent/extra-ca.pem")),
        );
        assert!(g.is_err(), "set-but-unreadable extra CA must fail closed");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common with_extra_ca`
Expected: FAIL — `no function ... with_extra_ca`.

- [ ] **Step 3: Implement `with_extra_ca`**

In `workers/web-common/src/proxy_connect.rs`, add this method to `impl ProxyConnectGet` (after `with_trust`). It builds webpki roots and, when present, adds the extra CA — fail-closed:

```rust
    /// Build the transport trusting the compiled-in **webpki public roots**
    /// plus, when `extra_ca` is `Some`, an additional CA (a self-signed test
    /// origin for hermetic e2e). Unlike [`with_trust`]'s `Some` branch, this does
    /// NOT drop the public roots — the worker validates real origins normally and
    /// *also* trusts the extra CA. A set-but-unreadable/invalid `extra_ca` is an
    /// error (fail closed; never silently ignore it). Used by transparent-tunnel
    /// workers (slice 5c) that do their own end-to-end TLS and cannot trust the
    /// proxy's per-instance MITM CA.
    pub fn with_extra_ca(
        user_agent: &str,
        uds: PathBuf,
        extra_ca: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");

        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = extra_ca {
            let pem = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("read extra CA {path:?}: {e}"))?;
            let mut added = 0usize;
            for der in CertificateDer::pem_slice_iter(&pem) {
                let der = der.map_err(|e| anyhow::anyhow!("parse extra CA {path:?}: {e}"))?;
                root_store
                    .add(der)
                    .map_err(|e| anyhow::anyhow!("add extra CA {path:?}: {e}"))?;
                added += 1;
            }
            if added == 0 {
                anyhow::bail!("extra CA {path:?} contained no certificates");
            }
        }
        let tls = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        );
        Ok(Self { user_agent: user_agent.to_string(), uds, tls, rt })
    }
```

- [ ] **Step 4: Run to verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common with_extra_ca`
Expected: 2 tests PASS.

- [ ] **Step 5: Write the failing test for the `http::make_transparent_get` factory**

Add to `#[cfg(test)] mod tests` in `workers/web-common/src/http.rs`:

```rust
    #[test]
    fn make_transparent_get_builds_a_transport() {
        let g = super::make_transparent_get(
            "kastellan-test/0",
            std::path::Path::new("/tmp/egress.sock"),
            None,
        );
        assert!(g.is_ok());
        assert_eq!(g.unwrap().transport_kind(), "proxy-connect");
    }
```

- [ ] **Step 6: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common make_transparent_get`
Expected: FAIL — `no function ... make_transparent_get`.

- [ ] **Step 7: Implement the factory in `http.rs`**

Add near `make_get` in `workers/web-common/src/http.rs`:

```rust
/// Build a transparent-tunnel CONNECT transport: reach origins ONLY via the
/// egress-proxy `uds`, validating them against the compiled-in webpki roots plus
/// an optional `extra_ca`. For workers that do their own end-to-end TLS (the
/// proxy tunnels ciphertext and cannot MITM them) — slice 5c. `extra_ca` is a
/// test-only self-signed origin cert; production callers pass `None`.
pub fn make_transparent_get(
    user_agent: &str,
    uds: &std::path::Path,
    extra_ca: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn HttpGet>> {
    let t = crate::proxy_connect::ProxyConnectGet::with_extra_ca(
        user_agent,
        uds.to_path_buf(),
        extra_ca.map(|p| p.to_path_buf()),
    )?;
    Ok(Box::new(t))
}
```

- [ ] **Step 8: Run the factory test + the whole crate + clippy**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-web-common && cargo clippy -p kastellan-worker-web-common --all-targets -D warnings`
Expected: all PASS, clippy clean.

- [ ] **Step 9: Commit**

```bash
git add workers/web-common/src/proxy_connect.rs workers/web-common/src/http.rs
git commit -m "$(printf 'feat(web-common): webpki+extra-CA transparent CONNECT transport (make_transparent_get)\n\nProxyConnectGet::with_extra_ca trusts webpki roots + an optional extra CA\n(fail-closed); public make_transparent_get factory for slice-5c transparent-tunnel\nworkers. MITM-CA-only path (with_trust) unchanged.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 3: `net-demo` `net.tls_probe` (hermetic in-process proof)

**Files:**
- Modify: `workers/net-demo/src/main.rs` (implement `net.tls_probe` + a hermetic test)
- Test: inline `mod tests`

**Interfaces:**
- Consumes: `kastellan_worker_web_common::http::make_transparent_get`, `kastellan_worker_web_common::http::HttpGet`, `url::Url`.
- Produces: `net.tls_probe {host, port=443}` → `{ok: bool, status: Option<u16>, error: Option<String>}` where `ok` is true iff the CONNECT + end-to-end TLS + GET `/` succeeded.

- [ ] **Step 1: Add a pure result-shaping helper + failing test**

In `workers/net-demo/src/main.rs`, add a pure helper above `impl Handler` (keeps the probe logic testable without a live proxy) and a test. First the test, in `mod tests`:

```rust
    #[test]
    fn probe_result_shape_ok_and_err() {
        use kastellan_worker_web_common::http::RawResponse;
        let ok = probe_result(Ok(RawResponse {
            status: 204, location: None, content_type: String::new(), body: vec![],
        }));
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["status"], 204);

        let err = probe_result(Err("connect proxy uds: nope".to_string()));
        assert_eq!(err["ok"], false);
        assert!(err["error"].as_str().unwrap().contains("nope"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-net-demo probe_result_shape`
Expected: FAIL — `cannot find function probe_result`.

- [ ] **Step 3: Implement `probe_result` + wire `net.tls_probe`**

Add the pure helper (above `impl Handler`):

```rust
/// Shape a probe outcome into the JSON result the caller sees. A transport error
/// is a *probe result* (`ok:false`), NOT an RPC error — the caller wants to know
/// the origin was unreachable, not that the worker malfunctioned.
fn probe_result(
    outcome: Result<kastellan_worker_web_common::http::RawResponse, String>,
) -> serde_json::Value {
    match outcome {
        Ok(resp) => serde_json::json!({ "ok": true, "status": resp.status, "error": null }),
        Err(e) => serde_json::json!({ "ok": false, "status": null, "error": e }),
    }
}
```

Replace the `"net.tls_probe"` arm body (the Task-1 stub) with:

```rust
            "net.tls_probe" => {
                let p: ProbeParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let uds = self.uds.as_ref().ok_or_else(|| {
                    RpcError::new(codes::OPERATION_FAILED, "KASTELLAN_EGRESS_PROXY_UDS not set".into())
                })?;
                let port = p.port.unwrap_or(443);
                let url = url::Url::parse(&format!("https://{}:{}/", p.host, port))
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad host: {e}")))?;
                let get = kastellan_worker_web_common::http::make_transparent_get(
                    "kastellan-net-demo/0", uds, self.extra_ca.as_deref(),
                )
                .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("build transport: {e}")))?;
                Ok(probe_result(get.get(&url)))
            }
```

- [ ] **Step 4: Add the hermetic end-to-end probe test (loopback stub proxy + loopback TLS origin)**

This is the crucial in-process proof that the transparent-tunnel TLS path works. Add a test helper + test to `mod tests`. It stands up a raw `UnixListener` "proxy" that answers `200` to CONNECT and then **splices** the client to a loopback TLS origin — proving the worker completes an end-to-end TLS handshake through an opaque tunnel and validates the origin against `extra_ca`.

```rust
    // A self-contained hermetic proof: an in-process "transparent-tunnel proxy"
    // (UDS: reads CONNECT, replies 200, then blindly pipes bytes) bridged to a
    // loopback rustls origin. The worker's make_transparent_get(...).get() must
    // complete TLS against the origin's self-signed cert (trusted via extra_ca)
    // and return ok:true. Requires a tiny TLS origin — reuse rcgen+rustls from
    // web-common's dev-deps by adding them to net-demo [dev-dependencies].
    //
    // NOTE FOR IMPLEMENTER: put the rustls-server + rcgen self-signed origin and
    // the CONNECT-splicing UDS proxy in a `mod probe_harness` here. The origin
    // binds 127.0.0.1:0, writes its cert PEM to a temp file (the extra_ca), and
    // replies to any request with `HTTP/1.1 204 No Content\r\n\r\n`. Assert:
    //   let result = NetHandler::new(Some(uds), Some(ca)).call("net.tls_probe",
    //       json!({"host":"127.0.0.1","port":origin_port}))?;
    //   assert_eq!(result["ok"], true);
    //   assert_eq!(result["status"], 204);
    // And a negative: extra_ca=None (webpki only) → ok:false (self-signed
    // untrusted). This pins that the worker really validates the chain.
```

Because this harness is ~120 lines of async rustls/rcgen glue, implement it faithfully (do NOT leave the comment as the test). Add to `workers/net-demo/Cargo.toml`:

```toml
[dev-dependencies]
rustls        = { workspace = true }
tokio-rustls  = { workspace = true }
tokio         = { workspace = true, features = ["rt", "net", "io-util", "macros"] }
rcgen         = { workspace = true }
rustls-pki-types = { workspace = true }
```

(Confirm these versions/features are the ones `web-common` already uses — copy them verbatim from `workers/web-common/Cargo.toml` so the workspace lock is unchanged. If `rcgen`/`rustls-pki-types` are not already workspace deps, add them to `[workspace.dependencies]` in the root `Cargo.toml` at the version `egress-proxy` uses, since it already self-signs a CA.)

- [ ] **Step 5: Run the probe tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-worker-net-demo`
Expected: all PASS (`probe_result_shape`, the hermetic loopback-TLS positive, the untrusted-self-signed negative).

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-worker-net-demo --all-targets -D warnings`
Expected: clean.

```bash
git add workers/net-demo/Cargo.toml workers/net-demo/src/main.rs
git commit -m "$(printf 'feat(net-demo): net.tls_probe — end-to-end TLS through a transparent tunnel\n\nProbes an origin via KASTELLAN_EGRESS_PROXY_UDS using web-common make_transparent_get\n(webpki + optional test CA). Hermetic in-process proof: CONNECT-splicing UDS proxy\nbridged to a loopback rustls origin; validates cert via extra_ca.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 4: `rewrite_worker_policy` gains `mitm: bool` (no CA in transparent mode)

**Files:**
- Modify: `core/src/egress/net_worker.rs`
- Test: inline `#[cfg(test)] mod tests` in the same file

**Interfaces:**
- Consumes: existing `rewrite_worker_policy(policy, uds, ca)` and its single caller `spawn_net_worker`.
- Produces: `rewrite_worker_policy(policy, uds, ca, mitm: bool)` — when `mitm == false`, the per-instance CA is **not** added to `fs_read` and `KASTELLAN_EGRESS_PROXY_CA` is **not** injected (only `proxy_uds` + the `/etc/resolv.conf` drop + `KASTELLAN_EGRESS_PROXY_UDS`). `spawn_net_worker` passes `mitm = !params.disable_mitm`.

- [ ] **Step 1: Write the failing transparent-mode test**

Add to `mod tests` in `core/src/egress/net_worker.rs`:

```rust
    #[test]
    fn rewrite_worker_policy_transparent_injects_no_ca() {
        let base = SandboxPolicy {
            net: Net::Allowlist(vec!["origin.example.com:443".into()]),
            fs_read: vec!["/etc/resolv.conf".into(), "/bin/worker".into()],
            env: vec![],
            ..SandboxPolicy::default()
        };
        let uds = std::path::PathBuf::from("/scratch/egress.sock");
        let ca = std::path::PathBuf::from("/scratch/ca.pem");
        // mitm = false → transparent tunnel: proxy_uds set, NO CA anywhere.
        let out = rewrite_worker_policy(base, &uds, &ca, false);
        assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
        assert!(!out.fs_read.contains(&ca), "no CA in fs_read in transparent mode");
        assert!(
            !out.env.iter().any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_CA"),
            "no CA env in transparent mode"
        );
        // UDS still injected; resolv.conf still dropped; worker bin preserved.
        assert!(out.env.iter().any(|(k, v)| k == ENV_UDS && v == "/scratch/egress.sock"));
        assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
        assert!(out.fs_read.contains(&"/bin/worker".into()));
    }
```

Update the two existing rewrite tests (`rewrite_worker_policy_forces_routing`, `rewrite_worker_policy_injects_ca_trust`, `rewrite_overwrites_stale_uds_env`) to pass `true` as the new 4th arg — the MITM path is unchanged.

- [ ] **Step 2: Run to verify failure**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::net_worker::tests::rewrite_worker_policy_transparent`
Expected: FAIL — arity mismatch (`rewrite_worker_policy` takes 3 args).

- [ ] **Step 3: Add the `mitm` parameter**

In `rewrite_worker_policy`, change the signature and gate the CA block:

```rust
pub fn rewrite_worker_policy(
    mut policy: SandboxPolicy,
    uds: &Path,
    ca: &Path,
    mitm: bool,
) -> SandboxPolicy {
    policy.proxy_uds = Some(uds.to_path_buf());
    policy.fs_read.retain(|p| p != Path::new("/etc/resolv.conf"));
    // MITM mode only: make the per-instance CA readable in-jail + announce it.
    // A transparent-tunnel worker (mitm=false) validates origins with its own
    // roots and must NOT receive our CA (it never terminates its TLS).
    policy.env.retain(|(k, _)| k != ENV_UDS && k != ENV_CA);
    if mitm {
        if !policy.fs_read.iter().any(|p| p == ca) {
            policy.fs_read.push(ca.to_path_buf());
        }
    }
    policy
        .env
        .push((ENV_UDS.to_string(), uds.to_string_lossy().into_owned()));
    if mitm {
        policy
            .env
            .push((ENV_CA.to_string(), ca.to_string_lossy().into_owned()));
    }
    policy
}
```

- [ ] **Step 4: Thread `mitm` through `spawn_net_worker`**

In `spawn_net_worker`, update the `rewrite_worker_policy` call (currently line ~204):

```rust
    let forced = rewrite_worker_policy(params.spec.policy.clone(), &uds, &ca, !params.disable_mitm);
```

- [ ] **Step 5: Run the net_worker unit tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::net_worker`
Expected: all PASS (the transparent test + the updated MITM tests).

- [ ] **Step 6: Clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -D warnings`
Expected: clean.

```bash
git add core/src/egress/net_worker.rs
git commit -m "$(printf 'feat(egress): rewrite_worker_policy gains mitm flag — no CA in transparent-tunnel mode\n\nA transparent-tunnel worker (disable_mitm) validates origins with its own roots\nand must not receive the per-instance proxy CA. mitm=true (every existing caller)\nis byte-identical. spawn_net_worker derives mitm = !disable_mitm.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 5: `NetClientTransport` + `spawn_net_transport` (egress/persistent_net.rs)

**Files:**
- Create: `core/src/egress/persistent_net.rs`
- Modify: `core/src/egress/mod.rs` (`pub mod persistent_net;`)
- Modify: `core/src/egress/net_worker.rs` (expose `pub(crate) fn EgressSidecar::from_parts`, `pub(crate) fn spawn_ingest_thread`)
- Test: inline `#[cfg(test)] mod tests` in `persistent_net.rs`

**Interfaces:**
- Consumes: `spawn::{spawn_sidecar, CA_FILE_NAME, UDS_FILE_NAME}`, `net_worker::{rewrite_worker_policy, EgressSidecar, spawn_ingest_thread, NetWorkerSpawn}`, `worker_lifecycle::persistent::{PersistentTransport, ClientTransport}`, `worker_lifecycle::ClientTransport::spawn`.
- Produces:
  - `NetClientTransport` implementing `PersistentTransport`, owning a `ClientTransport` + an `EgressSidecar`; `Drop` reaps both (VMM/worker child via `ClientTransport::drop`, sidecar+scratch via `EgressSidecar::drop`).
  - pure `forced_transparent_policy(base: SandboxPolicy, uds: &Path) -> SandboxPolicy` (delegates to `rewrite_worker_policy(.., mitm=false)` with a placeholder CA path — never used in transparent mode).
  - `spawn_net_transport(params: &NetTransportSpawn) -> anyhow::Result<NetClientTransport>` — sidecar-first fail-closed, transparent-tunnel, returns the bundle.
  - `NetTransportSpawn<'a>` params struct: `{ backend, proxy_bin, program, args, base_policy, allowlist, worker_name, extra_ca: Option<&Path> }`.

- [ ] **Step 1: Expose the two `net_worker` helpers as `pub(crate)`**

In `core/src/egress/net_worker.rs`:
- Add a constructor to `impl EgressSidecar`:

```rust
    /// Build a bundle from already-spawned parts. Used by
    /// [`super::persistent_net::spawn_net_transport`], which spawns the sidecar +
    /// worker itself (it needs the raw `Client`, not a `SupervisedWorker`) and
    /// owns the scratch dir for RAII cleanup.
    pub(crate) fn from_parts(
        sidecar: SidecarHandle,
        ingest: JoinHandle<()>,
        scratch: Option<PathBuf>,
    ) -> Self {
        Self { sidecar, _ingest: ingest, scratch }
    }
```

- Change `fn spawn_ingest_thread` to `pub(crate) fn spawn_ingest_thread`.

- [ ] **Step 2: Register the new module**

In `core/src/egress/mod.rs`, add after `pub mod net_worker;`:

```rust
pub mod persistent_net;
```

- [ ] **Step 3: Write the failing pure-policy test**

Create `core/src/egress/persistent_net.rs` with the pure helper + test first:

```rust
//! Long-lived net worker transport (slice 5c): bundle a JSON-RPC `Client` over a
//! sandboxed worker together with its transparent-tunnel egress `EgressSidecar`,
//! so `PersistentWorker` respawns both 1:1 (its off-thread drop of the dead
//! transport reaps the old worker AND tears down the old sidecar; the factory
//! then spawns a fresh pair). The sidecar runs in `disable_mitm` mode; the worker
//! does its own end-to-end TLS and receives no CA.

use std::path::Path;

use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

use super::net_worker::{rewrite_worker_policy, spawn_ingest_thread, EgressSidecar};
use super::spawn::{spawn_sidecar, CA_FILE_NAME};
use crate::worker_lifecycle::persistent::{ClientTransport, PersistentTransport};

/// Rewrite `base` for transparent-tunnel force-routing onto `uds`: proxy_uds set,
/// resolv.conf dropped, UDS env injected, and NO CA (transparent tunnel). The
/// `ca` path handed to `rewrite_worker_policy` is a placeholder — `mitm=false`
/// means it is never read or injected.
pub(crate) fn forced_transparent_policy(base: SandboxPolicy, uds: &Path) -> SandboxPolicy {
    let ca_placeholder = uds
        .parent()
        .map(|d| d.join(CA_FILE_NAME))
        .unwrap_or_else(|| std::path::PathBuf::from(CA_FILE_NAME));
    rewrite_worker_policy(base, uds, &ca_placeholder, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_sandbox::Net;

    #[test]
    fn forced_transparent_policy_sets_uds_and_no_ca() {
        let base = SandboxPolicy {
            net: Net::Allowlist(vec!["origin.example.com:443".into()]),
            fs_read: vec!["/etc/resolv.conf".into(), "/bin/net-demo".into()],
            ..SandboxPolicy::default()
        };
        let uds = std::path::PathBuf::from("/scratch/egress-1/egress.sock");
        let out = forced_transparent_policy(base, &uds);
        assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
        assert!(!out.env.iter().any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_CA"));
        assert!(out.env.iter().any(|(k, v)| k == "KASTELLAN_EGRESS_PROXY_UDS"
            && v == "/scratch/egress-1/egress.sock"));
        assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
        assert!(out.fs_read.contains(&"/bin/net-demo".into()));
    }
}
```

- [ ] **Step 4: Run the pure test**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core --lib egress::persistent_net`
Expected: PASS.

- [ ] **Step 5: Add `NetClientTransport` + `NetTransportSpawn` + `spawn_net_transport`**

Append to `core/src/egress/persistent_net.rs`:

```rust
/// Everything `spawn_net_transport` needs. `base_policy` is the worker's policy
/// BEFORE force-routing (its `sandbox_backend`/`Net::Allowlist`/`env` are set by
/// the caller — e.g. `FirecrackerVm` for the DGX path, Seatbelt/bwrap for the
/// hermetic path). `extra_ca` is a test-only origin cert delivered to the worker
/// (added to `fs_read` so the VM RO-share carries it); `None` in production.
pub struct NetTransportSpawn<'a> {
    pub backend: &'a dyn SandboxBackend,
    pub proxy_bin: &'a Path,
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub base_policy: SandboxPolicy,
    pub allowlist: &'a [String],
    pub worker_name: &'a str,
    pub extra_ca: Option<&'a Path>,
}

/// A long-lived net worker + its transparent-tunnel sidecar, driven by
/// `PersistentWorker`. `Drop` reaps BOTH children: `inner` (the worker/VMM child,
/// via `ClientTransport::drop`) then `egress` (the sidecar child + scratch, via
/// `EgressSidecar::drop`). Field declaration order fixes drop order.
pub struct NetClientTransport {
    inner: ClientTransport,
    // Dropped after `inner`. Owns the sidecar + per-worker scratch dir.
    _egress: EgressSidecar,
}

impl PersistentTransport for NetClientTransport {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner.call(method, params)
    }
    fn death_report(&mut self) -> Option<String> {
        self.inner.death_report()
    }
}

/// Spawn a long-lived net worker coupled to a transparent-tunnel egress sidecar.
/// Sidecar-first fail-closed: if the sidecar cannot start, no worker is spawned.
/// The worker's policy is force-routed onto the sidecar UDS with NO CA (the
/// worker does its own end-to-end TLS); when `extra_ca` is set it is appended to
/// `fs_read` so a VM RO-share carries it and the worker can trust a test origin.
/// The caller owns `scratch` (a unique per-worker dir); on the fail-closed path
/// the sidecar's `Drop` removes the UDS but NOT the dir — the caller cleans it.
pub fn spawn_net_transport(
    params: &NetTransportSpawn<'_>,
    scratch: &Path,
) -> anyhow::Result<NetClientTransport> {
    // 1. Sidecar first (transparent tunnel), fail-closed.
    let mut sidecar = spawn_sidecar(
        params.backend,
        params.proxy_bin,
        params.allowlist,
        scratch,
        params.worker_name,
        None,  // no cert pins
        true,  // disable_mitm — transparent tunnel
    )?;
    let stdout = sidecar.stdout();
    let uds = sidecar.uds_path.clone();

    // 2. Force-route the worker policy (transparent, no CA). Append the optional
    //    test CA to fs_read so a VM RO-share delivers it in-guest.
    let mut base = params.base_policy.clone();
    if let Some(ca) = params.extra_ca {
        if !base.fs_read.iter().any(|p| p == ca) {
            base.fs_read.push(ca.to_path_buf());
        }
    }
    let forced = forced_transparent_policy(base, &uds);

    // 3. Spawn the worker + connect the Client (ClientTransport applies the same
    //    lockdown-env derivation every spawn path uses). Fail-closed: if this
    //    errors, `sidecar` drops here and its Drop kills the proxy.
    let inner = ClientTransport::spawn(params.backend, &forced, params.program, params.args)?;

    // 4. Drain the sidecar's decision stdout (no-op sink — the demo doesn't audit
    //    to PG; draining prevents a full-pipe stall past ~64 KiB). Bundle for 1:1
    //    teardown; the caller hands the scratch dir to the bundle for RAII.
    let ingest = spawn_ingest_thread(stdout, |_row| {});
    let egress = EgressSidecar::from_parts(sidecar, ingest, Some(scratch.to_path_buf()));
    Ok(NetClientTransport { inner, _egress: egress })
}
```

- [ ] **Step 6: Run the crate build + clippy (the spawn path is covered by the e2e in Tasks 6/8)**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core && cargo clippy -p kastellan-core --all-targets -D warnings`
Expected: builds, clippy clean. (Runtime behavior — sidecar-first, Drop-reaps-both, respawn — is proven by the hermetic e2e in Task 6, which exercises the real spawn + a real respawn.)

- [ ] **Step 7: Re-export for tests + commit**

In `core/src/egress/mod.rs` (or wherever `egress` re-exports live), ensure `persistent_net::{NetClientTransport, NetTransportSpawn, spawn_net_transport}` are reachable from `core/tests/` — they are `pub` in a `pub mod`, so `kastellan_core::egress::persistent_net::…` works. Verify:

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --tests`
Expected: success.

```bash
git add core/src/egress/persistent_net.rs core/src/egress/mod.rs core/src/egress/net_worker.rs
git commit -m "$(printf 'feat(egress): NetClientTransport + spawn_net_transport — long-lived net worker in a transparent tunnel\n\nBundles the worker Client + its EgressSidecar so PersistentWorker respawns both\n1:1. Sidecar spawned disable_mitm; policy force-routed with no CA; optional test\nCA appended to fs_read for a VM RO-share. Drop reaps both children.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 6: Hermetic cross-platform e2e (Seatbelt/bwrap) — full-TLS + respawn

**Files:**
- Create: `core/tests/net_demo_egress_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_core::worker_lifecycle::{PersistentFactory, PersistentTransport, PersistentWorker}`, `kastellan_core::egress::persistent_net::{NetTransportSpawn, spawn_net_transport}`, `kastellan_sandbox::{Net, Profile, SandboxBackends, SandboxPolicy}`.
- Produces: an always-on (skip-as-pass) test proving transparent-tunnel end-to-end TLS + many-calls-one-boot + `net.crash`→1:1 respawn under the default OS sandbox, with a loopback self-signed TLS origin.

- [ ] **Step 1: Write the test harness + failing test**

Create `core/tests/net_demo_egress_e2e.rs`. Mirror `kv_demo_persistent_e2e.rs`'s structure (binary discovery, sandbox probe, `PersistentWorker` factory, crash→respawn poll), but (a) the factory calls `spawn_net_transport` instead of `ClientTransport::spawn`, and (b) it stands up a **loopback self-signed rustls origin** + writes its cert to `extra_ca`. Key skeleton (implement the rustls origin + cert write faithfully — reuse the same rcgen/rustls the net-demo dev-test used in Task 3, factored into a shared `mod origin` helper in this test file):

```rust
//! Cross-platform hermetic e2e (Seatbelt on macOS, bwrap on Linux) for slice 5c's
//! transparent-tunnel long-lived net worker WITHOUT a VM: a net-demo worker under
//! `PersistentWorker`, force-routed through a real transparent-tunnel egress
//! sidecar to a loopback self-signed TLS origin. Proves end-to-end TLS through the
//! tunnel, many-calls-one-boot, and net.crash → 1:1 sidecar+worker respawn.
//!
//! Skip-as-pass if the net-demo / egress-proxy binaries are not built or the
//! default OS sandbox is unavailable.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;
use std::time::Duration;

use kastellan_core::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use kastellan_core::worker_lifecycle::{PersistentFactory, PersistentTransport, PersistentWorker};
use kastellan_sandbox::{Net, Profile, SandboxBackends, SandboxPolicy};

fn target_bin(name: &str) -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.parent().unwrap().join("target"));
    let bin = target.join("debug").join(name);
    if bin.exists() { Some(bin) } else { None }
}

// mod origin { ... }  // loopback rustls server on 127.0.0.1:0, replies 204;
//                     // returns (port, ca_pem_path). Implement with rcgen+rustls
//                     // exactly as the net-demo Task-3 harness does.

#[test]
fn net_demo_tls_probe_survives_respawn_under_default_backend() {
    let net_demo = match target_bin("kastellan-worker-net-demo") {
        Some(b) => b,
        None => { eprintln!("[SKIP] net-demo not built"); return; }
    };
    let proxy_bin = match target_bin("kastellan-worker-egress-proxy") {
        Some(b) => b,
        None => { eprintln!("[SKIP] egress-proxy not built"); return; }
    };
    #[cfg(target_os = "linux")]
    { use kastellan_sandbox::linux_bwrap::LinuxBwrap;
      if LinuxBwrap::probe().is_err() { eprintln!("[SKIP] bwrap probe failed"); return; } }
    #[cfg(target_os = "macos")]
    { use kastellan_sandbox::macos_seatbelt::MacosSeatbelt;
      if MacosSeatbelt::probe().is_err() { eprintln!("[SKIP] sandbox-exec probe failed"); return; } }

    // Loopback self-signed origin; its cert becomes the worker's extra_ca.
    let (origin_port, ca_path) = origin::spawn_loopback_tls_origin();
    let allow = vec![format!("127.0.0.1:{origin_port}")];

    let backends = SandboxBackends::default_for_current_os();
    let backend = backends.resolve(None, None);
    let scratch_root = std::env::temp_dir().join(format!("kastellan-netdemo-5c-{}", std::process::id()));
    std::fs::create_dir_all(&scratch_root).unwrap();

    let factory: PersistentFactory = {
        let net_demo = net_demo.clone();
        let proxy_bin = proxy_bin.clone();
        let ca_path = ca_path.clone();
        let allow = allow.clone();
        let scratch_root = scratch_root.clone();
        let backend = std::sync::Arc::clone(&backend);
        let bin_dir = net_demo.parent().unwrap().to_path_buf();
        Box::new(move || {
            // Fresh per-worker scratch subdir (unique) each spawn/respawn.
            let scratch = scratch_root.join(format!("w-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch)?;
            let base = SandboxPolicy {
                net: Net::Allowlist(allow.clone()),
                profile: Profile::WorkerNetClient,
                fs_read: vec![bin_dir.clone()],  // loader needs the bin dir
                cpu_ms: 10_000,
                mem_mb: 256,
                ..SandboxPolicy::default()
            };
            let params = NetTransportSpawn {
                backend: &*backend,
                proxy_bin: &proxy_bin,
                program: &net_demo.to_string_lossy(),
                args: &[],
                base_policy: base,
                allowlist: &allow,
                worker_name: "net-demo",
                extra_ca: Some(&ca_path),
            };
            let t = spawn_net_transport(&params, &scratch)?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("net-demo-5c", factory).expect("spawn net-demo persistent worker");

    // Phase 1: end-to-end TLS through the transparent tunnel.
    let probe = h.call("net.tls_probe", serde_json::json!({"host":"127.0.0.1","port":origin_port}))
        .expect("net.tls_probe");
    assert_eq!(probe["ok"], true, "transparent-tunnel TLS must succeed, got {probe}");

    // Many calls on one boot.
    for i in 0..5 {
        h.call("net.stats", serde_json::json!({})).unwrap_or_else(|e| panic!("stats {i}: {e}"));
    }

    // Phase 2: deterministic death → respawn.
    let _ = h.call("net.crash", serde_json::json!({}));
    let mut ok = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(50));
        if let Ok(p) = h.call("net.tls_probe", serde_json::json!({"host":"127.0.0.1","port":origin_port})) {
            if p["ok"] == true { ok = true; break; }
        }
    }
    assert!(ok, "net.tls_probe must succeed again after 1:1 sidecar+worker respawn");

    h.shutdown();
    let _ = std::fs::remove_dir_all(&scratch_root);
}
```

- [ ] **Step 2: Run it (macOS dev box)**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-worker-net-demo -p kastellan-worker-egress-proxy && cargo test -p kastellan-core --test net_demo_egress_e2e -- --nocapture`
Expected: PASS on macOS (Seatbelt available). If the egress-proxy or net-demo binary is missing it skip-as-passes — build them first (the command above does).

- [ ] **Step 3: Clippy the test target**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --test net_demo_egress_e2e --all-targets -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add core/tests/net_demo_egress_e2e.rs
git commit -m "$(printf 'test(5c): hermetic cross-platform e2e — transparent-tunnel TLS + 1:1 respawn (no VM)\n\nnet-demo under PersistentWorker, force-routed through a real transparent-tunnel\nsidecar to a loopback self-signed TLS origin (trusted via extra_ca). Proves\nend-to-end TLS through the tunnel + many-calls-one-boot + net.crash respawn.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 7: `net-demo` micro-VM rootfs build script

**Files:**
- Create: `scripts/workers/microvm/build-net-demo-rootfs.sh`

**Interfaces:**
- Consumes: the release `kastellan-worker-net-demo` + `kastellan-microvm-init` binaries.
- Produces: `net-demo.ext4` in `KASTELLAN_MICROVM_DIR` (default `/var/lib/kastellan/microvm`), selected at spawn via `KASTELLAN_MICROVM_ROOTFS=net-demo.ext4`. **No OS CA bundle** — the worker trusts compiled-in webpki roots (+ a test CA delivered per-spawn via RO-share).

- [ ] **Step 1: Write the script (mirrors `build-kv-demo-rootfs.sh`, adds `/run` for the egress relay)**

Create `scripts/workers/microvm/build-net-demo-rootfs.sh`:

```bash
#!/usr/bin/env bash
# Build the net-demo micro-VM rootfs (ext4) beside the shared vmlinux. net-demo is
# a pure-Rust Net::Allowlist worker that does its OWN end-to-end TLS through the
# egress proxy (transparent tunnel). No python. NO OS ca-certificates bundle — the
# worker trusts compiled-in webpki roots; a test origin's CA (when present) is
# delivered per-spawn via the RO-share. /run is the egress-relay mountpoint (4a).
if [ -z "${BASH_VERSION:-}" ]; then
    echo "Run with bash: ./scripts/workers/microvm/build-net-demo-rootfs.sh" >&2; exit 1
fi
set -euo pipefail
OUT_DIR="${KASTELLAN_MICROVM_DIR:-/var/lib/kastellan/microvm}"
HOST_ARCH="$(uname -m)"
case "${HOST_ARCH}" in
    x86_64|aarch64) KERNEL_ARCH="${HOST_ARCH}" ;;
    *) echo "Unsupported arch '${HOST_ARCH}'." >&2; exit 1 ;;
esac
KERNEL_URL="https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/${KERNEL_ARCH}/vmlinux-6.1.102"
ROOTFS_MIB=128

if ! mkdir -p "$OUT_DIR" 2>/dev/null || [ ! -w "$OUT_DIR" ]; then
    echo "Cannot write micro-VM dir: $OUT_DIR — run sudo ./scripts/linux/install-firecracker-vsock.sh or set KASTELLAN_MICROVM_DIR." >&2
    exit 1
fi
[ -f "$OUT_DIR/vmlinux" ] || curl -fL --retry 3 -o "$OUT_DIR/vmlinux" "$KERNEL_URL"

source "$HOME/.cargo/env"
cargo build --release -p kastellan-worker-net-demo -p kastellan-microvm-init

WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
install -D -m0755 target/release/kastellan-microvm-init "$WORK/sbin/init"
install -D -m0755 target/release/kastellan-worker-net-demo "$WORK/usr/local/bin/kastellan-worker-net-demo"

copy_lib_closure() {
    for obj in "$@"; do
        ldd "$obj" 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'
    done | sort -u | while read -r lib; do
        [ -f "$lib" ] || continue
        install -D -m0755 "$lib" "$WORK$lib"
    done
}
copy_lib_closure target/release/kastellan-microvm-init target/release/kastellan-worker-net-demo

# Pseudo-fs + slice-3 share anchors + /run (egress relay, slice 4a).
mkdir -p "$WORK/proc" "$WORK/sys" "$WORK/tmp" "$WORK/dev" "$WORK/run" \
         "$WORK/ro-share" "$WORK/opt" "$WORK/data" "$WORK/srv" "$WORK/mnt" "$WORK/work"

mkfs.ext4 -q -F -O ^has_journal -L net-demo -d "$WORK" "$OUT_DIR/net-demo.ext4" "${ROOTFS_MIB}M"
echo "built $OUT_DIR/net-demo.ext4 (+ shared $OUT_DIR/vmlinux)"
```

- [ ] **Step 2: Make it executable + shellcheck**

Run: `chmod +x scripts/workers/microvm/build-net-demo-rootfs.sh && shellcheck scripts/workers/microvm/build-net-demo-rootfs.sh || true`
Expected: executable bit set; shellcheck clean (or matches the warnings the sibling `build-kv-demo-rootfs.sh` already carries — parity, not new issues).

- [ ] **Step 3: Commit**

```bash
git add scripts/workers/microvm/build-net-demo-rootfs.sh
git commit -m "$(printf 'build(5c): net-demo micro-VM rootfs script (+/run egress relay, no OS CA bundle)\n\nMirrors build-kv-demo-rootfs.sh; adds the /run egress-relay mountpoint. Worker\ntrusts compiled-in webpki roots, so no ca-certificates bundle is baked.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 8: DGX real-KVM `#[ignore]` e2e — net-demo in a VM

**Files:**
- Create: `core/tests/net_demo_firecracker_egress_e2e.rs`

**Interfaces:**
- Consumes: `kastellan_core::egress::persistent_net::{NetTransportSpawn, spawn_net_transport}`, `kastellan_core::worker_lifecycle::{PersistentFactory, PersistentTransport, PersistentWorker}`, `kastellan_sandbox::{linux_firecracker::*, Net, Profile, SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxPolicy}`.
- Produces: a `#[ignore]` DGX acceptance test: net-demo in a Firecracker VM does end-to-end TLS to a host loopback origin over the slice-4a vsock reverse-channel, serves many calls on one boot, and survives a launcher-SIGKILL respawn (1:1 sidecar+VM).

- [ ] **Step 1: Write the test (mirror `kv_demo_firecracker_persistent_e2e.rs` harness)**

Create `core/tests/net_demo_firecracker_egress_e2e.rs`. Reuse the exact `image_dir()` / `locate_microvm_run()` / `skip_if_no_microvm()` / `firecracker_backend()` helpers from `kv_demo_firecracker_persistent_e2e.rs` (copy them verbatim; they are test-local). The base policy uses `Net::Allowlist` + `KASTELLAN_MICROVM_ROOTFS=net-demo.ext4`; the test CA is delivered into the guest via `extra_ca` (Task-5 appends it to `fs_read` → the slice-3/4b RO-share file-aware bind materializes it in-guest — the CA must live under a SHARE_ANCHOR, so write it under `/tmp`).

```rust
//! Slice 5c DGX e2e: a long-lived net-demo worker in a Firecracker VM does its
//! own end-to-end TLS to a host loopback origin through a transparent-tunnel
//! egress sidecar over the slice-4a vsock reverse-channel, serves many calls on
//! one boot, and survives a launcher-SIGKILL respawn (1:1 sidecar+VM).
//! `#[ignore]`: needs /dev/kvm + /dev/vhost-vsock + net-demo.ext4 + the RELEASE
//! launcher. Run on the DGX:
//!   export PATH=$HOME/.local/bin:$PATH
//!   cargo build --release -p kastellan-microvm-run
//!   cargo build -p kastellan-worker-egress-proxy   # host sidecar (debug ok)
//!   ./scripts/workers/microvm/build-net-demo-rootfs.sh
//!   cargo test -p kastellan-core --test net_demo_firecracker_egress_e2e -- --ignored --nocapture
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kastellan_core::egress::persistent_net::{spawn_net_transport, NetTransportSpawn};
use kastellan_core::worker_lifecycle::{PersistentFactory, PersistentTransport, PersistentWorker};
use kastellan_sandbox::linux_firecracker::{FirecrackerImage, LinuxFirecracker};
use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxBackendKind, SandboxBackends, SandboxPolicy};

// image_dir(), locate_microvm_run(), skip_if_no_microvm(), firecracker_backend()
// — copy verbatim from kv_demo_firecracker_persistent_e2e.rs, changing only the
// rootfs filename to "net-demo.ext4" in firecracker_image().

// mod origin { ... } — loopback rustls origin bound to 127.0.0.1:0 on the HOST,
// replies 204; returns (port, ca_pem_path under /tmp). Same helper as Task 6.

#[test]
#[ignore = "DGX-only: real KVM + vsock + net-demo rootfs + egress-proxy sidecar"]
fn net_demo_tls_probe_through_vm_survives_respawn() {
    if skip_if_no_microvm() { return; }

    let proxy_bin = {
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().join("target");
        let candidates = [target.join("debug").join("kastellan-worker-egress-proxy"),
                          target.join("release").join("kastellan-worker-egress-proxy")];
        match candidates.into_iter().find(|p| p.is_file()) {
            Some(p) => p,
            None => { eprintln!("[SKIP] egress-proxy not built"); return; }
        }
    };

    let (origin_port, ca_path) = origin::spawn_loopback_tls_origin(); // ca under /tmp
    let allow = vec![format!("127.0.0.1:{origin_port}")];
    let backend = firecracker_backend();
    let scratch_root = std::env::temp_dir();

    let factory: PersistentFactory = {
        let backend = Arc::clone(&backend);
        let proxy_bin = proxy_bin.clone();
        let ca_path = ca_path.clone();
        let allow = allow.clone();
        let img = image_dir();
        Box::new(move || {
            let scratch = scratch_root_subdir(); // unique per spawn (helper below)
            let base = SandboxPolicy {
                net: Net::Allowlist(allow.clone()),
                profile: Profile::WorkerNetClient,
                mem_mb: 256,
                env: vec![
                    ("KASTELLAN_MICROVM_DIR".to_string(), img.clone()),
                    ("KASTELLAN_MICROVM_ROOTFS".to_string(), "net-demo.ext4".to_string()),
                ],
                ..SandboxPolicy::default()
            };
            let params = NetTransportSpawn {
                backend: &*backend,
                proxy_bin: &proxy_bin,
                program: "/usr/local/bin/kastellan-worker-net-demo",
                args: &[],
                base_policy: base,
                allowlist: &allow,
                worker_name: "net-demo",
                extra_ca: Some(&ca_path),
            };
            let t = spawn_net_transport(&params, &scratch)?;
            Ok(Box::new(t) as Box<dyn PersistentTransport>)
        })
    };

    let h = PersistentWorker::spawn("net-demo-vm", factory).expect("boot net-demo VM");

    // Phase 1: end-to-end TLS through the VM's vsock reverse-channel.
    let probe = h.call("net.tls_probe", serde_json::json!({"host":"127.0.0.1","port":origin_port}))
        .expect("net.tls_probe");
    assert_eq!(probe["ok"], true, "in-VM transparent-tunnel TLS must succeed, got {probe}");
    for i in 0..5 { let _ = h.call("net.stats", serde_json::json!({})).unwrap_or_else(|e| panic!("stats {i}: {e}")); }

    // Phase 2: SIGKILL the launcher → 1:1 sidecar+VM respawn.
    let _ = std::process::Command::new("pkill").args(["-9","kastellan-microvm-run"]).status();
    let _ = h.call("net.tls_probe", serde_json::json!({"host":"127.0.0.1","port":origin_port}));
    let mut ok = false;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        if let Ok(p) = h.call("net.tls_probe", serde_json::json!({"host":"127.0.0.1","port":origin_port})) {
            if p["ok"] == true { ok = true; break; }
        }
    }
    assert!(ok, "net.tls_probe must succeed again within ~30s after VM+sidecar respawn");
    h.shutdown();
}
```

Implement `scratch_root_subdir()` as a small helper minting a unique `std::env::temp_dir().join(format!("netdemo-vm-{}-{}", pid, atomic_seq))` and `create_dir_all`-ing it (the sun_path guard from `make_worker_scratch_dir` applies — keep the dir shallow under `/tmp`).

- [ ] **Step 2: Compile-check on the Mac (cannot run — `ring` C-dep), cross-clippy the sandbox touchpoints**

Run: `source "$HOME/.cargo/env" && cargo build -p kastellan-core --tests` (compiles the test; does not run the `#[ignore]` VM body on the Mac)
Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-sandbox --target aarch64-unknown-linux-gnu --all-targets -D warnings`
Expected: both clean. (No sandbox source changed this task, but this pins the Linux-cfg gate the whole slice must hold.)

- [ ] **Step 3: Commit**

```bash
git add core/tests/net_demo_firecracker_egress_e2e.rs
git commit -m "$(printf 'test(5c): DGX #[ignore] e2e — net-demo end-to-end TLS in a VM + 1:1 respawn\n\nnet-demo in a Firecracker VM does its own TLS to a host loopback origin over the\nslice-4a vsock reverse-channel (transparent tunnel), many-calls-one-boot, and\nSIGKILL-launcher → PersistentWorker respawns VM+sidecar 1:1. Hermetic (loopback\norigin, no real-net/DNS) so it is DGX-green.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

- [ ] **Step 4: DGX acceptance run (operator/executor on the DGX, not the Mac)**

On the DGX (`ssh dgx '<cmd>'` per the memory note — bare `ssh dgx '...'`, flags before the hostname get denied):
```sh
source "$HOME/.cargo/env"
export PATH=$HOME/.local/bin:$PATH
cargo build --release -p kastellan-microvm-run
cargo build -p kastellan-worker-egress-proxy
./scripts/workers/microvm/build-net-demo-rootfs.sh
cargo test -p kastellan-core --test net_demo_firecracker_egress_e2e -- --ignored --nocapture
```
Expected: the `#[ignore]` test PASSES; `0` orphan run-dirs left in `<run_dir>` after. Also run the full DGX gate: `cargo test --workspace` + `cargo clippy --workspace --all-targets -D warnings` (both green; slice-1/2/3/4a/4b/5b e2e unregressed).

---

### Task 9: Update HANDOVER.md + ROADMAP.md

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Interfaces:** none (docs).

- [ ] **Step 1: Tick slice 5c in ROADMAP.md**

In `docs/devel/ROADMAP.md`, change the slice-5c line (currently `- [ ] **SLICE 5c (next):** …`) to `- [x] **SLICE 5c** …` with the merge/commit hash and a one-line summary (net-demo TLS-in-a-VM through a transparent-tunnel sidecar, 1:1 respawn; spec/plan `docs/superpowers/{specs,plans}/2026-07-01-firecracker-microvm-slice5c-*`). Reframe `SLICE 5b-4` as the next `[ ]` item.

- [ ] **Step 2: Update the HANDOVER header + Recently-completed + Next TODO**

In `docs/devel/handovers/HANDOVER.md`: bump `Last updated`, set the top "Current state" line to slice 5c done on this branch, move slice 5c from "Next TODO" into a fresh "Recently completed (this session)" block (files, the transparent-tunnel trade-off, the DGX gotchas, the test-count delta), refresh "Working state" (new `net-demo` crate + `egress/persistent_net.rs` + `web-common::make_transparent_get`), and write a new "Next TODO (pick one)" leading with **slice 5b-4 (Matrix adopts the foundation)**. Copy the exact `passed / failed / ignored` counts from the DGX `cargo test --workspace` run in Task 8 Step 4 into `Session-end verification:`.

- [ ] **Step 3: Commit both docs together**

```bash
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(printf 'docs(handover): slice 5c done — net egress in a VM (transparent tunnel), next = 5b-4\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Self-Review

**1. Spec coverage:**
- *net-aware `PersistentTransport` bundling Client + EgressSidecar* → Task 5 (`NetClientTransport`). ✓
- *sidecar in transparent-tunnel mode + 1:1 respawn* → Task 5 (`spawn_net_transport` with `disable_mitm=true`; `PersistentWorker`'s off-thread drop gives 1:1) + proven in Tasks 6/8. ✓
- *net-demo worker doing end-to-end TLS to an allowlisted host* → Tasks 1/3 (`net.tls_probe`). ✓
- *`mitm`-conditional policy rewrite (no CA in transparent mode)* → Task 4. ✓
- *webpki+extra-CA transport* → Task 2. ✓
- *rootfs (no OS CA bundle — refinement resolving the spec's open CA-bundle item toward compiled-in webpki roots)* → Task 7. ✓
- *hermetic loopback-TLS e2e (cross-platform) + DGX VM e2e + (real-net deferred)* → Tasks 6/8. The `#[ignore]` real-net probe is available for free (drop `extra_ca`, allowlist a real host) but is operator-run; noted, not a separate task (YAGNI — the loopback gate covers TLS). ✓
- *security posture: no virtio-net, allowlist+SSRF unchanged, transparent-tunnel trade-off, no CA to transparent worker, 1:1 reap* → enforced by Tasks 4/5 + the unchanged plan/proxy; documented in module docs. ✓
- *cross-platform, additive/byte-identical* → Global Constraints + Task 4 (MITM path unchanged) + Task 2 (`with_trust` unchanged). ✓

**2. Placeholder scan:** The two e2e tests (Tasks 6/8) and the Task-3 hermetic harness carry a `mod origin` / `mod probe_harness` **to be implemented faithfully** (rustls+rcgen loopback origin ~120 lines) rather than inlined verbatim — this is the one place the plan describes-not-shows, because the glue is mechanical rustls-server boilerplate and belongs to the executor's TDD cycle. Every production code change (Tasks 1–5, 7) is shown in full. Flagged here so the executor writes the harness, not a stub. No `TODO`/`TBD` in shipped code.

**3. Type consistency:**
- `rewrite_worker_policy(policy, uds, ca, mitm)` — 4-arg signature used identically in Task 4 (def) and Task 5 (`forced_transparent_policy` caller). ✓
- `NetTransportSpawn` fields (`backend, proxy_bin, program, args, base_policy, allowlist, worker_name, extra_ca`) — defined in Task 5, consumed identically in Tasks 6/8. ✓
- `spawn_net_transport(params, scratch)` — 2-arg (params + caller-owned scratch) in Task 5 def and Tasks 6/8 calls. ✓
- `make_transparent_get(ua, uds: &Path, extra_ca: Option<&Path>)` — Task 2 def matches Task 3 call. ✓
- `EgressSidecar::from_parts(sidecar, ingest, scratch)` + `spawn_ingest_thread` `pub(crate)` — Task 5 Step 1 exposes them; Task 5 Step 5 consumes them. ✓
- `probe_result(Result<RawResponse, String>)` — Task 3 def + test. ✓

One open item the executor confirms during TDD (flagged in the spec too): the exact `rcgen`/`rustls-pki-types` versions/features for the test harnesses — copy verbatim from `workers/egress-proxy/Cargo.toml` (it already self-signs a CA) so the workspace lock is unchanged.
