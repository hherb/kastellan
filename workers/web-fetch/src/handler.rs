//! JSON-RPC handler for `web.fetch`.
//!
//! Flow: parse params → validate URL (https + allowlist) → drive redirects
//! (re-checking each hop) → extract readable text → build the result object.
//! Errors map onto the protocol code vocabulary (POLICY_DENIED / INVALID_PARAMS
//! / OPERATION_FAILED / METHOD_NOT_FOUND). No silent fallbacks: any failure is
//! an error, never an empty-but-success result.

use hhagent_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use url::Url;

use crate::allowlist::HostAllowlist;
use crate::extract::{extract, main_type};
use crate::fetch::{drive, FetchError, HttpGet, ReqwestGet};

#[derive(Deserialize)]
struct FetchParams {
    url: String,
}

/// Outcome of validating the initial request URL.
enum CheckError {
    BadUrl(String),
    NotHttps(String),
    HostMissing,
    HostDenied(String),
}

/// Validate the initial URL: parse, require https, require allowlisted host.
fn check_url(raw: &str, allowlist: &HostAllowlist) -> Result<Url, CheckError> {
    let url = Url::parse(raw).map_err(|e| CheckError::BadUrl(e.to_string()))?;
    if url.scheme() != "https" {
        return Err(CheckError::NotHttps(url.scheme().to_string()));
    }
    let host = url.host_str().ok_or(CheckError::HostMissing)?;
    if !allowlist.is_allowed(host) {
        return Err(CheckError::HostDenied(host.to_string()));
    }
    Ok(url)
}

fn check_err_to_rpc(e: CheckError) -> RpcError {
    match e {
        CheckError::BadUrl(m) => RpcError::new(codes::INVALID_PARAMS, format!("bad url: {m}")),
        CheckError::HostMissing => {
            RpcError::new(codes::INVALID_PARAMS, "url has no host".to_string())
        }
        CheckError::NotHttps(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("scheme {s:?} not allowed; https only"),
        ),
        CheckError::HostDenied(h) => {
            RpcError::new(codes::POLICY_DENIED, format!("host {h:?} not on allowlist"))
        }
    }
}

fn fetch_err_to_rpc(e: FetchError) -> RpcError {
    match e {
        FetchError::HostDenied(h) => RpcError::new(
            codes::POLICY_DENIED,
            format!("redirect host {h:?} not on allowlist"),
        ),
        FetchError::NonHttps(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("redirect scheme {s:?} not allowed; https only"),
        ),
        FetchError::TooManyRedirects => {
            RpcError::new(codes::OPERATION_FAILED, "too many redirects".to_string())
        }
        FetchError::MissingLocation => RpcError::new(
            codes::OPERATION_FAILED,
            "redirect without Location header".to_string(),
        ),
        FetchError::BadUrl(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("bad redirect url: {m}"))
        }
        // reqwest redacts passwords in its Display impl but may echo the request
        // URL; the URL was caller-supplied and is HTTPS-only, so plaintext-credential
        // leaks are unlikely. Full-URL exposure to the core is acceptable at this
        // trust level — truncating it would make operator-facing errors opaque.
        FetchError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("fetch failed: {m}"))
        }
    }
}

/// The worker handler, generic over the transport so tests inject a fake.
pub struct WebFetchHandler<T: HttpGet> {
    allowlist: HostAllowlist,
    transport: T,
}

impl WebFetchHandler<ReqwestGet> {
    /// Build from env: allowlist JSON + real reqwest transport.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var("HHAGENT_WEB_FETCH_ALLOWLIST").unwrap_or_else(|_| "[]".to_string());
        let allowlist = HostAllowlist::from_env_json(&raw)?;
        let transport = ReqwestGet::new()?;
        Ok(Self { allowlist, transport })
    }
}

impl<T: HttpGet> WebFetchHandler<T> {
    #[cfg(test)]
    fn with_parts(allowlist: HostAllowlist, transport: T) -> Self {
        Self { allowlist, transport }
    }
}

impl<T: HttpGet> Handler for WebFetchHandler<T> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if method != "web.fetch" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: FetchParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;

        // `check_url` validates the *initial* URL up front so we can return the
        // precise INVALID_PARAMS vs POLICY_DENIED distinction (bad-url vs
        // denied-host). `drive` then re-validates https+allowlist on every hop,
        // including this first one — the overlap is intentional defense in
        // depth: `drive` is safe to call with any URL, never trusting that its
        // caller pre-checked.
        let url = check_url(&p.url, &self.allowlist).map_err(check_err_to_rpc)?;
        let outcome = drive(&self.transport, &self.allowlist, url).map_err(fetch_err_to_rpc)?;
        let extracted = extract(&outcome.content_type, &outcome.body).map_err(|e| {
            RpcError::new(codes::OPERATION_FAILED, format!("extraction failed: {e}"))
        })?;

        Ok(serde_json::json!({
            "final_url": outcome.final_url,
            "status": outcome.status,
            "content_type": main_type(&outcome.content_type),
            "title": extracted.title,
            "text": extracted.text,
            "truncated": extracted.truncated,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::RawResponse;
    use crate::test_transport::{al, FakeGet};

    fn handler(entries: &[&str], responses: Vec<RawResponse>) -> WebFetchHandler<FakeGet> {
        WebFetchHandler::with_parts(al(entries), FakeGet::new(responses))
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_url_is_invalid_params() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h.call("web.fetch", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn non_https_is_policy_denied() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h
            .call("web.fetch", serde_json::json!({"url": "http://example.com/"}))
            .unwrap_err();
        assert_eq!(err.code, codes::POLICY_DENIED);
    }

    #[test]
    fn non_allowlisted_host_is_policy_denied() {
        let mut h = handler(&["example.com"], vec![]);
        let err = h
            .call("web.fetch", serde_json::json!({"url": "https://evil.test/"}))
            .unwrap_err();
        assert_eq!(err.code, codes::POLICY_DENIED);
    }

    #[test]
    fn happy_path_returns_extracted_text() {
        let body = "just some plain text body";
        let resp = RawResponse {
            status: 200,
            location: None,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: body.as_bytes().to_vec(),
        };
        let mut h = handler(&["example.com"], vec![resp]);
        let out = h
            .call("web.fetch", serde_json::json!({"url": "https://example.com/page"}))
            .unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["content_type"], "text/plain");
        assert_eq!(out["text"], body);
        assert_eq!(out["final_url"], "https://example.com/page");
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn redirect_to_denied_host_is_policy_denied_end_to_end() {
        let resp = RawResponse {
            status: 302,
            location: Some("https://evil.test/".to_string()),
            content_type: String::new(),
            body: Vec::new(),
        };
        let mut h = handler(&["example.com"], vec![resp]);
        let err = h
            .call("web.fetch", serde_json::json!({"url": "https://example.com/"}))
            .unwrap_err();
        assert_eq!(err.code, codes::POLICY_DENIED);
    }
}
