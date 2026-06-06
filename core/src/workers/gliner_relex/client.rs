//! Typed client wrapping [`crate::tool_host::dispatch`] for the
//! gliner-relex worker's `extract` method.
//!
//! Slice 2 deliberately did NOT ship a client (the worker was driven
//! only through `PlannedStep`s on the scheduler). The v2 entity-
//! extraction consumer is the first non-dispatcher caller that needs to
//! land an `extract` request as a typed function call; [`Client`] is the
//! chokepoint for that path, funnelling every consumer through the same
//! `acquire` → `tool_host::dispatch` → crash-classify shape the step
//! dispatcher uses so audit rows, warm-slot bookkeeping, and crash
//! recovery all behave identically.

use std::sync::{Arc, OnceLock};

use hhagent_protocol::client::ClientError as ProtocolClientError;
use sqlx::PgPool;

use super::wire::{ExtractRequest, ExtractResponse};
use crate::scheduler::ToolEntry;
use crate::tool_host::{self, ToolHostError};
use crate::worker_lifecycle::WorkerLifecycleManager;

/// Typed client wrapping [`crate::tool_host::dispatch`] for the
/// gliner-relex worker's `extract` method.
///
/// One [`Client`] per daemon — holds the
/// [`Arc<dyn WorkerLifecycleManager>`][WorkerLifecycleManager] shared
/// with the step dispatcher (so the client lands on the SAME warm slot
/// that scheduled steps land on, when `entry.lifecycle ==
/// Lifecycle::IdleTimeout`), plus a snapshot of the worker's
/// [`ToolEntry`]. The entry is the same one registered in the tool
/// registry; cloning the manifest into the client avoids exposing the
/// registry's internals to non-dispatch callers.
///
/// ## Why this exists
///
/// Slice 2 deliberately did NOT ship a typed client (see this module's
/// header doc). The v2 entity-extraction consumer slice (Task 11's
/// `GlinerRelexExtractor`) is the first non-dispatcher caller that needs
/// to land an `extract` request as a typed function call rather than
/// wiring a `PlannedStep` through the scheduler. This client is the
/// chokepoint for that path — it funnels every consumer through the same
/// `acquire` → `tool_host::dispatch` → crash-classify shape the step
/// dispatcher uses, so audit rows, warm-slot bookkeeping, and crash
/// recovery all behave identically.
///
/// ## What it does NOT do
///
/// - **No batching.** One [`extract`][Self::extract] call = one
///   JSON-RPC round trip. Higher-level batchers compose this client.
/// - **No retry on RPC errors.** `INVALID_INPUT` / `INFERENCE_FAILED`
///   are surfaced as [`ClientError::RpcError`] for the caller to
///   classify; the worker stays alive (per
///   [`dispatch_indicates_worker_dead`][cd]'s `Rpc(_)` → alive
///   classification).
/// - **No retry on worker death.** Crashes report through to the
///   lifecycle manager via
///   [`WorkerHandle::report_crash`][rc], which bumps the restart
///   backoff; the caller sees [`ClientError::WorkerDead`] and decides
///   whether to retry. This matches the step dispatcher's behaviour.
///
/// [cd]: crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead
/// [rc]: crate::worker_lifecycle::WorkerHandle::report_crash
pub struct Client {
    lifecycle: Arc<dyn WorkerLifecycleManager>,
    pool: PgPool,
    entry: ToolEntry,
    tool_name: &'static str,
}

impl Client {
    /// Logical tool name registered for the gliner-relex worker. This
    /// is the same string [`GlinerRelexManifest`](super::GlinerRelexManifest)
    /// reports from `name()` when registering the entry in the
    /// [`ToolRegistry`][reg], so the warm-cache key in
    /// [`IdleTimeoutLifecycle`][itl] matches whether the call originates
    /// from the step dispatcher or this client.
    ///
    /// [reg]: crate::scheduler::ToolRegistry
    /// [itl]: crate::worker_lifecycle::IdleTimeoutLifecycle
    pub const TOOL_NAME: &'static str = "gliner-relex";

    /// Construct a client. Production callers (Task 15) pass the
    /// `Arc<dyn WorkerLifecycleManager>` shared with the step
    /// dispatcher and a snapshot of the registered [`ToolEntry`].
    pub fn new(
        lifecycle: Arc<dyn WorkerLifecycleManager>,
        pool: PgPool,
        entry: ToolEntry,
    ) -> Self {
        Self {
            lifecycle,
            pool,
            entry,
            tool_name: Self::TOOL_NAME,
        }
    }

    /// Single round-trip extract. Wraps acquire → dispatch → crash-
    /// classify → decode.
    ///
    /// The audit row for the dispatch is written automatically by
    /// [`tool_host::dispatch`]; the caller does not need to log
    /// anything separately for SQL-queryable history.
    ///
    /// On RPC-level errors (worker reachable, request rejected) the
    /// numeric `-32xxx` code is preserved in
    /// [`ClientError::RpcError`] so callers can branch on the
    /// wire-stable code (e.g. `-32001 INVALID_INPUT` retries are
    /// pointless; `-32003 INFERENCE_FAILED` retries may help).
    /// On worker-death errors (`Io`, `Protocol(EarlyExit|Io|Decode|IdMismatch)`)
    /// the lifecycle manager is notified via
    /// [`WorkerHandle::report_crash`][rc] before the error returns, so
    /// the next acquire on the same warm slot waits behind the
    /// restart-backoff.
    ///
    /// [rc]: crate::worker_lifecycle::WorkerHandle::report_crash
    pub async fn extract(
        &self,
        req: ExtractRequest,
    ) -> Result<ExtractResponse, ClientError> {
        let req_value = serde_json::to_value(&req)
            .map_err(|e| ClientError::EncodeError(e.to_string()))?;

        let mut handle = self
            .lifecycle
            .acquire(self.tool_name, &self.entry)
            .await
            .map_err(|e| ClientError::WorkerSpawnFailed(e.to_string()))?;

        // gliner-relex calls never carry secret refs in params
        // (the extraction request is a plain string, not an agent
        // tool call). Pass a process-wide shared empty vault so the
        // substitution walk is a no-op but the API contract is
        // satisfied — and we don't pay a HashMap allocation per call.
        let result = tool_host::dispatch(
            &self.pool,
            empty_vault(),
            handle.worker_mut(),
            self.tool_name,
            "extract",
            req_value,
        )
        .await;

        // Crash classification — same chokepoint the step dispatcher
        // uses (`scheduler::tool_dispatch::ToolHostStepDispatcher::dispatch_step`).
        // Keeping the call here means warm-slot bookkeeping for client
        // calls and scheduler calls converges in `idle_timeout.rs`.
        if crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead(
            &result,
        ) {
            handle.report_crash();
        }

        match result {
            Ok(v) => serde_json::from_value::<ExtractResponse>(v)
                .map_err(|e| ClientError::DecodeError(e.to_string())),
            // RPC-level error: the worker is alive and rejected the
            // call. Preserve the wire-stable numeric code + message so
            // callers can branch on `-32001 INVALID_INPUT` /
            // `-32002 MODEL_LOAD_FAILED` / `-32003 INFERENCE_FAILED`
            // without re-parsing the message string.
            Err(ToolHostError::Protocol(ProtocolClientError::Rpc(rpc))) => {
                Err(ClientError::RpcError {
                    code: rpc.code,
                    message: rpc.message,
                })
            }
            // Everything else (Sandbox spawn failure already converted
            // above by the acquire arm; Io; Protocol(EarlyExit|Io|
            // Decode|IdMismatch)) means the worker is gone. The
            // crash-classifier already flipped `died = true` on the
            // handle so the lifecycle manager will not return it to
            // the warm slot.
            Err(e) => Err(ClientError::WorkerDead(e.to_string())),
        }
    }
}

/// Process-wide shared empty Vault for gliner-relex dispatches. The
/// `tool_host::dispatch` API takes `&Vault` mandatorily, but gliner-
/// relex requests never carry `secret://` refs — the extraction input
/// is a plain string. Sharing one immutable Vault across all calls
/// avoids the per-dispatch `HashMap` allocation a fresh `Vault::new()`
/// would pay.
fn empty_vault() -> &'static crate::secrets::Vault {
    static EMPTY: OnceLock<crate::secrets::Vault> = OnceLock::new();
    EMPTY.get_or_init(crate::secrets::Vault::new)
}

/// Errors returned by [`Client::extract`].
///
/// Split into five disjoint variants so callers can branch without
/// stringly-typed matching:
///
/// - [`EncodeError`][Self::EncodeError]: serialising the
///   [`ExtractRequest`] to JSON failed. Practically unreachable —
///   `ExtractRequest`'s fields all serialise infallibly — but kept as
///   a typed variant rather than `unwrap()` so the failure surface is
///   explicit.
/// - [`WorkerSpawnFailed`][Self::WorkerSpawnFailed]: the lifecycle
///   manager's `acquire` returned an error (sandbox couldn't spawn,
///   restart-backoff still active, …). The worker never started for
///   this call.
/// - [`WorkerDead`][Self::WorkerDead]: dispatch returned an error
///   variant classified as "worker died" by
///   [`dispatch_indicates_worker_dead`][cd]
///   (Io / Protocol::{EarlyExit, Io, Decode, IdMismatch}).
///   [`Client::extract`] has already notified the handle via
///   [`report_crash`][rc] before returning this.
/// - [`RpcError`][Self::RpcError]: worker is alive and rejected the
///   call. The numeric `code` is wire-stable per the JSON-RPC error
///   table in the [worker README][readme].
/// - [`DecodeError`][Self::DecodeError]: dispatch succeeded but the
///   response did not deserialise into [`ExtractResponse`]. Indicates
///   a worker/client wire-shape drift bug.
///
/// [cd]: crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead
/// [rc]: crate::worker_lifecycle::WorkerHandle::report_crash
/// [readme]: https://github.com/hherb/hhagent/blob/main/workers/gliner-relex/README.md
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("encode error: {0}")]
    EncodeError(String),
    #[error("worker spawn failed: {0}")]
    WorkerSpawnFailed(String),
    #[error("worker dead mid-call: {0}")]
    WorkerDead(String),
    #[error("rpc error code={code}: {message}")]
    RpcError { code: i32, message: String },
    #[error("decode error: {0}")]
    DecodeError(String),
}
