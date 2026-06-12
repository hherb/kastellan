//! Peer authorization: the seam that decides whether an inbound message comes
//! from a peer the operator has paired. **Fail-closed**: the default knows no
//! peers, so every message is rejected until pairing (comms slice #3) populates
//! the recognised set. This slice ships the trait + a static implementation; the
//! TOTP/HOTP/WebAuthn pairing handshake that *adds* peers is slice #3.

use std::collections::HashSet;

use super::PeerId;

/// Outcome of authorizing one inbound peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    /// Peer is paired; the message may proceed to screening + enqueue.
    Recognised,
    /// Peer is unknown/unpaired; the message must be dropped + audited.
    Rejected,
}

/// The authorization seam. Dyn-safe. Slice #3 adds a DB-backed implementation
/// reading the `pairings` table; this slice ships [`StaticPairings`].
pub trait PeerAuthorizer: Send + Sync {
    fn authorize(&self, peer: &PeerId) -> AuthDecision;
}

/// A fixed set of recognised peers. **Empty by default → deny all** (the
/// fail-closed posture). Constructed from the operator's configured peer ids;
/// until slice #3's pairing flow lands, this is how a peer becomes recognised.
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

impl PeerAuthorizer for StaticPairings {
    fn authorize(&self, peer: &PeerId) -> AuthDecision {
        if self.recognised.contains(peer) {
            AuthDecision::Recognised
        } else {
            AuthDecision::Rejected
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pairings_deny_everyone() {
        let a = StaticPairings::new();
        assert_eq!(a.authorize(&PeerId("@anyone:srv".into())), AuthDecision::Rejected);
    }

    #[test]
    fn recognised_peer_is_allowed_others_denied() {
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&PeerId("@me:srv".into())), AuthDecision::Recognised);
        assert_eq!(a.authorize(&PeerId("@me:other".into())), AuthDecision::Rejected);
    }

    #[test]
    fn peer_id_match_is_exact_not_substring() {
        // No accidental prefix/substring acceptance — impersonation defense.
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&PeerId("@me:srv.evil".into())), AuthDecision::Rejected);
        assert_eq!(a.authorize(&PeerId("evil@me:srv".into())), AuthDecision::Rejected);
    }
}
