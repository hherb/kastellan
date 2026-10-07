//! #827: a pairing lookup that could not be completed is not a claim that the
//! peer is unpaired.
//!
//! Before #827 `DbPeerAuthorizer` mapped a failed lookup to
//! [`AuthDecision::Rejected`], so a Postgres outage audited the operator's own
//! paired account as `channel.rejected_unpaired`. Failing closed was right and
//! stays; only the row's claim changes.

use super::*;

/// An authorizer whose lookup always fails — the shape a Postgres outage takes
/// at `DbPeerAuthorizer` (whose own mapping is pinned in `auth.rs`).
struct UnverifiableAuthorizer;

#[async_trait::async_trait]
impl PeerAuthorizer for UnverifiableAuthorizer {
    async fn authorize(
        &self,
        _c: &ChannelId,
        _p: &PeerId,
        _evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        AuthDecision::RejectedUnverifiable
    }
}

/// The body is the live pairing code, so the carve-out WOULD pair this peer if
/// it ran. It must not: we do not know whether the peer is paired, and the
/// carve-out is for peers known to be unpaired.
const LIVE_CODE: &str = "SECRET-CODE-827";

#[tokio::test]
async fn an_unverifiable_peer_is_dropped_with_its_own_row_and_no_carve_out() {
    let ev = FakeEvents::default();
    let pairing = FakePairing { code: Some(LIVE_CODE) };

    let ack =
        handle_inbound(&UnverifiableAuthorizer, Some(&pairing), None, &ev, &msg("@me:srv", LIVE_CODE))
            .await;

    assert!(ack.is_none(), "fail-closed: no pairing ack, nothing the peer could probe");
    assert!(ev.enqueued.lock().unwrap().is_empty(), "fail-closed: never enqueued");
    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "one row, and not channel.paired: {audited:?}");
    let (action, payload) = &audited[0];
    // The literal, not only the constant: operators query `audit_log` by this
    // string, so a respelling of the constant is a breaking change.
    assert_eq!(action, "channel.rejected_unverifiable");
    assert_eq!(action, actions::REJECTED_UNVERIFIABLE);
    assert_eq!(payload, &serde_json::json!({"channel": "matrix", "peer": "@me:srv"}));
    assert!(!payload.to_string().contains(LIVE_CODE), "never the body: {payload}");
}

/// The same holds for an evidence-bearing transport (email): no carve-out, the
/// same row, and none of the evidence in it.
#[tokio::test]
async fn an_unverifiable_email_peer_writes_the_same_row_and_no_evidence() {
    let ev = FakeEvents::default();

    let ack = handle_inbound(
        &UnverifiableAuthorizer,
        None,
        None,
        &ev,
        &email_msg("summarise my mail", true, Some("TOKEN-827")),
    )
    .await;

    assert!(ack.is_none());
    assert!(ev.enqueued.lock().unwrap().is_empty());
    let audited = ev.audited.lock().unwrap().clone();
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].0, actions::REJECTED_UNVERIFIABLE);
    assert_eq!(
        audited[0].1,
        serde_json::json!({"channel": "email", "peer": "me@example.org"})
    );
    assert!(!audited[0].1.to_string().contains("TOKEN-827"), "never the token");
}
