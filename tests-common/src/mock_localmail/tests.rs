//! Unit tests for [`super`] — the mock's routes and response shapes.

use super::*;
use std::io::{Read, Write};
use std::net::TcpStream;

/// Drive one raw GET /v1/accounts against the mock and confirm it answers
/// with the localmail accounts array shape (a JSON list).
#[test]
fn serves_accounts_as_a_json_array() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(spawn_mock_localmail());
    let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
    let mut s = TcpStream::connect(&addr).unwrap();
    write!(
        s,
        "GET /v1/accounts HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
    let body = resp.split("\r\n\r\n").nth(1).unwrap();
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(v.is_array(), "accounts must be a JSON array, got {v}");
}

/// The attachment-text endpoint must return the localmail envelope shape
/// `application/json {"text": …}` (the #487 contract), NOT plain text.
#[test]
fn attachment_text_is_json_text_envelope() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(spawn_mock_localmail());
    let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
    let mut s = TcpStream::connect(&addr).unwrap();
    write!(
        s,
        "GET /v1/attachments/{CANNED_SHA256}/text HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.contains("application/json"), "content-type: {resp}");
    let body = resp.split("\r\n\r\n").nth(1).unwrap();
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["text"], CANNED_ATTACHMENT_TEXT);
}

/// `GET /v1/changes` must return `message_id` (and `next_cursor`) as JSON
/// STRINGS, matching the real localmail contract confirmed in
/// `task-1-report.md`'s "Final response shapes" — `workers/email-in`'s
/// handler reads `message_id` via `serde_json::Value::as_str`, which
/// returns `None` for a JSON number and silently SKIPS the message
/// (`handler.rs::poll`'s `let Some(message_id) = … else { continue }`),
/// never erroring loudly. `is_string` is the assertion that actually
/// catches that regression — a looser "the field is present" check would
/// not have. Chosen over a full `workers/email-in`-driven e2e (this
/// crate's own test module, not a new integration test elsewhere) because
/// `kastellan-worker-email-in` currently has zero dev-dependencies
/// (neither `tokio` nor `kastellan-tests-common`), and pulling both in
/// just to exercise one mock route is disproportionate to the fix; this
/// still fails loudly on exactly the bug that shipped.
#[test]
fn changes_returns_message_id_and_next_cursor_as_strings() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(spawn_mock_localmail());
    let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
    let mut s = TcpStream::connect(&addr).unwrap();
    write!(
        s,
        "GET /v1/changes?subscription=test HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
    let body = resp.split("\r\n\r\n").nth(1).unwrap();
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    let messages = v["new_messages"].as_array().expect("new_messages must be an array");
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]["message_id"].is_string(),
        "message_id must be a JSON string, not a number, or email-in's handler silently \
         drops every message via .as_str() == None; got {}",
        messages[0]["message_id"]
    );
    assert_eq!(messages[0]["message_id"], CANNED_MESSAGE_ID.to_string());
    assert!(
        v["next_cursor"].is_string(),
        "next_cursor must be a JSON string; got {}",
        v["next_cursor"]
    );
}

/// `POST /v1/changes/ack` must answer `204 No Content` with an empty
/// body — the real contract confirmed in `task-1-report.md`;
/// `EmailClient::ack` never parses the body, only checks the status.
#[test]
fn changes_ack_is_204_with_empty_body() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(spawn_mock_localmail());
    let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
    let mut s = TcpStream::connect(&addr).unwrap();
    let payload = br#"{"subscription":"test","cursor":"7"}"#;
    write!(
        s,
        "POST /v1/changes/ack HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    )
    .unwrap();
    s.write_all(payload).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp);
    assert!(resp.starts_with("HTTP/1.1 204"), "resp: {resp}");
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    assert!(body.is_empty(), "204 must have an empty body, got: {body:?}");
}

/// One raw `GET /v1/messages/{id}{query}` against the mock; returns the
/// status line's code and the body text.
fn message_detail_raw(query: &str) -> (String, String) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(spawn_mock_localmail());
    let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
    let mut s = TcpStream::connect(&addr).unwrap();
    write!(
        s,
        "GET /v1/messages/{CANNED_MESSAGE_ID}{query} HTTP/1.1\r\nHost: x\r\n\
         Authorization: Bearer t\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp.split_whitespace().nth(1).unwrap_or("").to_string();
    (status, resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

/// [`message_detail_raw`], asserting a 200 and parsing the body.
fn message_detail(query: &str) -> serde_json::Value {
    let (status, body) = message_detail_raw(query);
    assert_eq!(status, "200", "body: {body}");
    serde_json::from_str(&body).unwrap()
}

/// `GET /v1/messages/{id}` must serve `from` as an address OBJECT and the
/// plain-text body as `body_text` — the real localmail contract
/// (`api/messages.py::get_message`). `email-in`'s `build_event` reads
/// `from.address`; against a bare `"from": "a@b"` string it returns `None`
/// and the message becomes a `skipped` entry instead of an inbound event,
/// with no error anywhere. `is_string()` on the nested address is the
/// assertion that catches that regression — "the field is present" would not.
#[test]
fn message_detail_serves_from_as_an_address_object_and_body_text() {
    let v = message_detail("");
    assert!(
        v["from"]["address"].is_string(),
        "from must be an address object, not a bare string, or email-in's build_event \
         silently skips every message; got from = {}",
        v["from"]
    );
    assert_eq!(v["from"]["address"], CANNED_FROM_ADDRESS);
    assert_eq!(v["body_text"], CANNED_BODY_TEXT);
    // Same numeric-vs-string trap `changes_returns_message_id_and_next_cursor_as_strings`
    // guards one route over: localmail serves `"id": str(mid)`.
    assert!(v["id"].is_string(), "id must be a JSON string; got {}", v["id"]);
}

/// `headers` is served only under `?headers=full`, exactly as localmail
/// gates it (`full_headers=(headers == "full")`), and each value is an
/// ARRAY of that header's wire occurrences. Both halves matter: without the
/// gate the mock would hide the "wrong query spelling ⇒ no
/// Authentication-Results ⇒ every message fails DMARC closed" trap, and
/// without the array shape `email-in`'s `header_values` would fall through
/// to its defensive string arm rather than the real path.
#[test]
fn message_detail_gates_headers_on_the_full_query_pair() {
    let compact = message_detail("");
    assert!(
        compact.get("headers").is_none(),
        "a compact request must get NO headers key; got {}",
        compact
    );

    let full = message_detail("?headers=full");
    let auth = full["headers"]["Authentication-Results"]
        .as_array()
        .expect("Authentication-Results must be an ARRAY of wire occurrences");
    assert_eq!(auth, &vec![serde_json::json!(CANNED_AUTH_RESULTS)]);
    assert_eq!(
        full["headers"]["Message-ID"],
        serde_json::json!([CANNED_MESSAGE_ID_HEADER])
    );
}

/// `?headers=list` serves one `{name, value}` per occurrence in wire order —
/// localmail #381's shape, which the mail worker asks for (#760) and checks
/// entry by entry, so the mock must serve exactly those two keys.
#[test]
fn message_detail_serves_the_per_occurrence_header_list() {
    let list = message_detail("?headers=list");
    assert_eq!(
        list["headers"],
        serde_json::json!([
            {"name": "Message-ID", "value": CANNED_MESSAGE_ID_HEADER},
            {"name": "Authentication-Results", "value": CANNED_AUTH_RESULTS},
        ])
    );
}

/// Since localmail #381 an unknown mode is a 400, not a silent compact 200 —
/// and #500's old *parameter name* is still ignored, not refused.
#[test]
fn message_detail_refuses_an_unknown_header_mode_but_ignores_an_unknown_parameter() {
    let (status, body) = message_detail_raw("?headers=bogus");
    assert_eq!(status, "400", "body: {body}");
    assert!(message_detail("?full_headers=true").get("headers").is_none());
}

/// A request with no bearer is refused (auth wiring is exercised).
#[test]
fn missing_bearer_is_401() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt.block_on(spawn_mock_localmail());
    let addr = mock.base_url.strip_prefix("http://").unwrap().to_string();
    let mut s = TcpStream::connect(&addr).unwrap();
    write!(s, "GET /v1/accounts HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 401"), "no-bearer must 401, got: {resp}");
}

/// The TLS mock serves the same `/v1/search` `results` shape as the plain mock,
/// over TLS, to a client trusting only the returned cert — the exact trust path
/// the force-routed MITM e2e relies on (proxy upstream extra CA), without a sandbox.
#[test]
fn tls_mock_serves_search_results_over_tls() {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, ServerName};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (mock, cert_pem) = spawn_mock_localmail_tls().await;
        let port: u16 = mock.base_url.rsplit(':').next().unwrap().parse().unwrap();

        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap()).unwrap();
        let cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(std::sync::Arc::new(cfg));

        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let sni = ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into());
        let mut tls = connector.connect(sni, tcp).await.expect("tls handshake");
        tls.write_all(
            b"POST /v1/search HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer t\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n",
        ).await.unwrap();
        let mut resp = Vec::new();
        tls.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 200"), "resp: {resp}");
        let body = resp.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert!(v["results"].is_array(), "expected results array, got {v}");
    });
}

/// Drive the pure router with a minimal well-formed request head.
/// `route` refuses a request with no non-empty bearer, so one is supplied.
fn routed(request_line: &str) -> serde_json::Value {
    let head = format!("{request_line}\r\nHost: x\r\nAuthorization: Bearer t\r\n");
    let (status, ctype, body) = route(&head);
    assert!(status.starts_with("200"), "unexpected status {status} for {request_line}");
    assert_eq!(ctype, "application/json", "for {request_line}");
    serde_json::from_slice(&body).expect("json body")
}

/// `/v1/search` must serve `message_id` as a JSON string. The mock served a
/// NUMBER until 2026-08-09, which is precisely why no hermetic test caught
/// #527: `mail.get_message` takes an `i64`, so the mock agreed with the
/// worker while the real service disagreed with both.
#[test]
fn search_returns_message_id_as_a_string() {
    let v = routed("POST /v1/search HTTP/1.1");
    assert!(
        v["results"][0]["message_id"].is_string(),
        "search message_id must be a JSON string (live localmail serves \"20973\"); got {}",
        v["results"][0]["message_id"]
    );
}

/// The list route keys rows under `messages` and serves string ids. It used
/// `results` + a number, disagreeing with the live service on both counts.
#[test]
fn list_messages_keys_rows_under_messages_with_string_ids() {
    let v = routed("GET /v1/messages?limit=50 HTTP/1.1");
    assert!(
        v["messages"].is_array(),
        "list route must key rows under `messages` (that is the live shape; \
         `results` is the SEARCH route); got keys {:?}",
        v.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
    assert!(
        v["messages"][0]["message_id"].is_string(),
        "list message_id must be a JSON string; got {}",
        v["messages"][0]["message_id"]
    );
}

/// `/v1/accounts` serves `id` as a string, like every other id localmail emits.
#[test]
fn accounts_return_id_as_a_string() {
    let v = routed("GET /v1/accounts HTTP/1.1");
    assert!(
        v[0]["id"].is_string(),
        "account id must be a JSON string; got {}",
        v[0]["id"]
    );
}

/// #760: the mail worker refuses a localmail older than API 1.3, so the
/// mock must claim one it accepts — and in localmail's own field types.
#[test]
fn version_reports_an_api_the_mail_worker_accepts() {
    let v = routed("GET /v1/version HTTP/1.1");
    assert_eq!(v["api_major"], 1);
    assert!(v["api_minor"].as_u64().is_some_and(|m| m >= 3), "{v}");
}

/// Slice D: text comes with its paging fields on both text routes.
#[test]
fn both_text_routes_serve_a_paged_envelope() {
    for line in [
        format!("GET /v1/attachments/{CANNED_SHA256}/text?offset=0&limit=8000 HTTP/1.1"),
        format!("GET /v1/messages/{CANNED_MESSAGE_ID}/attachments/0/text?offset=0&limit=8000 HTTP/1.1"),
    ] {
        let v = routed(&line);
        assert_eq!(v["text"], CANNED_ATTACHMENT_TEXT, "{line}");
        assert_eq!(v["offset"], 0, "{line}");
        assert!(v["total"].is_u64() && v["next_offset"].is_null(), "{line}: {v}");
    }
}

/// The canned message has one attachment, so index 0 serves its bytes and
/// index 1 is localmail's shared 404.
#[test]
fn the_index_route_serves_position_zero_and_404s_past_the_end() {
    let head = |t: &str| format!("GET {t} HTTP/1.1\r\nAuthorization: Bearer t\r\n");
    let (status, ctype, body) = route(&head(&format!("/v1/messages/{CANNED_MESSAGE_ID}/attachments/0")));
    assert!(status.starts_with("200") && ctype == "application/pdf");
    assert_eq!(body, CANNED_ATTACHMENT_BYTES);
    let (status, _, _) = route(&head(&format!("/v1/messages/{CANNED_MESSAGE_ID}/attachments/1")));
    assert!(status.starts_with("404"), "{status}");
}

/// `CANNED_SHA256` is the real hash of the canned bytes, as localmail's is of
/// a blob's. The mail worker checks bytes fetched by position against it
/// (#760), so a placeholder would fail every message-resolved fetch.
#[test]
fn the_canned_sha_is_the_hash_of_the_canned_bytes() {
    use sha2::{Digest, Sha256};
    assert_eq!(format!("{:x}", Sha256::digest(CANNED_ATTACHMENT_BYTES)), CANNED_SHA256);
}

/// The text routes echo the requested `offset`, as localmail does; the mail
/// worker refuses a page whose offset is not the one it asked for.
#[test]
fn text_routes_echo_the_requested_offset() {
    let line = format!(
        "GET /v1/messages/{CANNED_MESSAGE_ID}/attachments/0/text?offset=5&limit=8000 HTTP/1.1"
    );
    let v = routed(&line);
    assert_eq!(v["offset"], 5, "{v}");
    let rest: String = CANNED_ATTACHMENT_TEXT.chars().skip(5).collect();
    assert_eq!(v["text"], rest, "{v}");
    assert!(v["next_offset"].is_null(), "{v}");
}

/// A search hit carries exactly the keys the mail worker projects to — the
/// shape real localmail serves it (the live gate pins that side).
#[test]
fn search_hits_are_the_projected_shape() {
    let v = routed("POST /v1/search HTTP/1.1");
    let mut got: Vec<&str> =
        v["results"][0].as_object().unwrap().keys().map(String::as_str).collect();
    got.sort_unstable();
    assert_eq!(
        got,
        ["account", "date", "from", "has_attachments", "message_id", "snippet", "subject"]
    );
}
