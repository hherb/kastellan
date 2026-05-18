//! Pure-type definitions for the worker lifecycle policy.
//!
//! Spec: `docs/superpowers/specs/2026-05-18-worker-lifecycle-policy-design.md`.
//!
//! Slice 1 ships:
//!   - `Lifecycle::SingleUse` — current shell-exec behaviour (spawn → one request → exit).
//!   - `Lifecycle::IdleTimeout { caps, contract }` — declarable shape only; the runtime path
//!     (`IdleTimeoutLifecycle::acquire`) panics until slice 2 fills it in.
//!
//! All types here are pure: no I/O, no clock, no spawn calls. The runtime layer lives in
//! `super::manager`.

/// Lifecycle policy declared on a `ToolEntry`.
///
/// `SingleUse` is the conservative default and matches today's shell-exec behaviour:
/// spawn a fresh sandboxed process per request, run one JSON-RPC call, exit. This is the
/// right policy for transient operations where per-request isolation is the security
/// model itself.
///
/// `IdleTimeout` is the warm-keeping policy for stateless inference workers with
/// non-trivial startup cost (GLiNER-Relex, sentiment, embedding, classification, OCR).
/// The supervisor holds a single live process per worker type and re-uses it across
/// requests; caps are evaluated post-completion only (never mid-flight).
///
/// **Slice 1 ships the `IdleTimeout` variant declarable but inert** —
/// `IdleTimeoutLifecycle::acquire` panics until slice 2 implements warm-keeping.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Lifecycle {
    /// Spawn a fresh process per request. Caps don't apply.
    #[default]
    SingleUse,
    /// Spawn on first request, stay alive, tear down post-completion when any of the
    /// caps fires.
    IdleTimeout {
        caps: IdleTimeoutCaps,
        contract: Contract,
    },
}

impl Lifecycle {
    /// Validated constructor for the `IdleTimeout` variant.
    ///
    /// Rejects `Contract { stateless: false }` because slice-1 / v1 of the spec only
    /// supports stateless workers under warm-keeping. A future `stateless = false`
    /// worker needs its own threat review (per spec §"The stateless contract") and
    /// will reach this constructor via a different path.
    ///
    /// The struct-style variant literal (`Lifecycle::IdleTimeout { caps, contract }`)
    /// remains accessible for tests that need to plant an invalid value deliberately.
    pub fn idle_timeout(
        caps: IdleTimeoutCaps,
        contract: Contract,
    ) -> Result<Self, LifecycleValidationError> {
        if !contract.stateless {
            return Err(LifecycleValidationError::StatelessRequiredForIdleTimeout);
        }
        Ok(Self::IdleTimeout { caps, contract })
    }
}

/// Construction-time validation errors for `Lifecycle`.
///
/// Distinct from a generic `String` error because callers (slice 2's manifest parser,
/// the worker-author's `WorkerManifest::validate`) will programmatically branch on the
/// reason. Slice 1 has only one variant; future variants slot in cleanly.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum LifecycleValidationError {
    #[error("idle_timeout lifecycle requires Contract {{ stateless: true }} in spec v1")]
    StatelessRequiredForIdleTimeout,
}

/// Post-completion caps that bound a warm worker's lifetime.
///
/// All four are evaluated after a JSON-RPC response has been written — never mid-flight.
/// See spec §"Cap-check semantics" for the load-bearing invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdleTimeoutCaps {
    /// Tear down after this many seconds with no in-flight or queued request.
    pub idle_seconds: u64,
    /// Rotate after this many requests served cumulatively (slow-leak hygiene).
    pub max_requests: u64,
    /// Rotate after the process has been alive this many seconds (drift hygiene).
    pub max_age_seconds: u64,
    /// SIGTERM grace before SIGKILL during graceful shutdown.
    pub grace_period_seconds: u64,
}

/// Per-request statelessness contract declared by the worker author.
///
/// `stateless = true` is the only value v1 supports for `IdleTimeout` workers — see
/// spec §"The stateless contract" for what the worker author is asserting at code-review
/// time. The bool field shape is forward-compatible with a future `stateless = false`
/// path that ships with its own threat review.
///
/// For `SingleUse` workers this field is irrelevant — there is no "next request" in the
/// same process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Contract {
    pub stateless: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_default_is_single_use() {
        // Default Lifecycle is `SingleUse` so a freshly-constructed `ToolEntry`
        // (or any other consumer using `..Default::default()`) gets the conservative
        // current-shell-exec semantics, not an inference-worker policy.
        let l: Lifecycle = Lifecycle::default();
        assert!(matches!(l, Lifecycle::SingleUse));
    }

    #[test]
    fn idle_timeout_caps_carries_four_named_durations() {
        // The four caps from the spec, exposed by name so a future consumer can read
        // each one without positional indexing. All four are required at construction.
        let caps = IdleTimeoutCaps {
            idle_seconds: 600,
            max_requests: 10_000,
            max_age_seconds: 86_400,
            grace_period_seconds: 5,
        };
        assert_eq!(caps.idle_seconds, 600);
        assert_eq!(caps.max_requests, 10_000);
        assert_eq!(caps.max_age_seconds, 86_400);
        assert_eq!(caps.grace_period_seconds, 5);
    }

    #[test]
    fn contract_stateless_true_is_the_only_v1_supported_value() {
        // Slice-1 / v1 of this spec only supports `stateless = true` workers under
        // `idle_timeout`. The field exists as a bool to keep the shape forward-compatible
        // with a future `stateless = false` worker that needs its own threat review.
        let c = Contract { stateless: true };
        assert!(c.stateless);
    }

    #[test]
    fn idle_timeout_variant_carries_caps_and_contract() {
        // Round-trip the IdleTimeout variant — the struct-style variant is what slice 2's
        // runtime will pattern-match on.
        let l = Lifecycle::IdleTimeout {
            caps: IdleTimeoutCaps {
                idle_seconds: 60,
                max_requests: 100,
                max_age_seconds: 3600,
                grace_period_seconds: 5,
            },
            contract: Contract { stateless: true },
        };
        match l {
            Lifecycle::IdleTimeout { caps, contract } => {
                assert_eq!(caps.idle_seconds, 60);
                assert!(contract.stateless);
            }
            _ => panic!("expected IdleTimeout variant"),
        }
    }

    #[test]
    fn idle_timeout_requires_stateless_contract_per_spec_v1() {
        // Construction-time validation: a `Lifecycle::IdleTimeout` carrying
        // `Contract { stateless: false }` violates the v1 invariant (spec §"The stateless
        // contract"). `Lifecycle::idle_timeout(caps, contract)` is the validated constructor
        // that rejects this combination; the struct-style literal stays available for tests
        // that want to construct an invalid value deliberately.
        let bad = Lifecycle::idle_timeout(
            IdleTimeoutCaps {
                idle_seconds: 60,
                max_requests: 100,
                max_age_seconds: 3600,
                grace_period_seconds: 5,
            },
            Contract { stateless: false },
        );
        assert_eq!(
            bad,
            Err(LifecycleValidationError::StatelessRequiredForIdleTimeout)
        );
    }

    #[test]
    fn idle_timeout_accepts_stateless_contract() {
        // The positive control for the validator — a stateless=true contract under
        // idle_timeout must succeed.
        let ok = Lifecycle::idle_timeout(
            IdleTimeoutCaps {
                idle_seconds: 60,
                max_requests: 100,
                max_age_seconds: 3600,
                grace_period_seconds: 5,
            },
            Contract { stateless: true },
        );
        assert!(ok.is_ok());
        assert!(matches!(ok.unwrap(), Lifecycle::IdleTimeout { .. }));
    }
}
