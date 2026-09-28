//! Email wire codecs + the polled-driver spec. This is where the raw
//! material from `email-in`'s `email.poll` result becomes gated evidence:
//! [`parse_email_poll_with`] applies [`trusted_dmarc_pass`] over the
//! `Authentication-Results` headers the worker returned and strips the
//! per-pairing token out of every body via [`extract_token`], BEFORE the
//! body ever becomes a task instruction.
//!
//! ## Fail-closed on `auth_results_order_known == false`
//!
//! `email-in`'s wire shape (`workers/email-in/src/handler.rs::build_event`)
//! carries a per-event `auth_results_order_known` flag. `serde_json` in this
//! workspace has no `preserve_order` feature, so a JSON object's keys iterate
//! in BYTE order, not wire order. localmail groups every wire occurrence of
//! one EXACT-cased header name into one JSON array (preserving order WITHIN
//! that key) — but a header repeated with a DIFFERENT case (e.g. an attacker
//! writing `AUTHENTICATION-RESULTS:` into a message they send, alongside the
//! MX's genuine `Authentication-Results:`) becomes a SECOND, distinct object
//! key, and `'A'` (0x41) sorts before `'a'` (0x61) regardless of which one the
//! MX actually wrote first. When that happens, `email-in` cannot recover true
//! wire order and reports `auth_results_order_known: false` — element 0 of
//! `auth_results` may be the forgery.
//!
//! `trusted_dmarc_pass` trusts exactly element 0 (see `gate.rs`), so a caller
//! that ran it anyway on an order-unknown batch could be handed the attacker's
//! forged `dmarc=pass` instead of the MX's real `dmarc=fail`. This module
//! therefore never calls `trusted_dmarc_pass` at all when
//! `auth_results_order_known` is anything other than the literal JSON `true`
//! — an absent flag (older/buggy worker) is treated identically to `false`,
//! never defaulted to trusting the order. See
//! `order_unknown_forces_dmarc_fail_even_though_the_bare_check_would_admit`
//! and `missing_order_known_flag_fails_closed_not_open` below.
//!
//! ## The `skipped` list
//!
//! `email.poll` also returns `skipped: [{"message_id", "reason"}]` — messages
//! `email-in` could not turn into an event at all (no usable `From`, a failed
//! detail fetch). Core never sees these as `PolledEvent`s, so nothing would
//! ever ack them under the ordinary per-event path — the localmail
//! subscription cursor would stay pinned on the first one forever, wedging
//! the channel permanently. [`parse_email_skipped`] is a second, separate
//! pure extractor over the SAME raw poll `Value`
//! [`parse_email_poll_with`] sees, kept deliberately apart from it: folding a
//! skipped id into `parse_email_poll_with`'s output as a fabricated
//! `PolledEvent` would enqueue a bogus task onto the bus, which is worse than
//! not acking at all. `spawn_email_worker` instead wires
//! `parse_email_skipped` into `PolledWorkerDriver::spawn`'s
//! `parse_ack_only` parameter (see `core/src/channel/polled_driver.rs`),
//! which acks each id directly — no event, no bus, just the ack RPC — and
//! logs every id it acks (plus, when a real audit sink is wired in, records
//! it via `AckOnlyAudit`).

use std::sync::OnceLock;

use crate::channel::email::gate::{extract_token, trusted_dmarc_pass};
use crate::channel::polled_driver::{PolledEvent, PolledWorkerSpec, REFUSAL_BACKOFF};
use crate::channel::PeerEvidence;

/// Long-poll window for one `email.poll` call. Longer than Matrix's 2s: email
/// is an async fallback, not an interactive chat.
///
/// This bounds the WINDOW, not the request count. localmail's `/v1/changes`
/// answers immediately, so the worker emulates a long poll by re-asking every
/// `handler::POLL_INTERVAL` — a 15s window is `15_000 / POLL_INTERVAL`
/// upstream requests, not one. (An earlier version of this comment claimed
/// "each poll is an HTTP round trip to localmail", which was true of the
/// window and false of its interior; the interval was 250ms, so an idle
/// channel issued ~4 requests/s forever. See `handler::POLL_INTERVAL` for the
/// full cost breakdown.) Raising this value is cheap; lowering the interval is
/// not.
pub const POLL_MS: u64 = 15_000;

/// The email instantiation of the channel-generic polled driver.
pub const EMAIL_POLLED_SPEC: PolledWorkerSpec = PolledWorkerSpec {
    label: "email",
    init_method: "email.init",
    poll_method: "email.poll",
    send_method: "email.send",
    ack_method: Some("email.ack"),
    poll_timeout_ms: POLL_MS,
    refusal_backoff: REFUSAL_BACKOFF,
};

/// Configured authserv-id of our own MX. Set once at channel construction:
/// `ParsePoll` is a bare fn pointer with nowhere to carry state, so
/// `spawn_email_worker` records it here before starting the driver.
static AUTHSERV_ID: OnceLock<String> = OnceLock::new();

/// Record the authserv-id [`parse_email_poll`] will trust. Called by
/// `spawn_email_worker` before `PolledWorkerDriver::spawn`. Idempotent; a
/// second call with a different value is ignored, which is correct for a
/// single-daemon process and avoids a mid-flight trust change.
///
/// A *conflicting* second call is ignored **loudly**. This value is the gate's
/// entire trust root: if some later caller (a second channel instance, a
/// diagnostic subcommand) tried to install a different one, the effect would
/// be that every message is judged against the FIRST id — fail-closed, but
/// completely invisible, presenting as "email silently stopped working" (review
/// finding). Repeating the SAME value stays silent, which is what the e2e
/// suite does on every test.
pub fn set_authserv_id(id: &str) {
    if let Err(rejected) = AUTHSERV_ID.set(id.to_string()) {
        let in_force = AUTHSERV_ID.get().map(String::as_str).unwrap_or("");
        if in_force != rejected {
            tracing::warn!(
                in_force,
                rejected,
                "email authserv-id is already set to a DIFFERENT value; keeping the first. \
                 Every inbound message is judged against `in_force`, so if `rejected` is the \
                 correct one for this deployment the gate will reject everything."
            );
        }
    }
}

/// `ParsePoll` entry point (the fn-pointer shape `PolledWorkerDriver` needs).
/// An unset authserv-id yields `""`, which `trusted_dmarc_pass` treats as
/// fail-closed (never admits).
pub fn parse_email_poll(v: serde_json::Value) -> anyhow::Result<Vec<PolledEvent>> {
    parse_email_poll_with(v, AUTHSERV_ID.get().map(String::as_str).unwrap_or(""))
}

/// Pure core: decode one `email.poll` result's `events` array into driver
/// events, computing evidence and stripping the token from every body. Never
/// looks at `skipped` — that is [`parse_email_skipped`]'s job, kept
/// separate on purpose (see the module docs).
///
/// Fails closed on a missing/non-array `events` field (a malformed poll
/// result is a worker bug, surfaced as an error so the driver logs it and
/// skips the batch — never silently treated as "no events").
pub fn parse_email_poll_with(
    v: serde_json::Value,
    authserv_id: &str,
) -> anyhow::Result<Vec<PolledEvent>> {
    let events = v
        .get("events")
        .and_then(|e| e.as_array())
        .ok_or_else(|| anyhow::anyhow!("poll result missing 'events' array"))?;
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        // Lowercase defensively even though `email-in::build_event` already
        // lowercases the From address before putting it on the wire: the
        // pairing lookup this feeds into (`pair issue-token`'s stored peer)
        // is an exact-case-sensitive match, so a worker regression that
        // stopped lowercasing would silently turn into "every message from a
        // legitimately paired address is rejected as unpaired" — cheap
        // insurance against trusting the worker's own claim of normalization.
        let peer = str_field(e, "peer")?.to_ascii_lowercase();
        let conversation = str_field(e, "conversation")?;
        let raw_body = str_field(e, "body")?;
        let ack_token = e.get("ack_token").and_then(|t| t.as_str()).map(String::from);
        let headers: Vec<(String, String)> = e
            .get("auth_results")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|h| h.as_str())
                    .map(|s| ("Authentication-Results".to_string(), s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // Fail closed on anything other than an explicit `true` — a missing
        // key (older/buggy worker) is NOT the same as "order known", so it
        // must never default to trusting the order. See the module docs.
        let order_known = e.get("auth_results_order_known").and_then(|o| o.as_bool()).unwrap_or(false);
        let (presented_token, body) = extract_token(&raw_body);
        out.push(PolledEvent {
            peer,
            conversation,
            body,
            // ALWAYS Some for email: None would tell the bus this transport
            // authenticates its own peers, skipping the gate entirely.
            evidence: Some(PeerEvidence {
                // trusted_dmarc_pass is not even called when the order is
                // unknown — element 0 of `headers` may be a forgery, and
                // there is no safe way to "look further" (see gate.rs's own
                // docs on why that is unsafe too).
                dmarc_pass: order_known && trusted_dmarc_pass(&headers, authserv_id),
                presented_token,
            }),
            ack_token,
        });
    }
    Ok(out)
}

fn str_field(v: &serde_json::Value, key: &str) -> anyhow::Result<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("poll event missing '{key}'"))
}

/// Extract every `skipped[].(message_id, reason)` pair from a raw
/// `email.poll` result — the `ParseAckOnly` extractor `spawn_email_worker`
/// wires into `PolledWorkerDriver::spawn` so these ids get acked even though
/// they never became a `PolledEvent` (see the module docs' "The `skipped`
/// list"). `reason` travels alongside the id purely for observability: the
/// driver logs it and, when a caller supplies an `AckOnlyAudit`, hands it
/// straight to that too — it is `email-in`'s own short diagnostic (e.g. "no
/// usable From address"), NEVER the message body or any header value.
///
/// A missing/non-array `skipped` field yields no pairs rather than an error:
/// unlike a missing `events` array, an absent `skipped` list is not a
/// malformed poll result — it just means nothing was skipped this poll.
/// Malformed individual entries (no `message_id` string) are silently
/// dropped rather than aborting the whole extraction; a missing `reason`
/// falls back to a fixed placeholder so an id is never dropped just because
/// its reason string was absent.
pub fn parse_email_skipped(v: &serde_json::Value) -> Vec<(String, String)> {
    v.get("skipped")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|entry| {
                    let id = entry.get("message_id").and_then(|m| m.as_str())?.to_string();
                    let reason = entry
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("(no reason given)")
                        .to_string();
                    Some((id, reason))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Encode an ack cursor for `email.ack`. Used for both a real event's own
/// `ack_token` and a `skipped` entry's `message_id` — localmail's cursor is
/// keyed on the same message id either way (see `workers/email-in/src/client.rs::ack`).
pub fn encode_email_ack(cursor: &str) -> serde_json::Value {
    serde_json::json!({ "cursor": cursor })
}

/// Slice 1 has no outbound worker, so sending is not configured. Slice 2
/// replaces this with a real `email.send` encoding.
pub fn encode_email_send(_msg: &crate::channel::OutgoingMessage) -> serde_json::Value {
    serde_json::json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One event object shaped like `email-in`'s real wire output.
    fn event(auth_results: Vec<&str>, order_known: bool, body: &str) -> serde_json::Value {
        serde_json::json!({
            "peer": "me@example.org",
            "conversation": "<mid-1@example.org>",
            "body": body,
            "ack_token": "7",
            "auth_results": auth_results,
            "auth_results_order_known": order_known,
        })
    }

    fn poll_result(events: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "events": events, "skipped": [] })
    }

    #[test]
    fn parse_builds_evidence_from_our_mx_and_strips_the_token() {
        let v = poll_result(vec![event(
            vec!["mx.example.net; dmarc=pass"],
            true,
            "kastellan-token: abc123\nwhat is 17*23?",
        )]);
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.body, "what is 17*23?", "the token must not reach the instruction");
        assert_eq!(ev.ack_token.as_deref(), Some("7"));
        let evidence = ev.evidence.as_ref().expect("email always supplies evidence");
        assert!(evidence.dmarc_pass);
        assert_eq!(evidence.presented_token.as_deref(), Some("abc123"));
    }

    #[test]
    fn peer_is_lowercased_defensively_even_if_the_worker_did_not() {
        // Core must not blindly trust the worker's claim of normalization —
        // the pairing lookup this feeds is an exact-case match.
        let mut e = event(vec!["mx.example.net; dmarc=pass"], true, "hi");
        e["peer"] = serde_json::json!("Me@Example.ORG");
        let v = poll_result(vec![e]);
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert_eq!(events[0].peer, "me@example.org");
    }

    #[test]
    fn forged_auth_results_do_not_produce_a_passing_verdict() {
        let v = poll_result(vec![event(
            vec!["mx.example.net; dmarc=fail", "evil.example.com; dmarc=pass"],
            true,
            "kastellan-token: abc123\nhi",
        )]);
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert!(!events[0].evidence.as_ref().unwrap().dmarc_pass);
    }

    #[test]
    fn evidence_is_always_some_for_email_even_when_everything_fails() {
        // None would mean "the transport authenticates its own peers", which
        // for email would skip the gate entirely.
        let v = poll_result(vec![event(vec![], true, "no token here")]);
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        let ev = events[0].evidence.as_ref().expect("must be Some, never None");
        assert!(!ev.dmarc_pass);
        assert_eq!(ev.presented_token, None);
    }

    #[test]
    fn malformed_poll_result_is_an_error_not_a_silent_empty() {
        assert!(parse_email_poll_with(serde_json::json!({"nope": 1}), "mx").is_err());
    }

    #[test]
    fn ack_encodes_the_cursor() {
        assert_eq!(encode_email_ack("42"), serde_json::json!({ "cursor": "42" }));
    }

    // --- Fail closed on `auth_results_order_known == false` (or absent) ---

    #[test]
    fn order_unknown_forces_dmarc_fail_even_though_the_bare_check_would_admit() {
        let forged_pair = vec!["mx.example.net; dmarc=pass"];

        // Sanity: prove the underlying gate WOULD admit this exact header on
        // its own — otherwise this test could pass for the wrong reason (the
        // headers failing anyway, not the order-unknown override).
        let bare: Vec<(String, String)> =
            forged_pair.iter().map(|s| ("Authentication-Results".to_string(), s.to_string())).collect();
        assert!(
            trusted_dmarc_pass(&bare, "mx.example.net"),
            "sanity check: the bare gate must admit this header pair"
        );

        let v = poll_result(vec![event(forged_pair, false, "kastellan-token: abc\nhi")]);
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert!(
            !events[0].evidence.as_ref().unwrap().dmarc_pass,
            "auth_results_order_known: false must force dmarc_pass false, regardless of \
             what trusted_dmarc_pass alone would say about element 0"
        );
    }

    #[test]
    fn missing_order_known_flag_fails_closed_not_open() {
        // No `auth_results_order_known` key at all (an older/buggy worker)
        // must be treated identically to `false` — never defaulted to "true".
        let mut e = event(vec!["mx.example.net; dmarc=pass"], true, "hi");
        e.as_object_mut().unwrap().remove("auth_results_order_known");
        let v = poll_result(vec![e]);
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert!(!events[0].evidence.as_ref().unwrap().dmarc_pass);
    }

    #[test]
    fn order_known_true_with_a_genuine_pass_still_admits() {
        // Companion to the two tests above: proves the fail-closed change
        // did not also break the ordinary happy path.
        let v = poll_result(vec![event(vec!["mx.example.net; dmarc=pass"], true, "hi")]);
        let events = parse_email_poll_with(v, "mx.example.net").unwrap();
        assert!(events[0].evidence.as_ref().unwrap().dmarc_pass);
    }

    // --- `skipped` extraction (separate from `parse_email_poll_with`) ---

    #[test]
    fn skipped_pairs_are_extracted_for_the_ack_only_path() {
        let v = serde_json::json!({
            "events": [],
            "skipped": [
                {"message_id": "10", "reason": "no usable From address"},
                {"message_id": "11", "reason": "localmail 404: not found"}
            ]
        });
        assert_eq!(
            parse_email_skipped(&v),
            vec![
                ("10".to_string(), "no usable From address".to_string()),
                ("11".to_string(), "localmail 404: not found".to_string()),
            ]
        );
    }

    #[test]
    fn missing_reason_falls_back_to_a_placeholder_rather_than_dropping_the_id() {
        let v = serde_json::json!({ "events": [], "skipped": [{"message_id": "5"}] });
        let pairs = parse_email_skipped(&v);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "5");
    }

    #[test]
    fn missing_skipped_list_yields_no_pairs_not_an_error() {
        assert_eq!(parse_email_skipped(&serde_json::json!({"events": []})), Vec::<(String, String)>::new());
    }

    #[test]
    fn parse_email_poll_with_never_folds_skipped_into_events() {
        // parse_email_poll_with only ever turns `events` into PolledEvents;
        // `skipped` is parse_email_skipped's job, never a fabricated
        // event — see the module docs on why that split matters.
        let v = serde_json::json!({
            "events": [],
            "skipped": [{"message_id": "1", "reason": "x"}]
        });
        assert_eq!(parse_email_poll_with(v, "mx.example.net").unwrap(), Vec::new());
    }
}
