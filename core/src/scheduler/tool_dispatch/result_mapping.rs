//! Pure result-mapping helpers for the production `StepDispatcher`.
//!
//! Extracted from the parent [`super`] (`tool_dispatch`) module so the
//! wire-level error translation is unit-testable without spawning a
//! worker, and to keep the parent under the 500-LOC soft cap. Both
//! functions are **pure** — no I/O, no clock, no global state.
//!
//! Re-exported from the parent (`pub use result_mapping::{…}`) so the
//! public paths `scheduler::tool_dispatch::{rpc_code_name,
//! map_dispatch_result}` are byte-for-byte unchanged for callers and
//! for the parent's own `dispatch_step` impl. The tests for these two
//! functions live in the parent's sibling `tests.rs` (they reach these
//! symbols via the re-export through `use super::*`).

use kastellan_protocol::{client::ClientError, codes};

use crate::scheduler::inner_loop::StepOutcome;
use crate::tool_host::ToolHostError;

/// Map a JSON-RPC numeric error code to its mnemonic. The mnemonics
/// match the constants in [`kastellan_protocol::codes`]; an unknown code
/// surfaces as `"RPC_ERROR"` so the inner loop sees *something*
/// usable without a magic number.
///
/// This is the only place where the wire-level integer is rendered
/// back to a string consumers (the audit log, the inner loop's plan
/// reflection summary) will see, so the names are intentionally
/// short, ALL_CAPS, and identical to the protocol module's constant
/// names.
pub fn rpc_code_name(code: i32) -> &'static str {
    match code {
        codes::PARSE_ERROR => "PARSE_ERROR",
        codes::INVALID_REQUEST => "INVALID_REQUEST",
        codes::METHOD_NOT_FOUND => "METHOD_NOT_FOUND",
        codes::INVALID_PARAMS => "INVALID_PARAMS",
        codes::INTERNAL_ERROR => "INTERNAL_ERROR",
        codes::POLICY_DENIED => "POLICY_DENIED",
        codes::OPERATION_FAILED => "OPERATION_FAILED",
        codes::UPSTREAM_AUTH_FAILED => "UPSTREAM_AUTH_FAILED",
        _ => "RPC_ERROR",
    }
}

/// The worker's detail when `outcome` is an upstream credential refusal
/// ([`codes::UPSTREAM_AUTH_FAILED`]), else `None`. Pure.
///
/// Why the dispatcher asks (#674): an expired credential is an **operator**
/// problem that no plan can fix, yet the only party told about it is the
/// planner. On the DGX a localmail token expired and nothing noticed for five
/// days. The dispatcher uses this to put one operator-facing line in the
/// daemon log per refused call; the tool's own audit row already carries the
/// worker's message and the numeric code.
///
/// Compared through [`rpc_code_name`], not a second string literal, so the
/// mnemonic has one spelling.
pub fn upstream_auth_failure_detail(outcome: &StepOutcome) -> Option<&str> {
    match outcome {
        StepOutcome::Err { code, detail } if code == rpc_code_name(codes::UPSTREAM_AUTH_FAILED) => {
            Some(detail)
        }
        _ => None,
    }
}

/// Translate a `tool_host::dispatch` result into the inner-loop's
/// [`StepOutcome`]. Pure — extracted so the wire-level error mapping
/// is unit-testable without spawning a worker.
///
/// The mapping is:
///
/// | dispatch outcome                                     | StepOutcome                                                |
/// | ---------------------------------------------------- | ---------------------------------------------------------- |
/// | `Ok(value)`                                          | `Ok(value)`                                                |
/// | `Err(Sandbox(_))`                                    | `Err { code: "SPAWN_FAILED", detail }`                     |
/// | `Err(Io(_))`                                         | `Err { code: "IO_ERROR",     detail }`                     |
/// | `Err(Protocol(ClientError::Rpc { code: c, msg, .. }))`| `Err { code: rpc_code_name(c), detail: msg }`              |
/// | `Err(Protocol(_other))`                              | `Err { code: "PROTOCOL_ERROR", detail }`                   |
///
/// The first three buckets are pre-RPC failures the dispatcher itself
/// is responsible for. The fourth is the worker's structured rejection
/// (`POLICY_DENIED`, `OPERATION_FAILED`, etc.) and is the most common
/// failure mode in production. The fifth is decode / I/O at the
/// stdio-pipe layer.
pub fn map_dispatch_result(
    result: Result<serde_json::Value, ToolHostError>,
) -> StepOutcome {
    match result {
        Ok(v) => StepOutcome::Ok(v),
        Err(ToolHostError::Sandbox(e)) => StepOutcome::Err {
            code: "SPAWN_FAILED".into(),
            detail: e.to_string(),
        },
        Err(ToolHostError::Io(e)) => StepOutcome::Err {
            code: "IO_ERROR".into(),
            detail: e.to_string(),
        },
        Err(ToolHostError::Protocol(ClientError::Rpc(rpc))) => StepOutcome::Err {
            code: rpc_code_name(rpc.code).into(),
            detail: rpc.message,
        },
        Err(ToolHostError::Protocol(other)) => StepOutcome::Err {
            code: "PROTOCOL_ERROR".into(),
            detail: other.to_string(),
        },
        Err(ToolHostError::SecretRedemptionFailed(_)) => StepOutcome::Err {
            code: "POLICY_DENIED".to_string(),
            detail: "secret redemption failed before worker call".to_string(),
        },
        // Egress slice #3b (#268): provisioning failed before worker was called —
        // fail-closed, same treatment as POLICY_DENIED (no retry).
        Err(ToolHostError::EgressProvisionFailed(msg)) => StepOutcome::Err {
            code: "POLICY_DENIED".to_string(),
            detail: msg,
        },
    }
}
