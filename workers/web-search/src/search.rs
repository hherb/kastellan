//! Pure search logic: endpoint validation, request-URL building, and the
//! one-GET drive with the count cap. Pure over the [`HttpGet`] seam so the
//! security checks (scheme + host allowlist) are unit-tested with a fake.

use std::net::IpAddr;

use url::Url;

use hhagent_worker_web_common::allowlist::HostAllowlist;
use hhagent_worker_web_common::http::HttpGet;

use crate::parse::{parse_results, Hit};

/// Default number of hits returned when the caller does not specify `count`.
pub const DEFAULT_COUNT: usize = 10;
/// Hard cap on hits returned regardless of caller request.
pub const MAX_COUNT: usize = 20;

/// Failure modes of a search. The handler maps these to JSON-RPC codes.
#[derive(Debug)]
pub enum SearchError {
    /// Configured endpoint URL is unparseable / has no host.
    BadEndpoint(String),
    /// Endpoint scheme not permitted (https everywhere; http loopback-only).
    SchemeDenied(String),
    /// Endpoint host is not on the allowlist.
    HostDenied(String),
    /// The query string was empty/blank.
    EmptyQuery,
    /// Transport error talking to the endpoint.
    Transport(String),
    /// Endpoint returned a redirect (unexpected for a search endpoint).
    Redirected,
    /// Endpoint returned a non-200 status.
    BadStatus(u16),
    /// Response body was not valid SearxNG JSON.
    Parse(String),
}

/// True if `host` is loopback: a loopback IP (covers `127.0.0.0/8` and `::1`)
/// or the literal `localhost`.
pub fn is_loopback(host: &str) -> bool {
    // `url::Url::host_str` returns IPv6 literals wrapped in brackets (`"[::1]"`),
    // which `IpAddr::from_str` rejects — strip them before parsing so a
    // bracketed loopback like `[::1]` is recognised (the spec advertises `::1`).
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// Validate the configured endpoint: parse, enforce the scheme rule, and
/// require the host be on the allowlist. Returns the parsed `Url` on success.
pub fn validate_endpoint(raw: &str, allowlist: &HostAllowlist) -> Result<Url, SearchError> {
    let url = Url::parse(raw).map_err(|e| SearchError::BadEndpoint(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| SearchError::BadEndpoint("endpoint has no host".to_string()))?
        .to_string();
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&host) => {}
        other => return Err(SearchError::SchemeDenied(other.to_string())),
    }
    if !allowlist.is_allowed(&host) {
        return Err(SearchError::HostDenied(host));
    }
    Ok(url)
}

/// Build the SearxNG request URL from the validated endpoint: replace the query
/// string with `q=<query>&format=json`, preserving scheme/host/port/path.
///
/// Note: this `clear()`s any query string the operator put on the configured
/// endpoint — pre-pinned params like `?engines=google` on
/// `HHAGENT_WEB_SEARCH_ENDPOINT` are dropped, not merged. Per-call engine/
/// language tuning is deferred (see the design spec); configure those in
/// SearxNG's own `settings.yml` rather than on the endpoint URL.
pub fn build_query_url(endpoint: &Url, query: &str) -> Url {
    let mut url = endpoint.clone();
    url.query_pairs_mut()
        .clear()
        .append_pair("q", query)
        .append_pair("format", "json");
    url
}

/// Run one search: validate the host against the allowlist (defense in depth —
/// the endpoint was validated at startup, but re-check), reject an empty query,
/// GET the request URL once, reject redirects and non-200s, parse, and slice to
/// `count` (clamped to `1..=MAX_COUNT`).
pub fn search<T: HttpGet>(
    transport: &T,
    endpoint: &Url,
    allowlist: &HostAllowlist,
    query: &str,
    count: usize,
) -> Result<Vec<Hit>, SearchError> {
    if query.trim().is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    let host = endpoint
        .host_str()
        .ok_or_else(|| SearchError::BadEndpoint("endpoint has no host".to_string()))?;
    if !allowlist.is_allowed(host) {
        return Err(SearchError::HostDenied(host.to_string()));
    }

    let req = build_query_url(endpoint, query);
    let resp = transport.get(&req).map_err(SearchError::Transport)?;
    if (300..400).contains(&resp.status) {
        return Err(SearchError::Redirected);
    }
    if resp.status != 200 {
        return Err(SearchError::BadStatus(resp.status));
    }

    let mut hits = parse_results(&resp.body).map_err(|e| SearchError::Parse(e.to_string()))?;
    hits.truncate(count.clamp(1, MAX_COUNT));
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_worker_web_common::testing::al;

    #[test]
    fn loopback_recognises_localhost_and_loopback_ips() {
        assert!(is_loopback("localhost"));
        assert!(is_loopback("LocalHost"));
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("127.0.0.5"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("[::1]")); // url::Url::host_str brackets IPv6 literals
        assert!(!is_loopback("example.org"));
        assert!(!is_loopback("10.0.0.1"));
        assert!(!is_loopback("8.8.8.8"));
    }

    #[test]
    fn https_endpoint_on_allowlisted_host_is_accepted() {
        let a = al(&["searx.example.org"]);
        let u = validate_endpoint("https://searx.example.org/search", &a).unwrap();
        assert_eq!(u.host_str(), Some("searx.example.org"));
    }

    #[test]
    fn http_loopback_endpoint_is_accepted() {
        let a = al(&["127.0.0.1"]);
        let u = validate_endpoint("http://127.0.0.1:8888/search", &a).unwrap();
        assert_eq!(u.port(), Some(8888));
    }

    #[test]
    fn http_remote_endpoint_is_scheme_denied() {
        let a = al(&["searx.example.org"]);
        let err = validate_endpoint("http://searx.example.org/search", &a)
            .expect_err("must deny");
        assert!(matches!(err, SearchError::SchemeDenied(s) if s == "http"));
    }

    #[test]
    fn endpoint_host_not_on_allowlist_is_denied() {
        let a = al(&["searx.example.org"]);
        let err = validate_endpoint("https://evil.test/search", &a)
            .expect_err("must deny");
        assert!(matches!(err, SearchError::HostDenied(h) if h == "evil.test"));
    }

    #[test]
    fn unparseable_endpoint_is_bad_endpoint() {
        let a = al(&["x"]);
        let err = validate_endpoint("not a url", &a).expect_err("must error");
        assert!(matches!(err, SearchError::BadEndpoint(_)));
    }

    #[test]
    fn build_query_url_sets_q_and_format_preserving_path() {
        let endpoint = Url::parse("https://searx.example.org/search").unwrap();
        let req = build_query_url(&endpoint, "rust lifetimes");
        assert_eq!(req.path(), "/search");
        let pairs: Vec<(String, String)> = req
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(pairs.contains(&("q".into(), "rust lifetimes".into())));
        assert!(pairs.contains(&("format".into(), "json".into())));
    }

    use hhagent_worker_web_common::http::RawResponse;
    use hhagent_worker_web_common::testing::{json_resp, redirect_to, FakeGet};

    fn endpoint() -> Url {
        Url::parse("https://searx.example.org/search").unwrap()
    }

    #[test]
    fn search_returns_parsed_hits() {
        let json = r#"{"results":[
            {"title":"A","url":"https://a.test","content":"x","engine":"e"},
            {"title":"B","url":"https://b.test","content":"y","engine":"e"}
        ]}"#;
        let t = FakeGet::new(vec![json_resp(json)]);
        let a = al(&["searx.example.org"]);
        let hits = search(&t, &endpoint(), &a, "q", DEFAULT_COUNT).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://a.test");
    }

    #[test]
    fn search_truncates_to_count() {
        let results: String = (0..5)
            .map(|i| format!(r#"{{"url":"https://h{i}.test"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(r#"{{"results":[{results}]}}"#);
        let t = FakeGet::new(vec![json_resp(&json)]);
        let a = al(&["searx.example.org"]);
        let hits = search(&t, &endpoint(), &a, "q", 3).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn search_clamps_count_to_max() {
        let results: String = (0..30)
            .map(|i| format!(r#"{{"url":"https://h{i}.test"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(r#"{{"results":[{results}]}}"#);
        let t = FakeGet::new(vec![json_resp(&json)]);
        let a = al(&["searx.example.org"]);
        let hits = search(&t, &endpoint(), &a, "q", 999).unwrap();
        assert_eq!(hits.len(), MAX_COUNT);
    }

    #[test]
    fn empty_query_is_rejected() {
        let t = FakeGet::new(vec![]);
        let a = al(&["searx.example.org"]);
        let err = search(&t, &endpoint(), &a, "   ", DEFAULT_COUNT)
            .expect_err("must reject");
        assert!(matches!(err, SearchError::EmptyQuery));
    }

    #[test]
    fn non_200_status_is_bad_status() {
        let t = FakeGet::new(vec![RawResponse {
            status: 503,
            location: None,
            content_type: "text/plain".into(),
            body: Vec::new(),
        }]);
        let a = al(&["searx.example.org"]);
        let err = search(&t, &endpoint(), &a, "q", DEFAULT_COUNT)
            .expect_err("must error");
        assert!(matches!(err, SearchError::BadStatus(503)));
    }

    #[test]
    fn redirect_from_endpoint_is_rejected() {
        let t = FakeGet::new(vec![redirect_to("https://elsewhere.test/")]);
        let a = al(&["searx.example.org"]);
        let err = search(&t, &endpoint(), &a, "q", DEFAULT_COUNT)
            .expect_err("must error");
        assert!(matches!(err, SearchError::Redirected));
    }
}
