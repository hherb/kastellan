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

impl Handler for NetHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        self.calls_served += 1;
        match method {
            "net.tls_probe" => {
                let p: ProbeParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let uds = self.uds.as_ref().ok_or_else(|| {
                    RpcError::new(codes::OPERATION_FAILED, "KASTELLAN_EGRESS_PROXY_UDS not set")
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

    // ── Hermetic end-to-end proof of the transparent-tunnel TLS path ─────────
    //
    // Stands up two real endpoints in-process:
    //   1. a loopback rustls TLS origin (self-signed via rcgen, SAN 127.0.0.1)
    //      that replies `HTTP/1.1 204 No Content\r\n\r\n` to any request;
    //   2. a raw `UnixListener` "transparent-tunnel proxy" that reads the CONNECT
    //      line, replies `200 Connection Established`, then blindly splices bytes
    //      between the worker and a fresh TCP connection to the origin.
    //
    // The worker's `make_transparent_get(...).get()` must complete an end-to-end
    // TLS handshake through the opaque tunnel and validate the origin's cert
    // against `extra_ca`. This proves both the tunnel plumbing AND chain
    // validation: the negative test (`extra_ca = None`) fails closed.
    mod probe_harness {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::thread;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_rustls::TlsAcceptor;

        /// A running loopback TLS origin plus its self-signed cert PEM.
        pub struct Origin {
            pub port: u16,
            pub cert_pem: String,
        }

        /// Spawn a single-connection loopback rustls origin on 127.0.0.1:0 that
        /// answers any request with `204 No Content`. Returns its port + cert PEM.
        pub fn spawn_tls_origin() -> Origin {
            // Self-signed cert with a 127.0.0.1 IP SAN so rustls' server-name
            // (IP) verification against the origin succeeds.
            let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
                .expect("generate self-signed cert");
            let cert_pem = ck.cert.pem();
            let cert_der = ck.cert.der().clone();
            let key_der = rustls_pki_types::PrivateKeyDer::Pkcs8(
                rustls_pki_types::PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()),
            );

            let server_config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .expect("build server config");
            let acceptor = TlsAcceptor::from(Arc::new(server_config));

            // A tiny current-thread runtime dedicated to the origin. It binds the
            // port synchronously so the caller can read it, then serves one conn.
            let (tx, rx) = std::sync::mpsc::channel::<u16>();
            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("origin runtime");
                rt.block_on(async move {
                    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
                    let port = listener.local_addr().unwrap().port();
                    tx.send(port).unwrap();
                    let (tcp, _) = listener.accept().await.expect("accept origin");
                    let mut tls = acceptor.accept(tcp).await.expect("origin TLS handshake");
                    // Drain the (small) request; ignore its content.
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    tls.write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                        .await
                        .expect("write 204");
                    let _ = tls.shutdown().await;
                });
            });

            let port = rx.recv().expect("origin port");
            Origin { port, cert_pem }
        }

        /// Spawn a raw transparent-tunnel proxy on `uds`: read the CONNECT head to
        /// the blank line, reply `200`, then blindly pipe bytes between the worker
        /// and a fresh TCP connection to `origin_port`. TLS rides opaquely through.
        pub fn spawn_tunnel_proxy(uds: PathBuf, origin_port: u16) {
            let listener = UnixListener::bind(&uds).expect("bind proxy uds");
            thread::spawn(move || {
                let (mut client, _) = listener.accept().expect("accept proxy conn");
                // Drain CONNECT head up to the blank line.
                let mut buf = [0u8; 1024];
                let mut acc = Vec::new();
                loop {
                    let n = client.read(&mut buf).expect("read CONNECT");
                    if n == 0 {
                        return;
                    }
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                client
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .expect("write 200");

                // Dial the real origin and splice bidirectionally.
                let origin = TcpStream::connect(("127.0.0.1", origin_port)).expect("dial origin");
                splice(client, origin);
            });
        }

        /// Blindly pipe bytes in both directions until either side closes.
        fn splice(client: UnixStream, origin: TcpStream) {
            let mut c_read = client.try_clone().expect("clone client");
            let mut o_write = origin.try_clone().expect("clone origin");
            let up = thread::spawn(move || {
                let _ = std::io::copy(&mut c_read, &mut o_write);
                let _ = o_write.shutdown(std::net::Shutdown::Write);
            });

            let mut o_read = origin;
            let mut c_write = client;
            let _ = std::io::copy(&mut o_read, &mut c_write);
            let _ = c_write.shutdown(std::net::Shutdown::Write);
            let _ = up.join();
        }
    }

    /// A private scratch dir + UDS path unique to this test invocation.
    fn scratch_uds(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kastellan-netdemo-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let uds = dir.join("egress.sock");
        let _ = std::fs::remove_file(&uds);
        (dir, uds)
    }

    #[test]
    fn tls_probe_end_to_end_through_tunnel_trusts_extra_ca() {
        let origin = probe_harness::spawn_tls_origin();
        let (dir, uds) = scratch_uds("ok");
        // The origin's self-signed cert is the extra CA the worker trusts.
        let ca_path = dir.join("origin-ca.pem");
        std::fs::write(&ca_path, origin.cert_pem.as_bytes()).unwrap();
        probe_harness::spawn_tunnel_proxy(uds.clone(), origin.port);

        let mut h = NetHandler::new(Some(uds.clone()), Some(ca_path));
        let result = h
            .call(
                "net.tls_probe",
                serde_json::json!({ "host": "127.0.0.1", "port": origin.port }),
            )
            .expect("probe rpc ok");

        assert_eq!(result["ok"], true, "expected ok:true, got {result}");
        assert_eq!(result["status"], 204, "expected status 204, got {result}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_probe_without_extra_ca_rejects_self_signed_origin() {
        let origin = probe_harness::spawn_tls_origin();
        let (dir, uds) = scratch_uds("untrusted");
        probe_harness::spawn_tunnel_proxy(uds.clone(), origin.port);

        // No extra CA → webpki roots only → the self-signed origin is untrusted,
        // so the end-to-end TLS handshake must fail and the probe returns ok:false.
        let mut h = NetHandler::new(Some(uds.clone()), None);
        let result = h
            .call(
                "net.tls_probe",
                serde_json::json!({ "host": "127.0.0.1", "port": origin.port }),
            )
            .expect("probe rpc ok (transport error is a probe result, not an rpc error)");

        assert_eq!(result["ok"], false, "self-signed origin must be untrusted, got {result}");
        assert!(result["status"].is_null(), "no status on handshake failure, got {result}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
