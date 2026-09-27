//! One wording for "the worker's upstream refused its credential" (#673).
//!
//! A worker that talks to an authenticated HTTP service (the `mail` tool and
//! the `email-in` channel both talk to localmail) can be told **401** or
//! **403** by that service. Neither is kastellan refusing anything: the
//! operator's credential expired, was revoked, or lacks a grant. They used to
//! be reported as [`codes::POLICY_DENIED`] — the code the core's own policy
//! gate uses — so when the DGX's 30-day localmail token expired, the planner
//! concluded that *kastellan* had forbidden it, told the user so, and spent
//! three more plans trying to route around a policy that did not exist.
//!
//! [`upstream_auth_refusal`] is the one mapping, so both workers (and any
//! later one) say the same thing under the same code,
//! [`codes::UPSTREAM_AUTH_FAILED`]. It is **pure**: a status in, an error out.

use crate::{codes, RpcError};

/// Map an upstream HTTP status to the credential-refusal error, or `None`
/// when the status is not a credential refusal (the caller keeps its own
/// handling for every other status).
///
/// `service` names the upstream in the message (e.g. `"localmail"`), so the
/// planner — and the user it answers — learns *which* credential to renew.
///
/// The two statuses are worded differently because they call for different
/// operator actions:
///
/// * **401** — the credential itself was not accepted. On a schedule, that is
///   an **expiry** (the failure that actually happened), but on a fresh
///   install it is as likely a wrong token file, so the message names all
///   three: invalid, expired or revoked.
/// * **403** — a grant is missing, **or something in front of the service
///   refused**. localmail's own `/v1` API never answers 403 (an ACL miss is a
///   404), so for localmail a 403 points at a proxy; the message names both.
///
/// Both messages say "not a kastellan policy refusal" and "retrying will not
/// help", which is what stops the planner re-planning around the failure.
/// Both fit [`crate::STEP_ERR_DETAIL_MAX`], so the planner sees them whole.
pub fn upstream_auth_refusal(service: &str, status: u16) -> Option<RpcError> {
    let message = match status {
        401 => format!(
            "{service} rejected kastellan's credential (HTTP 401): it is invalid, expired or \
             revoked. Not a kastellan policy refusal; retrying will not help. The operator \
             must renew it."
        ),
        403 => format!(
            "{service} refused kastellan (HTTP 403): the credential lacks a grant, or a proxy in \
             front refused. Not a kastellan policy refusal; retrying will not help. The \
             operator must fix it."
        ),
        _ => return None,
    };
    Some(RpcError::new(codes::UPSTREAM_AUTH_FAILED, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_401_is_an_upstream_auth_failure_that_names_expiry() {
        let e = upstream_auth_refusal("localmail", 401).expect("401 is a credential refusal");
        assert_eq!(e.code, codes::UPSTREAM_AUTH_FAILED);
        assert!(e.message.starts_with("localmail "), "names the service: {}", e.message);
        assert!(e.message.contains("HTTP 401"), "{}", e.message);
        assert!(e.message.contains("expired"), "names the likely cause: {}", e.message);
        assert!(e.message.contains("invalid"), "a wrong token file is not an expiry: {}", e.message);
        assert!(e.message.contains("Not a kastellan policy refusal"), "{}", e.message);
        assert!(e.message.contains("retrying will not help"), "{}", e.message);
    }

    #[test]
    fn a_403_is_an_upstream_auth_failure_that_names_the_grant() {
        let e = upstream_auth_refusal("localmail", 403).expect("403 is a credential refusal");
        assert_eq!(e.code, codes::UPSTREAM_AUTH_FAILED);
        assert!(e.message.contains("HTTP 403"), "{}", e.message);
        assert!(e.message.contains("grant"), "{}", e.message);
        assert!(e.message.contains("proxy"), "localmail never 403s itself: {}", e.message);
        assert!(!e.message.contains("expired"), "a 403 is not an expiry: {}", e.message);
        assert!(e.message.contains("Not a kastellan policy refusal"), "{}", e.message);
        assert!(e.message.contains("retrying will not help"), "{}", e.message);
    }

    #[test]
    fn every_other_status_is_left_to_the_caller() {
        for status in [200, 400, 404, 407, 422, 429, 500, 502, 503] {
            assert!(upstream_auth_refusal("localmail", status).is_none(), "status {status}");
        }
    }

    /// The planner sees at most `STEP_ERR_DETAIL_MAX` chars of a detail. A
    /// message clamped mid-sentence would lose "the operator must …" — the
    /// half that stops a re-plan. Measured with a service name longer than
    /// any real one.
    #[test]
    fn both_messages_fit_the_planner_detail_clamp() {
        let long_service = "a-rather-long-upstream-name";
        for status in [401, 403] {
            let e = upstream_auth_refusal(long_service, status).unwrap();
            assert!(
                e.message.chars().count() <= crate::STEP_ERR_DETAIL_MAX,
                "{} chars > {}: {}",
                e.message.chars().count(),
                crate::STEP_ERR_DETAIL_MAX,
                e.message
            );
        }
    }

    /// The code must not collide with the core's policy verdict — that
    /// aliasing is the defect (#673) — nor with `-32003`, which two Python
    /// workers already use privately (`INFERENCE_FAILED`, `RENDER_FAILED`).
    #[test]
    fn the_code_is_distinct_from_every_other_code_in_use() {
        assert_ne!(codes::UPSTREAM_AUTH_FAILED, codes::POLICY_DENIED);
        assert_ne!(codes::UPSTREAM_AUTH_FAILED, codes::OPERATION_FAILED);
        assert_ne!(codes::UPSTREAM_AUTH_FAILED, -32003);
        assert!((-32099..=-32000).contains(&codes::UPSTREAM_AUTH_FAILED), "app-level range");
    }
}
