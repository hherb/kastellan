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

/// Minimal keyring surface the first-init logic needs, so the get→set→read-back
/// decision can be unit-tested without a real keyring. Production impl
/// ([`KeyringEntryOps`]) wraps `keyring::Entry`; tests fake it and can return a
/// different read-back value to simulate a racing writer.
trait KeyringOps {
    fn get_secret(&self) -> Result<Vec<u8>, KeyringOpsError>;
    fn set_secret(&self, bytes: &[u8]) -> Result<(), KeyringOpsError>;
}

/// Errors from a [`KeyringOps`] call. `NoEntry` is modelled explicitly (it
/// drives the first-init branch); everything else is opaque.
enum KeyringOpsError {
    NoEntry,
    Other(String),
}

/// How [`resolve_or_init`] resolved, for caller-side logging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstInit {
    /// An entry already existed; its key was returned.
    ExistingKey,
    /// No entry existed; we generated, stored, and read back OUR key.
    FreshKey,
    /// No entry existed; we stored a key but the read-back returned a DIFFERENT
    /// key — a concurrent process won the first-init race, and we adopted its
    /// key so both converge. See the concurrency note on
    /// [`OsKeyringProvider::ensure_initialized`]: this catches the race only
    /// when the competing `set` lands before our read-back; it is NOT a mutex.
    RacedConverged,
}

/// Validate a raw keyring value into a fixed-size key.
fn to_key(bytes: Vec<u8>) -> Result<[u8; KEY_LEN], SecretsError> {
    if bytes.len() != KEY_LEN {
        return Err(SecretsError::KeyLengthInvalid {
            expected: KEY_LEN,
            got: bytes.len(),
        });
    }
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Pure first-init logic over a [`KeyringOps`] seam. On `NoEntry`: generate a
/// key with `gen`, store it, then READ IT BACK. If the read-back differs from
/// what we wrote, a concurrent process overwrote us — adopt its key
/// (`RacedConverged`) so both processes converge on ONE key before any secret
/// is encrypted. `gen` is injected (OsRng in production, fixed in tests), so
/// this function is deterministic under test.
fn resolve_or_init(
    ops: &dyn KeyringOps,
    gen: impl FnOnce() -> [u8; KEY_LEN],
) -> Result<([u8; KEY_LEN], FirstInit), SecretsError> {
    match ops.get_secret() {
        Ok(existing) => Ok((to_key(existing)?, FirstInit::ExistingKey)),
        Err(KeyringOpsError::NoEntry) => {
            let fresh = gen();
            ops.set_secret(&fresh)
                .map_err(|e| SecretsError::Keyring(format!("set_secret failed: {}", op_err(e))))?;
            // Read back to detect a racing writer that overwrote us.
            let after = match ops.get_secret() {
                Ok(b) => to_key(b)?,
                Err(KeyringOpsError::NoEntry) => {
                    return Err(SecretsError::Keyring(
                        "keyring entry vanished immediately after set_secret".into(),
                    ))
                }
                Err(KeyringOpsError::Other(s)) => {
                    return Err(SecretsError::Keyring(format!("read-back get_secret failed: {s}")))
                }
            };
            if after == fresh {
                Ok((fresh, FirstInit::FreshKey))
            } else {
                Ok((after, FirstInit::RacedConverged))
            }
        }
        Err(KeyringOpsError::Other(s)) => {
            Err(SecretsError::Keyring(format!("get_secret failed: {s}")))
        }
    }
}

/// Render a [`KeyringOpsError`] for an error message.
fn op_err(e: KeyringOpsError) -> String {
    match e {
        KeyringOpsError::NoEntry => "no entry".into(),
        KeyringOpsError::Other(s) => s,
    }
}

/// Production [`KeyringOps`] wrapping a real `keyring::Entry`.
struct KeyringEntryOps {
    entry: keyring::Entry,
}

impl KeyringOps for KeyringEntryOps {
    fn get_secret(&self) -> Result<Vec<u8>, KeyringOpsError> {
        match self.entry.get_secret() {
            Ok(b) => Ok(b),
            Err(keyring::Error::NoEntry) => Err(KeyringOpsError::NoEntry),
            Err(other) => Err(KeyringOpsError::Other(other.to_string())),
        }
    }
    fn set_secret(&self, bytes: &[u8]) -> Result<(), KeyringOpsError> {
        self.entry
            .set_secret(bytes)
            .map_err(|e| KeyringOpsError::Other(e.to_string()))
    }
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
    /// **Concurrency contract.** First-init does a read-back-verify: after
    /// storing a freshly generated key it reads the entry back and, if a
    /// concurrent process overwrote it, ADOPTS that process's key so both
    /// converge on one (logged at WARN). This closes the common race window but
    /// is **not** a full mutex — the read-back only catches a competing `set`
    /// that lands before it, so an unfavourable interleaving (the other
    /// process's `get` precedes our `set`) can still leave the two holding
    /// different keys. Callers must therefore still ensure exactly one process
    /// performs the first-ever initialisation. The agent's single-daemon /
    /// single-user model makes this trivially true in practice; callers spawning
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
        let ops = KeyringEntryOps { entry };
        let (bytes, outcome) = resolve_or_init(&ops, || {
            let mut fresh = [0u8; KEY_LEN];
            OsRng.fill_bytes(&mut fresh);
            fresh
        })?;
        if outcome == FirstInit::RacedConverged {
            tracing::warn!(
                service,
                account,
                "concurrent keyring first-init detected; converged on the winning key. \
                 Defence-in-depth, NOT full serialisation — ensure exactly one process \
                 performs the first-ever init (see OsKeyringProvider docs)."
            );
        }
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
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Scripted [`KeyringOps`] fake: `get_secret` returns queued responses in
    /// order; `set_secret` records writes. A second queued get that differs from
    /// the stored write simulates a racing first-init writer.
    struct ScriptedOps {
        gets: RefCell<VecDeque<Result<Vec<u8>, KeyringOpsError>>>,
        sets: RefCell<Vec<Vec<u8>>>,
    }
    impl ScriptedOps {
        fn new(gets: Vec<Result<Vec<u8>, KeyringOpsError>>) -> Self {
            Self {
                gets: RefCell::new(gets.into()),
                sets: RefCell::new(Vec::new()),
            }
        }
    }
    impl KeyringOps for ScriptedOps {
        fn get_secret(&self) -> Result<Vec<u8>, KeyringOpsError> {
            self.gets
                .borrow_mut()
                .pop_front()
                .unwrap_or(Err(KeyringOpsError::NoEntry))
        }
        fn set_secret(&self, bytes: &[u8]) -> Result<(), KeyringOpsError> {
            self.sets.borrow_mut().push(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn resolve_returns_existing_key_without_writing() {
        let ops = ScriptedOps::new(vec![Ok(vec![7u8; KEY_LEN])]);
        let (key, outcome) = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap();
        assert_eq!(key, [7u8; KEY_LEN]);
        assert_eq!(outcome, FirstInit::ExistingKey);
        assert!(ops.sets.borrow().is_empty(), "must not write when an entry exists");
    }

    #[test]
    fn resolve_generates_and_stores_on_no_entry() {
        // NoEntry, then read-back returns exactly what we stored → FreshKey.
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Ok(vec![1u8; KEY_LEN])]);
        let (key, outcome) = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap();
        assert_eq!(key, [1u8; KEY_LEN]);
        assert_eq!(outcome, FirstInit::FreshKey);
        assert_eq!(ops.sets.borrow().as_slice(), &[vec![1u8; KEY_LEN]]);
    }

    #[test]
    fn resolve_converges_on_racing_writers_key() {
        // NoEntry, we store K1, but the read-back returns a DIFFERENT valid key
        // K2 — a racer won. We adopt K2 (converge), not keep K1.
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Ok(vec![2u8; KEY_LEN])]);
        let (key, outcome) = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap();
        assert_eq!(key, [2u8; KEY_LEN], "must adopt the winner's key");
        assert_eq!(outcome, FirstInit::RacedConverged);
        assert_eq!(ops.sets.borrow().as_slice(), &[vec![1u8; KEY_LEN]]);
    }

    #[test]
    fn resolve_rejects_existing_wrong_length() {
        let ops = ScriptedOps::new(vec![Ok(vec![0u8; 10])]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::KeyLengthInvalid { expected, got }
            if expected == KEY_LEN && got == 10));
    }

    #[test]
    fn resolve_propagates_get_error() {
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::Other("boom".into()))]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::Keyring(s) if s.contains("boom")));
    }

    #[test]
    fn resolve_errors_when_readback_wrong_length() {
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Ok(vec![0u8; 5])]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::KeyLengthInvalid { got, .. } if got == 5));
    }

    #[test]
    fn resolve_errors_when_entry_vanishes_after_set() {
        let ops = ScriptedOps::new(vec![Err(KeyringOpsError::NoEntry), Err(KeyringOpsError::NoEntry)]);
        let err = resolve_or_init(&ops, || [1u8; KEY_LEN]).unwrap_err();
        assert!(matches!(err, SecretsError::Keyring(s) if s.contains("vanished")));
    }

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
