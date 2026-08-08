//! The daemon's startup notice when egress force-routing is OFF.
//!
//! `main.rs` logged an `info!` when force-routing was ON and said **nothing at
//! all** when it was off — the `if let Some(..)` had no `else`. With it off,
//! host workers fall back to `--share-net` with only the in-worker allowlist,
//! and no line, row or metric records that.
//!
//! That silence matters more than it looks. The unit sets
//! `Environment=KASTELLAN_EGRESS_FORCE_ROUTING=1`, but systemd applies
//! `EnvironmentFile=` **after** `Environment=` (measured on a live user manager,
//! not assumed), so the env file the installer regenerates — and the operator
//! overlay beside it — can turn this off. A posture that an ordinary config file
//! can flip must announce itself.
//!
//! The actor is `daemon`, not `egress_proxy`: this is the daemon's own startup
//! posture, and attributing it to a proxy that by definition is not running
//! would be wrong.

/// Operator-facing phrase, grep-able in `~/.local/state/kastellan/*.out`.
/// A `const` on purpose: three separate changes (#516, #524, #525) shipped an
/// operator-facing phrase as a literal typed twice and drifted.
pub const FORCE_ROUTING_DISABLED_LOG_PHRASE: &str = "EGRESS FORCE-ROUTING DISABLED";

/// Audit actor for daemon-level startup posture rows.
pub const ACTOR: &str = "daemon";

/// Audit action. Renaming is an audit-trail contract break.
pub const ACTION_FORCE_ROUTING_DISABLED: &str = "egress.force_routing_disabled";

/// Payload for the `egress.force_routing_disabled` row.
///
/// Pure, so the wire shape is unit-testable without a live pool.
pub fn force_routing_disabled_payload() -> serde_json::Value {
    serde_json::json!({
        "phrase": FORCE_ROUTING_DISABLED_LOG_PHRASE,
        "env_var": "KASTELLAN_EGRESS_FORCE_ROUTING",
        "consequence": "Net::Allowlist workers spawn with a direct network route; \
                        only the in-worker allowlist applies, and no egress proxy \
                        enforces host:port or SSRF checks.",
    })
}

#[cfg(test)]
mod tests;
