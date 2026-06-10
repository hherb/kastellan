//! Encrypt-at-rest for the `secrets` table.
//!
//! ## Threat model
//!
//! Plaintext secrets (API tokens, IMAP passwords, signing keys) live
//! exclusively in the agent's process memory and inside the OS keyring.
//! The Postgres row stores only AES-256-GCM ciphertext + nonce + AAD +
//! key_id. The wrapping key — 32 random bytes generated once on first
//! use — never leaves the OS keyring (libsecret on Linux, Keychain on
//! macOS).
//!
//! Concrete guarantees:
//!
//! 1. **Database compromise alone is not enough** to recover a secret.
//!    Reading every byte of `secrets` + every PG WAL gives the
//!    attacker ciphertext. Decryption requires the wrapping key from
//!    the OS keyring, which is gated by the OS user's keyring auth.
//!
//! 2. **Row swaps are detected.** The AAD passed to GCM begins with
//!    `b"kastellan-secrets-v1\0" || name.as_bytes() || \0`, so renaming
//!    a row (`UPDATE secrets SET name = ... WHERE id = ...`) breaks
//!    decryption: the recomputed AAD on read disagrees with the
//!    auth-tag's bound AAD, GCM auth fails, [`SecretsError::DecryptFailed`].
//!
//! 3. **Ciphertext tampering is detected.** Standard AES-GCM auth tag.
//!
//! 4. **Plaintext is wiped on drop.** Public read API returns
//!    [`Zeroizing<Vec<u8>>`]; key material is wrapped in
//!    [`Zeroizing`] too. A panic-unwind cannot leave plaintext in a
//!    half-collected stack frame — `Zeroizing::Drop` runs.
//!
//! ## What this module does NOT do
//!
//! - **Key rotation** is not in this slice. The schema's `key_id`
//!   column is forward-compatible (new writes record the current id;
//!   old ciphertexts can decrypt under their own id), but a "rotate
//!   all rows to a new key" job lands later.
//! - **Decryption from the worker** is impossible by construction:
//!   the worker is sandboxed and has no D-Bus / Keychain access.
//!   Decryption happens at the host boundary in `tool_host` and the
//!   plaintext crosses the trusted IPC pipe to the worker.
//! - **LLM injection.** Plaintext must never reach the LLM router.
//!   Enforcement lives in the LLM router (Phase 0 cont. Option J)
//!   and the credential-leak scanner (Phase 3); this module only
//!   provides the typed boundary.
//!
//! ## Test seam
//!
//! Tests construct a [`MapKeyProvider`] with a deterministic key.
//! Production uses [`OsKeyringProvider`], which is **not exercised by
//! the automated suite**: libsecret in headless CI either fails (no
//! D-Bus daemon) or prompts (locked keyring), and either outcome is
//! incompatible with `cargo test`. Manual smoke is "run the daemon
//! once, observe the keyring entry appears, restart, observe the
//! same entry is reused without prompt." If we ever add an opt-in
//! `#[ignore]`-gated test for the real keyring path it should live
//! next to this comment.

use std::collections::HashMap;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key as AesKey, Nonce as AesNonce};
use sqlx::types::time::OffsetDateTime;
use sqlx::Row;
use thiserror::Error;
use zeroize::Zeroizing;

/// AES-256 key length in bytes.
pub const KEY_LEN: usize = 32;

/// AES-GCM nonce length in bytes. The only safe GCM nonce length;
/// reusing one with the same key is catastrophic.
pub const NONCE_LEN: usize = 12;

/// Domain separator embedded as the AAD prefix. Distinguishes our
/// AEAD use of the wrapping key from any other future use; flipping
/// the version suffix is the migration knob if the AAD layout ever
/// has to change incompatibly.
pub const AAD_DOMAIN: &[u8] = b"kastellan-secrets-v1";

/// Soft cap on secret name length. The DB column is `TEXT` so PG
/// accepts much more, but anything past this is almost certainly a
/// caller bug.
pub const MAX_NAME_LEN: usize = 256;

/// Soft cap on plaintext length. Larger payloads (e.g. PEM bundles
/// of many MB) are out of scope for "secret material"; if the use
/// case is real we revisit. The cap also protects log lines: even an
/// accidental `tracing::debug!("{:?}", ciphertext)` stays bounded.
pub const MAX_PLAINTEXT_LEN: usize = 64 * 1024;

/// AES-GCM authentication-tag length appended to ciphertext. Pinned
/// at the protocol level by GCM (always 16 bytes for the standard
/// tag); kept as a named constant so the [`MAX_CIPHERTEXT_LEN`]
/// arithmetic below reads as "plaintext budget + tag overhead"
/// instead of an opaque `+ 16`.
pub const GCM_TAG_LEN: usize = 16;

/// Hard cap on ciphertext length accepted by [`get`]. A row with a
/// ciphertext column larger than this is treated as DB corruption /
/// an attacker who has write access; we refuse rather than feed it
/// into `aes-gcm::decrypt` (which would happily allocate to whatever
/// size we hand it). PG `bytea` could in principle hold up to 1 GB,
/// so the cap is load-bearing on the decrypt side.
pub const MAX_CIPHERTEXT_LEN: usize = MAX_PLAINTEXT_LEN + GCM_TAG_LEN;

/// Default keyring service name (= the entry's "service" field on
/// libsecret / Keychain). Combined with [`KEY_ACCOUNT`] it forms the
/// stable lookup key for [`OsKeyringProvider`].
///
/// **Do not rename this without a rotation migration.**
/// `OsKeyringProvider::current_id()` returns
/// `format!("{KEY_SERVICE}.{KEY_ACCOUNT}")`, which is persisted into
/// every `secrets.key_id` row at write time. Renaming the constant
/// detaches all stored rows from their wrapping key (subsequent `get`
/// returns `KeyNotFound`). The pinning unit test `constants_are_pinned`
/// catches the literal change but cannot enforce a rotation.
pub const KEY_SERVICE: &str = "kastellan";

/// Default keyring account name. Bumping the `vN` suffix is the only
/// rotation knob for now: the new id slots into [`KeyProvider::current_id`]
/// while the old id stays valid for ciphertexts that haven't been
/// re-encrypted yet.
///
/// **Do not rename this without a rotation migration** — see the
/// [`KEY_SERVICE`] doc comment for why; the same coupling applies.
pub const KEY_ACCOUNT: &str = "secrets-v1";

/// 32-byte AES-256 wrapping key, wiped on drop.
pub type SecretKey = Zeroizing<[u8; KEY_LEN]>;

/// 12-byte AES-GCM nonce.
pub type Nonce = [u8; NONCE_LEN];

/// Errors surfaced by the secrets runtime.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// Empty / oversize / control-character-laden secret name.
    #[error("secret name is invalid: {0}")]
    InvalidName(String),

    /// Plaintext exceeds [`MAX_PLAINTEXT_LEN`].
    #[error("plaintext is too large: {len} bytes (max {max})")]
    PlaintextTooLarge { len: usize, max: usize },

    /// Stored ciphertext exceeds [`MAX_CIPHERTEXT_LEN`]. Either the
    /// DB row is corrupt or an attacker has write access and is
    /// trying to push us into a large allocation in `aes-gcm::decrypt`.
    /// We refuse before allocating.
    #[error("stored ciphertext is too large: {len} bytes (max {max})")]
    CiphertextTooLarge { len: usize, max: usize },

    /// AES-GCM encrypt failed. Should be unreachable in practice
    /// (the `aead` crate only fails encrypt on impossibly-small
    /// buffers); kept for completeness.
    #[error("encryption failed (AES-GCM)")]
    EncryptFailed,

    /// AES-GCM decrypt's auth tag did not verify. One of: wrong key,
    /// wrong AAD, wrong nonce, tampered ciphertext, or a row swap
    /// that an attacker also re-AAD'd. The error is intentionally
    /// the same shape across all four because GCM is constant-time
    /// w.r.t. which check failed.
    #[error("decryption failed (AES-GCM authentication tag mismatch — wrong key, tampered ciphertext, or row swap)")]
    DecryptFailed,

    /// AAD prefix didn't match the recomputed `compute_aad(name)` —
    /// strong evidence of a `UPDATE secrets SET name = …` swap.
    #[error("stored AAD does not bind to the requested name (row was renamed without re-encryption)")]
    AadMismatch,

    /// Stored nonce is the wrong byte length (DB corruption).
    #[error("stored nonce length is wrong: expected {expected} bytes, got {got}")]
    NonceLengthInvalid { expected: usize, got: usize },

    /// Wrapping key from the provider isn't 32 bytes. Either the
    /// keyring entry has stale legacy material or a test fixture is
    /// wrong.
    #[error("key length is wrong: expected {expected} bytes, got {got}")]
    KeyLengthInvalid { expected: usize, got: usize },

    /// `secrets.name` UNIQUE lookup returned no row.
    #[error("secret not found: {0}")]
    NotFound(String),

    /// [`KeyProvider::get`] doesn't know the requested key id.
    /// Operator action: re-enrol the key or rotate.
    #[error("wrapping key not in provider: {0}")]
    KeyNotFound(String),

    /// Catch-all for OS-keyring backend failures (locked keyring,
    /// missing D-Bus daemon, Keychain ACL deny). The wrapped string
    /// is the underlying provider's display.
    #[error("keyring access failed: {0}")]
    Keyring(String),

    /// Forwarded from sqlx. The variant exists so callers can match
    /// on `Db(_)` separately from the cryptographic error variants.
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

// ─── Pure helpers ──────────────────────────────────────────────────

/// Validate that `name` is acceptable as a secret name.
///
/// Rules:
/// - non-empty
/// - <= [`MAX_NAME_LEN`] bytes
/// - no NUL byte (NUL is the AAD separator; allowing it lets a
///   crafted name push bytes into the "extra" half of AAD)
/// - no other control characters (defensive — accidentally embedded
///   `\n` would corrupt log lines that include the name)
pub fn validate_name(name: &str) -> Result<(), SecretsError> {
    if name.is_empty() {
        return Err(SecretsError::InvalidName("empty".into()));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(SecretsError::InvalidName(format!(
            "{} bytes (max {})",
            name.len(),
            MAX_NAME_LEN
        )));
    }
    for (i, b) in name.as_bytes().iter().enumerate() {
        if *b == 0 {
            return Err(SecretsError::InvalidName(format!(
                "contains NUL at byte {i}"
            )));
        }
        if *b < 0x20 || *b == 0x7f {
            return Err(SecretsError::InvalidName(format!(
                "control byte 0x{:02x} at byte {i}",
                *b
            )));
        }
    }
    Ok(())
}

/// Build the AAD bytes that bind a ciphertext to a secret name.
///
/// Format: `AAD_DOMAIN || 0x00 || name.as_bytes() || 0x00 || extra`
///
/// The domain separator means no other AEAD use of the same key can
/// produce a tag we'd accept. The trailing optional `extra` is for
/// future per-call binding (e.g. `tool_host` could pass the worker
/// tool name) without a schema change.
///
/// **Caller must** [`validate_name`] first. We do not re-validate here
/// because callers who already validated would otherwise pay twice;
/// `compute_aad` is also used in tests where invalid input is the
/// point.
pub fn compute_aad(name: &str, extra: Option<&[u8]>) -> Vec<u8> {
    let extra_bytes = extra.unwrap_or(&[]);
    let mut out = Vec::with_capacity(AAD_DOMAIN.len() + 1 + name.len() + 1 + extra_bytes.len());
    out.extend_from_slice(AAD_DOMAIN);
    out.push(0);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(extra_bytes);
    out
}

/// Encrypt `plaintext` under `key` with `aad`.
///
/// Generates a fresh 12-byte random nonce via `OsRng` (the OS CSPRNG
/// — `/dev/urandom` on Linux, `getentropy(2)` on macOS). Reusing a
/// nonce with the same key is catastrophic for AES-GCM, so callers
/// must not pass a nonce in.
///
/// Returns `(ciphertext, nonce)`. The nonce is part of the
/// public-domain ciphertext envelope — store it next to the
/// ciphertext (we do, in `secrets.nonce`).
pub fn encrypt(
    key: &SecretKey,
    plaintext: &[u8],
    aad: &[u8],
) -> Result<(Vec<u8>, Nonce), SecretsError> {
    if plaintext.len() > MAX_PLAINTEXT_LEN {
        return Err(SecretsError::PlaintextTooLarge {
            len: plaintext.len(),
            max: MAX_PLAINTEXT_LEN,
        });
    }
    let key_arr: &AesKey<Aes256Gcm> = AesKey::<Aes256Gcm>::from_slice(key.as_ref());
    let cipher = Aes256Gcm::new(key_arr);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, Payload { msg: plaintext, aad })
        .map_err(|_| SecretsError::EncryptFailed)?;
    let mut nonce_out = [0u8; NONCE_LEN];
    nonce_out.copy_from_slice(nonce.as_slice());
    Ok((ct, nonce_out))
}

/// Decrypt `ciphertext` under `key` with `nonce` + `aad`.
///
/// Plaintext is returned in a [`Zeroizing<Vec<u8>>`] so the buffer
/// is wiped on drop. Errors map to [`SecretsError::DecryptFailed`]
/// (auth tag mismatch — wrong key, wrong AAD, tampered ciphertext)
/// without distinguishing which: GCM is constant-time, and exposing
/// "auth-failed-because-wrong-key" vs "auth-failed-because-AAD" is
/// only useful to an attacker.
pub fn decrypt(
    key: &SecretKey,
    ciphertext: &[u8],
    nonce: &Nonce,
    aad: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SecretsError> {
    let key_arr: &AesKey<Aes256Gcm> = AesKey::<Aes256Gcm>::from_slice(key.as_ref());
    let cipher = Aes256Gcm::new(key_arr);
    let nonce_arr = AesNonce::from_slice(nonce);
    let pt = cipher
        .decrypt(nonce_arr, Payload { msg: ciphertext, aad })
        .map_err(|_| SecretsError::DecryptFailed)?;
    Ok(Zeroizing::new(pt))
}

// ─── KeyProvider ───────────────────────────────────────────────────

/// Source of wrapping keys, indexed by `key_id`.
///
/// Production: [`OsKeyringProvider`] reads libsecret / Keychain.
/// Tests: [`MapKeyProvider`] returns a hard-coded key.
///
/// `Send + Sync` because [`put`]/[`get`] call into the provider
/// while holding a `&PgPool` across `.await`s — the trait object
/// crosses await points.
pub trait KeyProvider: Send + Sync {
    /// Identifier of the key to use for *new* writes. Recorded in
    /// `secrets.key_id` so a future [`get`] knows which entry to
    /// look up.
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
/// on every [`get`] call.
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

// ─── Async DB I/O ──────────────────────────────────────────────────

/// Metadata-only listing entry returned by [`list`]. Crucially does
/// NOT include ciphertext or any plaintext-shaped field so a debug
/// dump can't accidentally leak material.
#[derive(Clone, Debug)]
pub struct SecretListing {
    pub name: String,
    pub key_id: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// UPSERT a secret by name.
///
/// `extra_aad` lets the caller bind additional context into the AAD
/// (e.g. the worker tool the secret will be injected into); it is
/// stored verbatim alongside the canonical name binding so [`get`]
/// can verify both halves.
///
/// Pre-flight checks performed before any DB or crypto work:
/// - [`validate_name`] on `name`
/// - [`MAX_PLAINTEXT_LEN`] on `plaintext`
/// - the provider must yield a 32-byte key for its own current id
pub async fn put<'e, E>(
    executor: E,
    key_provider: &dyn KeyProvider,
    name: &str,
    plaintext: &[u8],
    extra_aad: Option<&[u8]>,
) -> Result<(), SecretsError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    validate_name(name)?;
    let key_id = key_provider.current_id().to_string();
    let key = key_provider.get(&key_id)?;
    let aad = compute_aad(name, extra_aad);
    let (ciphertext, nonce) = encrypt(&key, plaintext, &aad)?;

    sqlx::query(
        "INSERT INTO secrets (name, ciphertext, nonce, aad, key_id, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, now(), now()) \
         ON CONFLICT (name) DO UPDATE SET \
           ciphertext = EXCLUDED.ciphertext, \
           nonce      = EXCLUDED.nonce, \
           aad        = EXCLUDED.aad, \
           key_id     = EXCLUDED.key_id, \
           updated_at = now()",
    )
    .bind(name)
    .bind(ciphertext.as_slice())
    .bind(nonce.as_slice())
    .bind(aad.as_slice())
    .bind(&key_id)
    .execute(executor)
    .await?;

    Ok(())
}

/// Decrypt and return the plaintext for `name`.
///
/// `extra_aad` must be byte-identical to the value passed at [`put`]
/// time; otherwise the recomputed AAD won't match the stored one
/// and decryption fails.
pub async fn get<'e, E>(
    executor: E,
    key_provider: &dyn KeyProvider,
    name: &str,
    extra_aad: Option<&[u8]>,
) -> Result<Zeroizing<Vec<u8>>, SecretsError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    validate_name(name)?;
    let row = sqlx::query(
        "SELECT ciphertext, nonce, aad, key_id FROM secrets WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(executor)
    .await?
    .ok_or_else(|| SecretsError::NotFound(name.to_string()))?;

    let ciphertext: Vec<u8> = row.try_get("ciphertext")?;
    let stored_nonce: Vec<u8> = row.try_get("nonce")?;
    let stored_aad: Vec<u8> = row.try_get("aad")?;
    let key_id: String = row.try_get("key_id")?;

    if ciphertext.len() > MAX_CIPHERTEXT_LEN {
        return Err(SecretsError::CiphertextTooLarge {
            len: ciphertext.len(),
            max: MAX_CIPHERTEXT_LEN,
        });
    }
    if stored_nonce.len() != NONCE_LEN {
        return Err(SecretsError::NonceLengthInvalid {
            expected: NONCE_LEN,
            got: stored_nonce.len(),
        });
    }
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&stored_nonce);

    // AAD strict-equality check: the canonical AAD for (name, extra)
    // is deterministic, so the stored AAD must equal the recomputed
    // one. This catches a row swap that did not also update the AAD
    // column (`UPDATE secrets SET name = …` alone). GCM auth catches
    // the case where AAD was also updated but without re-encryption
    // (the auth tag was bound to the old AAD). Both attacker variants
    // are detected, via different routes.
    let expected_aad = compute_aad(name, extra_aad);
    if stored_aad != expected_aad {
        return Err(SecretsError::AadMismatch);
    }

    let key = key_provider.get(&key_id)?;
    decrypt(&key, &ciphertext, &nonce, &expected_aad)
}

/// Return metadata-only listings for every secret.
///
/// Excludes ciphertext, nonce, and AAD on purpose: the listing is
/// safe to dump in operator UI / logs. Anything that wants the
/// plaintext goes through [`get`] under explicit policy.
pub async fn list<'e, E>(executor: E) -> Result<Vec<SecretListing>, SecretsError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT name, key_id, created_at, updated_at \
         FROM secrets \
         ORDER BY name ASC",
    )
    .fetch_all(executor)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(SecretListing {
            name: row.try_get("name")?,
            key_id: row.try_get("key_id")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        });
    }
    Ok(out)
}

/// Delete a secret by name.
///
/// Returns `Ok(true)` if a row was removed, `Ok(false)` if no row
/// matched the name (idempotent). The plaintext is gone the moment
/// the DB row is — there is no on-disk plaintext anywhere.
pub async fn delete<'e, E>(executor: E, name: &str) -> Result<bool, SecretsError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    validate_name(name)?;
    let result = sqlx::query("DELETE FROM secrets WHERE name = $1")
        .bind(name)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: encrypt → decrypt → original plaintext.
    /// Pin: Zeroizing<Vec<u8>> dereferences cleanly to &[u8].
    #[test]
    fn encrypt_then_decrypt_recovers_plaintext() {
        let key: SecretKey = Zeroizing::new([7u8; KEY_LEN]);
        let aad = compute_aad("alice", None);
        let pt = b"hunter2";
        let (ct, nonce) = encrypt(&key, pt, &aad).unwrap();
        let recovered = decrypt(&key, &ct, &nonce, &aad).unwrap();
        assert_eq!(&*recovered, pt);
    }

    /// Wrong key fails. GCM tag mismatch.
    #[test]
    fn decrypt_with_wrong_key_fails() {
        let k1: SecretKey = Zeroizing::new([1u8; KEY_LEN]);
        let k2: SecretKey = Zeroizing::new([2u8; KEY_LEN]);
        let aad = compute_aad("name", None);
        let (ct, nonce) = encrypt(&k1, b"plaintext", &aad).unwrap();
        let err = decrypt(&k2, &ct, &nonce, &aad).unwrap_err();
        assert!(matches!(err, SecretsError::DecryptFailed));
    }

    /// Wrong AAD fails. Pin: a single byte difference is enough.
    #[test]
    fn decrypt_with_wrong_aad_fails() {
        let key: SecretKey = Zeroizing::new([3u8; KEY_LEN]);
        let aad_a = compute_aad("alice", None);
        let aad_b = compute_aad("bob", None);
        let (ct, nonce) = encrypt(&key, b"x", &aad_a).unwrap();
        let err = decrypt(&key, &ct, &nonce, &aad_b).unwrap_err();
        assert!(matches!(err, SecretsError::DecryptFailed));
    }

    /// Tampered ciphertext is detected.
    #[test]
    fn decrypt_with_tampered_ciphertext_fails() {
        let key: SecretKey = Zeroizing::new([5u8; KEY_LEN]);
        let aad = compute_aad("k", None);
        let (mut ct, nonce) = encrypt(&key, b"some-secret-bytes", &aad).unwrap();
        ct[0] ^= 0x01; // flip a single bit
        let err = decrypt(&key, &ct, &nonce, &aad).unwrap_err();
        assert!(matches!(err, SecretsError::DecryptFailed));
    }

    /// Tampered nonce is detected.
    #[test]
    fn decrypt_with_tampered_nonce_fails() {
        let key: SecretKey = Zeroizing::new([6u8; KEY_LEN]);
        let aad = compute_aad("k", None);
        let (ct, mut nonce) = encrypt(&key, b"x", &aad).unwrap();
        nonce[0] ^= 0x01;
        let err = decrypt(&key, &ct, &nonce, &aad).unwrap_err();
        assert!(matches!(err, SecretsError::DecryptFailed));
    }

    /// Two encryptions under the same key+aad+plaintext yield distinct
    /// nonces and distinct ciphertexts (probabilistic — but with 96-bit
    /// random nonces, virtually certain). Catches a regression where
    /// `OsRng` were swapped for a deterministic seed.
    #[test]
    fn each_encrypt_call_uses_a_fresh_nonce() {
        let key: SecretKey = Zeroizing::new([8u8; KEY_LEN]);
        let aad = compute_aad("k", None);
        let (ct1, n1) = encrypt(&key, b"x", &aad).unwrap();
        let (ct2, n2) = encrypt(&key, b"x", &aad).unwrap();
        assert_ne!(n1, n2, "two encrypt calls yielded identical nonces");
        assert_ne!(ct1, ct2, "two encrypt calls yielded identical ciphertexts");
    }

    /// Plaintext over the cap is rejected before any crypto work.
    #[test]
    fn encrypt_rejects_oversized_plaintext() {
        let key: SecretKey = Zeroizing::new([9u8; KEY_LEN]);
        let big = vec![0u8; MAX_PLAINTEXT_LEN + 1];
        let aad = compute_aad("k", None);
        let err = encrypt(&key, &big, &aad).unwrap_err();
        assert!(matches!(
            err,
            SecretsError::PlaintextTooLarge { len, max }
                if len == MAX_PLAINTEXT_LEN + 1 && max == MAX_PLAINTEXT_LEN
        ));
    }

    /// AAD shape pin: domain separator first, NUL-delimited, name
    /// in the middle. A refactor that drops the domain separator
    /// would silently let attackers reuse our key for some other
    /// AEAD purpose.
    #[test]
    fn compute_aad_starts_with_domain_separator() {
        let aad = compute_aad("alice", None);
        assert!(aad.starts_with(AAD_DOMAIN));
        assert_eq!(aad[AAD_DOMAIN.len()], 0u8);
        assert!(aad.windows(5).any(|w| w == b"alice"));
    }

    /// AAD with extra context appends after the second NUL.
    #[test]
    fn compute_aad_appends_extra_after_second_nul() {
        let aad = compute_aad("k", Some(b"tool=imap"));
        // domain || 0 || "k" || 0 || "tool=imap"
        assert_eq!(aad.last().copied(), Some(b'p'));
        assert!(aad.ends_with(b"tool=imap"));
    }

    /// Empty aad column is structurally impossible: compute_aad
    /// always emits at least domain + NUL + 1+ byte of name + NUL.
    #[test]
    fn compute_aad_is_always_nonempty() {
        // shortest possible name passes validate_name (single char)
        let aad = compute_aad("x", None);
        assert!(!aad.is_empty());
        assert!(aad.len() > AAD_DOMAIN.len());
    }

    #[test]
    fn validate_name_rejects_empty() {
        let err = validate_name("").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName(_)));
    }

    #[test]
    fn validate_name_rejects_overlong() {
        let big = "a".repeat(MAX_NAME_LEN + 1);
        let err = validate_name(&big).unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName(_)));
    }

    #[test]
    fn validate_name_rejects_nul() {
        let err = validate_name("ab\0cd").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName(_)));
    }

    #[test]
    fn validate_name_rejects_control_chars() {
        let err = validate_name("ab\ncd").unwrap_err();
        assert!(matches!(err, SecretsError::InvalidName(_)));
    }

    #[test]
    fn validate_name_accepts_typical_names() {
        validate_name("imap_password").unwrap();
        validate_name("anthropic.api.token").unwrap();
        validate_name("user@example.com:ssh-key").unwrap();
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

    /// Constants are stable. A refactor that bumps these without
    /// thinking through migration would silently break every
    /// already-encrypted row in the field.
    #[test]
    fn constants_are_pinned() {
        assert_eq!(KEY_LEN, 32);
        assert_eq!(NONCE_LEN, 12);
        assert_eq!(GCM_TAG_LEN, 16);
        assert_eq!(AAD_DOMAIN, b"kastellan-secrets-v1");
        assert_eq!(KEY_SERVICE, "kastellan");
        assert_eq!(KEY_ACCOUNT, "secrets-v1");
        // Derived: ciphertext budget = plaintext budget + tag overhead.
        // The `get` length-guard math depends on this identity.
        assert_eq!(MAX_CIPHERTEXT_LEN, MAX_PLAINTEXT_LEN + GCM_TAG_LEN);
    }

    /// A real encrypt of a max-size plaintext fits inside
    /// [`MAX_CIPHERTEXT_LEN`]. If GCM ever changed its tag size or we
    /// fat-fingered the math, this fails — and the `get`-path guard
    /// would start rejecting legitimately-stored rows.
    #[test]
    fn max_size_plaintext_fits_within_ciphertext_cap() {
        let key: SecretKey = Zeroizing::new([0xA5u8; KEY_LEN]);
        let aad = compute_aad("k", None);
        let pt = vec![0u8; MAX_PLAINTEXT_LEN];
        let (ct, _nonce) = encrypt(&key, &pt, &aad).unwrap();
        assert!(
            ct.len() <= MAX_CIPHERTEXT_LEN,
            "encrypted output {} exceeded MAX_CIPHERTEXT_LEN {}",
            ct.len(),
            MAX_CIPHERTEXT_LEN
        );
        assert_eq!(ct.len(), MAX_PLAINTEXT_LEN + GCM_TAG_LEN);
    }
}
