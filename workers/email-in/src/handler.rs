//! JSON-RPC dispatch for the three `email.*` methods. This worker makes NO
//! security decisions (spec D6, `docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`):
//! it never inspects `Authentication-Results` or any per-pairing token — it
//! only fetches raw material from localmail and hands it to core, which is
//! the only place a rejection is decided AND audited. Tests live in
//! `handler/tests.rs` (kept in a separate file so this one stays under the
//! project's 500-LOC guideline, mirroring core's `bus.rs`/`bus/tests.rs` split).
//!
//! `email.poll`'s result carries two lists, never silently dropping a
//! message: `events` (usable, handed to the bus) and `skipped`
//! (`{"message_id", "reason"}` — unattributable or **permanently**
//! unfetchable, left for a later task to ack+audit; see the module docs on
//! `build_event` and `poll` for the full reasoning, and `task-7-report.md`'s
//! "Fix round 1" section for the review findings this addresses). A
//! **transient** fetch failure appears in NEITHER list on purpose, so the
//! server-side cursor cannot advance past a message the bus never saw — see
//! `is_permanent`.

use std::collections::HashSet;

use kastellan_protocol::{codes, server::Handler, RpcError};

use crate::client::{EmailClient, EmailError};

/// Gap between successive `GET /v1/changes` calls inside ONE `email.poll`
/// window.
///
/// This — not `email.poll`'s `timeout_ms` — is what sets the channel's idle
/// cost, because localmail's `/v1/changes` returns immediately rather than
/// holding the request open: `poll` emulates a long poll by re-asking, so one
/// 15s window issues `15_000 / POLL_INTERVAL` upstream requests, forever, even
/// on a completely empty inbox.
///
/// At the original 250ms that was 60 requests per window ≈ 4/s ≈ 345k/day,
/// which the design never intended (`wire::POLL_MS`'s own comment reasons about
/// "each poll is an HTTP round trip to localmail" — true of the window, false
/// of the interior) and which is far from free once the channel is
/// force-routed: `web_common::proxy_connect` pools nothing, so EVERY request is
/// a fresh UDS connect + proxy `CONNECT` + full TLS handshake, and every
/// CONNECT decision becomes an `egress.allowed` `audit_log` row via
/// `egress::net_worker::pg_decision_sink`. 5s cuts all three by 20× (≈17k
/// requests/day) at a worst-case cost of 5s of extra inbound latency — nothing
/// at all for an asynchronous fallback channel whose own transport is measured
/// in seconds-to-minutes. `poll_issues_one_changes_request_per_interval` pins
/// the ratio so the regression cannot come back silently.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

pub struct EmailInHandler {
    client: EmailClient,
    /// Named localmail subscription this worker polls/acks — kastellan holds
    /// no inbound cursor state at all; localmail owns it (spec D7).
    subscription: String,
    /// The agent's own email address, echoed back by `email.init`.
    address: String,
}

impl EmailInHandler {
    pub fn from_env() -> anyhow::Result<Self> {
        let subscription = std::env::var("KASTELLAN_EMAIL_SUBSCRIPTION")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_EMAIL_SUBSCRIPTION unset"))?;
        let address = std::env::var("KASTELLAN_EMAIL_ADDRESS")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_EMAIL_ADDRESS unset"))?;
        Ok(Self { client: EmailClient::from_env()?, subscription, address })
    }

    #[cfg(test)]
    pub fn with_client(client: EmailClient, subscription: String, address: String) -> Self {
        Self { client, subscription, address }
    }

    /// `email.init` — identity/login-proof method the driver calls once at
    /// spawn (see `core/src/channel/polled_driver.rs::spawn`). No I/O: just
    /// echoes the worker's own configured identity.
    fn init(&self) -> Result<serde_json::Value, RpcError> {
        Ok(serde_json::json!({
            "address": self.address,
            "subscription": self.subscription,
        }))
    }

    /// `email.poll {timeout_ms}` — long-polls: call `changes`, and if it
    /// comes back empty (or every message in it turns out unattributable /
    /// unfetchable), sleep in slices ([`POLL_INTERVAL`], further capped at
    /// whatever remains of the budget) until `timeout_ms` elapses.
    ///
    /// **`timeout_ms` bounds the window, [`POLL_INTERVAL`] sets the cost.**
    /// localmail's `/v1/changes` answers immediately — it is not itself a
    /// long-poll endpoint — so this loop *emulates* one, and the number of
    /// upstream requests per window is `timeout_ms / POLL_INTERVAL`, not one.
    /// See [`POLL_INTERVAL`] for why that ratio is deliberately small; the
    /// worst-case added inbound latency is one `POLL_INTERVAL`.
    ///
    /// **Must honour `timeout_ms` in every case, not just when `changes`
    /// itself is empty.** An earlier version returned the instant a batch's
    /// messages were all unattributable, even with most of the budget left —
    /// since `PolledWorkerDriver` only sleeps on a hard error (see
    /// `core/src/channel/polled_driver.rs::run`), that made a single stuck,
    /// unattributable message (e.g. `From: <>`) a remote-triggerable tight
    /// spin between this worker and the driver. Returning early is now
    /// reserved for when there is a real event to deliver; everything else
    /// waits out the full long-poll window like an ordinary empty poll would.
    ///
    /// A message that can't become an event is never silently dropped, but
    /// what happens to it depends on **why** — see [`is_permanent`]:
    ///
    /// * A **permanent** failure (unattributable `From`, or a
    ///   `message_detail` 4xx that will never succeed) is recorded in the
    ///   `skipped` list (`{"message_id", "reason"}`) and logged, so core can
    ///   ack+audit it instead of the subscription cursor wedging on it
    ///   forever (it would otherwise never advance, since nothing ever acks
    ///   an id nobody ever saw).
    /// * A **transient** failure (`Transport(_)`, 5xx, 408, 429) is logged
    ///   and omitted from BOTH lists, so nothing acks it, the cursor stays
    ///   put, and localmail redelivers the message on the next poll. Acking
    ///   it would move a MONOTONIC `GREATEST` cursor past a message the bus
    ///   never saw — permanent, silent mail loss on nothing worse than a
    ///   localmail restart.
    ///
    /// Either way the rest of the batch keeps being processed; one bad
    /// message never aborts the others.
    ///
    /// **Known residual (batch ordering).** Core acks each event's own id, so
    /// if a LATER message in the same batch succeeds while an earlier one
    /// failed transiently, the later ack still drags the shared `GREATEST`
    /// cursor past the earlier hole. This fix removes the common,
    /// fully-avoidable loss (a blip fails the fetches it touches — typically
    /// all of them, since the cause is the transport or localmail itself, in
    /// which case there is no successful later event to ack) and cannot make
    /// anything worse than the previous behaviour, which lost the message
    /// unconditionally. Closing the residual completely needs a per-message
    /// ack contract between the worker and localmail's cursor, not a
    /// worker-side change.
    ///
    /// This worker does not ack skipped ids itself — ack ownership stays in
    /// core (spec D6): only the separate `email.ack` RPC ever calls
    /// `client.ack`.
    ///
    /// `seen` deduplicates within one `poll()` call: while the subscription
    /// cursor is unmoved, `changes` keeps returning the same stuck message on
    /// every internal retry slice, and re-fetching/re-skipping it every
    /// ~250ms would pointlessly hammer localmail and pile up duplicate
    /// `skipped` entries for one id.
    fn poll(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            timeout_ms: u64,
        }
        let p: P = parse_params(params)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(p.timeout_ms);

        let mut events = Vec::new();
        let mut skipped = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        loop {
            let changes = self.client.changes(&self.subscription).map_err(email_err_to_rpc)?;
            let new_messages = changes
                .get("new_messages")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            for m in &new_messages {
                let Some(message_id) = m.get("message_id").and_then(|v| v.as_str()) else {
                    continue; // Malformed changes entry (no id at all) — nothing to key a skip on.
                };
                if !seen.insert(message_id.to_string()) {
                    continue; // Already resolved (event or skip) earlier in this same poll() call.
                }
                match self.client.message_detail(message_id) {
                    Ok(detail) => match build_event(&detail, message_id) {
                        Some(event) => events.push(event),
                        None => record_skip(&mut skipped, message_id, "no usable From address"),
                    },
                    // PERMANENT failure: this id will never fetch. Record it in
                    // `skipped` so core acks it and the cursor moves past — one
                    // poisoned message must not wedge the channel forever.
                    Err(e) if is_permanent(&e) => {
                        record_skip(&mut skipped, message_id, &describe_email_error(&e))
                    }
                    // TRANSIENT failure: deliberately in NEITHER list, so the
                    // cursor does not advance and localmail redelivers it. See
                    // `is_permanent`'s docs for why this distinction is a
                    // data-loss fix, not a nicety. `seen` already holds the id,
                    // so the retry slices below won't re-fetch it within this
                    // same poll() call.
                    Err(e) => record_transient(message_id, &describe_email_error(&e)),
                }
            }

            if !events.is_empty() {
                return Ok(serde_json::json!({ "events": events, "skipped": skipped }));
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(serde_json::json!({ "events": events, "skipped": skipped }));
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }

    /// `email.ack {cursor}` — advances the localmail subscription cursor.
    /// Called by the driver once an event has been handed to the bus; see
    /// `PolledWorkerSpec::ack_method`.
    fn ack(&self, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        #[derive(serde::Deserialize)]
        struct P {
            cursor: String,
        }
        let p: P = parse_params(params)?;
        self.client.ack(&self.subscription, &p.cursor).map_err(email_err_to_rpc)?;
        Ok(serde_json::json!({ "ok": true }))
    }
}

impl Handler for EmailInHandler {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        match method {
            "email.init" => self.init(),
            "email.poll" => self.poll(params),
            "email.ack" => self.ack(params),
            _ => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params)
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))
}

fn email_err_to_rpc(e: EmailError) -> RpcError {
    match e {
        EmailError::BadParams(m) => RpcError::new(codes::INVALID_PARAMS, m),
        // A 401/403 is localmail refusing the channel's credential, not
        // kastellan refusing anything: `UPSTREAM_AUTH_FAILED`, never the core's
        // own `POLICY_DENIED` (#673). The core's polled driver recognises this
        // code and says "credential refused" rather than "worker died" (#674).
        EmailError::Upstream { status, body } => {
            kastellan_protocol::upstream_auth_refusal("localmail", status).unwrap_or_else(|| {
                RpcError::new(codes::OPERATION_FAILED, format!("localmail {status}: {body}"))
            })
        }
        EmailError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("transport: {m}"))
        }
    }
}

/// Human-readable reason string for a `skipped` entry — unlike
/// `email_err_to_rpc`, this never aborts anything; it only labels one
/// message's `message_detail` failure so the batch can carry on (fix for
/// review finding I3: a single 404/timeout on one message must not stall
/// the whole inbound channel behind a misleading "worker died" log).
fn describe_email_error(e: &EmailError) -> String {
    match e {
        EmailError::BadParams(m) => format!("bad request: {m}"),
        EmailError::Upstream { status, body } => {
            let snippet: String = body.chars().take(200).collect();
            format!("localmail {status}: {snippet}")
        }
        EmailError::Transport(m) => format!("transport: {m}"),
    }
}

/// Is this `message_detail` failure **permanent** — i.e. will retrying the
/// exact same request never succeed?
///
/// This split is load-bearing for at-least-once delivery, and getting it
/// wrong in either direction is a real bug (final whole-branch review,
/// Important 2):
///
/// * **Permanent → `skipped`.** Core acks a skipped id
///   (`polled_driver::run`'s ack-only loop), which advances localmail's
///   subscription cursor past it. That is exactly what a 404 needs: without
///   it the cursor can never move and ONE unfetchable message wedges the
///   whole inbound channel forever.
/// * **Transient → neither list.** Acking a transient failure would advance
///   that same cursor — and it is a MONOTONIC `GREATEST` high-water mark, so
///   the message can never be redelivered. A localmail restart or an egress
///   blip in the window between `GET /v1/changes` and
///   `GET /v1/messages/{id}` would then destroy the user's email silently.
///   Leaving it unresolved keeps the cursor put and localmail redelivers it
///   on the next poll — the "no loss" the design spec §6 promises for a
///   localmail outage.
///
/// Classification, deliberately conservative (anything not provably permanent
/// is treated as transient, because the transient branch's failure mode is
/// redelivery and the permanent branch's is destruction):
///
/// * `Upstream { 400..=499 }` — permanent (404 gone, 400 malformed, …),
///   **except** four statuses:
///   * `408 Request Timeout` and `429 Too Many Requests` — explicitly
///     retryable in HTTP semantics.
///   * `401 Unauthorized` and `403 Forbidden` — retryable after an **operator
///     action**, and therefore NOT permanent by this function's own test
///     ("will retrying the exact same request never succeed?"). An expired or
///     rotated localmail bearer token, or a withdrawn `api-user` grant, makes
///     the identical request succeed again the moment the operator fixes it;
///     acking the message away first would destroy mail over a recoverable
///     credential problem, and destruction is the one direction this
///     classification must never guess wrong in. Note `email_err_to_rpc`
///     already treats these two as their own class ("check token / api-user
///     grant") — i.e. as configuration, not as a fact about the message —
///     so calling them permanent here was internally inconsistent too
///     (review finding). A wholly-revoked token normally fails the earlier
///     `changes` call and never reaches this branch; this covers the narrower
///     per-message/per-mailbox denial that does.
/// * `Upstream { 5xx }` — transient. A server error is by definition the
///   server's temporary problem.
/// * `Transport(_)` — transient: connection reset, TLS failure, no route.
/// * `BadParams(_)` — permanent: the worker built the request itself from the
///   message id, so an identical retry yields an identical failure.
///
/// **Known residual, accepted:** `Transport(_)` also covers a JSON decode
/// failure and a response exceeding `JSON_MAX_BYTES`, which are in practice
/// permanent for that message and will therefore be retried forever, holding
/// the cursor. That is the *safe* direction (nothing is lost) and the
/// alternative —
/// classifying decode failures as permanent — would ack away a message merely
/// because localmail hiccuped mid-body. A cap/decode-specific error variant
/// is the clean fix and belongs with the body-size cap already filed for a
/// later slice.
///
/// **Observability caveat (do not misread the line above):** a transient
/// failure produces NO `audit_log` row and only a worker-stderr line that the
/// daemon relays at `debug`, so at the default `info` verbosity it is
/// **invisible**. A message stuck in the retry loop above is therefore silent
/// unless the operator raises the log level. Giving transient skips an audit
/// row is filed as a follow-up.
fn is_permanent(e: &EmailError) -> bool {
    match e {
        EmailError::BadParams(_) => true,
        EmailError::Upstream { status, .. } => {
            (400..500).contains(status) && !matches!(*status, 401 | 403 | 408 | 429)
        }
        EmailError::Transport(_) => false,
    }
}

/// Record one message as unable to become an event — logged to stderr AND
/// appended to `skipped`, never silently dropped (review finding C2b: a
/// dropped-with-no-trace message meant the subscription cursor could never
/// advance past it, since nothing ever acks an id nobody ever saw). This
/// worker never acks a skipped id itself; that stays a core decision.
///
/// **Only ever call this for a PERMANENT failure** — see [`is_permanent`]:
/// core acks every id in `skipped`, which moves the cursor past it for good.
fn record_skip(skipped: &mut Vec<serde_json::Value>, message_id: &str, reason: &str) {
    eprintln!("kastellan-worker-email-in: skipping message {message_id}: {reason}");
    skipped.push(serde_json::json!({ "message_id": message_id, "reason": reason }));
}

/// Log a TRANSIENT `message_detail` failure. Deliberately does **not** touch
/// `skipped` — see [`is_permanent`]. Worded distinctly from
/// [`record_skip`]'s "skipping" line (which means "gone for good") so an
/// operator grepping stderr can tell "this will come back" from "this was
/// dropped", which is the whole point of the distinction.
fn record_transient(message_id: &str, reason: &str) {
    eprintln!(
        "kastellan-worker-email-in: transient failure on message {message_id} \
         (NOT acked, will be redelivered): {reason}"
    );
}

/// Build one event from a localmail message detail
/// (`GET /v1/messages/{id}?headers=full`). Pure so it is unit-testable
/// without a transport. Returns `None` when the detail has no usable From
/// address — nothing to attribute the message to.
///
/// Field names are confirmed against localmail's own source, not guessed:
/// `from.address` and `body_text` are unconditional fields
/// (`localmail/src/localmail/api/messages.py` — `_address()` and the
/// `msg["body_text"]` assignment); `headers` is present only because this
/// worker's client always requests `?headers=full`, and — this is the part
/// the brief guessed wrong — every header's value is a JSON ARRAY of every
/// occurrence of that EXACT-cased header name, in wire order
/// (`localmail/src/localmail/parser.py`'s `_headers_dict`, which iterates
/// Python's `email.message.Message.items()`), never a bare string. See
/// `task-7-report.md` for the full confirmation trail.
///
/// `peer` is the From address, lowercased to match the normalization
/// `pair issue-token` applies at pairing time — a case mismatch would
/// silently never authorize. It always comes from the top-level
/// `from.address` field, never from any header.
///
/// `Reply-To` is deliberately never consulted for the peer: honouring it
/// would let a sender who passes the gate redirect the agent's reply to a
/// third party by simply setting a Reply-To header.
///
/// `body` is localmail's `body_text` and nothing else — no HTML part is read or
/// converted. Since the per-pairing token is only ever found in this field
/// (`gate::extract_token`), an HTML-only message carries no discoverable token
/// and is refused `no_token` no matter how correctly its sender is paired. That
/// is the safe direction (a body we cannot parse must not be trusted to contain
/// the secret), and it is called out for operators in
/// `install::plan::render_email_help`'s TRAP 2.
///
/// `auth_results` is every `Authentication-Results` header value, in the
/// best order this worker can establish — this worker never inspects their
/// content, because core's gate (`core/src/channel/email/gate.rs`) decides
/// which one counts, consulting only the first.
///
/// `auth_results_order_known` is the CRITICAL signal that makes the above
/// safe: `true` when at most one exact-cased spelling of
/// `authentication-results` appears as an object key (the realistic case —
/// a two-milter Postfix emits two headers with the SAME literal name, and
/// localmail groups them into one JSON array, whose order is the true wire
/// order — see `header_values`'s doc). `false` when 2+ *distinct-cased*
/// spellings are present as separate object keys: this workspace's
/// `serde_json` has no `preserve_order` feature, so iterating a
/// `Value::Object` (a `BTreeMap`) visits keys in byte/alphabetical order,
/// NOT wire order. Concretely, `AUTHENTICATION-RESULTS` (`'A'` = 0x41) sorts
/// before `authentication-results` (`'a'` = 0x61) regardless of which one the
/// MX actually wrote first — so an attacker-forged all-caps header would
/// silently win element 0 of `auth_results`, which is exactly the element
/// `trusted_dmarc_pass` consults. This function does NOT resolve that
/// ambiguity itself (spec D6 — no security decisions here); it only signals
/// it, still returning every value it found, so core can fail closed on
/// `false` instead of trusting a coin-flip order.
pub fn build_event(detail: &serde_json::Value, message_id: &str) -> Option<serde_json::Value> {
    let from = detail
        .get("from")
        .and_then(|f| f.get("address"))
        .and_then(|a| a.as_str())?
        .trim()
        .to_ascii_lowercase();
    if from.is_empty() {
        return None;
    }

    let headers = detail.get("headers").and_then(|h| h.as_object());

    // Conversation = the RFC 5322 Message-ID, so a later slice's reply can
    // set In-Reply-To/References and thread. Fall back to a stable synthetic
    // value derived from the localmail id when the header is absent.
    let conversation = headers
        .map(|h| header_values(h, "Message-ID"))
        .unwrap_or_default()
        .into_iter()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("localmail:{message_id}"));

    let auth_results: Vec<String> = headers
        .map(|h| header_values(h, "Authentication-Results"))
        .unwrap_or_default()
        .into_iter()
        .map(String::from)
        .collect();

    let auth_results_order_known = headers
        .map(|h| header_key_variant_count(h, "Authentication-Results") <= 1)
        .unwrap_or(true);

    let body = detail.get("body_text").and_then(|b| b.as_str()).unwrap_or("").to_string();
    let subject = detail.get("subject").and_then(|s| s.as_str()).map(str::to_string);
    let date = detail.get("date").and_then(|d| d.as_str()).map(str::to_string);

    Some(serde_json::json!({
        "peer": from,
        "conversation": conversation,
        "subject": subject,
        "date": date,
        "body": body,
        "ack_token": message_id,
        "auth_results": auth_results,
        "auth_results_order_known": auth_results_order_known,
    }))
}

/// Count of distinct object keys that case-insensitively equal `name`. 0 or
/// 1 ⇒ a single array (or nothing) — order is fully known. 2+ ⇒ "the same"
/// header spelled with different cases landed in separate JSON keys, and
/// this side cannot tell which one the MX actually wrote first (see
/// `build_event`'s `auth_results_order_known` doc for why that matters).
fn header_key_variant_count(headers: &serde_json::Map<String, serde_json::Value>, name: &str) -> usize {
    headers.keys().filter(|k| k.eq_ignore_ascii_case(name)).count()
}

/// Every value under every header-map key that case-insensitively equals
/// `name`, concatenated in that key's own wire order.
///
/// localmail groups all wire occurrences of one EXACT-cased header name into
/// a single JSON array (see `build_event`'s doc), so the realistic case this
/// worker must get right — a two-milter Postfix emitting two
/// `Authentication-Results` headers with identical casing — comes back as
/// one key whose array preserves full wire order; that path is exercised by
/// `auth_results_are_returned_verbatim_and_in_order` in `handler/tests.rs`.
///
/// A header repeated with a DIFFERENT case becomes a second, distinct object
/// key in localmail's own JSON (confirmed by direct experiment against
/// Python's `email` package — see `task-7-report.md`). This workspace's
/// `serde_json` does not enable the `preserve_order` feature, so
/// `Value::Object` is backed by a `BTreeMap` (alphabetical key order) —
/// iterating across two *different-cased* keys is therefore NOT guaranteed
/// to reproduce true cross-key wire order. This function still returns every
/// value from every matching key (nothing is silently dropped), but that
/// specific cross-case-duplicate scenario cannot be given a wire-order
/// guarantee from the worker side; the loss of a unified ordering happens
/// upstream, in localmail's own parser, before this worker ever sees the
/// response.
fn header_values<'a>(headers: &'a serde_json::Map<String, serde_json::Value>, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case(name) {
            match v {
                serde_json::Value::Array(arr) => out.extend(arr.iter().filter_map(|x| x.as_str())),
                // Defensive only: real localmail responses always use an
                // array (`_headers_dict` groups every occurrence into a
                // list), even for a header seen exactly once.
                serde_json::Value::String(s) => out.push(s.as_str()),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "handler/tests.rs"]
mod tests;
