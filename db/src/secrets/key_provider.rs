//! Wrapping-key sources, indexed by `key_id`.
//!
//! The [`KeyProvider`] trait abstracts "where does the 32-byte AES
//! wrapping key come from". Production uses [`OsKeyringProvider`]
//! (libsecret / Keychain); tests use [`MapKeyProvider`] because
//! exercising the real keyring in CI is either flaky (D-Bus race) or
//! impossible (headless containers without `secret-service`).
//!
//! All names are re-exported from the parent [`crate::secrets`].

use std::collections::HashMap;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::OsRng;
use zeroize::Zeroizing;

use super::crypto::{SecretKey, KEY_ACCOUNT, KEY_LEN, KEY_SERVICE};
use super::error::SecretsError;

/// Source of wrapping keys, indexed by `key_id`.
///
/// Production: [`OsKeyringProvider`] reads libsecret / Keychain.
/// Tests: [`MapKeyProvider`] returns a hard-coded key.
///
/// `Send + Sync` because [`crate::secrets::put`]/[`crate::secrets::get`]
/// call into the provider while holding a `&PgPool` across `.await`s —
/// the trait object crosses await points.
pub trait KeyProvider: Send + Sync {
    /// Identifier of the key to use for *new* writes. Recorded in
    /// `secrets.key_id` so a future [`crate::secrets::get`] knows which
    /// entry to look up.
    fn current_id(&self) -> &str;

    /// Look up a key by id. Returns [`SecretsError::KeyNotFound`]
    /// when the id is unknown.
    fn get(&self, id: &str) -> Result<SecretKey, SecretsError>;
}

/// In-memory key provider for tests. Production uses
/// [`OsKeyringProvider`]; this exists only because exercising the
/// real keyring in CI is either flaky (D-Bus race) or impossible
/// (headless containers without `secret-service`).
pub struct MapKeyProvider {
    current: String,
    keys: HashMap<String, [u8; KEY_LEN]>,
}

impl MapKeyProvider {
    /// Construct with a single id ↔ key pairing. Use [`Self::insert`]
    /// to register additional historical ids (e.g. simulating a
    /// rotation in tests).
    pub fn new(current_id: impl Into<String>, key: [u8; KEY_LEN]) -> Self {
        let id_str = current_id.into();
        let mut keys = HashMap::new();
        keys.insert(id_str.clone(), key);
        Self {
            current: id_str,
            keys,
        }
    }

    /// Register a historical key id. Useful for "decrypt-old, write-
    /// new" rotation tests.
    pub fn insert(&mut self, id: impl Into<String>, key: [u8; KEY_LEN]) {
        self.keys.insert(id.into(), key);
    }
}

impl KeyProvider for MapKeyProvider {
    fn current_id(&self) -> &str {
        &self.current
    }

    fn get(&self, id: &str) -> Result<SecretKey, SecretsError> {
        self.keys
            .get(id)
            .map(|bytes| Zeroizing::new(*bytes))
            .ok_or_else(|| SecretsError::KeyNotFound(id.to_string()))
    }
}

/// OS-keyring-backed key provider.
///
/// On Linux, opens or creates the entry `("kastellan", "secrets-v1")`
/// in libsecret over D-Bus (gnome-keyring / KWallet). On macOS, the
/// same identity addresses a Keychain item. First-use generates a
/// fresh 32-byte random key via `OsRng` and writes it; subsequent
/// instantiations read the existing key.
///
/// The cached `key_bytes` field exists so the keyring lookup happens
/// once at startup (which may prompt for keyring unlock) rather than
/// on every [`KeyProvider::get`] call.
pub struct OsKeyringProvider {
    current_id: String,
    key_bytes: Zeroizing<[u8; KEY_LEN]>,
}

impl OsKeyringProvider {
    /// Open or initialize the keyring entry for the default
    /// `(kastellan, secrets-v1)` identity. See [`KEY_SERVICE`] /
    /// [`KEY_ACCOUNT`] for the literals.
    ///
    /// First call generates and stores a fresh key; subsequent calls
    /// retrieve it. Returns [`SecretsError::Keyring`] when the
    /// keyring is locked, missing, or otherwise unreachable.
    ///
    /// **Concurrency contract.** This is `get-then-set` and is **not**
    /// safe to call concurrently when no entry yet exists: two callers
    /// can both observe `NoEntry`, both generate distinct keys, and
    /// the second `set_secret` overwrites the first — leaving any data
    /// the first caller already encrypted unrecoverable. Callers must
    /// ensure exactly one process performs the first-ever
    /// initialisation. The agent's single-daemon / single-user model
    /// makes this trivially true in practice; callers spawning
    /// multiple instances must serialise the first call externally.
    pub fn ensure_initialized() -> Result<Self, SecretsError> {
        Self::ensure_initialized_for(KEY_SERVICE, KEY_ACCOUNT)
    }

    /// Same as [`Self::ensure_initialized`] but with a caller-chosen
    /// service+account, exposed for tests that want to write under
    /// a per-test entry name to avoid polluting the operator's
    /// keyring.
    pub fn ensure_initialized_for(service: &str, account: &str) -> Result<Self, SecretsError> {
        let entry = keyring::Entry::new(service, account)
            .map_err(|e| SecretsError::Keyring(format!("Entry::new failed: {e}")))?;
        let bytes: [u8; KEY_LEN] = match entry.get_secret() {
            Ok(existing) => {
                if existing.len() != KEY_LEN {
                    return Err(SecretsError::KeyLengthInvalid {
                        expected: KEY_LEN,
                        got: existing.len(),
                    });
                }
                let mut out = [0u8; KEY_LEN];
                out.copy_from_slice(&existing);
                out
            }
            Err(keyring::Error::NoEntry) => {
                let mut fresh = [0u8; KEY_LEN];
                OsRng.fill_bytes(&mut fresh);
                entry
                    .set_secret(&fresh)
                    .map_err(|e| SecretsError::Keyring(format!("set_secret failed: {e}")))?;
                fresh
            }
            Err(other) => {
                return Err(SecretsError::Keyring(format!(
                    "get_secret failed: {other}"
                )));
            }
        };
        Ok(Self {
            current_id: format!("{service}.{account}"),
            key_bytes: Zeroizing::new(bytes),
        })
    }
}

impl KeyProvider for OsKeyringProvider {
    fn current_id(&self) -> &str {
        &self.current_id
    }

    fn get(&self, id: &str) -> Result<SecretKey, SecretsError> {
        if id == self.current_id {
            Ok(Zeroizing::new(*self.key_bytes))
        } else {
            // Future rotation lands here (look up legacy ids by
            // mapping `id` → an alternative keyring entry). For now,
            // we only know the current id.
            Err(SecretsError::KeyNotFound(id.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MapKeyProvider returns the registered key and reports its id.
    #[test]
    fn map_key_provider_returns_registered_key() {
        let p = MapKeyProvider::new("test-id", [42u8; KEY_LEN]);
        assert_eq!(p.current_id(), "test-id");
        let k = p.get("test-id").unwrap();
        assert_eq!(*k, [42u8; KEY_LEN]);
    }

    /// MapKeyProvider returns KeyNotFound for unknown ids.
    #[test]
    fn map_key_provider_unknown_id_is_an_error() {
        let p = MapKeyProvider::new("known", [1u8; KEY_LEN]);
        let err = p.get("unknown").unwrap_err();
        assert!(matches!(err, SecretsError::KeyNotFound(s) if s == "unknown"));
    }
}
