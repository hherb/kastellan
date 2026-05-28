//! In-process secret materialization Vault. Holds plaintext keyed by
//! [`SecretRef`] with wall-clock TTL. See module-level docs in
//! [`super`] for the threat model.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

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
    // Used by Vault::materialize (Task 2) and FakeVault tests (Task 3); pre-emptively allowed during the Task 1 stub phase.
    #[allow(dead_code)]
    pub(crate) fn from_raw(s: String) -> Self {
        SecretRef(s)
    }
}

/// Per-daemon-process secret materialization cache. Threaded into
/// [`crate::tool_host::dispatch`] as `&Vault` (the daemon owns an
/// `Arc<Vault>` and shares it across the scheduler).
pub struct Vault {
    _ttl: Duration,
    _map: RwLock<HashMap<SecretRef, Entry>>,
}

/// Internal storage entry. Drop walks the Zeroizing and zeroes the
/// plaintext bytes automatically.
#[allow(dead_code)] // wired up in Task 2
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
            _ttl: ttl,
            _map: RwLock::new(HashMap::new()),
        }
    }

    /// Decrypt `name` via `db::secrets::get`, stash the plaintext keyed
    /// by a fresh ref, write the `policy / secret.materialized` audit
    /// row, and return the ref.
    pub async fn materialize(
        &self,
        _pool: &PgPool,
        _key_provider: &dyn KeyProvider,
        _name: &str,
        _actor: &str,
    ) -> Result<SecretRef, VaultError> {
        unimplemented!("Vault::materialize — filled in Task 2")
    }

    /// Sync redemption. Returns the discrimination between Hit / Expired
    /// / NotFound. Expired entries are lazily dropped on this call.
    pub fn redeem(&self, _r: &SecretRef) -> RedeemResult {
        unimplemented!("Vault::redeem — filled in Task 2")
    }
}

impl Default for Vault {
    fn default() -> Self {
        Vault::new()
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
