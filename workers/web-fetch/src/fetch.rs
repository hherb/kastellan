//! The redirect-following drive loop for web-fetch.
//!
//! `drive()` is pure over the [`HttpGet`] seam so the redirect cap and the
//! per-hop allowlist + https re-check (the security-critical bit: a 3xx to a
//! non-allowlisted or non-https target is refused) are unit-tested with a fake
//! transport. The transport itself lives in `hhagent_worker_web_common::http`.

use url::Url;

use hhagent_worker_web_common::allowlist::HostAllowlist;
use hhagent_worker_web_common::http::HttpGet;

/// Max redirect hops followed before giving up.
pub const MAX_REDIRECTS: usize = 5;

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

        // Any 3xx is treated as a redirect requiring a `Location`. A bodyless
        // 3xx without `Location` fails closed as MissingLocation.
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

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_worker_web_common::http::RawResponse;
    use hhagent_worker_web_common::testing::{al, ok_resp, redirect_to, FakeGet};

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
