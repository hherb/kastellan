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
//! ## Module layout
//!
//! The module is split across three siblings, all re-exported here so
//! callers keep using the flat `kastellan_db::secrets::*` paths:
//!
//! - [`crypto`] — size constants, type aliases, and the pure
//!   `validate_name` / `compute_aad` / `encrypt` / `decrypt` helpers.
//! - [`key_provider`] — the [`KeyProvider`] trait + [`MapKeyProvider`]
//!   (tests) + [`OsKeyringProvider`] (production).
//! - [`error`] — the shared [`SecretsError`] enum.
//!
//! This file owns the async DB I/O ([`put`] / [`get`] / [`list`] /
//! [`delete`] + [`SecretListing`]) that stitches those layers together
//! against the `secrets` table.
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

mod crypto;
mod error;
mod key_provider;

pub use crypto::{
    compute_aad, decrypt, encrypt, validate_name, Nonce, SecretKey, AAD_DOMAIN, GCM_TAG_LEN,
    KEY_ACCOUNT, KEY_LEN, KEY_SERVICE, MAX_CIPHERTEXT_LEN, MAX_NAME_LEN, MAX_PLAINTEXT_LEN,
    NONCE_LEN,
};
pub use error::SecretsError;
pub use key_provider::{KeyProvider, MapKeyProvider, OsKeyringProvider};

use sqlx::types::time::OffsetDateTime;
use sqlx::Row;
use zeroize::Zeroizing;

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
