//! Hermetic end-to-end test: drive the real `kastellan-worker-mail` binary over
//! JSON-RPC stdio against a local mock HTTP server standing in for `localmail
//! serve`. Exercises the full worker path — arg/env parsing, `from_env`, the
//! web-common transport, bearer auth, tool dispatch, and `get_attachment`
//! writing an original-format file into `KASTELLAN_WORKER_OUT`.
//!
//! No PG, no sandbox backend, no live localmail: the worker runs standalone
//! (the prelude's Linux lockdown is a no-op without `KASTELLAN_LANDLOCK_*`, and
//! macOS lockdown is a no-op), reaching the mock on loopback with a direct
//! transport. Runs on both hosts.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

/// Minimal HTTP/1.1 mock: one request per connection (`Connection: close`),
/// routed by path. Runs until the listener is dropped.
fn spawn_mock() -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { break };
            // Read the request head (+ any body); we only need the request line.
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            if n == 0 {
                continue;
            }
            let req = String::from_utf8_lossy(&buf[..n]);
            let first = req.lines().next().unwrap_or("");
            // Every request must carry the bearer we provisioned.
            assert!(
                req.to_lowercase().contains("authorization: bearer e2e-token"),
                "request missing bearer: {first}"
            );
            let (status, ctype, body): (&str, &str, Vec<u8>) = if first.contains("GET /v1/accounts")
            {
                ("200 OK", "application/json", br#"[{"id":1,"name":"work"}]"#.to_vec())
            } else if first.contains("POST /v1/search") {
                ("200 OK", "application/json", br#"{"hits":[{"message_id":7}],"next_cursor":null}"#.to_vec())
            } else if first.contains("/v1/attachments/") && first.contains("/text") {
                ("200 OK", "text/plain", b"extracted booking text".to_vec())
            } else if first.contains("/v1/attachments/") {
                ("200 OK", "application/pdf", b"%PDF-1.7 fake booking".to_vec())
            } else {
                ("404 Not Found", "text/plain", b"nope".to_vec())
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(&body);
            let _ = sock.flush();
        }
    });
    (base, handle)
}

/// Send one JSON-RPC request line and read one response line.
fn rpc(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut BufReader<std::process::ChildStdout>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    writeln!(stdin, "{req}").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response line {line:?}: {e}"))
}

#[test]
fn mail_worker_stdio_roundtrip_against_mock() {
    let (base, _mock) = spawn_mock();

    // 0600 token file + a workspace out/ dir.
    let tmp = std::env::temp_dir().join(format!("mail-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let token_file = tmp.join("token");
    std::fs::write(&token_file, "e2e-token\n").unwrap();
    let out_dir = tmp.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_kastellan-worker-mail"))
        .env("KASTELLAN_MAIL_ENDPOINT", &base)
        .env("KASTELLAN_MAIL_TOKEN_FILE", &token_file)
        .env("KASTELLAN_WORKER_OUT", &out_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mail worker");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // 1. list_accounts → the mock's one account.
    let r = rpc(&mut stdin, &mut stdout, 1, "mail.list_accounts", serde_json::json!({}));
    assert_eq!(r["result"][0]["id"], 1, "resp: {r}");

    // 2. search → a hit.
    let r = rpc(&mut stdin, &mut stdout, 2, "mail.search", serde_json::json!({"query": "qantas"}));
    assert_eq!(r["result"]["hits"][0]["message_id"], 7, "resp: {r}");

    // 3. get_attachment → original bytes written to out/, path returned, no bytes inline.
    let sha = "a".repeat(64);
    let r = rpc(
        &mut stdin,
        &mut stdout,
        3,
        "mail.get_attachment",
        serde_json::json!({"sha256": sha, "filename": "booking.pdf"}),
    );
    let path = r["result"]["path"].as_str().expect("path in result");
    assert!(std::path::Path::new(path).starts_with(&out_dir), "must be under out/: {path}");
    assert_eq!(std::fs::read(path).unwrap(), b"%PDF-1.7 fake booking");
    assert_eq!(r["result"]["content_type"], "application/pdf");
    assert!(r["result"].get("data_base64").is_none(), "no inline bytes");

    // 4. unknown method → JSON-RPC error (-32601).
    let r = rpc(&mut stdin, &mut stdout, 4, "mail.nope", serde_json::json!({}));
    assert_eq!(r["error"]["code"], -32601, "resp: {r}");

    drop(stdin); // EOF → worker exits its stdio loop.
    let _ = child.wait();
    std::fs::remove_dir_all(&tmp).ok();
}
