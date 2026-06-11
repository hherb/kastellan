//! Pure cryptographic primitives + the migration-critical constants.
//!
//! Everything here is side-effect-free (no DB, no keyring): the size
//! constants, the AES-256-GCM `encrypt`/`decrypt` pair, the AAD builder
//! that binds a ciphertext to its secret name, and `validate_name`.
//! The async DB I/O in the parent [`crate::secrets`] and the key
//! providers in [`super::key_provider`] both build on these.
//!
//! All names are re-exported from the parent module, so callers keep
//! using `kastellan_db::secrets::{encrypt, decrypt, KEY_LEN, …}`.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key as AesKey, Nonce as AesNonce};
use zeroize::Zeroizing;

use super::error::SecretsError;

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

/// Hard cap on ciphertext length accepted by [`crate::secrets::get`]. A
/// row with a ciphertext column larger than this is treated as DB
/// corruption / an attacker who has write access; we refuse rather
/// than feed it into `aes-gcm::decrypt` (which would happily allocate
/// to whatever size we hand it). PG `bytea` could in principle hold up
/// to 1 GB, so the cap is load-bearing on the decrypt side.
pub const MAX_CIPHERTEXT_LEN: usize = MAX_PLAINTEXT_LEN + GCM_TAG_LEN;

/// Default keyring service name (= the entry's "service" field on
/// libsecret / Keychain). Combined with [`KEY_ACCOUNT`] it forms the
/// stable lookup key for [`super::key_provider::OsKeyringProvider`].
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
/// rotation knob for now: the new id slots into
/// [`super::key_provider::KeyProvider::current_id`] while the old id
/// stays valid for ciphertexts that haven't been re-encrypted yet.
///
/// **Do not rename this without a rotation migration** — see the
/// [`KEY_SERVICE`] doc comment for why; the same coupling applies.
pub const KEY_ACCOUNT: &str = "secrets-v1";

/// 32-byte AES-256 wrapping key, wiped on drop.
pub type SecretKey = Zeroizing<[u8; KEY_LEN]>;

/// 12-byte AES-GCM nonce.
pub type Nonce = [u8; NONCE_LEN];

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
