//! JSON-RPC dispatch for the six read-only `mail.*` tools. Each arm validates
//! params, calls the localmail REST client, and maps failures to `RpcError`.
//! Attachments come back either as extracted text (`get_attachment_text`) or as
//! original-format files written to the task workspace `out/` (`get_attachment`).

use std::path::Path;

use kastellan_protocol::{codes, server::Handler, RpcError};

use crate::client::{MailClient, MailError};

pub struct MailHandler {
    client: MailClient,
}

impl MailHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self { client: MailClient::from_env()? })
    }

    #[cfg(test)]
    pub fn with_client(client: MailClient) -> Self {
        Self { client }
    }

    fn search(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            query: String,
            #[serde(default)]
            filters: Option<serde_json::Value>,
            #[serde(default)]
            sort: Option<String>,
            #[serde(default)]
            limit: Option<u32>,
            #[serde(default)]
            cursor: Option<String>,
        }
        let p: P = parse_params(params)?;
        let mut body = serde_json::json!({ "query": p.query });
        if let Some(f) = p.filters {
            body["filters"] = f;
        }
        if let Some(s) = p.sort {
            body["sort"] = serde_json::json!(s);
        }
        if let Some(l) = p.limit {
            body["limit"] = serde_json::json!(l);
        }
        if let Some(c) = p.cursor {
            body["cursor"] = serde_json::json!(c);
        }
        // `smart` (LLM query rewrite) deliberately never set — workers do not
        // call the LLM. The planner already decomposes/rewrites queries.
        self.client.post_json("/v1/search", &body).map_err(mail_err_to_rpc)
    }

    fn get_message(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            message_id: i64,
            #[serde(default)]
            full_headers: bool,
        }
        let p: P = parse_params(params)?;
        let path = format!("/v1/messages/{}?full_headers={}", p.message_id, p.full_headers);
        self.client.get_json(&path).map_err(mail_err_to_rpc)
    }

    fn list_messages(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            #[serde(default)]
            account_ids: Option<Vec<i64>>,
            #[serde(default)]
            folder_ids: Option<Vec<i64>>,
            #[serde(default)]
            limit: Option<u32>,
            #[serde(default)]
            cursor: Option<String>,
        }
        let p: P = parse_params(params)?;
        let mut q: Vec<String> = Vec::new();
        if let Some(a) = &p.account_ids {
            q.push(format!("account_ids={}", join_ids(a)));
        }
        if let Some(f) = &p.folder_ids {
            q.push(format!("folder_ids={}", join_ids(f)));
        }
        if let Some(l) = p.limit {
            q.push(format!("limit={l}"));
        }
        if let Some(c) = &p.cursor {
            q.push(format!("cursor={}", urlencode(c)));
        }
        let path = if q.is_empty() {
            "/v1/messages".to_string()
        } else {
            format!("/v1/messages?{}", q.join("&"))
        };
        self.client.get_json(&path).map_err(mail_err_to_rpc)
    }

    fn list_accounts(&self) -> Result<serde_json::Value, RpcError> {
        self.client.get_json("/v1/accounts").map_err(mail_err_to_rpc)
    }

    fn get_attachment_text(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            sha256: String,
        }
        let p: P = parse_params(params)?;
        validate_sha256(&p.sha256).map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
        // `get_bytes` (the higher attachment cap, not the JSON cap) — extracted
        // text of a large document can exceed the JSON-response ceiling.
        let (_ct, bytes) = self
            .client
            .get_bytes(&format!("/v1/attachments/{}/text", p.sha256))
            .map_err(mail_err_to_rpc)?;
        // localmail returns `application/json {"text": "..."}`; surface the inner
        // text so the agent gets the extracted content, not a JSON envelope
        // double-encoded as a string. Fall back to the raw body for a non-JSON
        // response (defensive — the API contract is JSON, but this keeps a
        // plain-text body usable rather than failing).
        let text = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
        Ok(serde_json::json!({ "sha256": p.sha256, "text": text }))
    }

    fn get_attachment(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            sha256: String,
            #[serde(default)]
            filename: Option<String>,
        }
        let p: P = parse_params(params)?;
        validate_sha256(&p.sha256).map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
        let out_dir = std::env::var("KASTELLAN_WORKER_OUT").map_err(|_| {
            RpcError::new(
                codes::OPERATION_FAILED,
                "no task output dir (KASTELLAN_WORKER_OUT unset) — attachment delivery unavailable"
                    .to_string(),
            )
        })?;
        let (content_type, bytes) = self
            .client
            .get_bytes(&format!("/v1/attachments/{}", p.sha256))
            .map_err(mail_err_to_rpc)?;
        let name = safe_attachment_name(p.filename.as_deref(), &p.sha256);
        let dir = Path::new(&out_dir);
        let dest = dir.join(&name);
        // Per-process-unique .partial so an interrupted write or two concurrent
        // same-name fetches never share/clobber the scratch file (M-1).
        let partial = dir.join(format!(".{}.{name}.partial", std::process::id()));
        std::fs::write(&partial, &bytes).map_err(|e| {
            // Best-effort: reclaim any truncated scratch file so it neither
            // lingers nor blocks the runner's empty-dir prune.
            let _ = std::fs::remove_file(&partial);
            RpcError::new(codes::OPERATION_FAILED, format!("write attachment: {e}"))
        })?;
        std::fs::rename(&partial, &dest).map_err(|e| {
            // Rename failed → the .partial is orphaned; reclaim it (same reason).
            let _ = std::fs::remove_file(&partial);
            RpcError::new(codes::OPERATION_FAILED, format!("finalize attachment: {e}"))
        })?;
        Ok(serde_json::json!({
            "sha256": p.sha256,
            "filename": name,
            "content_type": content_type,
            "size": bytes.len(),
            "path": dest.to_string_lossy(),
        }))
    }
}

impl Handler for MailHandler {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        match method {
            "mail.search" => self.search(params),
            "mail.get_message" => self.get_message(params),
            "mail.list_messages" => self.list_messages(params),
            "mail.list_accounts" => self.list_accounts(),
            "mail.get_attachment_text" => self.get_attachment_text(params),
            "mail.get_attachment" => self.get_attachment(params),
            _ => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            )),
        }
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))
}

fn mail_err_to_rpc(e: MailError) -> RpcError {
    match e {
        MailError::BadParams(m) => RpcError::new(codes::INVALID_PARAMS, m),
        MailError::Upstream { status: 401 | 403, .. } => RpcError::new(
            codes::POLICY_DENIED,
            "localmail auth/permission denied (check token / account ACL)".to_string(),
        ),
        MailError::Upstream { status, body } => {
            RpcError::new(codes::OPERATION_FAILED, format!("localmail {status}: {body}"))
        }
        MailError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("transport: {m}"))
        }
    }
}

/// Require exactly 64 lowercase hex chars — prevents any path traversal or
/// injection through the `{sha256}` URL segment.
fn validate_sha256(s: &str) -> Result<(), String> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(())
    } else {
        Err(format!("sha256 must be 64 lowercase hex chars, got {:?}", s.chars().take(8).collect::<String>()))
    }
}

/// Collision- and traversal-safe filename under `out/`: take only the final
/// path component of the requested name, keep `[A-Za-z0-9._-]`, drop leading
/// dots, then prefix the first 12 sha256 chars so two messages sharing bytes
/// under different names never clobber one another.
fn safe_attachment_name(requested: Option<&str>, sha256: &str) -> String {
    let base = requested
        .and_then(|r| Path::new(r).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    let stem = if cleaned.is_empty() { "attachment" } else { cleaned };
    let prefix: String = sha256.chars().take(12).collect();
    format!("{prefix}_{stem}")
}

fn join_ids(v: &[i64]) -> String {
    v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")
}

/// Percent-encode an opaque query value (the pagination cursor).
fn urlencode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::{HttpGet, RawResponse};
    use url::Url;

    fn client_with(transport: Box<dyn HttpGet>) -> MailClient {
        MailClient::for_test(Url::parse("http://127.0.0.1:8000").unwrap(), "tok".into(), transport)
    }

    fn json_resp(body: &[u8]) -> RawResponse {
        RawResponse { status: 200, location: None, content_type: "application/json".into(), body: body.to_vec() }
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        // Build via for_test with a transport that is never called.
        struct Never;
        impl HttpGet for Never {
            fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
            fn transport_kind(&self) -> &'static str { "never" }
        }
        let mut h = MailHandler::with_client(client_with(Box::new(Never)));
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    // --- mail.search: POSTs the query, never sets `smart` ---
    struct SearchFake;
    impl HttpGet for SearchFake {
        fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "fake" }
        fn post_authed(&self, url: &Url, bearer: &str, ct: &str, body: &[u8], _m: usize) -> Result<RawResponse, String> {
            assert_eq!(bearer, "tok");
            assert_eq!(ct, "application/json");
            assert!(url.path().ends_with("/v1/search"), "path {}", url.path());
            let s = String::from_utf8_lossy(body);
            assert!(s.contains("qantas"), "body missing query: {s}");
            assert!(!s.contains("smart"), "body must not carry smart: {s}");
            // Real localmail keys results under "results" (not "hits").
            Ok(json_resp(br#"{"results":[],"next_cursor":null}"#))
        }
    }

    #[test]
    fn search_posts_query_without_smart() {
        let mut h = MailHandler::with_client(client_with(Box::new(SearchFake)));
        let out = h.call("mail.search", serde_json::json!({"query": "qantas"})).unwrap();
        assert!(out["results"].is_array());
    }

    // --- GET path assertions for get_message / list_messages / list_accounts ---
    struct PathFake(&'static str);
    impl HttpGet for PathFake {
        fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "fake" }
        fn get_authed(&self, url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
            let got = match url.query() {
                Some(q) => format!("{}?{}", url.path(), q),
                None => url.path().to_string(),
            };
            assert_eq!(got, self.0, "unexpected request path");
            Ok(json_resp(br#"{"ok":true}"#))
        }
    }

    #[test]
    fn get_message_builds_path() {
        let mut h = MailHandler::with_client(client_with(Box::new(PathFake("/v1/messages/5?full_headers=false"))));
        h.call("mail.get_message", serde_json::json!({"message_id": 5})).unwrap();
    }

    #[test]
    fn list_messages_builds_query() {
        let mut h = MailHandler::with_client(client_with(Box::new(PathFake("/v1/messages?limit=10"))));
        h.call("mail.list_messages", serde_json::json!({"limit": 10})).unwrap();
    }

    #[test]
    fn list_accounts_builds_path() {
        let mut h = MailHandler::with_client(client_with(Box::new(PathFake("/v1/accounts"))));
        h.call("mail.list_accounts", serde_json::json!({})).unwrap();
    }

    // --- get_attachment_text returns text ---
    struct TextFake;
    impl HttpGet for TextFake {
        fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "fake" }
        fn get_authed(&self, url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
            assert!(url.path().ends_with("/text"), "path {}", url.path());
            // Real localmail returns application/json `{"text": "..."}`, NOT
            // text/plain — the worker must surface the inner text, not the envelope.
            Ok(RawResponse {
                status: 200,
                location: None,
                content_type: "application/json".into(),
                body: br#"{"text":"extracted body"}"#.to_vec(),
            })
        }
    }

    #[test]
    fn get_attachment_text_returns_text() {
        let mut h = MailHandler::with_client(client_with(Box::new(TextFake)));
        let out = h.call("mail.get_attachment_text", serde_json::json!({"sha256": "a".repeat(64)})).unwrap();
        assert_eq!(out["text"], "extracted body");
    }

    /// A non-JSON `/text` body (defensive fallback) is surfaced verbatim.
    struct PlainTextFake;
    impl HttpGet for PlainTextFake {
        fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "fake" }
        fn get_authed(&self, _url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
            Ok(RawResponse { status: 200, location: None, content_type: "text/plain".into(), body: b"raw text".to_vec() })
        }
    }

    #[test]
    fn get_attachment_text_falls_back_to_raw_for_non_json() {
        let mut h = MailHandler::with_client(client_with(Box::new(PlainTextFake)));
        let out = h.call("mail.get_attachment_text", serde_json::json!({"sha256": "a".repeat(64)})).unwrap();
        assert_eq!(out["text"], "raw text");
    }

    /// Valid JSON but without a `text` key → surfaced verbatim (same fallback as
    /// non-JSON: we only unwrap the envelope when the expected `text` field is a
    /// string, never a partial/foreign shape).
    struct NoTextKeyFake;
    impl HttpGet for NoTextKeyFake {
        fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "fake" }
        fn get_authed(&self, _url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
            Ok(RawResponse { status: 200, location: None, content_type: "application/json".into(), body: br#"{"other":"x"}"#.to_vec() })
        }
    }

    #[test]
    fn get_attachment_text_falls_back_when_json_lacks_text_key() {
        let mut h = MailHandler::with_client(client_with(Box::new(NoTextKeyFake)));
        let out = h.call("mail.get_attachment_text", serde_json::json!({"sha256": "a".repeat(64)})).unwrap();
        assert_eq!(out["text"], r#"{"other":"x"}"#);
    }

    #[test]
    fn bad_sha256_is_invalid_params() {
        let mut h = MailHandler::with_client(client_with(Box::new(TextFake)));
        let err = h.call("mail.get_attachment_text", serde_json::json!({"sha256": "../etc/passwd"})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    // --- get_attachment writes original bytes to out/ safely ---
    struct PdfFake;
    impl HttpGet for PdfFake {
        fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "fake" }
        fn get_authed(&self, _url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
            Ok(RawResponse { status: 200, location: None, content_type: "application/pdf".into(), body: b"%PDF-1.7 body".to_vec() })
        }
    }

    #[test]
    fn get_attachment_writes_to_out_dir_safely() {
        let out = std::env::temp_dir().join(format!("mailout-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        std::env::set_var("KASTELLAN_WORKER_OUT", &out);
        let mut h = MailHandler::with_client(client_with(Box::new(PdfFake)));
        let sha = "a".repeat(64);
        let out_json = h
            .call("mail.get_attachment", serde_json::json!({"sha256": sha, "filename": "../evil/booking.pdf"}))
            .unwrap();
        std::env::remove_var("KASTELLAN_WORKER_OUT");
        let path = std::path::PathBuf::from(out_json["path"].as_str().unwrap());
        assert!(path.starts_with(&out), "must stay within out dir: {path:?}");
        assert!(path.exists(), "file written");
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.7 body");
        assert_eq!(out_json["size"], 13);
        assert!(out_json.get("data_base64").is_none(), "no bytes in the result");
        assert!(!path.to_string_lossy().contains(".."), "no traversal in name");
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn safe_name_strips_traversal_and_prefixes_sha() {
        let n = safe_attachment_name(Some("../../etc/passwd"), &"b".repeat(64));
        assert_eq!(n, "bbbbbbbbbbbb_passwd");
        let n2 = safe_attachment_name(None, &"c".repeat(64));
        assert_eq!(n2, "cccccccccccc_attachment");
    }
}
