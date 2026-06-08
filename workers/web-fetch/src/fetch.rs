//! HTTP transport seam + the redirect-following drive loop.
//!
//! `drive()` is pure over the [`HttpGet`] trait so the redirect cap and the
//! per-hop allowlist + https re-check (the security-critical bit: a 3xx to a
//! non-allowlisted or non-https target is refused) are unit-tested with a fake
//! transport. [`ReqwestGet`] is the real `reqwest::blocking` implementation.

use std::time::Duration;

use url::Url;

use crate::allowlist::HostAllowlist;

/// Max redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 5;
/// Per-request timeout.
pub const TIMEOUT_SECS: u64 = 20;
/// Response body byte cap (5 MiB).
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// A single raw HTTP response, transport-agnostic.
pub struct RawResponse {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// The transport seam. One GET, no redirect following (the caller drives
/// redirects so it can re-check the allowlist per hop).
pub trait HttpGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String>;
}

/// Terminal outcome of a successful drive.
pub struct FetchOutcome {
    pub final_url: String,
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Errors from the drive loop. The handler maps these to JSON-RPC codes.
pub enum FetchError {
    /// A redirect targeted a host not on the allowlist.
    HostDenied(String),
    /// A redirect targeted a non-https scheme.
    NonHttps(String),
    TooManyRedirects,
    MissingLocation,
    BadUrl(String),
    Transport(String),
}

/// Follow redirects from `start`, re-validating https + allowlist on every hop,
/// up to [`MAX_REDIRECTS`]. Returns the terminal (non-3xx) response.
pub fn drive<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    start: Url,
) -> Result<FetchOutcome, FetchError> {
    let mut url = start;
    for _hop in 0..=MAX_REDIRECTS {
        if url.scheme() != "https" {
            return Err(FetchError::NonHttps(url.scheme().to_string()));
        }
        let host = url
            .host_str()
            .ok_or_else(|| FetchError::BadUrl("url has no host".to_string()))?;
        if !allowlist.is_allowed(host) {
            return Err(FetchError::HostDenied(host.to_string()));
        }

        let resp = transport.get(&url).map_err(FetchError::Transport)?;

        if (300..400).contains(&resp.status) {
            let loc = resp.location.ok_or(FetchError::MissingLocation)?;
            url = url
                .join(&loc)
                .map_err(|e| FetchError::BadUrl(e.to_string()))?;
            continue;
        }

        return Ok(FetchOutcome {
            final_url: url.to_string(),
            status: resp.status,
            content_type: resp.content_type,
            body: resp.body,
        });
    }
    Err(FetchError::TooManyRedirects)
}

/// Real transport over `reqwest::blocking` + rustls. Redirects disabled
/// (driven by [`drive`]); body capped while reading via `Read::take`.
pub struct ReqwestGet {
    client: reqwest::blocking::Client,
}

impl ReqwestGet {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .user_agent("hhagent-web-fetch/0")
            .build()?;
        Ok(Self { client })
    }
}

impl HttpGet for ReqwestGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        use std::io::Read;

        let resp = self
            .client
            .get(url.clone())
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let header = |name: reqwest::header::HeaderName| -> Option<String> {
            resp.headers()
                .get(&name)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        };
        let location = header(reqwest::header::LOCATION);
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();

        let mut body = Vec::new();
        resp.take((MAX_BODY_BYTES as u64) + 1)
            .read_to_end(&mut body)
            .map_err(|e| e.to_string())?;
        if body.len() > MAX_BODY_BYTES {
            return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
        }

        Ok(RawResponse { status, location, content_type, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Fake transport returning canned responses in order.
    struct FakeGet {
        responses: RefCell<VecDeque<RawResponse>>,
    }
    impl FakeGet {
        fn new(responses: Vec<RawResponse>) -> Self {
            Self { responses: RefCell::new(responses.into_iter().collect()) }
        }
    }
    impl HttpGet for FakeGet {
        fn get(&self, _url: &Url) -> Result<RawResponse, String> {
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| "no more canned responses".to_string())
        }
    }

    fn al(entries: &[&str]) -> HostAllowlist {
        let json = serde_json::to_string(entries).unwrap();
        HostAllowlist::from_env_json(&json).unwrap()
    }

    fn ok_resp(body: &str) -> RawResponse {
        RawResponse {
            status: 200,
            location: None,
            content_type: "text/plain".to_string(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn redirect_to(loc: &str) -> RawResponse {
        RawResponse {
            status: 302,
            location: Some(loc.to_string()),
            content_type: String::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn terminal_response_is_returned() {
        let t = FakeGet::new(vec![ok_resp("hello")]);
        let out = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .unwrap_or_else(|_| panic!("expected ok"));
        assert_eq!(out.status, 200);
        assert_eq!(out.body, b"hello");
        assert_eq!(out.final_url, "https://example.com/");
    }

    #[test]
    fn redirect_to_allowlisted_host_is_followed() {
        let t = FakeGet::new(vec![
            redirect_to("https://a.example.com/page"),
            ok_resp("landed"),
        ]);
        let out = drive(&t, &al(&[".example.com"]), Url::parse("https://example.com/").unwrap())
            .unwrap_or_else(|_| panic!("expected ok"));
        assert_eq!(out.body, b"landed");
        assert_eq!(out.final_url, "https://a.example.com/page");
    }

    #[test]
    fn redirect_to_non_allowlisted_host_is_refused() {
        let t = FakeGet::new(vec![redirect_to("https://evil.test/")]);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must refuse");
        assert!(matches!(err, FetchError::HostDenied(h) if h == "evil.test"));
    }

    #[test]
    fn redirect_to_non_https_is_refused() {
        let t = FakeGet::new(vec![redirect_to("http://example.com/")]);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must refuse");
        assert!(matches!(err, FetchError::NonHttps(s) if s == "http"));
    }

    #[test]
    fn redirect_loop_hits_the_cap() {
        // Always redirect back to the same allowlisted host → exceed the cap.
        let resps: Vec<RawResponse> =
            (0..MAX_REDIRECTS + 2).map(|_| redirect_to("https://example.com/next")).collect();
        let t = FakeGet::new(resps);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must error");
        assert!(matches!(err, FetchError::TooManyRedirects));
    }

    #[test]
    fn redirect_without_location_errors() {
        let t = FakeGet::new(vec![RawResponse {
            status: 302,
            location: None,
            content_type: String::new(),
            body: Vec::new(),
        }]);
        let err = drive(&t, &al(&["example.com"]), Url::parse("https://example.com/").unwrap())
            .err()
            .expect("must error");
        assert!(matches!(err, FetchError::MissingLocation));
    }
}
