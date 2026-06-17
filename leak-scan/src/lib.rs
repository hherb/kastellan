//! Pure, dependency-light credential-leak scanner shared by the egress proxy
//! (which *detects* leaks on MITM-terminated plaintext) and `core` (which
//! *provisions* the per-worker secret-value fingerprints).
//!
//! Single source of truth: the fingerprint algorithm here MUST stay identical
//! on both sides, so it lives in exactly one crate. Detection is hashes-only —
//! a [`SecretFingerprint`] carries only one-way hashes (a SHA-256 + a 64-bit
//! Rabin fingerprint) plus the length, never the secret value. See the design
//! doc `docs/superpowers/specs/2026-06-12-egress-proxy-slice3b-credential-leak-scanner-design.md`.

mod fingerprint;
mod matcher;
mod redact;
mod wire;

pub use fingerprint::{fingerprint_value, SecretFingerprint, MAX_SECRET_LEN, MIN_SECRET_LEN};
pub use matcher::{LeakHit, RollingMatcher};
pub use redact::{redact, RedactHit, RedactOutcome};
pub use wire::{parse_hashes, serialize_hashes};
