//! localmail REST client. Reuses web-common's transport (`make_get`) so
//! force-routing — proxy-CONNECT over the egress UDS with the per-instance MITM
//! CA — works unchanged; adds `Authorization: Bearer` via `get_authed`/
//! `post_authed`. Read-only: only GETs plus `POST /v1/search` (query body).

use kastellan_worker_web_common::http::{make_get, HttpGet, RawResponse};
use url::Url;

/// Cap for original-format attachment downloads (`get_bytes`). Overridable via
/// `KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES`. Realistic booking PDFs are well under.
const DEFAULT_ATTACHMENT_MAX_BYTES: usize = 25 * 1024 * 1024;
/// Cap for JSON responses (search pages, message detail, extracted text).
const JSON_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Failure modes surfaced to the handler, which maps them to JSON-RPC errors.
#[derive(Debug)]
pub enum MailError {
    /// The worker built a bad request (unparsable path, etc.).
    BadParams(String),
    /// localmail returned a non-2xx status.
    Upstream { status: u16, body: String },
    /// Transport or decode failure (no route, bad JSON, cap exceeded).
    Transport(String),
}

pub struct MailClient {
    base: Url,
    token: String,
    transport: Box<dyn HttpGet>,
    attachment_cap: usize,
}

impl MailClient {
    /// Build from the worker's environment: `KASTELLAN_MAIL_ENDPOINT` (base
    /// URL), `KASTELLAN_MAIL_TOKEN_FILE` (0600 file holding the bearer token),
    /// optional `KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES`. Transport is selected by
    /// `make_get` (proxy-CONNECT when force-routed, else direct).
    pub fn from_env() -> anyhow::Result<Self> {
        let base = std::env::var("KASTELLAN_MAIL_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_MAIL_ENDPOINT unset"))?;
        let base = Url::parse(&base)
            .map_err(|e| anyhow::anyhow!("KASTELLAN_MAIL_ENDPOINT invalid: {e}"))?;
        let token_file = std::env::var("KASTELLAN_MAIL_TOKEN_FILE")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_MAIL_TOKEN_FILE unset"))?;
        let token = std::fs::read_to_string(&token_file)
            .map_err(|e| anyhow::anyhow!("read token file {token_file}: {e}"))?
            .trim()
            .to_string();
        if token.is_empty() {
            anyhow::bail!("token file {token_file} is empty");
        }
        let attachment_cap = std::env::var("KASTELLAN_MAIL_ATTACHMENT_MAX_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_ATTACHMENT_MAX_BYTES);
        let transport = make_get("kastellan-mail/0")?;
        Ok(Self { base, token, transport, attachment_cap })
    }

    #[cfg(test)]
    pub fn for_test(base: Url, token: String, transport: Box<dyn HttpGet>) -> Self {
        Self { base, token, transport, attachment_cap: DEFAULT_ATTACHMENT_MAX_BYTES }
    }

    fn url(&self, path: &str) -> Result<Url, MailError> {
        self.base
            .join(path)
            .map_err(|e| MailError::BadParams(format!("bad path {path}: {e}")))
    }

    /// Reject a non-2xx upstream response, clamping the echoed body.
    fn check(resp: RawResponse) -> Result<RawResponse, MailError> {
        if (200..300).contains(&resp.status) {
            Ok(resp)
        } else {
            Err(MailError::Upstream {
                status: resp.status,
                body: String::from_utf8_lossy(&resp.body).chars().take(512).collect(),
            })
        }
    }

    pub fn get_json(&self, path: &str) -> Result<serde_json::Value, MailError> {
        let url = self.url(path)?;
        let resp = self
            .transport
            .get_authed(&url, &self.token, JSON_MAX_BYTES)
            .map_err(MailError::Transport)?;
        let resp = Self::check(resp)?;
        serde_json::from_slice(&resp.body)
            .map_err(|e| MailError::Transport(format!("bad json: {e}")))
    }

    pub fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, MailError> {
        let url = self.url(path)?;
        let raw = serde_json::to_vec(body).map_err(|e| MailError::BadParams(e.to_string()))?;
        let resp = self
            .transport
            .post_authed(&url, &self.token, "application/json", &raw, JSON_MAX_BYTES)
            .map_err(MailError::Transport)?;
        let resp = Self::check(resp)?;
        serde_json::from_slice(&resp.body)
            .map_err(|e| MailError::Transport(format!("bad json: {e}")))
    }

    /// Fetch raw bytes (attachment originals) with the higher attachment cap.
    /// Returns `(content_type, bytes)`.
    pub fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>), MailError> {
        let url = self.url(path)?;
        let resp = self
            .transport
            .get_authed(&url, &self.token, self.attachment_cap)
            .map_err(MailError::Transport)?;
        let resp = Self::check(resp)?;
        Ok((resp.content_type, resp.body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;

    /// Fake transport asserting the bearer + path on a GET, returning a stub.
    struct FakeAccounts;
    impl HttpGet for FakeAccounts {
        fn get(&self, _u: &Url) -> Result<RawResponse, String> {
            unreachable!("client uses get_authed")
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
        fn get_authed(&self, url: &Url, bearer: &str, _max: usize) -> Result<RawResponse, String> {
            assert_eq!(bearer, "tok123");
            assert!(url.path().ends_with("/v1/accounts"), "path was {}", url.path());
            Ok(RawResponse {
                status: 200,
                location: None,
                content_type: "application/json".into(),
                body: br#"[{"id":1}]"#.to_vec(),
            })
        }
    }

    #[test]
    fn get_json_uses_bearer_and_parses() {
        let c = MailClient::for_test(
            Url::parse("http://127.0.0.1:8000").unwrap(),
            "tok123".into(),
            Box::new(FakeAccounts),
        );
        let v = c.get_json("/v1/accounts").unwrap();
        assert_eq!(v[0]["id"], 1);
    }

    /// Fake transport returning a non-2xx → `check` maps it to Upstream.
    struct Fake403;
    impl HttpGet for Fake403 {
        fn get(&self, _u: &Url) -> Result<RawResponse, String> {
            unreachable!()
        }
        fn transport_kind(&self) -> &'static str {
            "fake"
        }
        fn get_authed(&self, _url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
            Ok(RawResponse {
                status: 403,
                location: None,
                content_type: "text/plain".into(),
                body: b"forbidden".to_vec(),
            })
        }
    }

    #[test]
    fn non_2xx_is_upstream_error() {
        let c = MailClient::for_test(
            Url::parse("http://127.0.0.1:8000").unwrap(),
            "t".into(),
            Box::new(Fake403),
        );
        match c.get_json("/v1/accounts") {
            Err(MailError::Upstream { status: 403, .. }) => {}
            other => panic!("expected Upstream 403, got {other:?}"),
        }
    }
}
