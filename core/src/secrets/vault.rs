//! In-process secret materialization Vault. Holds plaintext keyed by
//! [`SecretRef`] with wall-clock TTL. See module-level docs in
//! [`super`] for the threat model.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use rand::RngCore;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use zeroize::Zeroizing;

use hhagent_db::secrets::KeyProvider;

use super::{DEFAULT_TTL, REF_HEX_LEN, REF_PREFIX};

/// Opaque pointer into the in-process [`Vault`]. Constructed only by
/// [`Vault::materialize`]. Safe to embed in audit logs and (eventually)
/// in transcripts: reveals nothing without an active Vault.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretRef(String);

impl SecretRef {
    /// The full `secret://<8-hex>` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// SHA-256 of [`Self::as_str`]; 64-char lowercase hex. Audit rows
    /// carry this, not the ref itself — operators with audit-log read
    /// can correlate `materialized → redeemed → redemption_failed`
    /// across rows without being able to redeem.
    pub fn ref_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.0.as_bytes());
        format!("{:x}", h.finalize())
    }

    /// `pub(crate)` constructor — only called from inside this module
    /// (and from `pub(crate)` test helpers). Keeps the only public path
    /// through [`Vault::materialize`].
    pub(crate) fn from_raw(s: String) -> Self {
        SecretRef(s)
    }
}

/// Per-daemon-process secret materialization cache. Threaded into
/// [`crate::tool_host::dispatch`] as `&Vault` (the daemon owns an
/// `Arc<Vault>` and shares it across the scheduler).
pub struct Vault {
    ttl: Duration,
    map: RwLock<HashMap<SecretRef, Entry>>,
}

/// Internal storage entry. Drop walks the Zeroizing and zeroes the
/// plaintext bytes automatically.
struct Entry {
    plaintext: Zeroizing<Vec<u8>>,
    expires_at: Instant,
}

#[non_exhaustive]
#[derive(Debug)]
pub enum RedeemResult {
    Hit(Zeroizing<Vec<u8>>),
    Expired,
    NotFound,
}

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault: secret lookup failed: {0}")]
    Secrets(#[from] hhagent_db::secrets::SecretsError),

    /// Hard-fail on audit write — see spec §5.4. Wraps the existing
    /// `hhagent_db::DbError` returned by `audit::insert`.
    ///
    /// **No `#[from]` on purpose:** `DbError` is the crate-wide error
    /// type for `hhagent_db`; a blanket `From` would silently swallow
    /// any DbError from a future method on Vault. Callers map
    /// explicitly via `.map_err(VaultError::Audit)`.
    #[error("vault: audit row insert failed during materialize: {0}")]
    Audit(hhagent_db::DbError),

    #[error("vault: materialized plaintext is empty")]
    EmptyPlaintext,
}

impl Vault {
    /// Construct with [`DEFAULT_TTL`] (1 h).
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }

    /// Construct with a custom TTL (for tests).
    pub fn with_ttl(ttl: Duration) -> Self {
        Vault {
            ttl,
            map: RwLock::new(HashMap::new()),
        }
    }

    /// Decrypt `name` via `db::secrets::get`, stash the plaintext keyed
    /// by a fresh ref, write the `policy / secret.materialized` audit
    /// row, and return the ref.
    pub async fn materialize(
        &self,
        pool: &PgPool,
        key_provider: &dyn KeyProvider,
        name: &str,
        actor: &str,
    ) -> Result<SecretRef, VaultError> {
        // 1. Decrypt the secret at the host boundary.
        let plaintext: Zeroizing<Vec<u8>> =
            hhagent_db::secrets::get(pool, key_provider, name, None).await?;

        if plaintext.is_empty() {
            return Err(VaultError::EmptyPlaintext);
        }

        // 2. Generate the ref: 4 random bytes via OsRng → `secret://{:08x}`.
        //    OsRng is the cryptographic RNG; collision probability is
        //    negligible at any expected workload (see spec §2).
        let mut rng = rand::rngs::OsRng;
        let mut tail = [0u8; 4];
        rng.fill_bytes(&mut tail);
        let secret_ref = SecretRef::from_raw(format!(
            "{}{:02x}{:02x}{:02x}{:02x}",
            REF_PREFIX, tail[0], tail[1], tail[2], tail[3]
        ));

        debug_assert_eq!(
            secret_ref.as_str().len(),
            REF_PREFIX.len() + REF_HEX_LEN,
            // Belt-and-braces — the {:02x} format width over 4 bytes
            // mathematically guarantees 8 hex chars, but this catches any
            // future format-string typo at debug-mode test time.
            "freshly-built ref must satisfy the well-formed-ref length invariant"
        );

        // 3. Write the audit row FIRST. On failure we return Err without
        //    inserting into the vault — the spec's hard-fail-on-materialize-
        //    audit posture (§5.4) means no materialized-but-unaudited ref
        //    ever exists. Subsequent crash between this and the vault
        //    insert is acceptable: the audit row is the source of truth.
        let ref_hash = secret_ref.ref_hash();
        let ttl_secs = self.ttl.as_secs();
        let payload = json!({
            "name":     name,
            "ref_hash": ref_hash,
            "ttl_secs": ttl_secs,
            "actor":    actor,
        });
        hhagent_db::audit::insert(pool, "policy", "secret.materialized", payload)
            .await
            .map_err(VaultError::Audit)?;

        // 4. Insert into vault under the brief sync write lock. The
        //    Zeroizing<Vec<u8>> moves into the entry; on Vault::Drop or
        //    on TTL eviction, Zeroizing::Drop zeroes the plaintext bytes.
        let entry = Entry {
            plaintext,
            expires_at: Instant::now() + self.ttl,
        };
        {
            let mut map = self.map.write().expect("vault map poisoned");
            map.insert(secret_ref.clone(), entry);
        }

        Ok(secret_ref)
    }

    /// Sync redemption. Returns the discrimination between Hit / Expired
    /// / NotFound. Expired entries are lazily dropped on this call.
    pub fn redeem(&self, r: &SecretRef) -> RedeemResult {
        let now = Instant::now();

        // Fast path: read lock, check expiry, clone on Hit.
        {
            let map = self.map.read().expect("vault map poisoned");
            match map.get(r) {
                None => return RedeemResult::NotFound,
                Some(entry) if now < entry.expires_at => {
                    return RedeemResult::Hit(Zeroizing::new(entry.plaintext.to_vec()));
                }
                Some(_expired) => {
                    // Fall through to slow path below.
                }
            }
        }

        // Slow path: expired — release read lock (already done by scope exit),
        // acquire write lock, remove entry. The remove zeros the Zeroizing<Vec<u8>>
        // via Drop. Subsequent redeems return NotFound.
        {
            let mut map = self.map.write().expect("vault map poisoned");
            // Re-check under write lock in case another caller already GC'd
            // this ref in the meantime — defensive, not load-bearing.
            map.remove(r);
        }
        RedeemResult::Expired
    }
}

impl Default for Vault {
    fn default() -> Self {
        Vault::new()
    }
}

impl super::substitute::RedeemFromVault for Vault {
    fn redeem(&self, r: &SecretRef) -> RedeemResult {
        Vault::redeem(self, r)
    }
}

#[cfg(test)]
mod tests;

// Pin: the prefix-len + hex-len constants are referenced by both the
// walker (in `substitute.rs`) and the format string in
// `materialize`. Keeping them at the module root keeps the seam tight.
const _: () = {
    // Compile-time pin so a typo in REF_HEX_LEN trips a build error
    // here rather than at runtime via a length mismatch.
    assert!(REF_PREFIX.len() == 9, "REF_PREFIX must be 'secret://' (9 bytes)");
    assert!(REF_HEX_LEN == 8, "REF_HEX_LEN must match the 8-digit hex format width");
};
