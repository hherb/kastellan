//! Error type shared across the secrets layers.
//!
//! [`SecretsError`] is the single error surfaced by every public entry
//! point in this module — the pure crypto helpers ([`super::crypto`]),
//! the key providers ([`super::key_provider`]), and the async DB I/O in
//! the parent [`crate::secrets`]. It is re-exported from the parent so
//! callers keep using `kastellan_db::secrets::SecretsError`.

use thiserror::Error;

/// Errors surfaced by the secrets runtime.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// Empty / oversize / control-character-laden secret name.
    #[error("secret name is invalid: {0}")]
    InvalidName(String),

    /// Plaintext exceeds [`super::crypto::MAX_PLAINTEXT_LEN`].
    #[error("plaintext is too large: {len} bytes (max {max})")]
    PlaintextTooLarge { len: usize, max: usize },

    /// Stored ciphertext exceeds [`super::crypto::MAX_CIPHERTEXT_LEN`]. Either the
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

    /// [`super::key_provider::KeyProvider::get`] doesn't know the
    /// requested key id. Operator action: re-enrol the key or rotate.
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
