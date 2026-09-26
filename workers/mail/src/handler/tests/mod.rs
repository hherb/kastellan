//! Unit tests for [`super::MailHandler`], split by tool family.
//!
//! `search` covers `mail.search`, `messages` the plain GET tools
//! (`get_message`, `list_messages`, `list_accounts`), and `attachments` both
//! attachment tools. The shared fakes' helpers live here.

mod attachments;
mod messages;
mod search;

use super::*;
use kastellan_worker_web_common::http::{HttpGet, RawResponse};
use url::Url;

fn client_with(transport: Box<dyn HttpGet>) -> MailClient {
    MailClient::for_test(Url::parse("http://127.0.0.1:8000").unwrap(), "tok".into(), transport)
}

fn json_resp(body: &[u8]) -> RawResponse {
    RawResponse { status: 200, location: None, content_type: "application/json".into(), body: body.to_vec() }
}

#[test]
fn unknown_method_is_method_not_found() {
    // Build via for_test with a transport that is never called.
    struct Never;
    impl HttpGet for Never {
        fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
        fn transport_kind(&self) -> &'static str { "never" }
    }
    let mut h = MailHandler::with_client(client_with(Box::new(Never)));
    let err = h.call("nope", serde_json::json!({})).unwrap_err();
    assert_eq!(err.code, codes::METHOD_NOT_FOUND);
}

#[test]
fn safe_name_strips_traversal_and_prefixes_sha() {
    let n = safe_attachment_name(Some("../../etc/passwd"), &"b".repeat(64));
    assert_eq!(n, "bbbbbbbbbbbb_passwd");
    let n2 = safe_attachment_name(None, &"c".repeat(64));
    assert_eq!(n2, "cccccccccccc_attachment");
}

// --- the localmail version gate (#760) ---

/// Serves `/v1/version` with the given body (or a 404 for `None`) and answers
/// every other GET with an accounts list. POSTs (search) are never reached
/// when the gate refuses, and answer an empty page when it passes.
struct VersionFake(Option<&'static str>);
impl HttpGet for VersionFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn get_authed(&self, url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
        if url.path() == "/v1/version" {
            return Ok(match self.0 {
                Some(body) => json_resp(body.as_bytes()),
                None => RawResponse { status: 404, location: None, content_type: "application/json".into(), body: br#"{"detail":"Not Found"}"#.to_vec() },
            });
        }
        if url.path().starts_with("/v1/messages/") {
            // Message-shaped, so a compact `get_message` is a real read.
            return Ok(json_resp(br#"{"id":"5","subject":"s"}"#));
        }
        Ok(json_resp(br#"[{"id":"1"}]"#))
    }
    fn post_authed(&self, _: &Url, _: &str, _: &str, _: &[u8], _: usize) -> Result<RawResponse, String> {
        Ok(json_resp(br#"{"results":[],"next_cursor":null}"#))
    }
}

const CURRENT: &str = r#"{"api_major":1,"api_minor":3,"server_version":"0.3.0"}"#;
const SLICE_D_ONLY: &str = r#"{"api_major":1,"api_minor":2,"server_version":"0.2.9"}"#;

#[test]
fn a_current_localmail_passes_the_gate() {
    let mut h = MailHandler::with_client_unverified(client_with(Box::new(VersionFake(Some(CURRENT)))));
    h.call("mail.search", serde_json::json!({"query": "q"})).expect("1.3 is current");
}

/// Every gated tool — the slice D/E routes, and `get_message` asking for
/// headers — is refused against an older server, with the upgrade named —
/// never a misleading 404, a whole unpaged text or a header-less message.
#[test]
fn every_gated_tool_is_refused_against_an_older_localmail() {
    let calls = [
        ("mail.search", serde_json::json!({"query": "q"})),
        ("mail.get_attachment_text", serde_json::json!({"sha256": "a".repeat(64)})),
        ("mail.get_attachment", serde_json::json!({"sha256": "a".repeat(64)})),
        // Gated with the rest, by the one minimum (1.3). `?headers=list`
        // itself is api_minor 1, which an older server answers with a
        // header-less 200 rather than an error.
        ("mail.get_message", serde_json::json!({"message_id": 5, "full_headers": true})),
    ];
    for (method, params) in calls {
        for body in [Some(SLICE_D_ONLY), None] {
            let mut h = MailHandler::with_client_unverified(client_with(Box::new(VersionFake(body))));
            let err = h.call(method, params.clone()).unwrap_err();
            assert_eq!(err.code, codes::OPERATION_FAILED, "{method}");
            assert!(err.message.contains("upgrade localmail"), "{method}: {}", err.message);
        }
    }
}

/// The tools that use only routes every `/v1` serves keep working, and do not
/// even ask — so an operator diagnosing an old localmail still has them.
#[test]
fn ungated_tools_work_against_an_older_localmail() {
    let calls = [
        ("mail.list_accounts", serde_json::json!({})),
        ("mail.list_messages", serde_json::json!({})),
        ("mail.get_message", serde_json::json!({"message_id": 5})),
        ("mail.get_message", serde_json::json!({"message_id": 5, "full_headers": false})),
    ];
    for (method, params) in &calls {
        let req = Request::parse(Tool::from_method(method).unwrap(), params.clone()).unwrap();
        assert!(!req.needs_current_api(), "{method} {params}");
    }
    let mut h = MailHandler::with_client_unverified(client_with(Box::new(VersionFake(Some(SLICE_D_ONLY)))));
    h.call("mail.list_accounts", serde_json::json!({})).expect("list_accounts is not gated");
    let msg = h.call("mail.get_message", serde_json::json!({"message_id": 5})).expect("a compact read is not gated");
    assert_eq!(msg["id"], "5", "{msg}");
}

/// A `full_headers` that is not a JSON bool is invalid params — never coerced
/// to true and sent as `?headers=list` to a server nobody checked, and never
/// coerced to false and served compact. Since #765 it is refused before the
/// gate (so against any localmail); a lenient parse would fail this test.
#[test]
fn a_non_bool_full_headers_is_invalid_params_not_an_ungated_header_read() {
    for bad in [serde_json::json!("true"), serde_json::json!(1), serde_json::json!(null)] {
        let mut h = MailHandler::with_client_unverified(client_with(Box::new(VersionFake(Some(SLICE_D_ONLY)))));
        let err = h.call("mail.get_message", serde_json::json!({"message_id": 5, "full_headers": bad})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS, "{bad}: {}", err.message);
    }
}

/// A transport that fails the test on ANY request — so a call that reaches it
/// has done network work (the version gate included) before refusing.
struct NoNetwork;
impl HttpGet for NoNetwork {
    fn get(&self, url: &Url) -> Result<RawResponse, String> { panic!("unexpected GET {url}") }
    fn transport_kind(&self) -> &'static str { "no-network" }
    fn get_authed(&self, url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
        panic!("params must be refused before any request; got GET {url}")
    }
    fn post_authed(&self, url: &Url, _: &str, _: &str, _: &[u8], _: usize) -> Result<RawResponse, String> {
        panic!("params must be refused before any request; got POST {url}")
    }
}

/// #765: invalid params are `INVALID_PARAMS` whatever localmail is — checked
/// before the version gate, and before any other request. When the gate ran
/// first, an old or unreachable localmail turned each of these into "upgrade
/// localmail" (or a transport fault), and the planner retried a call it should
/// have corrected.
#[test]
fn invalid_params_are_refused_before_the_version_gate() {
    let calls = [
        // get_message asking for headers (the gated form), without a usable id.
        ("mail.get_message", serde_json::json!({"full_headers": true})),
        ("mail.get_message", serde_json::json!({"message_id": "abc", "full_headers": true})),
        ("mail.get_message", serde_json::json!({"message_id": 5, "full_headers": true, "x": 1})),
        // search: an unknown key, a wrong-typed field, and a bad filter.
        ("mail.search", serde_json::json!({"query": "q", "nope": 1})),
        ("mail.search", serde_json::json!({"query": 7})),
        ("mail.search", serde_json::json!({"query": "q", "filters": "not-an-object"})),
        // both attachment tools: nothing named, index without a message, a bad hash.
        ("mail.get_attachment_text", serde_json::json!({})),
        ("mail.get_attachment_text", serde_json::json!({"index": 0})),
        ("mail.get_attachment_text", serde_json::json!({"sha256": "../../etc/passwd"})),
        ("mail.get_attachment", serde_json::json!({"filename": "a.pdf"})),
        ("mail.get_attachment", serde_json::json!({"sha256": "A".repeat(64)})),
    ];
    for (method, params) in calls {
        let mut h = MailHandler::with_client_unverified(client_with(Box::new(NoNetwork)));
        let err = h.call(method, params.clone()).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS, "{method} {params}: {}", err.message);
        assert!(!err.message.contains("upgrade localmail"), "{method} {params}: {}", err.message);
    }
}

/// A refusal is not remembered: a localmail upgraded under a long-lived worker
/// is picked up on the next call. A success is, so it is asked once.
struct UpgradingFake(std::sync::atomic::AtomicUsize);
impl HttpGet for UpgradingFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn get_authed(&self, url: &Url, _b: &str, _m: usize) -> Result<RawResponse, String> {
        assert_eq!(url.path(), "/v1/version");
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(n < 2, "a passed gate must not be asked again");
        Ok(json_resp(if n == 0 { SLICE_D_ONLY } else { CURRENT }.as_bytes()))
    }
    fn post_authed(&self, _: &Url, _: &str, _: &str, _: &[u8], _: usize) -> Result<RawResponse, String> {
        Ok(json_resp(br#"{"results":[]}"#))
    }
}

#[test]
fn a_refusal_is_asked_again_and_a_pass_is_remembered() {
    let fake = UpgradingFake(std::sync::atomic::AtomicUsize::new(0));
    let mut h = MailHandler::with_client_unverified(client_with(Box::new(fake)));
    let q = serde_json::json!({"query": "q"});
    assert!(h.call("mail.search", q.clone()).is_err(), "1.2 refused");
    h.call("mail.search", q.clone()).expect("upgraded to 1.3");
    h.call("mail.search", q).expect("remembered; the fake panics on a third ask");
}
