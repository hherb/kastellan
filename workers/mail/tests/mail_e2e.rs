//! Hermetic end-to-end test: drive the real `kastellan-worker-mail` binary over
//! JSON-RPC stdio against a local mock HTTP server standing in for `localmail
//! serve`. Exercises the full worker path — arg/env parsing, `from_env`, the
//! web-common transport, bearer auth, tool dispatch, and `get_attachment`
//! writing an original-format file into `KASTELLAN_WORKER_OUT`.
//!
//! No PG, no sandbox backend, no live localmail: the worker runs standalone
//! (the prelude's Linux lockdown is a no-op without `KASTELLAN_LANDLOCK_*`, and
//! macOS lockdown is a no-op), reaching the mock on loopback with a direct
//! transport. Runs on both hosts, and — since #536 — in CI on every PR.
//!
//! The wire shapes below mirror `kastellan_tests_common::mock_localmail`, which
//! the live drift gate in `core/tests/mail_daemon_e2e.rs` pins against the real
//! service. They are a SECOND copy of that contract, kept here because
//! `workers/mail` is a bin-only crate with no dev-dependency on `tests-common`
//! (taking one would put `kastellan-core` in a leaf worker's dev graph). When
//! the gate says the live shapes moved, both copies need the edit.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The message id the canned search hit references, as localmail puts it on the
/// wire: a STRING.
const CANNED_MESSAGE_ID: &str = "7";

/// Bytes the index route serves for message 7's one attachment — distinct from
/// the hash route's, so step 5 can tell which route ran.
const BY_POSITION_BYTES: &[u8] = b"%PDF-1.7 by position";

/// The hash of message 7's one attachment: the real sha256 of
/// [`BY_POSITION_BYTES`], because the worker refuses bytes fetched by position
/// that do not hash to what the listing said (#760).
const CANNED_SHA: &str = "d9f1ba869130eabb6ea6cf19011d7fb7546f5cbcbfa39536eb62fa41a915f251";

/// localmail's paged text envelope (slice D): one page, which for these short
/// fixtures is the whole and last one.
fn text_page(text: &str) -> Vec<u8> {
    format!(
        r#"{{"text":"{text}","offset":0,"limit":8000,"total":{},"next_offset":null}}"#,
        text.len()
    )
    .into_bytes()
}

/// Does this request-target's query carry the exact pair `headers=full`?
///
/// Mirrors localmail's own `full_headers=(headers == "full")` rather than a
/// loose substring check, so a client sending some *other* spelling gets the
/// same header-less 200 the real service would give it. That asymmetry is #500,
/// and a mock that ignored the query could not reproduce it.
fn wants_full_headers(query: &str) -> bool {
    query.split('&').any(|pair| pair == "headers=full")
}

/// Minimal HTTP/1.1 mock: one request per connection (`Connection: close`),
/// routed by exact path. Runs until the listener is dropped.
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
            let first = req.lines().next().unwrap_or("").to_string();
            // Every request must carry the bearer we provisioned.
            assert!(
                req.to_lowercase().contains("authorization: bearer e2e-token"),
                "request missing bearer: {first}"
            );
            // Route on the parsed method + path, never on `contains`: a
            // substring match makes `/v1/messages/77` indistinguishable from
            // `/v1/messages/7`, so a bug that corrupted the id would be served
            // the canned body anyway and the test would read back its own
            // constant. The detail route echoes the id it was actually asked
            // for, which is what makes the assertion mean something.
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("");
            let (path, query) = target.split_once('?').unwrap_or((target, ""));

            let (status, ctype, body): (&str, &str, Vec<u8>) = match (method, path) {
                // Unauthenticated on the real service; the worker's version gate
                // (#760) asks before every slice D/E tool.
                ("GET", "/v1/version") => (
                    "200 OK",
                    "application/json",
                    br#"{"api_major":1,"api_minor":3,"server_version":"0.3.0"}"#.to_vec(),
                ),
                // Live localmail serves ids as STRINGS on every route.
                ("GET", "/v1/accounts") => (
                    "200 OK",
                    "application/json",
                    br#"[{"id":"1","name":"work"}]"#.to_vec(),
                ),
                // Real localmail keys results under "results" (not "hits") and
                // serves `message_id` as a STRING. Serving a number here is what
                // let #527 hide: the worker's i64 agreed with this fixture and
                // not with the service.
                ("POST", "/v1/search") => (
                    "200 OK",
                    "application/json",
                    format!(r#"{{"results":[{{"message_id":"{CANNED_MESSAGE_ID}","snippet":"…"}}],"next_cursor":null}}"#)
                        .into_bytes(),
                ),
                ("GET", p) if p.starts_with("/v1/attachments/") && p.ends_with("/text") => {
                    // Real localmail returns application/json {"text": "...", paging}.
                    ("200 OK", "application/json", text_page("extracted booking text"))
                }
                // By position within message 7 (slice D): its one attachment.
                ("GET", "/v1/messages/7/attachments/0/text") => {
                    ("200 OK", "application/json", text_page("extracted by position"))
                }
                ("GET", "/v1/messages/7/attachments/0") => {
                    ("200 OK", "application/pdf", BY_POSITION_BYTES.to_vec())
                }
                ("GET", p) if p.starts_with("/v1/attachments/") => {
                    ("200 OK", "application/pdf", b"%PDF-1.7 fake booking".to_vec())
                }
                ("GET", p) if p.starts_with("/v1/messages/") => {
                    let id = p.trim_start_matches("/v1/messages/");
                    // `headers` appears ONLY for the exact `?headers=full`
                    // spelling — the #500 asymmetry, modelled rather than hidden.
                    let headers = if wants_full_headers(query) {
                        r#","headers":{"Message-ID":"<canned@example.test>"}"#
                    } else {
                        ""
                    };
                    (
                        "200 OK",
                        "application/json",
                        format!(
                            r#"{{"id":"{id}","subject":"invoice","attachments":[{{"filename":"booking.pdf","sha256":"{CANNED_SHA}","content_type":"application/pdf","size":20}}]{headers}}}"#
                        )
                            .into_bytes(),
                    )
                }
                _ => ("404 Not Found", "text/plain", b"nope".to_vec()),
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

/// Spawn the worker against `base`, with a 0600 token file under `tmp`.
///
/// `out_dir` is `Some` only for the attachment leg; the id-contract test
/// deliberately runs without `KASTELLAN_WORKER_OUT` to keep its surface small.
fn spawn_worker(
    base: &str,
    tmp: &Path,
    out_dir: Option<&Path>,
) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    std::fs::create_dir_all(tmp).unwrap();
    let token_file = tmp.join("token");
    std::fs::write(&token_file, "e2e-token\n").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kastellan-worker-mail"));
    cmd.env("KASTELLAN_MAIL_ENDPOINT", base)
        .env("KASTELLAN_MAIL_TOKEN_FILE", &token_file)
        // Opt out of the prelude's Linux self-lockdown: this e2e exercises the
        // worker's application logic standalone, so it does not receive the
        // daemon-derived landlock RW set (which, in production, includes out/).
        // Without this, the unset KASTELLAN_LANDLOCK_RW yields an empty writable
        // set and the out/ write is denied on Linux (macOS has no landlock).
        .env("KASTELLAN_LANDLOCK_PROFILE", "none")
        .env("KASTELLAN_SECCOMP_PROFILE", "none")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(out) = out_dir {
        std::fs::create_dir_all(out).unwrap();
        cmd.env("KASTELLAN_WORKER_OUT", out);
    }
    let mut child = cmd.spawn().expect("spawn mail worker");
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

/// Send one JSON-RPC request line and read one response line.
fn rpc(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
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

/// A rejected call must come back as an INVALID_PARAMS error carrying `needle`.
///
/// Asserting the error is *present* first matters: the regression that counts is
/// the bad value being ACCEPTED, and a bare `["error"]["message"].as_str()
/// .unwrap_or_default()` then fails with "cursor must be named" — the opposite
/// diagnosis to the truth.
#[track_caller]
fn assert_rejected_with(resp: &serde_json::Value, needle: &str) {
    assert!(
        resp.get("error").is_some(),
        "the value must be REJECTED, not accepted; got {resp}"
    );
    assert_eq!(resp["error"]["code"], -32602, "must be INVALID_PARAMS: {resp}");
    let msg = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(msg.contains(needle), "expected {needle:?} in the advice; got {resp}");
}

#[test]
fn mail_worker_stdio_roundtrip_against_mock() {
    let (base, _mock) = spawn_mock();
    let tmp = std::env::temp_dir().join(format!("mail-e2e-{}", std::process::id()));
    let out_dir = tmp.join("out");
    let (mut child, mut stdin, mut stdout) = spawn_worker(&base, &tmp, Some(&out_dir));

    // 1. list_accounts → the mock's one account, id served as a string.
    let r = rpc(&mut stdin, &mut stdout, 1, "mail.list_accounts", serde_json::json!({}));
    assert_eq!(r["result"][0]["id"], "1", "resp: {r}");

    // 2. search → a hit under localmail's real "results" key, with the
    //    STRING message_id the real service emits.
    let r = rpc(&mut stdin, &mut stdout, 2, "mail.search", serde_json::json!({"query": "qantas"}));
    assert_eq!(r["result"]["results"][0]["message_id"], "7", "resp: {r}");

    // 3. get_attachment_text → localmail returns application/json {"text": …};
    // the worker must surface the inner text, not the JSON envelope as a string.
    let text_sha = "a".repeat(64);
    let r = rpc(&mut stdin, &mut stdout, 3, "mail.get_attachment_text", serde_json::json!({"sha256": text_sha}));
    assert_eq!(r["result"]["text"], "extracted booking text", "resp: {r}");

    // 4. get_attachment → original bytes written to out/, path returned, no bytes inline.
    let sha = "a".repeat(64);
    let r = rpc(
        &mut stdin,
        &mut stdout,
        4,
        "mail.get_attachment",
        serde_json::json!({"sha256": sha, "filename": "booking.pdf"}),
    );
    let path = r["result"]["path"].as_str().expect("path in result");
    assert!(std::path::Path::new(path).starts_with(&out_dir), "must be under out/: {path}");
    assert_eq!(std::fs::read(path).unwrap(), b"%PDF-1.7 fake booking");
    assert_eq!(r["result"]["content_type"], "application/pdf");
    assert!(r["result"].get("data_base64").is_none(), "no inline bytes");

    // 5. By position (#760): get_message numbers the attachment, and both
    //    attachment tools fetch it through the index route — the mock serves
    //    distinct bodies there, so reading them back proves which route ran.
    let r = rpc(&mut stdin, &mut stdout, 5, "mail.get_message", serde_json::json!({"message_id": "7"}));
    let index = r["result"]["attachments"][0]["index"].clone();
    assert_eq!(index, 0, "resp: {r}");
    let r = rpc(&mut stdin, &mut stdout, 6, "mail.get_attachment_text",
        serde_json::json!({"message_id": "7", "index": index}));
    assert_eq!(r["result"]["text"], "extracted by position", "resp: {r}");
    assert_eq!(r["result"]["filename"], "booking.pdf", "resp: {r}");
    assert!(r["result"]["next_offset"].is_null(), "resp: {r}");
    let r = rpc(&mut stdin, &mut stdout, 7, "mail.get_attachment",
        serde_json::json!({"message_id": "7", "index": 0}));
    let path = r["result"]["path"].as_str().expect("path in result");
    assert_eq!(std::fs::read(path).unwrap(), BY_POSITION_BYTES);
    assert_eq!(r["result"]["sha256"], CANNED_SHA, "resp: {r}");
    assert_eq!(r["result"]["index"], 0, "resp: {r}");

    // 6. unknown method → JSON-RPC error (-32601).
    let r = rpc(&mut stdin, &mut stdout, 8, "mail.nope", serde_json::json!({}));
    assert_eq!(r["error"]["code"], -32601, "resp: {r}");

    drop(stdin); // EOF → worker exits its stdio loop.
    let _ = child.wait();
    std::fs::remove_dir_all(&tmp).ok();
}

/// The #527 regression, reproduced end to end: take the `message_id` **exactly
/// as `mail.search` returned it** and hand it straight to `mail.get_message`.
///
/// That is what the planner does, and until this fix it failed with
/// `invalid type: string "7", expected i64` — 7 of the 14 live failures. Feeding
/// the value through rather than retyping it as a literal is the whole point of
/// the test: a hand-written `7` passes with or without the fix.
#[test]
fn a_message_id_taken_verbatim_from_a_search_hit_is_accepted() {
    let (base, _mock) = spawn_mock();
    let tmp = std::env::temp_dir().join(format!("mail-chain-{}", std::process::id()));
    let (mut child, mut stdin, mut stdout) = spawn_worker(&base, &tmp, None);

    let hit = rpc(&mut stdin, &mut stdout, 1, "mail.search", serde_json::json!({"query": "invoice"}));
    let id = hit["result"]["results"][0]["message_id"].clone();
    assert!(id.is_string(), "fixture must serve the live string shape, got {id}");

    // Verbatim — no parsing, no re-typing.
    let got = rpc(&mut stdin, &mut stdout, 2, "mail.get_message", serde_json::json!({"message_id": id}));
    assert!(
        got.get("error").is_none(),
        "get_message must accept the id search just returned; got {got}"
    );
    // The mock echoes back the id it was actually asked for, so this asserts the
    // id SURVIVED the round trip rather than reading back a canned constant.
    assert_eq!(got["result"]["id"], CANNED_MESSAGE_ID, "resp: {got}");

    // The other 7 live failures: a cursor and a placeholder must now come back
    // with text the planner can act on, since inner_loop feeds it the error.
    let bad = rpc(&mut stdin, &mut stdout, 3, "mail.get_message",
        serde_json::json!({"message_id": "ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0"}));
    assert_rejected_with(&bad, "next_cursor");

    let bad = rpc(&mut stdin, &mut stdout, 4, "mail.get_message",
        serde_json::json!({"message_id": "{{message_id}}"}));
    assert_rejected_with(&bad, "NO template substitution");

    drop(stdin);
    let _ = child.wait();
    std::fs::remove_dir_all(&tmp).ok();
}

/// #500, asserted BEHAVIOURALLY rather than by string-equality on a URL.
///
/// The other #500 tests check that the worker *sends* `?headers=full`, against a
/// fake that was handed that same string — they cannot catch "our reading of
/// localmail is wrong". This one asserts the thing the tool actually promises:
/// ask for full headers, get headers back. The mock reproduces the real
/// service's asymmetry (`headers` appears only for the exact `headers=full`
/// pair), so reverting `detail_path` to the old `?full_headers=true` spelling
/// fails here with a missing `headers` block — the production symptom, not a
/// string mismatch.
///
/// Since #702's review the block is a list of `{name, values}` rather than
/// localmail's name-keyed object (see `headers.rs`), so this also proves that
/// reshaping runs on the real stdio path.
#[test]
fn asking_for_full_headers_actually_returns_headers() {
    let (base, _mock) = spawn_mock();
    let tmp = std::env::temp_dir().join(format!("mail-headers-{}", std::process::id()));
    let (mut child, mut stdin, mut stdout) = spawn_worker(&base, &tmp, None);

    let compact = rpc(&mut stdin, &mut stdout, 1, "mail.get_message",
        serde_json::json!({"message_id": "7"}));
    assert!(
        compact["result"].get("headers").is_none(),
        "compact is localmail's default; the flag must not be sent: {compact}"
    );

    let full = rpc(&mut stdin, &mut stdout, 2, "mail.get_message",
        serde_json::json!({"message_id": "7", "full_headers": true}));
    assert_eq!(
        full["result"]["headers"],
        serde_json::json!([{"name": "Message-ID", "values": ["<canned@example.test>"]}]),
        "full_headers: true must produce the header list, names as values: {full}"
    );

    drop(stdin);
    let _ = child.wait();
    std::fs::remove_dir_all(&tmp).ok();
}
