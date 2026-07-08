//! Shared unit-test helpers: a fake [`HttpGet`] transport plus small
//! allowlist/response builders, behind the `testing` cargo feature so each
//! worker's unit suite shares one canned-response transport.

use std::cell::RefCell;
use std::collections::VecDeque;

use url::Url;

use crate::allowlist::HostAllowlist;
use crate::http::{HttpGet, RawResponse};

/// Fake transport returning canned responses in FIFO order.
pub struct FakeGet {
    responses: RefCell<VecDeque<RawResponse>>,
}

impl FakeGet {
    pub fn new(responses: Vec<RawResponse>) -> Self {
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

    fn transport_kind(&self) -> &'static str {
        "fake"
    }

    fn post(&self, _url: &Url, _content_type: &str, _body: &[u8])
        -> Result<RawResponse, String>
    {
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| "no more canned responses".to_string())
    }
}

/// Build a [`HostAllowlist`] from bare string entries.
pub fn al(entries: &[&str]) -> HostAllowlist {
    let json = serde_json::to_string(entries).unwrap();
    HostAllowlist::from_env_json(&json).unwrap()
}

/// A `200 text/plain` response carrying `body`.
pub fn ok_resp(body: &str) -> RawResponse {
    RawResponse {
        status: 200,
        location: None,
        content_type: "text/plain".to_string(),
        body: body.as_bytes().to_vec(),
    }
}

/// A `302` redirect to `loc`.
pub fn redirect_to(loc: &str) -> RawResponse {
    RawResponse {
        status: 302,
        location: Some(loc.to_string()),
        content_type: String::new(),
        body: Vec::new(),
    }
}

/// A `200 application/json` response carrying `json` (for search-style workers).
pub fn json_resp(json: &str) -> RawResponse {
    RawResponse {
        status: 200,
        location: None,
        content_type: "application/json".to_string(),
        body: json.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod post_fake_tests {
    use super::*;
    #[test]
    fn fake_post_pops_next_response() {
        let f = FakeGet::new(vec![ok_resp("embedded")]);
        let r = f.post(&url::Url::parse("http://e.test/embeddings").unwrap(),
                       "application/json", b"{}").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"embedded");
    }
}
