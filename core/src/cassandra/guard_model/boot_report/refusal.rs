//! The durable row for a guard tier that **refused to boot** (issue [#906]).
//!
//! Every [`GuardTierError`] stops the daemon (wiring-spec D6), and before
//! this module the refusal existed only in `kastellan-core.err`. On the DGX
//! that cost five days: the daemon refused 1500+ times between 2026-10-19
//! and 10-24, and `audit_log` held nothing at all for the whole stretch —
//! the only evidence was a growing `NRestarts` and a rotating stderr log.
//! D5's argument is that the row, not the log, is the forensic surface, so
//! "the tier refused to boot" is a row too.
//!
//! **Its own action, not a third `guard_tier.boot` shape.** Readers of
//! `guard_tier.boot` (operator queries, `guard_tier_boot_payload`'s
//! one-row-per-boot assertion) are written against the configured and
//! unconfigured payloads; a refusal is neither, and a boot that refused
//! never reached the point where those rows are written.
//!
//! **Closed tokens only, never the error text** — the same rule as
//! [#616]: a `/props` refusal carries the backend's own bytes in its
//! message, and none of them may reach a durable row. Every value this
//! payload can hold is a `&'static str` written here or in
//! [`GuardErrorKind`].
//!
//! [#616]: https://github.com/hherb/kastellan/issues/616
//! [#906]: https://github.com/hherb/kastellan/issues/906

use std::time::Duration;

use serde_json::{json, Value};

use super::super::tier::{error_kind, GuardErrorKind, GuardTierError};

/// The `audit_log.action` a refusal is recorded under (actor `policy`).
pub const REFUSED_ACTION: &str = "guard_tier.boot_refused";

/// How long one refusal row stands for every repeat of the same refusal.
///
/// A crash-looping daemon refuses once per restart; without thinning that
/// is one row per restart for as long as the misconfiguration lasts. One
/// row per kind per hour still shows when the loop started and that it
/// went on, and a *different* refusal kind is written at once.
pub const REFUSAL_ROW_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The closed token naming which refusal stopped the boot.
///
/// An exhaustive match, so a new [`GuardTierError`] variant is a compile
/// error here rather than a row with a guessed token.
pub fn refusal_kind(e: &GuardTierError) -> &'static str {
    match e {
        GuardTierError::Config(_) => "config",
        GuardTierError::Tau(_) => "tau",
        GuardTierError::PropsUnavailable(_) => "props_unavailable",
        GuardTierError::Context(_) => "context",
        GuardTierError::Timeout(_) => "timeout",
        GuardTierError::Required => "required",
    }
}

/// The `policy / guard_tier.boot_refused` payload.
///
/// `error_kind` says *why* `/props` could not be read (a refused connection
/// is a backend that is not running; an HTTP status or a decode failure is
/// the wrong endpoint), and is `null` on every other refusal. Present on
/// every row, `null` when it does not apply, for the reason
/// [`super::super::GuardReport::audit_value`] gives: a key that appears only
/// sometimes cannot tell "not applicable" from "predates the field".
///
/// The whole payload doubles as the thinning key: two refusals with the
/// same payload are the same refusal (see `main/guard_refusal.rs`).
pub fn refusal_payload(e: &GuardTierError) -> Value {
    let error_kind: Option<GuardErrorKind> = match e {
        GuardTierError::PropsUnavailable(re) => Some(error_kind::classify(re)),
        _ => None,
    };
    json!({
        "refusal":    refusal_kind(e),
        "error_kind": error_kind.map(|k| k.as_str()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::guard_model::context_pin::GuardContextError;
    use crate::cassandra::guard_model::tier::TauError;
    use crate::cassandra::guard_model::timeout::TimeoutError;
    use kastellan_llm_router::RouterError;

    /// One of each variant, with the token its row must carry. Spelled out
    /// so a renamed token — a durable wire value — fails here.
    fn every_refusal() -> Vec<(GuardTierError, &'static str)> {
        vec![
            (GuardTierError::Config(RouterError::Config("x".into())), "config"),
            (GuardTierError::Tau(TauError::AboveOne(2.0)), "tau"),
            (
                GuardTierError::PropsUnavailable(RouterError::HttpStatus {
                    status: 404,
                    body: "not found".into(),
                }),
                "props_unavailable",
            ),
            (GuardTierError::Context(GuardContextError::NoContextSize), "context"),
            (GuardTierError::Timeout(TimeoutError::Zero), "timeout"),
            (GuardTierError::Required, "required"),
        ]
    }

    #[test]
    fn each_refusal_carries_its_own_token() {
        for (e, token) in every_refusal() {
            assert_eq!(refusal_payload(&e)["refusal"], json!(token), "{e:?}");
        }
    }

    #[test]
    fn a_props_refusal_names_why_props_failed() {
        let e = GuardTierError::PropsUnavailable(RouterError::HttpStatus {
            status: 404,
            body: "not found".into(),
        });
        assert_eq!(refusal_payload(&e)["error_kind"], json!("http_status"));
    }

    #[test]
    fn error_kind_is_present_and_null_on_every_other_refusal() {
        for (e, token) in every_refusal() {
            if token == "props_unavailable" {
                continue;
            }
            let payload = refusal_payload(&e);
            assert!(
                payload.get("error_kind").is_some_and(Value::is_null),
                "{token}: {payload}"
            );
        }
    }

    /// The backend's bytes must never reach the row (#616's rule): a
    /// `/props` body and a config message both stay out.
    #[test]
    fn no_error_text_reaches_the_payload() {
        let e = GuardTierError::PropsUnavailable(RouterError::HttpStatus {
            status: 500,
            body: "attacker-chosen body".into(),
        });
        let payload = refusal_payload(&e);
        assert_eq!(payload.as_object().map(|o| o.len()), Some(2), "{payload}");
        assert!(!payload.to_string().contains("attacker"), "{payload}");
    }
}
