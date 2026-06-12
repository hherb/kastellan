//! Peer authorization: decides whether an inbound message comes from a peer the
//! operator has paired. **Fail-closed.** Authorization is keyed on
//! `(channel, peer)` and is `async` because the production authorizer
//! ([`DbPeerAuthorizer`]) is a DB fact — at single-user volume a query per
//! inbound message is trivial and lets operator revocation take effect
//! immediately with no cache. [`StaticPairings`] remains for tests/legacy.

use std::collections::HashSet;

use super::{ChannelId, PeerId};

/// Outcome of authorizing one inbound peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    /// Peer is paired; the message may proceed to screening + enqueue.
    Recognised,
    /// Peer is unknown/unpaired; the bus drops it (after the pairing carve-out).
    Rejected,
}

/// The authorization seam. Async + `(channel, peer)`-scoped. Dyn-safe.
#[async_trait::async_trait]
pub trait PeerAuthorizer: Send + Sync {
    async fn authorize(&self, channel: &ChannelId, peer: &PeerId) -> AuthDecision;
}

/// A fixed set of recognised peers (peer-only match, channel-agnostic). **Empty
/// by default → deny all.** Useful for tests + a legacy operator-config path; the
/// production authorizer is [`DbPeerAuthorizer`].
#[derive(Default, Clone)]
pub struct StaticPairings {
    recognised: HashSet<PeerId>,
}

impl StaticPairings {
    /// Empty → denies everyone (fail-closed).
    pub fn new() -> Self {
        Self { recognised: HashSet::new() }
    }

    /// Build from an iterator of recognised peer ids.
    pub fn from_peers<I: IntoIterator<Item = PeerId>>(peers: I) -> Self {
        Self { recognised: peers.into_iter().collect() }
    }
}

#[async_trait::async_trait]
impl PeerAuthorizer for StaticPairings {
    async fn authorize(&self, _channel: &ChannelId, peer: &PeerId) -> AuthDecision {
        if self.recognised.contains(peer) {
            AuthDecision::Recognised
        } else {
            AuthDecision::Rejected
        }
    }
}

/// Production authorizer: an active (non-revoked) row in the `pairings` table for
/// `(channel, peer)` means recognised. A DB error fails **closed** (`Rejected`,
/// logged) — an authorization lookup that can't be confirmed must not admit.
pub struct DbPeerAuthorizer {
    pool: sqlx::PgPool,
}

impl DbPeerAuthorizer {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl PeerAuthorizer for DbPeerAuthorizer {
    async fn authorize(&self, channel: &ChannelId, peer: &PeerId) -> AuthDecision {
        match kastellan_db::pairings::is_paired(&self.pool, &channel.0, &peer.0).await {
            Ok(true) => AuthDecision::Recognised,
            Ok(false) => AuthDecision::Rejected,
            Err(e) => {
                tracing::warn!(error = %e, channel = %channel.0, "pairing lookup failed; failing closed");
                AuthDecision::Rejected
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch() -> ChannelId {
        ChannelId("matrix".into())
    }

    #[tokio::test]
    async fn empty_pairings_deny_everyone() {
        let a = StaticPairings::new();
        assert_eq!(a.authorize(&ch(), &PeerId("@anyone:srv".into())).await, AuthDecision::Rejected);
    }

    #[tokio::test]
    async fn recognised_peer_is_allowed_others_denied() {
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv".into())).await, AuthDecision::Recognised);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:other".into())).await, AuthDecision::Rejected);
    }

    #[tokio::test]
    async fn peer_id_match_is_exact_not_substring() {
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv.evil".into())).await, AuthDecision::Rejected);
        assert_eq!(a.authorize(&ch(), &PeerId("evil@me:srv".into())).await, AuthDecision::Rejected);
    }
}
