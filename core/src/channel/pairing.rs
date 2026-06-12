//! Production [`PairingService`]: the bus's pairing carve-out backed by the
//! `pairing_codes` / `pairings` tables (migration 0018). Consulted only for
//! authorizer-rejected peers; it compares the message body against an
//! operator-issued single-use code (by SHA-256) and, on a match, atomically
//! consumes the code and binds `(channel, peer)`. The body is never interpreted,
//! enqueued, or echoed — see the slice-#3 design's security analysis.

use super::bus::{PairingOutcome, PairingService};
use super::ingest::sha256_hex;
use super::{ChannelId, PeerId};

/// DB-backed pairing carve-out over the daemon runtime pool. The runtime role has
/// SELECT+UPDATE on `pairing_codes` (find + consume) and SELECT+INSERT on
/// `pairings` (bind) — it can complete an operator-authorized pairing but cannot
/// mint codes or revoke pairings (migration 0018 grants).
pub struct DbPairingService {
    pool: sqlx::PgPool,
}

impl DbPairingService {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl PairingService for DbPairingService {
    async fn try_pair(&self, channel: &ChannelId, peer: &PeerId, body: &str) -> PairingOutcome {
        // Fast inert gate: with no claimable code pending, the carve-out does
        // nothing (the common case — keeps unpaired traffic cheap to reject).
        match kastellan_db::pairings::any_active_code(&self.pool).await {
            Ok(false) => return PairingOutcome::NotAPairingAttempt,
            Ok(true) => {}
            Err(e) => {
                tracing::warn!(error = %e, "pairing any_active_code failed; treating as no-attempt");
                return PairingOutcome::NotAPairingAttempt;
            }
        }

        let hash = sha256_hex(body.trim().as_bytes());
        let by = format!("{}/{}", channel.0, peer.0);

        // Claim + bind atomically: a consumed code that fails to bind would strand
        // the peer (code gone, not recognised), so both happen in one transaction.
        let outcome: anyhow::Result<bool> = async {
            let mut tx = self.pool.begin().await?;
            let claimed = kastellan_db::pairings::claim_code(&mut *tx, &hash, &by).await?;
            if !claimed {
                tx.rollback().await?;
                return Ok(false);
            }
            kastellan_db::pairings::insert_pairing(&mut *tx, &channel.0, &peer.0, "code").await?;
            tx.commit().await?;
            Ok(true)
        }
        .await;

        match outcome {
            Ok(true) => PairingOutcome::Paired,
            Ok(false) => PairingOutcome::NotAPairingAttempt,
            Err(e) => {
                tracing::warn!(error = %e, "pairing claim/bind failed; not pairing");
                PairingOutcome::NotAPairingAttempt
            }
        }
    }
}
