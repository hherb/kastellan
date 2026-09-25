use super::*;

// --- mail.search: POSTs the query, never sets `smart` ---
struct SearchFake;
impl HttpGet for SearchFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn post_authed(&self, url: &Url, bearer: &str, ct: &str, body: &[u8], _m: usize) -> Result<RawResponse, String> {
        assert_eq!(bearer, "tok");
        assert_eq!(ct, "application/json");
        assert!(url.path().ends_with("/v1/search"), "path {}", url.path());
        let s = String::from_utf8_lossy(body);
        assert!(s.contains("qantas"), "body missing query: {s}");
        assert!(!s.contains("smart"), "body must not carry smart: {s}");
        // Real localmail keys results under "results" (not "hits").
        Ok(json_resp(br#"{"results":[],"next_cursor":null}"#))
    }
}

#[test]
fn search_posts_query_without_smart() {
    let mut h = MailHandler::with_client(client_with(Box::new(SearchFake)));
    let out = h.call("mail.search", serde_json::json!({"query": "qantas"})).unwrap();
    assert!(out["results"].is_array());
}

/// Echoes the sort back into the POST body so the test can read what was
/// sent, which is the half `SearchFake` cannot show.
struct SortEchoFake;
impl HttpGet for SortEchoFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn post_authed(&self, _: &Url, _: &str, _: &str, body: &[u8], _m: usize) -> Result<RawResponse, String> {
        let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
        Ok(json_resp(
            serde_json::to_vec(&serde_json::json!({"results": [], "sent_sort": sent["sort"]}))
                .unwrap()
                .as_slice(),
        ))
    }
}

/// #559: a planner that names no sort still gets a request whose ordering
/// this worker knows, rather than one that inherits localmail's default.
#[test]
fn search_sends_an_explicit_sort_when_the_planner_omits_it() {
    let mut h = MailHandler::with_client(client_with(Box::new(SortEchoFake)));
    let out = h.call("mail.search", serde_json::json!({"query": "q"})).unwrap();
    assert_eq!(out["sent_sort"], serde_json::json!(sort::DEFAULT_SORT));
}

#[test]
fn search_forwards_an_explicit_sort_unchanged() {
    let mut h = MailHandler::with_client(client_with(Box::new(SortEchoFake)));
    let out = h.call("mail.search", serde_json::json!({"query": "q", "sort": "date"})).unwrap();
    assert_eq!(out["sent_sort"], serde_json::json!("date"));
}

/// The defect #559 actually fixes: the ordering has to be readable in the
/// output, because that is where this planner has been shown to act on
/// advice (`ids::explain`) and not act on it (the parameter docs).
#[test]
fn search_annotates_the_response_with_the_ordering_it_requested() {
    let mut h = MailHandler::with_client(client_with(Box::new(SortEchoFake)));
    let out = h.call("mail.search", serde_json::json!({"query": "q"})).unwrap();
    let note = out[sort::ORDERING_KEY].as_str().expect("no ordering note");
    assert!(note.contains("NOT date order"), "{note}");

    let out = h.call("mail.search", serde_json::json!({"query": "q", "sort": "date"})).unwrap();
    let note = out[sort::ORDERING_KEY].as_str().expect("no ordering note");
    assert!(note.contains("newest first"), "{note}");
}

/// A localmail problem+json refusal, reproduced from the wire.
struct Problem400;
impl HttpGet for Problem400 {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn post_authed(&self, _: &Url, _: &str, _: &str, _: &[u8], _m: usize) -> Result<RawResponse, String> {
        Ok(RawResponse {
            status: 400,
            location: None,
            content_type: "application/problem+json".into(),
            body: br#"{"type": "/problems/validation-failed", "title": "Validation failed", "status": 400, "detail": "cursor: this cursor continues a date-sorted search; pass sort='date' or omit sort (got 'rank')"}"#.to_vec(),
        })
    }
}

/// The end of the chain the `problem` module exists for: a refusal must
/// reach the planner as localmail's sentence, not as an envelope that
/// spends the budget on `type`/`title`/`status` and truncates the advice.
#[test]
fn an_upstream_problem_json_surfaces_its_detail_not_the_envelope() {
    let mut h = MailHandler::with_client(client_with(Box::new(Problem400)));
    let err = h
        .call("mail.search", serde_json::json!({"query": "q", "sort": "rank", "cursor": "K|abc"}))
        .unwrap_err();
    assert_eq!(err.code, codes::OPERATION_FAILED);
    assert!(err.message.contains("pass sort='date'"), "{}", err.message);
    assert!(
        !err.message.contains("validation-failed"),
        "the envelope must not reach the planner: {}",
        err.message
    );
    // The whole sentence has to fit, tail included — that is the guarantee.
    let seen: String =
        err.message.chars().take(kastellan_protocol::STEP_ERR_DETAIL_MAX).collect();
    assert!(seen.contains("(got 'rank')"), "clamped to: {seen:?}");
}

/// #561: paging without a named sort must send **no** `sort` field, so the
/// cursor's own ordering stands. A defaulted `rank` here contradicts a date
/// cursor and localmail silently restarts at page one.
#[test]
fn search_sends_no_sort_when_paging_without_one() {
    let mut h = MailHandler::with_client(client_with(Box::new(SortEchoFake)));
    let out = h
        .call("mail.search", serde_json::json!({"query": "q", "cursor": "K|abc"}))
        .unwrap();
    assert_eq!(out["sent_sort"], serde_json::Value::Null, "sort must be absent while paging");
    let note = out[sort::ORDERING_KEY].as_str().expect("no ordering note");
    assert!(note.contains("cannot tell which"), "{note}");
}

/// A sort the planner named explicitly is still sent while paging — this
/// worker does not adjudicate the mismatch (that needs the cursor format,
/// which belongs to localmail).
#[test]
fn search_still_sends_an_explicit_sort_while_paging() {
    let mut h = MailHandler::with_client(client_with(Box::new(SortEchoFake)));
    let out = h
        .call(
            "mail.search",
            serde_json::json!({"query": "q", "cursor": "K|abc", "sort": "date"}),
        )
        .unwrap();
    assert_eq!(out["sent_sort"], serde_json::json!("date"));
}

// --- mail.search: the id filters the planner actually writes ---

/// Echoes the whole POST body back so a test can read what was sent.
pub(super) struct BodyEchoFake;
impl HttpGet for BodyEchoFake {
    fn get(&self, _: &Url) -> Result<RawResponse, String> { unreachable!() }
    fn transport_kind(&self) -> &'static str { "fake" }
    fn post_authed(&self, _: &Url, _: &str, _: &str, body: &[u8], _m: usize) -> Result<RawResponse, String> {
        let sent: serde_json::Value = serde_json::from_slice(body).unwrap();
        Ok(json_resp(
            serde_json::to_vec(&serde_json::json!({"results": [], "sent": sent}))
                .unwrap()
                .as_slice(),
        ))
    }
}

/// Live task 161 wrote this twice and was refused twice: `account_ids` where
/// `mail.list_messages` takes it. It is now folded into `filters`, as the
/// strings localmail's model requires.
#[test]
fn search_accepts_top_level_account_ids_and_sends_them_as_filter_strings() {
    let mut h = MailHandler::with_client(client_with(Box::new(BodyEchoFake)));
    let out = h
        .call("mail.search", serde_json::json!({"query": "flight", "account_ids": [1]}))
        .unwrap();
    assert_eq!(out["sent"]["filters"]["account_ids"], serde_json::json!(["1"]));
}

/// The other half of task 161: nested, but numeric — which localmail
/// answered with a raw 422 validation envelope.
#[test]
fn search_coerces_numeric_ids_inside_filters_to_strings() {
    let mut h = MailHandler::with_client(client_with(Box::new(BodyEchoFake)));
    let out = h
        .call(
            "mail.search",
            serde_json::json!({"query": "flight", "filters": {"account_ids": [1], "has_attachment": true}}),
        )
        .unwrap();
    assert_eq!(out["sent"]["filters"]["account_ids"], serde_json::json!(["1"]));
    assert_eq!(out["sent"]["filters"]["has_attachment"], serde_json::json!(true));
}

#[test]
fn search_without_id_filters_sends_no_filters_key() {
    let mut h = MailHandler::with_client(client_with(Box::new(BodyEchoFake)));
    let out = h.call("mail.search", serde_json::json!({"query": "flight"})).unwrap();
    assert!(out["sent"].get("filters").is_none(), "sent: {}", out["sent"]);
}
