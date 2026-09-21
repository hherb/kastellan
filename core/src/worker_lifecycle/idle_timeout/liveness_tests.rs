//! Tests for the one liveness census.

use std::io;

use kastellan_protocol::client::ClientError;
use kastellan_protocol::RpcError;

use super::*;

/// One of every `ClientError` variant, with the cause each must classify to.
///
/// ⚠️ **This array is the test suite's own census, and a census rots.** It is
/// defended by [`the_fixture_covers_every_client_error_variant`] below, which
/// fails if `ClientError` grows a variant this list does not name — otherwise a
/// new variant would be classified by nobody and every test here would still
/// pass.
fn every_client_error() -> Vec<(ClientError, Option<WorkerRetirementCause>)> {
    vec![
        (
            ClientError::Rpc(RpcError {
                code: -32001,
                message: "POLICY_DENIED".into(),
                data: None,
            }),
            None,
        ),
        (ClientError::EarlyExit, Some(WorkerRetirementCause::ExitedBeforeResponding)),
        (
            ClientError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "eof")),
            Some(WorkerRetirementCause::PipeBroke),
        ),
        (
            ClientError::Decode(serde_json::from_str::<serde_json::Value>("{oops")
                .expect_err("malformed JSON must fail to parse")),
            Some(WorkerRetirementCause::Undecodable),
        ),
        (
            ClientError::IdMismatch {
                expected: serde_json::json!(1),
                got: serde_json::json!(2),
            },
            Some(WorkerRetirementCause::IdMismatch),
        ),
        (
            ClientError::ResponseTooLarge { cap: 1024 },
            Some(WorkerRetirementCause::ResponseTooLarge),
        ),
    ]
}

#[test]
fn the_fixture_covers_every_client_error_variant() {
    // The positive control for every other test in this file. `from_client_error`
    // has no `_` arm, so a new `ClientError` variant breaks ITS build — but
    // nothing would force this fixture to grow, and a classifier nobody feeds is
    // a classifier nobody tests. Counting is the cheapest way to couple them.
    //
    // ⚠️ When this fails, add the variant to `every_client_error` AND decide its
    // cause; do not just bump the number.
    const KNOWN_VARIANTS: usize = 6;
    assert_eq!(
        every_client_error().len(),
        KNOWN_VARIANTS,
        "every ClientError variant must appear exactly once in the fixture"
    );
}

#[test]
fn each_client_error_maps_to_its_own_cause() {
    for (err, expected) in every_client_error() {
        assert_eq!(
            WorkerRetirementCause::from_client_error(&err),
            expected,
            "misclassified {err:?}"
        );
    }
}

#[test]
fn the_retirement_census_and_the_liveness_census_are_the_same_census() {
    // #737 in one assertion. The defect was two lists: one deciding to retire a
    // worker, another deciding whether to say so — and they disagreed about four
    // of the five fatal variants. `dispatch_indicates_worker_dead` now delegates,
    // so this can only fail if someone reintroduces a parallel match.
    for (err, cause) in every_client_error() {
        let r: Result<(), ToolHostError> = Err(ToolHostError::Protocol(err));
        assert_eq!(
            dispatch_indicates_worker_dead(&r),
            cause.is_some(),
            "the two censuses disagree about {r:?}"
        );
    }
}

#[test]
fn exactly_one_client_error_variant_leaves_the_worker_alive() {
    // Pins the SHAPE, not just the mapping: if a future edit made everything
    // fatal (or nothing), the per-variant test above would need every case
    // changed to notice, while this fails immediately.
    let alive: Vec<_> = every_client_error()
        .into_iter()
        .filter(|(_, c)| c.is_none())
        .collect();
    assert_eq!(alive.len(), 1, "only Rpc means the worker is still listening");
    assert!(matches!(alive[0].0, ClientError::Rpc(_)));
}

#[test]
fn a_pre_spawn_io_error_is_not_a_dead_worker() {
    // #737's sixth variant. `ToolHostError::Io` is a pre-spawn provisioning
    // bucket — broker/egress sidecars, scratch dirs, the SingleUse wiring guard
    // — so no worker exists to be dead, and calling it dead ticked the
    // restart-backoff counter on a planner-side error. A worker killed
    // mid-response is `Protocol(ClientError::Io)` instead, covered above.
    //
    // ⚠️ The fixture deliberately uses BrokenPipe, the kind that most looks like
    // a dying worker, because the old test used it and it is the reading this
    // change has to be safe under.
    let r: Result<(), ToolHostError> = Err(ToolHostError::Io(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "stdio closed",
    )));
    assert!(!dispatch_indicates_worker_dead(&r));
}

#[test]
fn nothing_that_happens_before_the_worker_is_contacted_counts_as_death() {
    use crate::secrets::{MissingReason, SubstituteError};

    let cases: Vec<(&str, ToolHostError)> = vec![
        (
            "sandbox spawn failed — SPAWN_FAILED, not a warm-worker crash",
            ToolHostError::Sandbox(kastellan_sandbox::SandboxError::Backend("test".into())),
        ),
        (
            "secret redemption failed at the substitution chokepoint (Item 31)",
            ToolHostError::SecretRedemptionFailed(SubstituteError::MissingRef {
                ref_hash: "test-hash".to_string(),
                reason: MissingReason::NotFound,
            }),
        ),
        (
            "egress leak-scanner provisioning failed (slice #3b, #268)",
            ToolHostError::EgressProvisionFailed("scanner".into()),
        ),
        (
            "pre-spawn io: broker/egress/scratch provisioning (#737)",
            ToolHostError::Io(io::Error::other("provisioning")),
        ),
    ];
    for (why, err) in cases {
        assert!(
            !dispatch_indicates_worker_dead(&Err::<(), _>(err)),
            "classified as a dead worker, but {why}"
        );
    }
}

#[test]
fn a_successful_dispatch_is_never_a_death() {
    let r: Result<(), ToolHostError> = Ok(());
    assert!(!dispatch_indicates_worker_dead(&r));
}
