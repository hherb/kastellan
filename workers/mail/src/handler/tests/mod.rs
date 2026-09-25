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
