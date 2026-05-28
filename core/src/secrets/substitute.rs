//! Substitution walker. Mutates `serde_json::Value` in place,
//! replacing every `Value::String` that is exactly `secret://<8-hex>`
//! with the redeemed plaintext (interpreted as UTF-8). One
//! [`RedemptionEvent`] is emitted per substitution; the dispatcher
//! translates each into a `policy / secret.redeemed` audit row.

use super::vault::{RedeemResult, SecretRef};
use super::{REF_HEX_LEN, REF_PREFIX};

/// Test seam: the walker takes a `&dyn RedeemFromVault` so unit tests
/// can supply a `FakeVault` without spinning up a real [`Vault`].
/// Production passes `&*vault` (Task 2 adds `impl RedeemFromVault for Vault` via `Vault::redeem`).
pub trait RedeemFromVault {
    fn redeem(&self, r: &SecretRef) -> RedeemResult;
}

/// One successful substitution. The chokepoint translates each event
/// into a `policy / secret.redeemed` audit row.
#[derive(Debug, Clone)]
pub struct RedemptionEvent {
    pub ref_hash: String, // SHA-256(ref.as_str()), 64-char lowercase hex
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingReason {
    NotFound,
    Expired,
}

impl MissingReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            MissingReason::NotFound => "not_found",
            MissingReason::Expired => "expired",
        }
    }
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SubstituteError {
    #[error("substitute: ref {ref_hash} missing from vault (reason: {})", reason.as_str())]
    MissingRef {
        ref_hash: String,
        reason: MissingReason,
    },

    #[error("substitute: ref {ref_hash} plaintext is not valid UTF-8")]
    PlaintextNotUtf8 { ref_hash: String },
}

/// True iff `s` is exactly `secret://` + 8 lowercase hex chars and
/// nothing else. The lowercase-only check is belt-and-braces (refs
/// are generated with `{:08x}` which is always lowercase) so a
/// planner can't synthesise a casing-shifted ref to evade.
fn is_well_formed_ref(s: &str) -> bool {
    if s.len() != REF_PREFIX.len() + REF_HEX_LEN {
        return false;
    }
    if !s.starts_with(REF_PREFIX) {
        return false;
    }
    s[REF_PREFIX.len()..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Walk `value` and substitute every `Value::String` whose contents
/// are exactly a well-formed `secret://<8-hex>` ref with the redeemed
/// plaintext. Returns one [`RedemptionEvent`] per substitution.
///
/// Fails closed at the first miss / UTF-8 error — `value` is left in
/// an unspecified state; callers must drop it on `Err`.
pub fn substitute_refs_in_params(
    value: &mut serde_json::Value,
    vault: &dyn RedeemFromVault,
) -> Result<Vec<RedemptionEvent>, SubstituteError> {
    let mut events = Vec::new();
    walk(value, vault, &mut events)?;
    Ok(events)
}

// No explicit recursion depth guard: mirrors the injection-guard
// precedent (see `core::cassandra::injection_guard`). A shared depth
// helper for both walkers is tracked as a Slice 2 candidate (spec §9).
// In practice, serde_json's own parser already bounds depth.
fn walk(
    value: &mut serde_json::Value,
    vault: &dyn RedeemFromVault,
    events: &mut Vec<RedemptionEvent>,
) -> Result<(), SubstituteError> {
    match value {
        serde_json::Value::String(s) => {
            if !is_well_formed_ref(s) {
                return Ok(());
            }
            // Construct the SecretRef directly from the well-formed string.
            let secret_ref = SecretRef::from_raw(s.clone());
            let ref_hash = secret_ref.ref_hash();
            match vault.redeem(&secret_ref) {
                RedeemResult::Hit(pt) => {
                    // Convert plaintext to UTF-8 String; reject on
                    // invalid UTF-8 (binary secrets are out of scope).
                    let plaintext = String::from_utf8(pt.to_vec()).map_err(|_| {
                        SubstituteError::PlaintextNotUtf8 {
                            ref_hash: ref_hash.clone(),
                        }
                    })?;
                    *s = plaintext;
                    events.push(RedemptionEvent { ref_hash });
                    // pt drops here (Zeroizing zeroes its bytes); the
                    // new `s` is a regular String — see spec §9
                    // limitation 1 (known and accepted).
                    Ok(())
                }
                RedeemResult::Expired => Err(SubstituteError::MissingRef {
                    ref_hash,
                    reason: MissingReason::Expired,
                }),
                RedeemResult::NotFound => Err(SubstituteError::MissingRef {
                    ref_hash,
                    reason: MissingReason::NotFound,
                }),
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                walk(item, vault, events)?;
            }
            Ok(())
        }
        serde_json::Value::Object(map) => {
            for (_key, val) in map.iter_mut() {
                // Object keys are intentionally NOT substituted — a ref in a key
                // is a planner error. (Keys aren't even `Value` types in
                // serde_json, so they can't carry refs by construction; if a
                // string key happens to look like `secret://...`, it stays as
                // the key. The value walk below will surface any refs in
                // value-position the planner intended to substitute.)
                walk(val, vault, events)?;
            }
            Ok(())
        }
        // Number, Bool, Null — structurally cannot contain refs.
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
