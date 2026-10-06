//! Peer authorization: decides whether an inbound message comes from a peer the
//! operator has paired. **Fail-closed.** Authorization is keyed on
//! `(channel, peer)` and is `async` because the production authorizer
//! ([`DbPeerAuthorizer`]) is a DB fact — at single-user volume a query per
//! inbound message is trivial and lets operator revocation take effect
//! immediately with no cache. [`StaticPairings`] remains for tests/legacy.

use std::collections::HashSet;

use super::{ChannelId, PeerEvidence, PeerId};

/// Outcome of authorizing one inbound peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    /// Peer is paired; the message may proceed to screening + enqueue.
    Recognised,
    /// Peer is unknown/unpaired; the bus drops it (after the pairing carve-out).
    Rejected,
    /// Peer is paired but the transport-supplied evidence (DMARC and/or the
    /// per-pairing token) didn't check out. Distinct from [`Rejected`] because
    /// the bus must skip the pairing carve-out entirely for this outcome — see
    /// `bus::handle_inbound`. Carries a [`UnauthenticReason`] so the audit row
    /// can say *which* check failed: without it, a wrong
    /// `KASTELLAN_EMAIL_AUTHSERV_ID` (which rejects every single message —
    /// TRAP 1 in `install::plan::render_email_help`) is byte-identical in
    /// `audit_log` to a token typo, and the operator has nothing to diagnose
    /// with. Design spec §4.3 + §6 both promise this reason code.
    ///
    /// [`Rejected`]: AuthDecision::Rejected
    RejectedUnauthentic(UnauthenticReason),
    /// The pairing lookup itself failed (a database error), so whether the
    /// peer is paired is **unknown** (#827). Fail-closed like [`Rejected`] —
    /// dropped, never enqueued, no ack — and, unlike it, the bus skips the
    /// pairing carve-out: that is for a peer known to be unpaired, and this
    /// one may be the operator's own paired account.
    ///
    /// Its own outcome so the audit row says what happened. Before #827 a
    /// Postgres outage audited every paired peer's message as
    /// `channel.rejected_unpaired`, a false claim about the pairing table in
    /// the one record an operator reads after the outage.
    ///
    /// [`Rejected`]: AuthDecision::Rejected
    Unverifiable,
}

/// Why an evidence-bearing message was refused. Every variant is a **non-secret
/// classification label** — deliberately so, because [`Self::as_str`]'s output is
/// written verbatim into a durable `audit_log` payload by
/// `bus::handle_inbound`. A reason must therefore never be derived from, nor
/// carry any fragment of, the message body, its headers, or the presented
/// token. Adding a variant means adding another fixed label, never an
/// interpolated value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnauthenticReason {
    /// The pairing row carries **no** token, but the transport handed us
    /// evidence — i.e. it is a transport that cannot vouch for its own sender.
    /// Such a peer is admitted purely on the strength of the per-pairing
    /// token, so a token-less row is *misconfigured* for it, not permissive.
    /// See [`DbPeerAuthorizer::authorize`]'s `Ok(Some(None))` arm.
    PairingHasNoToken,
    /// The pairing requires a token but the transport supplied no evidence at
    /// all (so neither a DMARC verdict nor a token could be checked).
    NoEvidence,
    /// The transport's DMARC verdict did not pass. This also covers the
    /// **order-unknown** case: `email::wire::parse_email_poll_with` folds an
    /// un-orderable `Authentication-Results` set into `dmarc_pass: false`
    /// before the authorizer ever sees it, so the two are indistinguishable
    /// here by construction (noted in the review's §5).
    DmarcFail,
    /// DMARC passed but the body presented no token at all.
    NoToken,
    /// A token was presented but did not match the pairing's stored hash.
    TokenMismatch,
}

impl UnauthenticReason {
    /// Stable audit label. These strings land in `audit_log` payloads that
    /// operators query, so treat them as an interface: add new ones freely,
    /// but do not rename an existing one.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PairingHasNoToken => "pairing_has_no_token",
            Self::NoEvidence => "no_evidence",
            Self::DmarcFail => "dmarc_fail",
            Self::NoToken => "no_token",
            Self::TokenMismatch => "token_mismatch",
        }
    }
}

/// The authorization seam. Async + `(channel, peer)`-scoped. Dyn-safe.
///
/// `evidence` is `None` for transports that authenticate their own peers
/// (Matrix); those implementations ignore it. `Some` transports (email) hand
/// over what they could verify and the authorizer decides.
#[async_trait::async_trait]
pub trait PeerAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        channel: &ChannelId,
        peer: &PeerId,
        evidence: Option<&PeerEvidence>,
    ) -> AuthDecision;
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
    async fn authorize(
        &self,
        _channel: &ChannelId,
        peer: &PeerId,
        _evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        // StaticPairings is peer-only (test/legacy) — evidence is a DB concept
        // (DbPeerAuthorizer), so it's deliberately ignored here.
        if self.recognised.contains(peer) {
            AuthDecision::Recognised
        } else {
            AuthDecision::Rejected
        }
    }
}

/// Production authorizer: an active (non-revoked) row in the `pairings` table for
/// `(channel, peer)` means recognised. A DB error fails **closed**
/// (`Unverifiable`, logged — #827) — an authorization lookup that can't be
/// confirmed must not admit.
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
    async fn authorize(
        &self,
        channel: &ChannelId,
        peer: &PeerId,
        evidence: Option<&PeerEvidence>,
    ) -> AuthDecision {
        match kastellan_db::pairings::token_hash_for(&self.pool, &channel.0, &peer.0).await {
            // No active pairing.
            Ok(None) => AuthDecision::Rejected,
            // Paired, no token required — the Matrix shape, and valid ONLY for
            // a transport that authenticates its own peers.
            //
            // `evidence.is_some()` is this branch's general marker for "this
            // transport cannot vouch for its sender" — the SAME marker
            // `bus::handle_inbound` uses to gate the pairing carve-out (spec
            // D8). An evidence-bearing peer is admitted purely on the strength
            // of its per-pairing token, so a row that carries none is
            // MISCONFIGURED for that transport, not permissive: admitting here
            // would collapse the entire email gate (no DMARC check, no token)
            // for that address. Nothing creates such a row today — but nothing
            // *prevents* one either (no DB CHECK; `insert_pairing`'s only
            // caller is the now-guarded carve-out), so the guard is the thing
            // that keeps it safe rather than an accident of call sites.
            // Fail closed.
            //
            // Matrix always passes `evidence: None` (`matrix/wire.rs`), so this
            // guard is Matrix-neutral by construction — pinned by
            // `channel_bus_pg_e2e::db_peer_authorizer_covers_all_evidence_arms_…`.
            Ok(Some(None)) => {
                if evidence.is_some() {
                    tracing::warn!(channel = %channel.0,
                        "pairing carries no token but the transport supplied evidence; \
                         refusing (pairing is misconfigured for this transport)");
                    return AuthDecision::RejectedUnauthentic(
                        UnauthenticReason::PairingHasNoToken,
                    );
                }
                AuthDecision::Recognised
            }
            // Paired WITH a token: the transport must supply evidence, DMARC
            // must pass, and the token must match.
            Ok(Some(Some(expected))) => {
                let Some(ev) = evidence else {
                    tracing::warn!(channel = %channel.0,
                        "pairing requires a token but the transport supplied no evidence");
                    return AuthDecision::RejectedUnauthentic(UnauthenticReason::NoEvidence);
                };
                if !ev.dmarc_pass {
                    return AuthDecision::RejectedUnauthentic(UnauthenticReason::DmarcFail);
                }
                let presented = match ev.presented_token.as_deref() {
                    Some(t) => crate::channel::ingest::sha256_hex(t.as_bytes()),
                    None => return AuthDecision::RejectedUnauthentic(UnauthenticReason::NoToken),
                };
                if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                    AuthDecision::Recognised
                } else {
                    AuthDecision::RejectedUnauthentic(UnauthenticReason::TokenMismatch)
                }
            }
            // The lookup failed: we cannot say whether the peer is paired.
            // Fail closed, as `Unverifiable` rather than `Rejected` (#827) —
            // see that variant. The error text goes to the log only; the
            // audit row carries channel + peer.
            Err(e) => {
                tracing::warn!(error = %e, channel = %channel.0,
                    "pairing lookup failed; failing closed");
                AuthDecision::Unverifiable
            }
        }
    }
}

/// Length-independent byte comparison, so a token check cannot be narrowed by
/// timing. Both inputs here are fixed-length hex digests, but comparing them
/// with `==` would still short-circuit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
        assert_eq!(a.authorize(&ch(), &PeerId("@anyone:srv".into()), None).await, AuthDecision::Rejected);
    }

    #[tokio::test]
    async fn recognised_peer_is_allowed_others_denied() {
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv".into()), None).await, AuthDecision::Recognised);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:other".into()), None).await, AuthDecision::Rejected);
    }

    #[tokio::test]
    async fn peer_id_match_is_exact_not_substring() {
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv.evil".into()), None).await, AuthDecision::Rejected);
        assert_eq!(a.authorize(&ch(), &PeerId("evil@me:srv".into()), None).await, AuthDecision::Rejected);
    }

    /// #827: a lookup that FAILS (here, a dead port standing in for a Postgres
    /// outage) is `Unverifiable` — still a refusal, but not the claim that the
    /// peer is unpaired, which `Rejected` makes. For both transport shapes:
    /// the `Err` arm must not be reached through an evidence branch.
    #[tokio::test]
    async fn a_failed_pairing_lookup_is_unverifiable_not_rejected() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(500))
            .connect_lazy("postgres://invalid:invalid@127.0.0.1:1/none")
            .expect("lazy pool construction does not connect");
        let a = DbPeerAuthorizer::new(pool);
        let ev = PeerEvidence { dmarc_pass: true, presented_token: Some("t".into()) };
        assert_eq!(
            a.authorize(&ch(), &PeerId("@me:srv".into()), None).await,
            AuthDecision::Unverifiable
        );
        assert_eq!(
            a.authorize(&ch(), &PeerId("me@example.org".into()), Some(&ev)).await,
            AuthDecision::Unverifiable
        );
    }

    #[tokio::test]
    async fn static_pairings_ignore_evidence() {
        // StaticPairings is the test/legacy authorizer; evidence is a DB concept.
        let a = StaticPairings::from_peers([PeerId("@me:srv".into())]);
        let ev = PeerEvidence { dmarc_pass: false, presented_token: None };
        assert_eq!(a.authorize(&ch(), &PeerId("@me:srv".into()), Some(&ev)).await,
                   AuthDecision::Recognised);
    }

    // ---- constant_time_eq: pure, so plain (non-tokio) #[test]s. These pin the
    // function's actual comparison behaviour so a broken implementation (e.g.
    // one that always returns `true`, or that drops the length check) is
    // caught here rather than only showing up as an admitted forged token in
    // `DbPeerAuthorizer`, several layers away. ----

    #[test]
    fn constant_time_eq_true_for_identical_inputs() {
        assert!(constant_time_eq(b"0123456789abcdef", b"0123456789abcdef"));
    }

    #[test]
    fn constant_time_eq_false_when_first_byte_differs() {
        assert!(!constant_time_eq(b"X123456789abcdef", b"0123456789abcdef"));
    }

    #[test]
    fn constant_time_eq_false_when_last_byte_differs() {
        assert!(!constant_time_eq(b"0123456789abcdeX", b"0123456789abcdef"));
    }

    #[test]
    fn constant_time_eq_false_for_different_lengths() {
        assert!(!constant_time_eq(b"short", b"a much longer string"));
    }

    // ---- UnauthenticReason: the audit labels are an operator-facing
    // interface, so pin the exact strings AND the "no secret ever" property. ----

    #[test]
    fn unauthentic_reason_labels_are_stable() {
        // Renaming any of these silently breaks operator audit_log queries.
        assert_eq!(UnauthenticReason::PairingHasNoToken.as_str(), "pairing_has_no_token");
        assert_eq!(UnauthenticReason::NoEvidence.as_str(), "no_evidence");
        assert_eq!(UnauthenticReason::DmarcFail.as_str(), "dmarc_fail");
        assert_eq!(UnauthenticReason::NoToken.as_str(), "no_token");
        assert_eq!(UnauthenticReason::TokenMismatch.as_str(), "token_mismatch");
    }

    #[test]
    fn unauthentic_reason_labels_are_fixed_lowercase_identifiers_not_interpolated() {
        // A reason is written verbatim into a durable audit row, so it must be
        // a fixed classification label — never anything that could have picked
        // up a body fragment, a header, or a token.
        for r in [
            UnauthenticReason::PairingHasNoToken,
            UnauthenticReason::NoEvidence,
            UnauthenticReason::DmarcFail,
            UnauthenticReason::NoToken,
            UnauthenticReason::TokenMismatch,
        ] {
            let s = r.as_str();
            assert!(!s.is_empty());
            assert!(
                s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "reason label {s:?} must be a fixed [a-z_] identifier"
            );
        }
    }
}
