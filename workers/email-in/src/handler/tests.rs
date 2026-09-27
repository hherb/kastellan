//! Split out of `handler.rs` to keep that file under the project's 500-LOC
//! guideline (mirrors core's `bus.rs`/`bus/tests.rs` split). `use super::*`
//! brings in everything from `handler.rs` (`EmailInHandler`, `build_event`,
//! `header_values`, …).

use super::*;

#[test]
fn poll_maps_changes_and_detail_into_events() {
    let mut h = handler_with_canned();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let ev = &out["events"][0];
    assert_eq!(ev["peer"], "me@example.org", "peer is the From address");
    assert_eq!(ev["conversation"], "<mid-1@example.org>", "conversation is the Message-ID");
    assert_eq!(ev["ack_token"], "7", "ack_token is the localmail message id");
    assert!(ev["body"].as_str().unwrap().contains("what is 17*23"));
}

#[test]
fn from_address_is_lowercased_so_it_matches_the_paired_peer() {
    let mut h = handler_with_from("Me@Example.ORG");
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"][0]["peer"], "me@example.org");
}

#[test]
fn reply_to_is_never_used_as_the_peer() {
    // Honouring Reply-To would let a sender who passes the gate redirect
    // the agent's reply to a third party.
    let mut h = handler_with_reply_to("attacker@evil.example");
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"][0]["peer"], "me@example.org");
}

#[test]
fn auth_results_are_returned_verbatim_and_in_order() {
    // NOTE: this fixture puts both values under ONE JSON key (a single
    // "Authentication-Results" spelling) — that only exercises the trivial
    // fact that a JSON array preserves its own element order. It does NOT
    // exercise cross-key wire-order preservation; the harder property (two
    // DIFFERENT-cased spellings, order NOT reconstructible) is covered by
    // `mixed_case_auth_results_headers_mark_order_unknown` below.
    let mut h = handler_with_auth_results(vec![
        "mx.example.net; dmarc=pass".to_string(),
        "evil.example.com; dmarc=pass".to_string(),
    ]);
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let ev = &out["events"][0];
    let ar = ev["auth_results"].as_array().unwrap();
    assert_eq!(ar.len(), 2, "every header is surfaced; core decides which counts");
    assert_eq!(ar[0], "mx.example.net; dmarc=pass", "single-key JSON array order is preserved (trivial)");
    assert_eq!(ar[1], "evil.example.com; dmarc=pass", "single-key JSON array order is preserved (trivial)");
    assert_eq!(ev["auth_results_order_known"], true, "a single exact-cased header key means order IS fully known");
}

#[test]
fn mixed_case_auth_results_headers_mark_order_unknown() {
    // Two DISTINCT-cased spellings of the same logical header land in two
    // separate JSON object keys (confirmed against localmail's own parser —
    // see task-7-report.md). Iterating them via serde_json's BTreeMap-backed
    // `Value::Object` is alphabetical, not wire order, so this worker cannot
    // tell which header the MX actually wrote first — it must say so rather
    // than silently pick the BTreeMap's order (the exploitable gate bypass
    // from the task-7 review: an attacker's all-caps forgery would otherwise
    // always sort first and win element 0).
    let mut h = handler_with_mixed_case_auth_results();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let ev = &out["events"][0];
    assert_eq!(
        ev["auth_results_order_known"], false,
        "two distinct-cased Authentication-Results keys cannot be ordered against each other"
    );
    let ar = ev["auth_results"].as_array().unwrap();
    assert_eq!(ar.len(), 2, "both occurrences are still surfaced; nothing is silently dropped");
}

#[test]
fn conversation_falls_back_to_localmail_id_when_message_id_header_absent() {
    let mut h = handler_with_no_message_id_header();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"][0]["conversation"], "localmail:7");
}

#[test]
fn empty_changes_yields_no_events() {
    let mut h = handler_with_empty_changes();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"].as_array().unwrap().len(), 0);
    assert_eq!(out["skipped"].as_array().unwrap().len(), 0);
}

#[test]
fn poll_honours_timeout_when_batch_yields_no_events() {
    // Regression test for the tight-spin bug: a batch that resolves to zero
    // EVENTS (everything skipped) must not return before timeout_ms elapses
    // — `PolledWorkerDriver` only sleeps on a hard error, so returning early
    // here would make an unattributable message a remote-triggerable tight
    // poll loop between the driver and this worker.
    let mut h = handler_with_unattributable_message();
    let start = std::time::Instant::now();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 50})).unwrap();
    let elapsed = start.elapsed();
    assert!(elapsed >= std::time::Duration::from_millis(40), "poll returned too early: {elapsed:?}");
    assert_eq!(out["events"].as_array().unwrap().len(), 0);
}

/// The idle cost of the channel is `timeout_ms / POLL_INTERVAL` upstream
/// requests per window, NOT one — localmail's `/v1/changes` answers
/// immediately, so `poll` re-asks to emulate a long poll. At the original
/// 250ms interval a 15s window issued 60 requests (~4/s forever, and — once
/// force-routed — 60 fresh TLS handshakes and 60 `egress.allowed` audit rows,
/// since `proxy_connect` pools nothing). Pinned here so raising the window, or
/// dropping the interval back, can't quietly restore that.
#[test]
fn poll_issues_one_changes_request_per_interval_not_one_per_250ms() {
    let (mut h, log) = handler_recording_requests();
    // A 1.5s window: with a seconds-scale interval that is one call at t=0 and
    // one at the deadline. At the original 250ms it was ~7 — so `<= 3` fails
    // loudly on a regression while keeping this test's own wall clock at 1.5s
    // (the sleep is `POLL_INTERVAL.min(deadline - now)`, so a longer interval
    // never makes the test slower than the window it asks for).
    h.call("email.poll", serde_json::json!({"timeout_ms": 1_500})).unwrap();
    let calls = log.lock().unwrap();
    let changes = calls.iter().filter(|c| c.contains("/v1/changes")).count();
    assert!(
        (1..=3).contains(&changes),
        "a 1.5s window must issue ~2 changes calls, not one per 250ms; got {changes}: {calls:?}"
    );
}

#[test]
fn poll_interval_is_seconds_not_milliseconds() {
    // A guard on the constant itself, so the reasoning in its doc comment
    // can't be undone by a one-token edit without a test failing.
    assert!(
        POLL_INTERVAL >= std::time::Duration::from_secs(1),
        "a sub-second interval makes an idle channel hammer localmail: {POLL_INTERVAL:?}"
    );
}

#[test]
fn unattributable_message_lands_in_skipped_not_vanishing() {
    let mut h = handler_with_unattributable_message();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert_eq!(out["events"].as_array().unwrap().len(), 0, "no usable From ⇒ no event");
    let skipped = out["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "the message must be reported, not silently dropped");
    assert_eq!(skipped[0]["message_id"], "7");
    assert!(skipped[0]["reason"].as_str().unwrap().contains("From"));
}

#[test]
fn failed_message_detail_for_one_message_does_not_abort_the_batch() {
    let mut h = handler_with_one_good_one_failing_detail();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let events = out["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "the good message still becomes an event");
    assert_eq!(events[0]["ack_token"], "7");
    let skipped = out["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "the failing message is recorded, not silently lost");
    assert_eq!(skipped[0]["message_id"], "8");
}

// ---- PERMANENT vs TRANSIENT `message_detail` failures (final whole-branch
// review, Important 2). A skipped id is ACKED by core, which advances
// localmail's monotonic `GREATEST` cursor past it FOREVER — so only a failure
// that can never succeed may be skipped. Anything retryable must be omitted
// from both lists so the message is redelivered. ----

#[test]
fn is_permanent_classifies_each_failure_class() {
    // 4xx: permanently unfetchable (the id is gone / the request is malformed).
    assert!(is_permanent(&EmailError::Upstream { status: 404, body: "gone".into() }));
    assert!(is_permanent(&EmailError::Upstream { status: 400, body: "bad".into() }));
    assert!(is_permanent(&EmailError::Upstream { status: 410, body: "gone".into() }));
    // …except the four retryable 4xx statuses. 408/429 are retryable by HTTP
    // semantics; 401/403 are retryable after an OPERATOR action (a rotated
    // localmail token, a re-granted api-user), so acking them away would
    // destroy mail over a recoverable credential problem — see `is_permanent`.
    assert!(!is_permanent(&EmailError::Upstream { status: 408, body: "timeout".into() }));
    assert!(!is_permanent(&EmailError::Upstream { status: 429, body: "slow down".into() }));
    assert!(!is_permanent(&EmailError::Upstream { status: 401, body: "unauthorized".into() }));
    assert!(!is_permanent(&EmailError::Upstream { status: 403, body: "nope".into() }));
    // 5xx: the server's temporary problem.
    assert!(!is_permanent(&EmailError::Upstream { status: 500, body: "boom".into() }));
    assert!(!is_permanent(&EmailError::Upstream { status: 503, body: "restarting".into() }));
    // Transport: reset / TLS / no route — retryable.
    assert!(!is_permanent(&EmailError::Transport("connection reset".into())));
    // Worker-built request: an identical retry fails identically.
    assert!(is_permanent(&EmailError::BadParams("not a base url".into())));
}

#[test]
fn a_permanent_404_lands_in_skipped_so_the_cursor_can_move_past_it() {
    // One poisoned message must not wedge the channel forever: core acks
    // skipped ids, which is exactly what a 404 needs.
    let mut h = handler_with_detail_status("8", 404);
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let events = out["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "the good message still becomes an event");
    assert_eq!(events[0]["ack_token"], "7");
    let skipped = out["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "a permanent failure is recorded for ack");
    assert_eq!(skipped[0]["message_id"], "8");
    assert!(skipped[0]["reason"].as_str().unwrap().contains("404"));
}

#[test]
fn a_transient_5xx_is_not_skipped_so_the_message_is_redelivered() {
    // Acking this would advance localmail's GREATEST cursor past a message
    // the bus never saw — the user's email would be gone for good.
    let mut h = handler_with_detail_status("8", 503);
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let events = out["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "the batch still continues with the other messages");
    assert_eq!(events[0]["ack_token"], "7");
    assert!(
        out["skipped"].as_array().unwrap().is_empty(),
        "a 5xx must NOT be acked away: {:?}",
        out["skipped"]
    );
}

#[test]
fn an_auth_denied_detail_is_not_acked_away_so_a_token_rotation_can_recover_it() {
    // 401/403 mean "fix the localmail credential", not "this message is gone".
    // Acking either would advance the monotonic cursor past mail the operator
    // could otherwise have recovered by re-granting the api-user (review).
    for status in [401u16, 403] {
        let mut h = handler_with_detail_status("8", status);
        let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
        let events = out["events"].as_array().unwrap();
        assert_eq!(events.len(), 1, "the good message still becomes an event ({status})");
        assert_eq!(events[0]["ack_token"], "7");
        assert!(
            out["skipped"].as_array().unwrap().is_empty(),
            "a {status} must NOT be acked away: {:?}",
            out["skipped"]
        );
    }
}

#[test]
fn a_transient_transport_error_is_not_skipped_so_the_message_is_redelivered() {
    // The localmail-restart / egress-blip case the review named explicitly.
    let mut h = handler_with_transport_failure_on("8");
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    let events = out["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "the batch still continues with the other messages");
    assert_eq!(events[0]["ack_token"], "7");
    assert!(
        out["skipped"].as_array().unwrap().is_empty(),
        "a transport failure must NOT be acked away: {:?}",
        out["skipped"]
    );
}

#[test]
fn a_whole_batch_failing_transiently_acks_nothing_at_all() {
    // localmail down between `changes` and `messages/{id}`: nothing may be
    // acked, so every message comes back on the next poll.
    let mut h = handler_with_all_details_failing_transiently();
    let out = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap();
    assert!(out["events"].as_array().unwrap().is_empty());
    assert!(
        out["skipped"].as_array().unwrap().is_empty(),
        "nothing may be acked when the whole batch failed transiently: {:?}",
        out["skipped"]
    );
}

#[test]
fn ack_posts_the_cursor_upstream() {
    let (mut h, recorder) = handler_recording_requests();
    h.call("email.ack", serde_json::json!({"cursor": "7"})).unwrap();
    assert!(recorder.lock().unwrap().iter().any(|r| r.contains("/v1/changes/ack")));
}

#[test]
fn unknown_method_is_rejected() {
    let mut h = handler_with_canned();
    assert!(h.call("email.nope", serde_json::json!({})).is_err());
}

#[test]
fn init_returns_configured_address_and_subscription() {
    let mut h = handler_with_canned();
    let out = h.call("email.init", serde_json::json!({})).unwrap();
    assert_eq!(out["address"], "agent@example.org");
    assert_eq!(out["subscription"], "sub");
}

// --- test helpers (fakes over kastellan_worker_web_common::http::HttpGet) ---

use kastellan_worker_web_common::http::{HttpGet, RawResponse};
use std::sync::{Arc, Mutex};
use url::Url;

fn json_resp(body: &serde_json::Value) -> RawResponse {
    RawResponse {
        status: 200,
        location: None,
        content_type: "application/json".into(),
        body: serde_json::to_vec(body).unwrap(),
    }
}

fn client_with(transport: Box<dyn HttpGet>) -> crate::client::EmailClient {
    crate::client::EmailClient::for_test(
        Url::parse("http://127.0.0.1:8443").unwrap(),
        "tok".into(),
        transport,
    )
}

fn changes_resp(ids: &[&str]) -> RawResponse {
    let msgs: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "message_id": id,
                "subject": "s",
                "from": {"address": "ignored@example.org", "name": serde_json::Value::Null},
                "date": serde_json::Value::Null,
                "account": {"id": "1", "name": "acct"},
            })
        })
        .collect();
    json_resp(&serde_json::json!({
        "new_messages": msgs,
        "next_cursor": ids.last().unwrap_or(&"0"),
    }))
}

fn empty_changes_resp() -> RawResponse {
    json_resp(&serde_json::json!({"new_messages": [], "next_cursor": "0"}))
}

/// One localmail `GET /v1/messages/{id}?headers=full` response.
fn detail_resp(
    from: &str,
    message_id_header: Option<&str>,
    reply_to: Option<&str>,
    auth_results: &[&str],
) -> RawResponse {
    let mut headers = serde_json::Map::new();
    headers.insert("From".to_string(), serde_json::json!([from]));
    if let Some(mid) = message_id_header {
        headers.insert("Message-ID".to_string(), serde_json::json!([mid]));
    }
    if let Some(rt) = reply_to {
        headers.insert("Reply-To".to_string(), serde_json::json!([rt]));
    }
    if !auth_results.is_empty() {
        headers.insert("Authentication-Results".to_string(), serde_json::json!(auth_results));
    }
    json_resp(&serde_json::json!({
        "id": "7",
        "subject": "s",
        "from": {"address": from, "name": serde_json::Value::Null},
        "body_text": "what is 17*23",
        "body_html": serde_json::Value::Null,
        "headers": headers,
    }))
}

fn path_and_query(url: &Url) -> String {
    match url.query() {
        Some(q) => format!("{}?{}", url.path(), q),
        None => url.path().to_string(),
    }
}

/// Fake transport: GETs to a `/v1/changes` path get `changes`, anything
/// else (the message detail fetch) gets `detail`. Not FIFO — safe to call
/// any number of times, which the long-poll retry loop needs.
struct PathFake {
    changes: RawResponse,
    detail: RawResponse,
}
impl HttpGet for PathFake {
    fn get(&self, _u: &Url) -> Result<RawResponse, String> {
        unreachable!("client uses get_authed")
    }
    fn transport_kind(&self) -> &'static str {
        "fake"
    }
    fn get_authed(&self, url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
        if url.path().starts_with("/v1/changes") {
            Ok(self.changes.clone())
        } else {
            Ok(self.detail.clone())
        }
    }
}

fn fake_with(changes: RawResponse, detail: RawResponse) -> Box<dyn HttpGet> {
    Box::new(PathFake { changes, detail })
}

fn handler_with_canned() -> crate::handler::EmailInHandler {
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("me@example.org", Some("<mid-1@example.org>"), None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_from(addr: &str) -> crate::handler::EmailInHandler {
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp(addr, Some("<mid-1@example.org>"), None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_reply_to(reply_to: &str) -> crate::handler::EmailInHandler {
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("me@example.org", Some("<mid-1@example.org>"), Some(reply_to), &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_auth_results(v: Vec<String>) -> crate::handler::EmailInHandler {
    let refs: Vec<&str> = v.iter().map(String::as_str).collect();
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("me@example.org", Some("<mid-1@example.org>"), None, &refs),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_empty_changes() -> crate::handler::EmailInHandler {
    let transport = fake_with(
        empty_changes_resp(),
        detail_resp("me@example.org", None, None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_no_message_id_header() -> crate::handler::EmailInHandler {
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("me@example.org", None, None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

/// A detail response with TWO distinct-cased `authentication-results`
/// object keys — `detail_resp` only ever builds one exact-cased key, so this
/// is a dedicated raw builder for the mixed-case scenario.
fn mixed_case_auth_results_detail() -> RawResponse {
    let mut headers = serde_json::Map::new();
    headers.insert("From".to_string(), serde_json::json!(["me@example.org"]));
    headers.insert("Message-ID".to_string(), serde_json::json!(["<mid-1@example.org>"]));
    headers.insert("Authentication-Results".to_string(), serde_json::json!(["mx.example.net; dmarc=pass"]));
    headers.insert("AUTHENTICATION-RESULTS".to_string(), serde_json::json!(["forged.example; dmarc=pass"]));
    json_resp(&serde_json::json!({
        "id": "7",
        "subject": "s",
        "from": {"address": "me@example.org", "name": serde_json::Value::Null},
        "body_text": "body",
        "body_html": serde_json::Value::Null,
        "headers": headers,
    }))
}

fn handler_with_mixed_case_auth_results() -> crate::handler::EmailInHandler {
    let transport = fake_with(changes_resp(&["7"]), mixed_case_auth_results_detail());
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_unattributable_message() -> crate::handler::EmailInHandler {
    // Empty From address ⇒ build_event returns None ⇒ the message is
    // unattributable and must land in `skipped`, never vanish.
    let transport = fake_with(
        changes_resp(&["7"]),
        detail_resp("", Some("<mid-1@example.org>"), None, &[]),
    );
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

/// How a `/v1/messages/{id}` fetch fails in [`DetailOutcomeFake`].
enum DetailOutcome {
    /// A non-2xx HTTP status → `EmailError::Upstream { status, .. }`.
    Status(u16),
    /// A transport-level failure (reset / TLS / no route) →
    /// `EmailError::Transport(_)`.
    TransportError,
}

/// Fake transport: `/v1/changes` returns two new message ids ("7", "8") and
/// every `/v1/messages/{id}` succeeds EXCEPT the ids in `failing`, which
/// produce `outcome`. One fake covering every `message_detail` failure class,
/// so the permanent-vs-transient split is exercised over identical fixtures.
struct DetailOutcomeFake {
    changes: RawResponse,
    good_detail: RawResponse,
    failing: Vec<String>,
    outcome: DetailOutcome,
}
impl HttpGet for DetailOutcomeFake {
    fn get(&self, _u: &Url) -> Result<RawResponse, String> {
        unreachable!("client uses get_authed")
    }
    fn transport_kind(&self) -> &'static str {
        "fake"
    }
    fn get_authed(&self, url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
        if url.path().starts_with("/v1/changes") {
            return Ok(self.changes.clone());
        }
        let id = url.path().rsplit('/').next().unwrap_or_default();
        if self.failing.iter().any(|f| f == id) {
            return match self.outcome {
                DetailOutcome::Status(status) => Ok(RawResponse {
                    status,
                    location: None,
                    content_type: "text/plain".into(),
                    body: format!("upstream said {status}").into_bytes(),
                }),
                DetailOutcome::TransportError => Err("connection reset by peer".to_string()),
            };
        }
        Ok(self.good_detail.clone())
    }
}

fn handler_with_detail_outcome(
    failing: &[&str],
    outcome: DetailOutcome,
) -> crate::handler::EmailInHandler {
    let transport: Box<dyn HttpGet> = Box::new(DetailOutcomeFake {
        changes: changes_resp(&["7", "8"]),
        good_detail: detail_resp("me@example.org", Some("<mid-1@example.org>"), None, &[]),
        failing: failing.iter().map(|s| (*s).to_string()).collect(),
        outcome,
    });
    crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    )
}

fn handler_with_detail_status(id: &str, status: u16) -> crate::handler::EmailInHandler {
    handler_with_detail_outcome(&[id], DetailOutcome::Status(status))
}

fn handler_with_transport_failure_on(id: &str) -> crate::handler::EmailInHandler {
    handler_with_detail_outcome(&[id], DetailOutcome::TransportError)
}

fn handler_with_all_details_failing_transiently() -> crate::handler::EmailInHandler {
    handler_with_detail_outcome(&["7", "8"], DetailOutcome::TransportError)
}

/// `/v1/messages/7` succeeds, `/v1/messages/8` 404s — proves one message's
/// PERMANENT `message_detail` failure does not abort the rest of the batch.
fn handler_with_one_good_one_failing_detail() -> crate::handler::EmailInHandler {
    handler_with_detail_status("8", 404)
}

/// Fake transport recording every request's method + path+query, so the
/// ack test can assert what was actually sent upstream.
struct RecordingFake {
    log: Arc<Mutex<Vec<String>>>,
}
impl HttpGet for RecordingFake {
    fn get(&self, _u: &Url) -> Result<RawResponse, String> {
        unreachable!("client uses get_authed/post_authed")
    }
    fn transport_kind(&self) -> &'static str {
        "fake"
    }
    fn get_authed(&self, url: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
        self.log.lock().unwrap().push(format!("GET {}", path_and_query(url)));
        Ok(empty_changes_resp())
    }
    fn post_authed(
        &self,
        url: &Url,
        _bearer: &str,
        _content_type: &str,
        _body: &[u8],
        _max: usize,
    ) -> Result<RawResponse, String> {
        self.log.lock().unwrap().push(format!("POST {}", path_and_query(url)));
        Ok(RawResponse { status: 204, location: None, content_type: String::new(), body: Vec::new() })
    }
}

fn handler_recording_requests() -> (crate::handler::EmailInHandler, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let transport: Box<dyn HttpGet> = Box::new(RecordingFake { log: log.clone() });
    let h = crate::handler::EmailInHandler::with_client(
        client_with(transport),
        "sub".to_string(),
        "agent@example.org".to_string(),
    );
    (h, log)
}

/// #673: an expired or under-granted channel credential (401/403) is reported
/// as `UPSTREAM_AUTH_FAILED` — the code the core's polled driver recognises as
/// "credential refused" — never as the core's own `POLICY_DENIED`.
#[test]
fn a_401_or_403_maps_to_upstream_auth_failed_never_policy_denied() {
    for status in [401u16, 403] {
        let e = email_err_to_rpc(EmailError::Upstream { status, body: "unauthorized".into() });
        assert_eq!(e.code, codes::UPSTREAM_AUTH_FAILED, "status {status}: {}", e.message);
        assert!(e.message.starts_with("localmail "), "names the upstream: {}", e.message);
    }
    let e = email_err_to_rpc(EmailError::Upstream { status: 500, body: "boom".into() });
    assert_eq!(e.code, codes::OPERATION_FAILED, "only the credential statuses moved");
    assert_eq!(e.message, "localmail 500: boom");
}

/// Answers every authenticated GET with one fixed status — `/v1/changes`
/// included — as a localmail refusing the channel's credential would.
struct RefusingFake(u16);
impl HttpGet for RefusingFake {
    fn get(&self, _u: &Url) -> Result<RawResponse, String> {
        unreachable!("client uses get_authed")
    }
    fn transport_kind(&self) -> &'static str {
        "fake"
    }
    fn get_authed(&self, _u: &Url, _bearer: &str, _max: usize) -> Result<RawResponse, String> {
        Ok(RawResponse {
            status: self.0,
            location: None,
            content_type: "application/problem+json".into(),
            body: br#"{"detail":"token is invalid, expired, or revoked"}"#.to_vec(),
        })
    }
}

/// The incident path end to end within the worker (#673): the credential is
/// refused on `/v1/changes`, so `email.poll` itself fails — and it must fail
/// as `UPSTREAM_AUTH_FAILED`, the code the core's polled driver turns into an
/// operator ERROR. (A refusal on one message's detail fetch is a different
/// path: deliberately transient, see `is_permanent`.)
#[test]
fn a_poll_refused_by_localmail_fails_as_upstream_auth_failed() {
    for status in [401u16, 403] {
        let mut h = crate::handler::EmailInHandler::with_client(
            client_with(Box::new(RefusingFake(status))),
            "sub".to_string(),
            "agent@example.org".to_string(),
        );
        let e = h.call("email.poll", serde_json::json!({"timeout_ms": 10})).unwrap_err();
        assert_eq!(e.code, codes::UPSTREAM_AUTH_FAILED, "status {status}: {}", e.message);
        assert!(e.message.contains(&format!("HTTP {status}")), "{}", e.message);
    }
}
