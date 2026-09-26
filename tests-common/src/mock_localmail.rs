//! Canned-response mock of localmail's `/v1` REST API, serving the six
//! endpoints the mail worker hits in localmail's REAL response shapes (as #487
//! corrected them: search → `results`, attachment text → `application/json
//! {"text": …}`). Response SHAPES are pinned against real localmail by the
//! Mac-only contract test in `core/tests/mail_daemon_e2e.rs`.
//!
//! Also serves the **three** endpoints `workers/email-in` (the email fallback
//! channel's worker) hits — `GET /v1/changes?subscription=<name>`,
//! `GET /v1/messages/{id}?headers=full`, and `POST /v1/changes/ack` — so this
//! mock stays a faithful stand-in for worker-level email-in tests too (added
//! alongside the task-9 hermetic channel e2e, which itself does not use this
//! mock — that test's fake worker speaks JSON-RPC directly, with no localmail
//! HTTP involved at all). The message-detail route is shared with the mail
//! tool, which reads only `attachments`; `email-in` additionally reads
//! `from.address`, `body_text` and (only under `?headers=full`) `headers` —
//! see [`route`] for the source-confirmed shapes.
//!
//! Two spawn flavours, same request routing/response bodies, different
//! transport:
//! * [`spawn_mock_localmail`] — plain HTTP. Deliberate: it sidesteps the
//!   webpki-only TLS wall entirely (that only bites TLS), so the mail worker's
//!   DIRECT transport round-trips hermetically against it. It is NOT reachable
//!   via the force-routed transport (HTTPS-only).
//! * [`spawn_mock_localmail_tls`] — self-signed HTTPS, added for #491. It IS
//!   reachable via the force-routed transport, once the egress proxy is handed
//!   its cert as `upstream_extra_ca` (see `pins::build_upstream_client_config`)
//!   so the MITM's upstream re-origination leg trusts it.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The attachment sha the canned message advertises and the attachment
/// endpoints key on: the **real** sha256 of [`CANNED_ATTACHMENT_BYTES`], as
/// localmail's own is of the blob's bytes. It has to be — since #760 the mail
/// worker hashes bytes fetched by position and refuses any that disagree with
/// the listing, so a placeholder here would fail every message-resolved
/// `mail.get_attachment`. Pinned by a test in `mock_localmail/tests.rs`.
pub const CANNED_SHA256: &str =
    "8635c9b562b665d3ee8c3d02775c0745a9857ec29535e75715f6de2f3f9ded49";
/// Extracted text surfaced by `mail.get_attachment_text`.
pub const CANNED_ATTACHMENT_TEXT: &str = "NORTH COAST AREA HEALTH SERVICE invoice total 42.00";
/// Original-format bytes delivered by `mail.get_attachment`.
pub const CANNED_ATTACHMENT_BYTES: &[u8] = b"%PDF-1.4 canned attachment bytes";
/// The numeric message id the canned search/list hits reference.
///
/// **Every route serves it as a JSON string** (`.to_string()`) — localmail never
/// puts a bare number on the wire. Compare against `CANNED_MESSAGE_ID.to_string()`,
/// never against this const directly: `hit["message_id"] == CANNED_MESSAGE_ID` is
/// `false`, and believing otherwise is #527 in miniature.
pub const CANNED_MESSAGE_ID: i64 = 7;
/// The canned account id, as it appears on the wire: a STRING, like every other
/// id localmail emits. Single-sourced because this fixture's string-vs-number
/// disagreement with the live service is exactly what #527 was.
pub const CANNED_ACCOUNT_ID: &str = "1";
/// The canned account's name, paired with [`CANNED_ACCOUNT_ID`].
pub const CANNED_ACCOUNT_NAME: &str = "horst-gmail";
/// A realistic opaque paging token. Base64 of `d|2026-08-08T22:01:58+00:00|37474`,
/// copied from a live `/v1/messages` response. It is here because the live audit
/// log shows the planner pasting this value into `message_id` (3 of 14 failures) —
/// a `null` cursor cannot reproduce that, so the mock would hide it.
pub const CANNED_NEXT_CURSOR: &str = "ZHwyMDI2LTA4LTA4VDIyOjAxOjU4KzAwOjAwfDM3NDc0";
/// The canned message's From address. On the wire localmail wraps it in an
/// address OBJECT (`{"address", "name"}`), never a bare string — see [`route`].
/// Already lowercase, matching the peer `email-in` derives from it.
pub const CANNED_FROM_ADDRESS: &str = "billing@example.test";
/// The canned message's plain-text body. localmail names this field
/// `body_text` (not `body`); it is what becomes an inbound event's body.
pub const CANNED_BODY_TEXT: &str = "please find the invoice attached";
/// The canned message's RFC 5322 `Message-ID` header value, which `email-in`
/// turns into the inbound event's conversation id.
pub const CANNED_MESSAGE_ID_HEADER: &str = "<mid-7@example.test>";
/// authserv-id of the "our own MX" half of [`CANNED_AUTH_RESULTS`]. A consumer
/// that configures this as its trusted authserv-id gets `dmarc_pass: true`.
pub const CANNED_AUTHSERV_ID: &str = "mx.example.net";
/// The canned `Authentication-Results` header value: a genuine `dmarc=pass`
/// stamped by [`CANNED_AUTHSERV_ID`]. Only ever served under `?headers=full`.
pub const CANNED_AUTH_RESULTS: &str = "mx.example.net; dmarc=pass";

/// A live plain-HTTP localmail mock. Aborts its listener task on drop.
pub struct MockLocalmail {
    pub base_url: String,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for MockLocalmail {
    fn drop(&mut self) {
        if let Some(h) = self.join.take() {
            h.abort();
        }
    }
}

/// Bind an ephemeral loopback port and serve the six `/v1` endpoints. Every
/// request must carry a non-empty `Authorization: Bearer` header (asserted).
pub async fn spawn_mock_localmail() -> MockLocalmail {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            serve_localmail_conn(&mut sock).await;
        }
    });

    MockLocalmail { base_url, join: Some(join) }
}

/// Serve one localmail connection: read the request head (draining the declared
/// body so the close is a clean FIN, not an RST that truncates the client's
/// read), route it via [`route`], and write the response. Generic over the
/// stream so the plain-TCP and TLS spawns share exactly one implementation.
async fn serve_localmail_conn<S>(sock: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Read until end-of-headers, THEN drain the declared Content-Length body.
    // localmail's search is a POST; its body doesn't change the canned page,
    // but we must consume it before responding+closing — a socket closed with
    // unread inbound bytes is RST'd by the kernel, which can truncate the
    // response the client is mid-read on (the sibling `scripted_llm` drains
    // its body for exactly this reason). The request line + headers are all
    // we route on.
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 512];
    let head = loop {
        let n = match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => break None,
            Ok(n) => n,
        };
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_str = match std::str::from_utf8(&buf[..i]) {
                Ok(s) => s.to_owned(),
                Err(_) => break None,
            };
            // Consume the body so the close is a clean FIN, not an RST.
            let want = (i + 4) + content_length(&header_str);
            while buf.len() < want && buf.len() <= 64 * 1024 {
                match sock.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            break Some(header_str);
        }
        if buf.len() > 64 * 1024 {
            break None;
        }
    };
    let (status, ctype, body): (&str, &str, Vec<u8>) = match head.as_deref() {
        Some(h) => route(h),
        None => ("400 Bad Request", "text/plain", b"bad request".to_vec()),
    };
    let resp_head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(resp_head.as_bytes()).await;
    let _ = sock.write_all(&body).await;
    let _ = sock.flush().await;
    let _ = sock.shutdown().await;
}

/// A live **self-signed-HTTPS** localmail mock at `https://127.0.0.1:<port>`.
/// Returns the mock (aborts its listener on drop) and the cert PEM (the caller
/// writes it wherever the egress proxy's upstream extra CA must live). Serves the
/// identical `/v1` shapes as [`spawn_mock_localmail`] — the force-routed MITM path
/// can reach it once the proxy is given this cert as its upstream extra CA (#491).
///
/// Unlike the plain flavour, which serves connections sequentially in its accept
/// loop, this one spawns a task per connection (a stalled TLS handshake must not
/// wedge the next client). `MockLocalmail`'s drop aborts only the accept loop, so
/// an in-flight connection task can briefly outlive the mock — harmless for tests,
/// but don't assume the two flavours have identical teardown semantics.
pub async fn spawn_mock_localmail_tls() -> (MockLocalmail, String) {
    let (cert_der, key_der, cert_pem) = crate::tls_origin::generate_loopback_cert();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("build localmail tls server config");
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("https://127.0.0.1:{port}");

    let join = tokio::spawn(async move {
        loop {
            let (tcp, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let mut tls = match acceptor.accept(tcp).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                serve_localmail_conn(&mut tls).await;
            });
        }
    });

    (MockLocalmail { base_url, join: Some(join) }, cert_pem)
}

/// The request's `Content-Length` (0 when absent or unparseable). Used only to
/// know how many body bytes to drain before closing the connection.
fn content_length(head: &str) -> usize {
    for line in head.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

/// Which header shape a message-detail request-target asks for — localmail's
/// `headers` query parameter (`compact` when absent), read as the real
/// service reads it: by the exact pair, not a substring, so a client sending
/// some *other* parameter name (#500's `full_headers=true`) gets the same
/// header-less compact 200 a real localmail would give it.
///
/// `None` for a value localmail refuses: since localmail #381 an unknown mode
/// is a 400, where it used to be a silent compact 200.
fn header_mode(path: &str) -> Option<&str> {
    let query = path.split_once('?').map_or("", |(_, q)| q);
    let mode = query.split('&').find_map(|pair| pair.strip_prefix("headers=")).unwrap_or("compact");
    ["compact", "full", "list"].contains(&mode).then_some(mode)
}

/// localmail's text-route body since slice D (`api_minor` 2): one page plus
/// its paging fields. The canned text is short, so every window holds all
/// that is left of it and is the last page (`next_offset: null`).
///
/// `offset` is **echoed** from the request, as localmail does
/// (`text_window.py`): the mail worker refuses a page whose offset is not the
/// one it asked for, so a mock that always said `0` would hide a server
/// ignoring the parameter — or fail a worker that is right.
fn text_page_body(path: &str) -> String {
    let offset: usize = path
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("offset=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let total = CANNED_ATTACHMENT_TEXT.chars().count();
    let text: String = CANNED_ATTACHMENT_TEXT.chars().skip(offset).collect();
    serde_json::json!({
        "text": text,
        "offset": offset,
        "limit": 8000,
        "total": total,
        "next_offset": null
    })
    .to_string()
}

/// Pure request-line/headers → (status, content-type, body). Asserts a
/// non-empty bearer so the auth wiring is exercised, then routes by path.
fn route(head: &str) -> (&'static str, &'static str, Vec<u8>) {
    // Bearer presence (auth wiring). A request with no non-empty bearer is a 401.
    let has_bearer = head.lines().any(|l| {
        let mut p = l.splitn(2, ':');
        matches!((p.next(), p.next()), (Some(n), Some(v))
            if n.trim().eq_ignore_ascii_case("authorization")
               && v.trim().strip_prefix("Bearer ").map(|t| !t.trim().is_empty()).unwrap_or(false))
    });
    if !has_bearer {
        return ("401 Unauthorized", "text/plain", b"no bearer".to_vec());
    }
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");

    let json = |s: String| ("200 OK", "application/json", s.into_bytes());

    // /v1/messages/<id> exactly or with a query string — NOT a numeric prefix
    // (so id 7 does not also match 70..=79). Bound here so the if-chain below
    // stays a simple condition (clippy::blocks_in_conditions).
    let is_message_by_id = {
        let m = format!("/v1/messages/{CANNED_MESSAGE_ID}");
        path == m || path.starts_with(&format!("{m}?"))
    };

    // Order matters: the more specific /v1/changes/ack must be checked before
    // the more general /v1/changes prefix (an ack path also starts with it),
    // and both before the attachment/message paths below.
    // Message-scoped attachment routes (slice D, `api_minor` 2): the canned
    // message's one attachment sits at index 0. Anything else under the prefix
    // is localmail's shared 404.
    let by_index = format!("/v1/messages/{CANNED_MESSAGE_ID}/attachments/");
    let by_index_rest = path.split('?').next().and_then(|p| p.strip_prefix(&by_index));

    if path.starts_with("/v1/version") {
        // Unauthenticated on the real service (reached here only with a bearer,
        // which every client of this mock sends). The mail worker's version
        // gate (#760) asks before every tool that uses a slice D/E feature.
        json(serde_json::json!({
            "api_major": 1, "api_minor": 3, "server_version": "0.3.0",
            "build_hash": null, "build_source": "wheel", "version_source": "installed"
        }).to_string())
    } else if let Some(rest) = by_index_rest {
        match rest {
            "0/text" => json(text_page_body(path)),
            "0" => ("200 OK", "application/pdf", CANNED_ATTACHMENT_BYTES.to_vec()),
            _ => ("404 Not Found", "application/problem+json",
                  br#"{"type":"/problems/not-found","title":"Not found","status":404}"#.to_vec()),
        }
    } else if path.starts_with("/v1/changes/ack") {
        ("204 No Content", "text/plain", Vec::new())
    } else if path.starts_with("/v1/changes") {
        // Shape confirmed against the real localmail route (task-1-report.md's
        // "Final response shapes" — `message_id`, `next_cursor`, and the
        // embedded `account.id` are all STRINGS on the wire, and so is every
        // other id on every localmail route, including `/v1/accounts`' own
        // `id` below (measured live 2026-08-09); `email-in`'s handler reads
        // `message_id` via `.as_str()` and silently skips anything else, so a
        // number here would swallow every message with no error at all.
        json(serde_json::json!({
            "new_messages": [{
                "message_id": CANNED_MESSAGE_ID.to_string(),
                "subject": "invoice",
                "from": {"address": "billing@example.test", "name": "Billing"},
                "date": "2026-07-28T00:00:00+00:00",
                "account": {"id": CANNED_ACCOUNT_ID, "name": CANNED_ACCOUNT_NAME}
            }],
            // Deliberately a plain id rather than [`CANNED_NEXT_CURSOR`]:
            // `email-in` round-trips this value straight back into the next
            // `/v1/changes` request, and the ack path keys on it, so a short
            // recognisable token keeps those tests readable. The opaque-blob
            // shape matters only where the planner can SEE the cursor and paste
            // it into an id — the search and list routes, which use it.
            "next_cursor": CANNED_MESSAGE_ID.to_string()
        }).to_string())
    } else if path.starts_with("/v1/search") {
        // Shapes measured against the live localmail 2026-08-09: `message_id` is
        // a STRING on this route, exactly as on /v1/changes above. The mock
        // previously served a NUMBER here, which is why a hermetic
        // search -> get_message chain passed while 7 of the 26 live
        // `mail.get_message` dispatches failed on exactly this (of 14 failures
        // in all, across three causes — #527): the worker's `i64` agreed with
        // the mock and not with the service. `results` (not `hits`) is correct
        // and stays.
        //
        // The hit is the **projected** shape (slice E, `api_minor` 3): the mail
        // worker sends `fields` = `search_params::HIT_FIELDS` on every search,
        // so real localmail answers it with exactly those keys — including a
        // plain-text `snippet` rather than the default `snippet_html`. This
        // mock does not read the request body, so it serves that shape
        // unconditionally; the mail worker is the only client of this route.
        //
        // `next_cursor` deliberately still serves the base64 `CANNED_NEXT_CURSOR`
        // shape, not the hex format /v1/search actually uses live (e.g.
        // `"6f6dd7a731…"` — one of the three live cursor-paste failures was
        // exactly a hex cursor). Not modelled: nothing downstream of this route
        // parses cursor bytes, only round-trips the opaque string, so the two
        // formats are behaviourally interchangeable for every consumer this mock
        // stands in for. Flagged here rather than silently collapsed.
        json(serde_json::json!({
            "results": [{
                "message_id": CANNED_MESSAGE_ID.to_string(),
                "account": {"id": CANNED_ACCOUNT_ID, "name": serde_json::Value::Null},
                "subject": "invoice",
                "from": {"address": CANNED_FROM_ADDRESS, "name": "Billing"},
                "date": "2026-07-28T00:00:00+00:00",
                "has_attachments": true,
                "snippet": "…"
            }],
            "next_cursor": CANNED_NEXT_CURSOR
        }).to_string())
    } else if path.starts_with("/v1/accounts") {
        // Measured live 2026-08-09: `id` is a STRING here too.
        json(serde_json::json!([
            {"id": CANNED_ACCOUNT_ID, "name": CANNED_ACCOUNT_NAME}
        ]).to_string())
    } else if path.contains("/text") && path.starts_with("/v1/attachments/") {
        json(text_page_body(path))
    } else if path.starts_with("/v1/attachments/") {
        ("200 OK", "application/pdf", CANNED_ATTACHMENT_BYTES.to_vec())
    } else if is_message_by_id {
        // Shape confirmed against localmail's own source
        // (`localmail/src/localmail/api/messages.py::get_message`), because
        // `email-in`'s `build_event` reads three fields the earlier
        // mail-tool-only shape got wrong or omitted:
        //   * `from` is an address OBJECT (`_address()` → `{"address","name"}`),
        //     NOT a bare string. `build_event` reads `from.address`, so a bare
        //     string yields `None` — the message becomes a `skipped` entry and
        //     never an inbound event, silently.
        //   * the plain-text body is `body_text`, not `body`.
        //   * `id` is a STRING (`"id": str(mid)`), not a number — the same
        //     numeric-vs-string trap `changes_returns_message_id_and_next_cursor_as_strings`
        //     already guards on the `/v1/changes` route, and the shape
        //     `email-in`'s own message-detail fixtures use (`handler/tests.rs`:
        //     `"id": "7"`; `client.rs`'s `message_detail_requests_full_headers`:
        //     `"id":"42"`).
        //     No PRODUCTION code reads this field (the only mail-adjacent `id`
        //     reads are fixtures and the real localmail LIST route), so it is
        //     fidelity rather than a behaviour fix — but it is no longer
        //     unread: since #536, `workers/mail/tests/mail_e2e.rs` asserts on it
        //     to check the id survived a search → get_message round trip.
        //   * `headers` exists ONLY when the request carried `?headers=full`
        //     or `?headers=list` (see `header_mode`). Under `full` it is an
        //     object whose every value is an ARRAY of that exact-cased
        //     header's occurrences in wire order (email-in reads this); under
        //     `list` (localmail #381, `api_minor` 1) it is one
        //     `{name, value}` per occurrence in wire order (the mail worker
        //     reads this, #760). Gating it here keeps the mock honest about a
        //     real trap: a client that asks with the wrong query spelling gets
        //     a 200 with no headers at all, hence no `Authentication-Results`,
        //     hence a fail-closed DMARC verdict for every message — which
        //     looks like a delivery bug.
        let mut msg = serde_json::json!({
            "id": CANNED_MESSAGE_ID.to_string(),
            "subject": "invoice",
            "from": {"address": CANNED_FROM_ADDRESS, "name": "Billing"},
            "date": "2026-07-28T00:00:00+00:00",
            "body_text": CANNED_BODY_TEXT,
            "attachments": [{
                "filename": "invoice.pdf",
                "sha256": CANNED_SHA256,
                "content_type": "application/pdf",
                "size": CANNED_ATTACHMENT_BYTES.len()
            }]
        });
        match header_mode(path) {
            Some("full") => {
                msg["headers"] = serde_json::json!({
                    "Message-ID": [CANNED_MESSAGE_ID_HEADER],
                    "Authentication-Results": [CANNED_AUTH_RESULTS],
                });
            }
            Some("list") => {
                msg["headers"] = serde_json::json!([
                    {"name": "Message-ID", "value": CANNED_MESSAGE_ID_HEADER},
                    {"name": "Authentication-Results", "value": CANNED_AUTH_RESULTS},
                ]);
            }
            Some(_) => {}
            None => {
                return (
                    "400 Bad Request",
                    "application/problem+json",
                    br#"{"detail":"headers must be one of compact, full, list"}"#.to_vec(),
                )
            }
        }
        json(msg.to_string())
    } else if path.starts_with("/v1/messages") {
        // Measured live 2026-08-09: the list route keys rows under `messages`
        // (NOT `results` — that is the search route) and serves `message_id` as
        // a STRING. Both differed from this mock.
        json(serde_json::json!({
            "messages": [{
                "message_id": CANNED_MESSAGE_ID.to_string(),
                "subject": "invoice",
                "account": {"id": CANNED_ACCOUNT_ID, "name": CANNED_ACCOUNT_NAME}
            }],
            "next_cursor": CANNED_NEXT_CURSOR
        }).to_string())
    } else {
        ("404 Not Found", "text/plain", b"no such endpoint".to_vec())
    }
}

#[cfg(test)]
mod tests;
