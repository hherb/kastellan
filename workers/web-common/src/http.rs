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
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// The transport seam. One GET, no redirect following.
///
/// `Send + Sync` so a single transport can be shared by reference across the
/// scoped fetch threads in `web-research`'s parallel fetch phase. Every concrete
/// impl (reqwest / proxy-connect) is already thread-safe; test doubles must be
/// too (see `FakeGet`).
pub trait HttpGet: Send + Sync {
    fn get(&self, url: &Url) -> Result<RawResponse, String>;
    /// Stable identifier of the concrete transport (for tests + diagnostics).
    fn transport_kind(&self) -> &'static str;

    /// POST `body` with `content_type` to `url`, no redirect following.
    /// Default: unsupported — only transports that need it (the embedding POST)
    /// override this, so GET-only siblings (web-search, web-fetch) are untouched.
    fn post(&self, _url: &Url, _content_type: &str, _body: &[u8])
        -> Result<RawResponse, String>
    {
        Err("post: unsupported by this transport".to_string())
    }
}

impl HttpGet for Box<dyn HttpGet> {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        (**self).get(url)
    }

    fn transport_kind(&self) -> &'static str {
        (**self).transport_kind()
    }

    fn post(&self, url: &Url, content_type: &str, body: &[u8])
        -> Result<RawResponse, String>
    {
        (**self).post(url, content_type, body)
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

    fn post(&self, url: &Url, content_type: &str, body: &[u8])
        -> Result<RawResponse, String>
    {
        use std::io::Read;
        let resp = self
            .client
            .post(url.clone())
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body.to_vec())
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let header = |name: reqwest::header::HeaderName| -> Option<String> {
            resp.headers().get(&name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
        };
        let location = header(reqwest::header::LOCATION);
        let content_type = header(reqwest::header::CONTENT_TYPE).unwrap_or_default();
        let mut out = Vec::new();
        resp.take((MAX_BODY_BYTES as u64) + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
        if out.len() > MAX_BODY_BYTES {
            return Err(format!("response body exceeds {MAX_BODY_BYTES} bytes"));
        }
        Ok(RawResponse { status, location, content_type, body: out })
    }
}

/// Inner selection logic for `make_get`. `uds_override` is the value of
/// `KASTELLAN_EGRESS_PROXY_UDS` and `ca_override` the value of
/// `KASTELLAN_EGRESS_PROXY_CA` (both already extracted by the caller), or
/// `None` when the respective variable is absent or empty.
///
/// When a UDS is set, `ca_override` selects the worker's trust posture: `Some`
/// → trust ONLY that per-instance MITM CA (fail closed if it can't be
/// read/parsed — never a silent webpki fallback); `None` → webpki public roots
/// (slice #1/#2 back-compat). When no UDS is set the CA is irrelevant — the
/// direct `ReqwestGet` carries its own rustls roots.
///
/// `pub` (not `pub(crate)`) on purpose: it is the documented DI seam that the
/// `core` crate's e2e tests drive directly so they can exercise every branch
/// **without touching process env** (env mutation is a data race when other
/// threads read the same var).
pub fn make_get_inner(
    user_agent: &str,
    uds_override: Option<&str>,
    ca_override: Option<&str>,
) -> anyhow::Result<Box<dyn HttpGet>> {
    match uds_override {
        Some(uds) if !uds.is_empty() => {
            let ca = ca_override.filter(|s| !s.is_empty()).map(PathBuf::from);
            Ok(Box::new(crate::proxy_connect::ProxyConnectGet::with_trust(
                user_agent,
                PathBuf::from(uds),
                ca,
            )?))
        }
        _ => Ok(Box::new(ReqwestGet::new(user_agent)?)),
    }
}

/// Build the appropriate `HttpGet` for the current environment. When
/// `KASTELLAN_EGRESS_PROXY_UDS` is set (force-routing active), egress MUST go
/// through the proxy, so return [`crate::proxy_connect::ProxyConnectGet`] —
/// trusting only `KASTELLAN_EGRESS_PROXY_CA` when that is set (MITM posture),
/// else webpki roots. Otherwise the direct [`ReqwestGet`] for dev/no-proxy runs.
pub fn make_get(user_agent: &str) -> anyhow::Result<Box<dyn HttpGet>> {
    // Treat absent *and* empty the same way (empty = effectively unset).
    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS").ok();
    let ca = std::env::var("KASTELLAN_EGRESS_PROXY_CA").ok();
    make_get_inner(user_agent, uds.as_deref(), ca.as_deref())
}

/// Build a transparent-tunnel CONNECT transport: reach origins ONLY via the
/// egress-proxy `uds`, validating them against the compiled-in webpki roots plus
/// an optional `extra_ca`. For workers that do their own end-to-end TLS (the
/// proxy tunnels ciphertext and cannot MITM them) — slice 5c. `extra_ca` is a
/// test-only self-signed origin cert; production callers pass `None`.
pub fn make_transparent_get(
    user_agent: &str,
    uds: &std::path::Path,
    extra_ca: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn HttpGet>> {
    let t = crate::proxy_connect::ProxyConnectGet::with_extra_ca(
        user_agent,
        uds.to_path_buf(),
        extra_ca.map(|p| p.to_path_buf()),
    )?;
    Ok(Box::new(t))
}

#[cfg(test)]
mod make_get_tests {
    use super::*;

    /// All branches exercised via `make_get_inner` — no env mutation, no race.
    #[test]
    fn make_get_inner_threads_ca_override_into_proxy_connect() {
        // No UDS → reqwest (CA ignored).
        let g = make_get_inner("kastellan-test/0", None, None).unwrap();
        assert_eq!(g.transport_kind(), "reqwest");
        // UDS, no CA → proxy-connect (webpki; socket needn't exist to construct).
        let g = make_get_inner("kastellan-test/0", Some("/tmp/x.sock"), None).unwrap();
        assert_eq!(g.transport_kind(), "proxy-connect");
        // UDS + a set-but-unreadable CA path → FAIL CLOSED (no silent webpki fallback).
        let err = make_get_inner("kastellan-test/0", Some("/tmp/x.sock"), Some("/nonexistent/ca.pem"));
        assert!(err.is_err(), "a set-but-unreadable CA must fail closed, not fall back");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn make_transparent_get_builds_a_transport() {
        let g = super::make_transparent_get(
            "kastellan-test/0",
            std::path::Path::new("/tmp/egress.sock"),
            None,
        );
        assert!(g.is_ok());
        assert_eq!(g.unwrap().transport_kind(), "proxy-connect");
    }
}

#[cfg(test)]
mod post_tests {
    use super::*;

    struct GetOnly;
    impl HttpGet for GetOnly {
        fn get(&self, _url: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "get-only" }
        // deliberately does NOT override post -> exercises the default
    }

    #[test]
    fn default_post_is_unsupported() {
        let t = GetOnly;
        let err = t.post(&Url::parse("https://x.test/e").unwrap(), "application/json", b"{}")
            .unwrap_err();
        assert!(err.contains("unsupported"), "got: {err}");
    }
}
