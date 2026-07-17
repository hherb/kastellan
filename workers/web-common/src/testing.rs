//! Shared unit-test helpers: a fake [`HttpGet`] transport plus small
//! allowlist/response builders, behind the `testing` cargo feature so each
//! worker's unit suite shares one canned-response transport.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

use url::Url;

use crate::allowlist::HostAllowlist;
use crate::http::{HttpGet, RawResponse};

/// Fake transport returning canned responses in FIFO order. `Mutex`-backed so it
/// is `Sync` (the `HttpGet` seam now requires it); FIFO order is fine for
/// single-fetch tests — use `KeyedFakeGet` when a test issues concurrent fetches.
pub struct FakeGet {
    responses: Mutex<VecDeque<RawResponse>>,
}

impl FakeGet {
    pub fn new(responses: Vec<RawResponse>) -> Self {
        Self { responses: Mutex::new(responses.into_iter().collect()) }
    }
}

impl HttpGet for FakeGet {
    fn get(&self, _url: &Url) -> Result<RawResponse, String> {
        self.responses
            .lock()
            .expect("FakeGet mutex poisoned")
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
            .lock()
            .expect("FakeGet mutex poisoned")
            .pop_front()
            .ok_or_else(|| "no more canned responses".to_string())
    }
}

/// URL host+path → response. Unlike `FakeGet`'s FIFO queue, lookups are
/// order-independent, so a test can drive concurrent fetches and assert results
/// deterministically. The query string is ignored (search requests carry `?q=…`).
/// Immutable after construction ⇒ `Send + Sync`.
pub struct KeyedFakeGet {
    responses: HashMap<String, RawResponse>,
}

fn keyed_url(url: &Url) -> String {
    format!("{}{}", url.host_str().unwrap_or(""), url.path())
}

impl KeyedFakeGet {
    /// Build from `(url, response)` pairs. Each URL is reduced to its host+path key.
    pub fn new(pairs: Vec<(&str, RawResponse)>) -> Self {
        let responses = pairs
            .into_iter()
            .map(|(u, r)| (keyed_url(&Url::parse(u).expect("valid test url")), r))
            .collect();
        Self { responses }
    }

    fn lookup(&self, url: &Url) -> Result<RawResponse, String> {
        let key = keyed_url(url);
        self.responses
            .get(&key)
            .cloned()
            .ok_or_else(|| format!("no canned response for {key}"))
    }
}

impl HttpGet for KeyedFakeGet {
    fn get(&self, url: &Url) -> Result<RawResponse, String> {
        self.lookup(url)
    }

    fn transport_kind(&self) -> &'static str {
        "keyed-fake"
    }

    fn post(&self, url: &Url, _content_type: &str, _body: &[u8]) -> Result<RawResponse, String> {
        self.lookup(url)
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

/// One-shot stub search-broker on `sock`: binds a Unix listener, accepts one
/// connection, reads a single JSON-RPC request line, then writes `response_json`
/// followed by a newline. Shared by the `search_provider` seam tests and the
/// web-research handler tests — both exercise `BrokeredSearchProvider` over a
/// real UDS. Returns the join handle so the caller can await the stub. Requires
/// the `search` feature (pulls `kastellan-protocol` for the record reader).
#[cfg(feature = "search")]
pub fn stub_broker(
    sock: std::path::PathBuf,
    response_json: String,
) -> std::thread::JoinHandle<()> {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    let listener = UnixListener::bind(&sock).expect("bind stub-broker socket");
    std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept stub-broker connection");
        let mut br = std::io::BufReader::new(conn.try_clone().expect("clone stub-broker conn"));
        let _ = kastellan_protocol::read_capped_record(&mut br, 1_000_000)
            .expect("read stub-broker request");
        conn.write_all(response_json.as_bytes())
            .expect("write stub-broker response");
        conn.write_all(b"\n").expect("write stub-broker newline");
        conn.flush().expect("flush stub-broker");
    })
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

#[cfg(test)]
mod send_sync_tests {
    use crate::http::HttpGet;

    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn transport_seam_is_thread_shareable() {
        _assert_send_sync::<super::FakeGet>();
        _assert_send_sync::<Box<dyn HttpGet>>();
        _assert_send_sync::<super::KeyedFakeGet>();
    }
}

#[cfg(test)]
mod keyed_fake_tests {
    use super::*;
    use url::Url;

    #[test]
    fn matches_by_host_and_path_ignoring_query() {
        let t = KeyedFakeGet::new(vec![
            ("https://searx.example.org/search", json_resp(r#"{"results":[]}"#)),
            ("https://docs.example.org/a", ok_resp("page a")),
        ]);
        // Search request carries a ?q=... query — must still match by host+path.
        let s = t.get(&Url::parse("https://searx.example.org/search?q=hello&format=json").unwrap())
            .unwrap();
        assert_eq!(s.status, 200);
        let a = t.get(&Url::parse("https://docs.example.org/a").unwrap()).unwrap();
        assert_eq!(a.body, b"page a");
        // Unregistered URL is an explicit error.
        let miss = t.get(&Url::parse("https://docs.example.org/missing").unwrap());
        assert!(miss.is_err());
    }
}
