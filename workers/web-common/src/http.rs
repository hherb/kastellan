//! HTTP transport seam shared by net-egress workers.
//!
//! `HttpGet` is the seam tests fake; [`ReqwestGet`] is the real
//! `reqwest::blocking` + rustls implementation. Redirects are disabled at the
//! client — callers that need them drive redirects themselves so they can
//! re-check their allowlist on every hop. The body is capped while reading.

use std::path::PathBuf;
use std::time::Duration;

use url::Url;

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

/// The transport seam. One GET, no redirect following.
pub trait HttpGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String>;
    /// Stable identifier of the concrete transport (for tests + diagnostics).
    fn transport_kind(&self) -> &'static str;
}

impl HttpGet for Box<dyn HttpGet> {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        (**self).get(url)
    }

    fn transport_kind(&self) -> &'static str {
        (**self).transport_kind()
    }
}

/// Real transport over `reqwest::blocking` + rustls. Redirects disabled; body
/// capped while reading via `Read::take`.
pub struct ReqwestGet {
    client: reqwest::blocking::Client,
}

impl ReqwestGet {
    /// Build the transport with a caller-supplied `User-Agent`. Each worker
    /// passes its own (`kastellan-web-fetch/0`, `kastellan-web-search/0`, …) so the
    /// UA on the wire stays attributable per worker and unchanged by the shared
    /// crate move.
    pub fn new(user_agent: &str) -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .user_agent(user_agent)
            .build()?;
        Ok(Self { client })
    }
}

impl HttpGet for ReqwestGet {
    fn transport_kind(&self) -> &'static str {
        "reqwest"
    }

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

/// Inner selection logic for `make_get`. `uds_override` is the value of
/// `KASTELLAN_EGRESS_PROXY_UDS` (already extracted by the caller), or `None`
/// when the variable is absent or empty.
///
/// Extracted so tests can exercise both branches **without touching process
/// env** (env mutation is a data race when other threads read the same var).
pub(crate) fn make_get_inner(
    user_agent: &str,
    uds_override: Option<&str>,
) -> anyhow::Result<Box<dyn HttpGet>> {
    match uds_override {
        Some(uds) if !uds.is_empty() => Ok(Box::new(
            crate::proxy_connect::ProxyConnectGet::new(user_agent, PathBuf::from(uds)),
        )),
        _ => Ok(Box::new(ReqwestGet::new(user_agent)?)),
    }
}

/// Build the appropriate `HttpGet` for the current environment. When
/// `KASTELLAN_EGRESS_PROXY_UDS` is set (force-routing active), egress MUST go
/// through the proxy, so return [`crate::proxy_connect::ProxyConnectGet`];
/// otherwise the direct [`ReqwestGet`] for dev/no-proxy runs.
pub fn make_get(user_agent: &str) -> anyhow::Result<Box<dyn HttpGet>> {
    // Treat absent *and* empty the same way (empty = effectively unset).
    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS").ok();
    make_get_inner(user_agent, uds.as_deref())
}

#[cfg(test)]
mod make_get_tests {
    use super::*;

    /// Both branches exercised via `make_get_inner` — no env mutation, no race.
    #[test]
    fn make_get_inner_selects_transport_by_uds() {
        // No UDS → reqwest.
        let g = make_get_inner("kastellan-test/0", None).unwrap();
        assert_eq!(g.transport_kind(), "reqwest");

        // UDS set → proxy-connect (socket doesn't need to exist for construction).
        let g = make_get_inner("kastellan-test/0", Some("/tmp/x.sock")).unwrap();
        assert_eq!(g.transport_kind(), "proxy-connect");
    }
}
