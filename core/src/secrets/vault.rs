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

use kastellan_db::secrets::KeyProvider;
use kastellan_leak_scan::{fingerprint_value, SecretFingerprint};

use super::{DEFAULT_TTL, REF_HEX_LEN, REF_PREFIX};

/// Opaque pointer into the in-process [`Vault`]. Constructed only by
/// [`Vault::materialize`]. Safe to embed in audit logs and (eventually)
/// in transcripts: reveals nothing without an active Vault.
///
/// `Debug` is implemented manually and prints only [`Self::ref_hash`],
/// never the underlying `secret://<8-hex>` string. This defends against
/// careless `{:?}` formatting in `tracing::error!(?ref, ...)`,
/// `assert!(... "{r:?}")`, derived `Debug` on enclosing structs, etc.
/// The audit log's privacy promise is that ONLY the `ref_hash` ever
/// appears in observable contexts; this impl makes that the default.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SecretRef(String);

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Print only the hash so {:?} on a SecretRef can never leak the
        // ref string itself. Callers that genuinely need the ref string
        // for substitution call `as_str()` explicitly.
        f.debug_tuple("SecretRef")
            .field(&format_args!("ref_hash={}", self.ref_hash()))
            .finish()
    }
}

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

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault: secret lookup failed: {0}")]
    Secrets(#[from] kastellan_db::secrets::SecretsError),

    /// Hard-fail on audit write — see spec §5.4. Wraps the existing
    /// `kastellan_db::DbError` returned by `audit::insert`.
    ///
    /// **No `#[from]` on purpose:** `DbError` is the crate-wide error
    /// type for `kastellan_db`; a blanket `From` would silently swallow
    /// any DbError from a future method on Vault. Callers map
    /// explicitly via `.map_err(VaultError::Audit)`.
    #[error("vault: audit row insert failed during materialize: {0}")]
    Audit(kastellan_db::DbError),

    #[error("vault: materialized plaintext is empty")]
    EmptyPlaintext,

    /// Two `materialize` calls minted the same `secret://<8-hex>` ref.
    /// With OsRng over a 2^32 namespace this is astronomically rare,
    /// but if it ever does happen we MUST NOT silently overwrite the
    /// existing entry — that would re-bind the original `SecretRef` to
    /// a different plaintext on its next `redeem`, a real correctness
    /// hazard. We fail loud instead. The caller can retry; on retry a
    /// fresh ref is generated.
    #[error("vault: ref collision during materialize (rare; safe to retry)")]
    RefCollision,

    /// A `seed_known_ref_for_test` caller supplied a tail that is not exactly
    /// [`REF_HEX_LEN`] lowercase hex digits. Test-only — gated on
    /// `debug_assertions`, so it does not exist in a release build (matching
    /// the seam method itself; see [`Vault::seed_known_ref_for_test`]).
    #[cfg(debug_assertions)]
    #[error("vault: malformed test-seed ref tail (need 8 lowercase hex digits): {0:?}")]
    MalformedTestRef(String),
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
            kastellan_db::secrets::get(pool, key_provider, name, None).await?;

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
        kastellan_db::audit::insert(pool, "policy", "secret.materialized", payload)
            .await
            .map_err(VaultError::Audit)?;

        // 4. Insert into vault under the brief sync write lock. The
        //    Zeroizing<Vec<u8>> moves into the entry; on Vault::Drop or
        //    on TTL eviction, Zeroizing::Drop zeroes the plaintext bytes.
        //    An orphan `secret.materialized` audit row may remain on the
        //    collision path (spec §5.4 — audited-but-not-inserted is the
        //    acceptable side of the asymmetry).
        let entry = Entry {
            plaintext,
            expires_at: Instant::now() + self.ttl,
        };
        self.insert_fresh(secret_ref, entry)
    }

    /// Insert `entry` at `secret_ref` under the write lock, returning the ref
    /// on success and [`VaultError::RefCollision`] (rather than overwriting)
    /// if the ref is already bound.
    ///
    /// Uses `Entry::Vacant` so the type system guarantees no silent rebind:
    /// a ref collision (vanishingly rare under OsRng over the 2^32 namespace)
    /// must NOT re-point an existing `SecretRef` at a different plaintext —
    /// that would make the ref's next `redeem` yield the wrong secret, a real
    /// correctness hazard. We fail loud; the caller can retry (a retry mints a
    /// fresh ref).
    ///
    /// Extracted from [`Self::materialize`] step 4 so the `Occupied` arm is
    /// unit-testable without a live PG pool (issue #149): a test calls this
    /// twice with the same ref and asserts the second returns `RefCollision`
    /// and leaves the original plaintext intact.
    fn insert_fresh(&self, secret_ref: SecretRef, entry: Entry) -> Result<SecretRef, VaultError> {
        use std::collections::hash_map::Entry as MapEntry;
        let mut map = self.map.write().expect("vault map poisoned");
        match map.entry(secret_ref.clone()) {
            MapEntry::Vacant(v) => {
                v.insert(entry);
                Ok(secret_ref)
            }
            MapEntry::Occupied(_) => Err(VaultError::RefCollision),
        }
    }

    /// **TEST-ONLY seam (#298).** Bind `plaintext` under the caller-chosen ref
    /// `secret://<ref_hex>` so an *out-of-process* test can know the ref string
    /// up front and pass it as a `params` value that the daemon's
    /// [`crate::tool_host::dispatch`] substitutes back to this plaintext.
    ///
    /// **Why this exists / why it is safe:** the production ref minted by
    /// [`Self::materialize`] is random (OsRng) and never logged (only its
    /// `ref_hash`), so a separate CLI process cannot learn it — which is
    /// exactly the desired production property, but blocks a full-daemon
    /// output-scrub e2e. This method is the minimal seam that unblocks it.
    /// It is gated on `debug_assertions`, so it is **physically absent from any
    /// release build** (`cargo build --release` disables `debug_assertions`;
    /// the deployed daemon is built that way — see `scripts/build-release.sh`).
    /// There is therefore no code path in production that can bind a
    /// caller-known plaintext to a known ref.
    ///
    /// Validates `ref_hex` against the same well-formed-ref invariant
    /// `materialize` mints and `substitute` parses (exactly [`REF_HEX_LEN`]
    /// lowercase hex digits) and rejects an empty plaintext, then reuses the
    /// collision-safe [`Self::insert_fresh`] path (a re-seed at a bound ref
    /// returns [`VaultError::RefCollision`], never a silent rebind).
    #[cfg(debug_assertions)]
    pub fn seed_known_ref_for_test(
        &self,
        ref_hex: &str,
        plaintext: &[u8],
    ) -> Result<SecretRef, VaultError> {
        let well_formed = ref_hex.len() == REF_HEX_LEN
            && ref_hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !well_formed {
            return Err(VaultError::MalformedTestRef(ref_hex.to_string()));
        }
        if plaintext.is_empty() {
            return Err(VaultError::EmptyPlaintext);
        }
        let secret_ref = SecretRef::from_raw(format!("{REF_PREFIX}{ref_hex}"));
        let entry = Entry {
            plaintext: Zeroizing::new(plaintext.to_vec()),
            expires_at: Instant::now() + self.ttl,
        };
        self.insert_fresh(secret_ref, entry)
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

    /// Compute a one-way [`SecretFingerprint`] of the secret's value for the
    /// egress credential-leak scanner (slice #3b), **without exposing the
    /// plaintext**. Returns `None` if the ref is absent/expired or the value is
    /// below `MIN_SECRET_LEN`. Takes the read lock and fingerprints in place; the
    /// plaintext never leaves this method.
    pub fn value_fingerprint(&self, r: &SecretRef) -> Option<SecretFingerprint> {
        let now = Instant::now();
        let map = self.map.read().expect("vault map poisoned");
        let entry = map.get(r)?;
        if now >= entry.expires_at {
            return None;
        }
        fingerprint_value(&entry.plaintext)
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
