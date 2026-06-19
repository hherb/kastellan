//! Opaque secret references and in-process materialization vault.
//!
//! Planner-visible references have the shape `secret://<8-hex>` (e.g.
//! `secret://abc12345`). Core substitutes refs → plaintext at the
//! `tool_host::dispatch` chokepoint, immediately before the JSON-RPC
//! envelope is handed to the worker process. Operators (and Slice 2's
//! CLI) materialize refs via [`Vault::materialize`]; the planner
//! never *names* a secret directly.
//!
//! ## Threat model
//!
//! Plaintext secrets must never appear in the LLM's conversation
//! history, the `audit_log` payload of any `actor='policy'` row, or
//! any future operator UI replaying transcripts. The tool row's
//! `payload.req` field IS allowed to carry plaintext (precedent set
//! by `injection_guard` slice 1, commit `45627fd`) — the privacy
//! invariant is scoped to `actor='policy'` rows only.
//!
//! See [`docs/superpowers/specs/2026-05-28-opaque-secret-references-design.md`](../../../docs/superpowers/specs/2026-05-28-opaque-secret-references-design.md)
//! for the full design.

pub mod admin;
pub mod collect;
pub mod substitute;
pub mod vault;

pub use collect::collect_refs_in_params;
pub use substitute::{
    substitute_refs_in_params, MissingReason, RedeemFromVault, RedemptionEvent, SubstituteError,
};
pub use vault::{RedeemResult, SecretRef, Vault, VaultError};

use std::time::Duration;

/// Default Vault TTL — 1 hour. Bounded blast radius if a ref leaks
/// into transcript history. Construct with [`Vault::with_ttl`] to
/// override (tests use ~100 ms).
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Prefix every well-formed ref starts with.
pub const REF_PREFIX: &str = "secret://";

/// Number of lowercase-hex digits in a well-formed ref's tail.
/// 4 random bytes via `OsRng` formatted as `{:08x}`. 4-byte namespace
/// (~4.3 B) is comfortably large for one-process TTL'd refs.
pub const REF_HEX_LEN: usize = 8;

#[cfg(test)]
mod tests;
