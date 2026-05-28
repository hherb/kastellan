//! Substitution walker. Mutates `serde_json::Value` in place,
//! replacing every `Value::String` that is exactly `secret://<8-hex>`
//! with the redeemed plaintext (interpreted as UTF-8). One
//! [`RedemptionEvent`] is emitted per substitution; the dispatcher
//! translates each into a `policy / secret.redeemed` audit row.

use super::vault::{RedeemResult, SecretRef};

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

/// Walk `value` and substitute every `Value::String` whose contents
/// are exactly a well-formed `secret://<8-hex>` ref with the redeemed
/// plaintext. Returns one [`RedemptionEvent`] per substitution.
///
/// Fails closed at the first miss / UTF-8 error — `value` is left in
/// an unspecified state; callers must drop it on `Err`.
pub fn substitute_refs_in_params(
    _value: &mut serde_json::Value,
    _vault: &dyn RedeemFromVault,
) -> Result<Vec<RedemptionEvent>, SubstituteError> {
    unimplemented!("substitute_refs_in_params — filled in Task 3")
}

#[cfg(test)]
mod tests;
