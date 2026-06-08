//! Shared unit-test helpers: a fake [`HttpGet`] transport plus small
//! allowlist/response builders, used by the `fetch` and `handler` test modules
//! so the canned-response transport lives in exactly one place.

use std::cell::RefCell;
use std::collections::VecDeque;

use url::Url;

use crate::allowlist::HostAllowlist;
use crate::fetch::{HttpGet, RawResponse};

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
