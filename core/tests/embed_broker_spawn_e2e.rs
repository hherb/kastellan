//! End-to-end: core spawns the real `kastellan-worker-embed-broker` sidecar under
//! the platform sandbox (Seatbelt on macOS, bwrap on Linux), the broker binds its
//! UDS, forwards an `embed` request to a loopback stub backend, and a direct UDS
//! JSON-RPC client gets the vectors back.
//!
//! This exercises the security-critical spawn path added in Slice B Tasks 3+4:
//! `spawn_embed_broker` (scratch mint, sandboxed spawn, lockdown-env derivation,
//! UDS readiness) and the broker's own `Net::Allowlist([backend host:port])` with
//! the `WorkerNetClient` seccomp profile (which must permit AF_UNIX accept and
//! AF_INET connect). No real Ollama needed — the stub stands in for the embedding
//! backend, so the test is hermetic apart from requiring the built broker binary
//! and a working sandbox.
//!
//! `#[ignore]` by convention for real-spawn e2e (opt in with `--ignored`). Skips
//! (not fails) if the broker binary isn't built or the sandbox is unavailable
//! (e.g. Linux without the bwrap userns workaround — see CLAUDE.md).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::thread;

use kastellan_core::embed_broker::{spawn_embed_broker, EmbedBrokerConfig, EmbedBrokerSpec};
use kastellan_tests_common::{backend, skip_if_sandbox_unavailable, workspace_target_binary};

/// A one-shot loopback HTTP stub that answers any request with a canned
/// OpenAI-compatible embeddings body. Returns the bound `127.0.0.1:<port>` and a
/// join handle; the server serves exactly `expected` requests then exits.
fn spawn_stub_backend(embedding: Vec<f32>, expected: usize) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub backend");
    let addr = listener.local_addr().expect("stub addr");
    let nums: Vec<String> = embedding.iter().map(|x| x.to_string()).collect();
    let body = format!(r#"{{"data":[{{"index":0,"embedding":[{}]}}]}}"#, nums.join(","));
    let handle = thread::spawn(move || {
        for _ in 0..expected {
            let (mut sock, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => return,
            };
            // Drain the request head (up to the blank line) so the client's write
            // completes; we don't need the body for a canned response.
            let mut reader = BufReader::new(sock.try_clone().expect("clone stub sock"));
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            if content_length > 0 {
                let mut body_buf = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body_buf);
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    (format!("http://{addr}/v1/embeddings"), handle)
}

#[test]
#[ignore = "real broker spawn under the OS sandbox; opt in with --ignored"]
fn broker_spawns_binds_and_forwards_embed() {
    if skip_if_sandbox_unavailable() {
        return;
    }
    let broker_bin = workspace_target_binary("kastellan-worker-embed-broker");
    if !broker_bin.exists() {
        eprintln!("\n[SKIP] embed-broker binary not built; run cargo build --workspace\n");
        return;
    }

    let expected = vec![0.1_f32, 0.2, 0.3, 0.4];
    let (endpoint, stub) = spawn_stub_backend(expected.clone(), 1);

    // Short scratch root so `<scratch>/embed.sock` fits sun_path on macOS.
    let scratch_root = std::env::temp_dir();
    let cfg = EmbedBrokerConfig::new(broker_bin, scratch_root);
    let spec = EmbedBrokerSpec::new(&endpoint, "test-model");
    let be = backend();

    let (sidecar, uds) =
        spawn_embed_broker(&cfg, &spec, &*be).expect("spawn embed-broker under sandbox");
    assert!(uds.exists(), "broker must have bound its UDS at {uds:?}");

    // Talk the same JSON-RPC line protocol BrokeredEmbedder uses.
    let mut stream = UnixStream::connect(&uds).expect("connect broker UDS");
    let req = br#"{"jsonrpc":"2.0","id":1,"method":"embed","params":{"model":"test-model","input":["hello"]}}"#;
    stream.write_all(req).expect("write embed request");
    stream.write_all(b"\n").expect("write newline");
    stream.flush().ok();

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read broker response");
    let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("parse response JSON");

    assert!(resp.get("error").is_none(), "unexpected broker error: {resp}");
    let data = resp
        .pointer("/result/data")
        .and_then(|d| d.as_array())
        .expect("result.data array");
    assert_eq!(data.len(), 1, "one vector for one input");
    let got: Vec<f32> = data[0]
        .get("embedding")
        .and_then(|e| e.as_array())
        .expect("embedding array")
        .iter()
        .map(|v| v.as_f64().unwrap() as f32)
        .collect();
    assert_eq!(got, expected, "broker forwarded the backend vector verbatim");

    // Teardown: dropping the sidecar kills the broker + removes its scratch.
    drop(reader);
    drop(stream);
    drop(sidecar);
    let _ = stub.join();
}
