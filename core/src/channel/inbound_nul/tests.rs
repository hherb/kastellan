//! Unit tests for [`super`] — the inbound NUL boundary (#818) — and for its
//! place at the head of [`crate::channel::bus::handle_inbound`]. In-process
//! fakes only; no Postgres.

use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;
use crate::channel::auth::{AuthDecision, PeerAuthorizer, StaticPairings};
use crate::channel::bus::{handle_inbound, ChannelEvents};
use crate::channel::{actions, ChannelId, ConversationId, PeerEvidence, PeerId};
use kastellan_db::nul::NUL_ESCAPE;
use kastellan_db::tasks::Lane;

/// `␀`, spelled once so a fixture cannot drift from the constant.
const E: char = NUL_ESCAPE;

fn msg(channel: &str, peer: &str, conversation: &str, body: &str) -> IncomingMessage {
    IncomingMessage {
        channel: ChannelId(channel.into()),
        peer: PeerId(peer.into()),
        conversation: ConversationId(conversation.into()),
        body: body.into(),
        evidence: None,
    }
}

// ── The pure screen ──────────────────────────────────────────────────────

/// A clean message is passed through untouched — the overwhelmingly common
/// case costs a scan and no copy.
#[test]
fn a_clean_message_is_clean() {
    assert_eq!(screen(&msg("matrix", "@a:x", "!r:x", "hello")), NulScreen::Clean);
}

/// Each identity field refuses on its own, and the field is named so the
/// audit row can say which one was malformed.
#[test]
fn a_nul_in_any_identity_field_is_malformed_and_named() {
    let cases = [
        (msg("matrix\0", "@a:x", "!r:x", "hi"), MalformedField::Channel),
        (msg("matrix", "@a\0:x", "!r:x", "hi"), MalformedField::Peer),
        (msg("matrix", "@a:x", "!r\0:x", "hi"), MalformedField::Conversation),
    ];
    for (m, field) in cases {
        assert_eq!(screen(&m), NulScreen::Malformed { field }, "{m:?}");
    }
}

/// Identity is checked before the body: a message with NULs in both is
/// refused, not escaped — escaping the body must never launder the id.
#[test]
fn an_identity_nul_wins_over_a_body_nul() {
    assert_eq!(
        screen(&msg("matrix", "@a\0:x", "!r:x", "b\0")),
        NulScreen::Malformed { field: MalformedField::Peer }
    );
}

/// A NUL in the body is escaped and counted; every other field — the
/// evidence included — is carried over unchanged.
#[test]
fn a_nul_in_the_body_is_escaped_and_counted() {
    let mut m = msg("email", "a@x", "<t@x>", "pay \0 to \0b");
    m.evidence = Some(PeerEvidence { dmarc_pass: true, presented_token: Some("tok".into()) });
    let NulScreen::BodyEscaped { msg: out, nuls } = screen(&m) else {
        panic!("expected BodyEscaped");
    };
    assert_eq!(nuls, 2);
    assert_eq!(out.body, format!("pay {E} to {E}b"));
    assert_eq!(IncomingMessage { body: m.body.clone(), ..out }, m);
}

/// The fixed audit labels: `[a-z_]` only, so they are safe to persist.
#[test]
fn malformed_field_labels_are_fixed_snake_case() {
    for f in [MalformedField::Channel, MalformedField::Peer, MalformedField::Conversation] {
        assert!(f.as_str().chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{}", f.as_str());
    }
    assert_eq!(REASON_NUL, "nul");
}

// ── At the head of handle_inbound ────────────────────────────────────────

#[derive(Default)]
struct FakeEvents {
    enqueued: Mutex<Vec<(Lane, Value)>>,
    audited: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl ChannelEvents for FakeEvents {
    async fn enqueue(&self, lane: Lane, payload: Value) -> anyhow::Result<i64> {
        self.enqueued.lock().unwrap().push((lane, payload));
        Ok(7)
    }
    async fn audit(&self, action: &str, payload: Value) {
        self.audited.lock().unwrap().push((action.to_string(), payload));
    }
}

/// An authorizer that records whether it was consulted at all — the claim
/// "a NUL peer never reaches the pairing lookup" is about the call, not
/// about its answer.
#[derive(Default)]
struct RecordingAuthorizer {
    calls: Mutex<usize>,
}

#[async_trait::async_trait]
impl PeerAuthorizer for RecordingAuthorizer {
    async fn authorize(&self, _: &ChannelId, _: &PeerId, _: Option<&PeerEvidence>) -> AuthDecision {
        *self.calls.lock().unwrap() += 1;
        AuthDecision::Recognised
    }
}

/// A NUL peer is refused before authorization, with one
/// `channel.rejected_malformed` row naming the field, and nothing enqueued.
/// Before #818 it reached the pairing lookup, which failed in Postgres and
/// was logged as "pairing lookup failed" and audited as an ordinary
/// unpaired peer.
#[tokio::test]
async fn a_nul_peer_is_rejected_malformed_before_authorization() {
    let events = FakeEvents::default();
    let auth = RecordingAuthorizer::default();
    let reply = handle_inbound(&auth, None, None, &events, &msg("matrix", "@m\0:x", "!r:x", "hi")).await;

    assert!(reply.is_none(), "a malformed message gets no reply");
    assert_eq!(*auth.calls.lock().unwrap(), 0, "the authorizer must not be consulted");
    assert!(events.enqueued.lock().unwrap().is_empty());
    let audited = events.audited.lock().unwrap();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, actions::REJECTED_MALFORMED);
    assert_eq!(
        audited[0].1,
        json!({"channel": "matrix", "peer": "@m\0:x", "field": "peer", "reason": "nul"}),
        "the raw peer goes to the audit layer, which escapes it on insert (#816)"
    );
}

/// A paired peer's body with a NUL is enqueued — the message is kept, with
/// the NUL rewritten — and the received row is written as usual. Before
/// #818 the task insert failed and the message was dropped with no row.
#[tokio::test]
async fn a_nul_in_a_paired_peers_body_is_escaped_and_enqueued() {
    let events = FakeEvents::default();
    let auth = StaticPairings::from_peers([PeerId("@a:x".into())]);
    let reply = handle_inbound(&auth, None, None, &events, &msg("matrix", "@a:x", "!r:x", "hi\0there")).await;

    assert!(reply.is_none());
    let enqueued = events.enqueued.lock().unwrap();
    assert_eq!(enqueued.len(), 1, "the message must be enqueued, not dropped");
    assert_eq!(enqueued[0].1["instruction"], json!(format!("hi{E}there")));
    assert!(!kastellan_db::nul::json_contains_nul(&enqueued[0].1), "{}", enqueued[0].1);
    let audited = events.audited.lock().unwrap();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, actions::RECEIVED);
}
