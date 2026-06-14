//! tool_host: spawn sandboxed worker processes and talk to them over the
//! JSON-RPC stdio protocol from `kastellan_protocol`.
//!
//! The agent core is the only thing that ever spawns a worker. Spawning goes
//! through the configured [`SandboxBackend`] so workers cannot run unjailed
//! by accident — there is intentionally no "spawn unsandboxed" escape hatch.
//!
//! Phase 0 covers single-shot spawn-and-talk usage. Long-lived workers,
//! restart-on-crash supervision, and per-worker UDS multiplexing are
//! follow-on work.

use std::time::Instant;

use sha2::{Digest, Sha256};

use kastellan_protocol::client::{Client, ClientError};
use kastellan_sandbox::{SandboxBackend, SandboxError, SandboxPolicy};

mod audit_sink;
pub use audit_sink::{AuditSink, PgAuditSink};

mod lockdown_env;
pub use lockdown_env::{derive_lockdown_env, ENV_CPU_MS, ENV_LANDLOCK_RO, ENV_LANDLOCK_RW, ENV_SECCOMP_PROFILE};

mod watchdog;

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]   // NEW — Item 31. First variant addition since Option M (2026-05-10).
pub enum ToolHostError {
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] ClientError),

    /// NEW — Item 31. Substitution failed before the worker call.
    /// The dispatch's audit-row side-effect
    /// (`policy / secret.redemption_failed`) happened before this
    /// error was returned. Scheduler should treat this like
    /// POLICY_DENIED — task step fails fast, no retry budget burned.
    #[error("tool_host: secret redemption failed: {0}")]
    SecretRedemptionFailed(#[from] crate::secrets::SubstituteError),

}

/// A sealed JSON-RPC request shape. The fields and constructor are
/// **module-private** (no visibility modifier — narrower than
/// `pub(crate)`), and [`SupervisedWorker::call`] is module-private
/// too. Together they pin the dispatcher chokepoint invariant from
/// both sides:
///
/// * **Out-of-crate callers** cannot reach the constructor (or the
///   fields, or `.call`) because none of them are exported. The
///   `compile_fail` doctest below is the regression pin for that
///   side.
/// * **Sibling modules inside `kastellan_core`** (e.g.
///   `scheduler::tool_dispatch`, or any future module) cannot reach
///   the constructor either — module-private items are visible only
///   from the declaring module and its descendants, not from sibling
///   modules. The `tool_host` module's own child modules
///   (`audit_sink`, `lockdown_env`, `watchdog`, `tests`) *are*
///   descendants and so technically could reach it, but none does;
///   adding a new caller therefore requires editing the `tool_host`
///   module (`tool_host.rs` or a file under `tool_host/`), which is
///   the explicit "reviewable opt-out" called for by [issue #16].
///
/// The build itself is the in-crate regression test: if a future
/// sibling module attempted `WorkerCommand::new(...)` or
/// `worker.call(...)`, the workspace would fail to compile with a
/// "function is private" error.
///
/// See `docs/architecture.md` and HANDOVER's Option M notes for the
/// chokepoint contract; the issue-#16 fix (2026-05-13) narrowed the
/// originally-`pub(crate)` constructor to module-private so the seal
/// holds against sibling modules too.
///
/// [issue #16]: https://github.com/hherb/kastellan/issues/16
///
/// ```compile_fail
/// // Each doctest is compiled as a separate crate that depends on
/// // `kastellan_core`. `WorkerCommand` is `pub` so the `use` line
/// // compiles, but the constructor is module-private (no `pub`
/// // keyword at all) so the `::new` line is unreachable from any
/// // crate other than `kastellan_core` itself — and even within
/// // `kastellan_core` it is reachable only from `tool_host` and its
/// // descendants. Touch either fact and the test trips.
/// use kastellan_core::tool_host::WorkerCommand;
/// let _via_new = WorkerCommand::new("echo", serde_json::Value::Null);
/// ```
pub struct WorkerCommand {
    method: String,
    params: serde_json::Value,
}

impl WorkerCommand {
    /// Build a sealed command. **Module-private** (no visibility
    /// modifier) so only `tool_host`'s own functions can construct
    /// one — the canonical (and currently only) caller is
    /// [`dispatch`]. Sibling modules inside `kastellan_core` cannot
    /// reach this constructor at compile time; adding a new caller
    /// requires editing the `tool_host` module (`tool_host.rs` or a
    /// file under `tool_host/`), which is the reviewable opt-out for
    /// the dispatcher chokepoint.
    fn new(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

/// Single chokepoint for tool invocations: make one JSON-RPC call
/// against `worker` and write a row into `audit_log` describing what
/// happened.
///
/// Every Phase-0+ tool call SHOULD go through `dispatch` — see the
/// "dispatcher chokepoint" invariant in `docs/architecture.md` and
/// HANDOVER's Option I notes. The shape mirrors IronClaw's
/// `ToolDispatcher::dispatch()` and is the place where Phase-1 policy
/// checks, rate limits, and per-tool budgets will hook in.
///
/// ## Audit-log shape
///
/// **One to many rows per call.** The standard happy path writes one
/// `'tool:<name>'` row. Two additional row kinds are possible:
///
/// * `policy / secret.redeemed` — emitted once per `secret://<8-hex>`
///   ref that was substituted from `params` (Item 31). Carries
///   `{tool, method, ref_hash, ms}`; never the plaintext.
/// * `policy / injection.blocked` — emitted when the prompt-injection
///   guard blocks a worker result (Item 30). Carries SHA-256 + length
///   + score + class codes; never the raw scanned body.
///
/// On a substitution miss the chokepoint writes exactly one row,
/// `policy / secret.redemption_failed`, and returns
/// `ToolHostError::SecretRedemptionFailed`. The tool row is NOT
/// written (the worker was not called).
///
/// * `actor`  = `"tool:<tool>"` — caller-supplied logical name (e.g.
///   `"shell-exec"`, `"web-fetch"`). The worker binary path may be
///   long and host-specific; the logical name is what operators
///   filter on.
/// * `action` = `<method>` — the JSON-RPC method name (`"echo"`,
///   `"call"`, etc.).
/// * `payload` = `{"req": <params>, "result": <ok value>, "ms": <duration>}`
///   on success, or
///   `{"req": <params>, "err": "<error string>", "ms": <duration>}`
///   on failure. Payloads larger than 4 KiB are replaced inside
///   [`kastellan_db::audit::insert`] with a SHA-256 envelope.
///
/// **Secret refs are redacted in `payload.req` (issue #147).** The
/// `req` snapshot is taken BEFORE secret-ref substitution, so the
/// tool row records the opaque `secret://<8-hex>` refs exactly as the
/// planner issued them — never the redeemed plaintext. The worker
/// still receives the substituted plaintext (it is the authorised
/// consumer); only the audit snapshot is pre-substitution. The
/// privacy invariant (no redeemed plaintext in `audit_log`) therefore
/// now covers the tool row's `req` as well as every `actor='policy'`
/// row. Note the worker's *output* may still echo a secret it was
/// legitimately given — that lands in `payload.result`, which is the
/// worker's own response, not the request.
///
/// ## Why the audit insert is *best-effort*
///
/// If the worker call succeeded but the audit insert fails (cluster
/// down, pool exhausted, transient error), the caller MUST still
/// receive the worker's result — silently swapping a successful
/// tool call for an error because we couldn't log it would be a much
/// worse failure mode than missing one audit row. The audit error is
/// logged via [`tracing::error`] so an operator notices the gap, but
/// it does not propagate. Conversely, if the worker call failed,
/// the worker's error is what the caller gets — the audit insert is
/// best-effort there too.
///
/// In Phase 1, when the audit log gains stronger durability
/// guarantees (e.g. every dispatcher call is required to land an
/// audit row before its result is returned to the scheduler), this
/// behaviour will tighten — but the right tightening depends on what
/// failure modes the scheduler can actually tolerate, which we don't
/// know yet.
///
/// ## Why `block_in_place` around the sync `worker.call`
///
/// `Client::call` from `kastellan-protocol` is synchronous (it uses
/// `std::io::Read`/`Write` over the worker's piped stdio).
/// [`tokio::task::block_in_place`] runs that on the current
/// async-runtime worker thread without handing off — which means we
/// can keep the existing `&mut SupervisedWorker` handle and don't
/// need an Arc<Mutex<>> dance.
///
/// Requirement: the calling tokio runtime must be multi-threaded
/// (`Builder::new_multi_thread()` or `#[tokio::main]`'s default).
/// `current_thread` runtimes panic from `block_in_place`. Tests that
/// exercise `dispatch` are responsible for choosing the right
/// runtime; the daemon's `#[tokio::main]` already does.
pub async fn dispatch(
    pool: &sqlx::PgPool,
    vault: &crate::secrets::Vault,        // NEW — Item 31
    worker: &mut SupervisedWorker,
    tool: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, ToolHostError> {
    // Production always routes through PgAuditSink. The sink seam
    // ([`dispatch_with_sink`]) exists for fault-injection tests (issue #148),
    // not as a production audit-policy knob — see the `audit_sink` module docs.
    dispatch_with_sink(&PgAuditSink::new(pool), vault, worker, tool, method, params).await
}

/// Fault-injectable core of [`dispatch`]. Behaviourally identical, but audit
/// rows are written through `sink` rather than a hard-wired pool, so a test
/// can force an individual `audit_log` insert to fail and assert the
/// best-effort *swallow-and-continue* paths around secret redemption (issue
/// #148). **Production code calls [`dispatch`]**, which pins `sink` to a real
/// [`PgAuditSink`]; this entry point is `pub` only because the fault-injection
/// tests live in the separate integration-test crate.
pub async fn dispatch_with_sink(
    sink: &dyn AuditSink,
    vault: &crate::secrets::Vault,
    worker: &mut SupervisedWorker,
    tool: &str,
    method: &str,
    mut params: serde_json::Value,        // NOTE: `mut` — substitution rewrites refs in place
) -> Result<serde_json::Value, ToolHostError> {
    let started = Instant::now();

    // Snapshot the request for the tool audit row BEFORE secret-ref
    // substitution (issue #147). At this point `params` still carries
    // the opaque `secret://<8-hex>` refs exactly as the planner issued
    // them, so the redeemed plaintext never reaches the `audit_log`.
    // The worker still receives the real plaintext via the mutated
    // `params` passed to `WorkerCommand::new` below — only the audit
    // snapshot is taken pre-substitution. This faithfully records the
    // request-as-issued and removes the audit-log secret-recovery path
    // that slice 1 left open (the privacy invariant is no longer scoped
    // to `actor='policy'` rows only — the tool row's `req` is redacted
    // too). On the fail-closed error path below this snapshot is simply
    // unused (no tool row is written).
    let req_for_audit = params.clone();

    // ── Secret-ref substitution (Item 31, slice 1). ──
    //
    // Walk `params` and substitute every exact-match `secret://<8-hex>`
    // string with the redeemed plaintext. Fail-closed: any miss or
    // UTF-8 failure stops the dispatch before `worker.call` and
    // emits `policy / secret.redemption_failed`; the worker is not
    // called and no `tool:<n>` row is written.
    //
    // Redemption events are saved for emission AFTER `worker.call`
    // (so the elapsed_ms field is the dispatch elapsed time, not the
    // pre-call elapsed time).
    let redemption_events = match crate::secrets::substitute_refs_in_params(&mut params, vault) {
        Ok(events) => events,
        Err(e) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let (ref_hash, reason) = match &e {
                crate::secrets::SubstituteError::MissingRef { ref_hash, reason } => {
                    (ref_hash.clone(), reason.as_str())
                }
                crate::secrets::SubstituteError::PlaintextNotUtf8 { ref_hash } => {
                    (ref_hash.clone(), "plaintext_not_utf8")
                }
            };
            let payload = serde_json::json!({
                "tool":     tool,
                "method":   method,
                "ref_hash": ref_hash,
                "reason":   reason,
                "ms":       elapsed_ms,
            });
            // Best-effort audit insert. The dispatch is already going to
            // fail-closed below with `SecretRedemptionFailed`; if the
            // audit row insert ALSO fails, masking the original error
            // (which the scheduler maps to `POLICY_DENIED`) with a
            // database error would be strictly worse than losing the
            // forensic row. We log via `tracing` so the failure isn't
            // silent. Asymmetry with materialize-time audit (hard-fail
            // per spec §5.4) is intentional: materialize must not yield
            // a ref the audit log doesn't know about, but this path
            // never yielded a ref at all.
            if let Err(audit_err) =
                sink.insert("policy", "secret.redemption_failed", payload).await
            {
                tracing::error!(
                    tool = %tool,
                    method = %method,
                    error = %audit_err,
                    "secret.redemption_failed audit insert failed"
                );
            }
            return Err(ToolHostError::SecretRedemptionFailed(e));
        }
    };

    // `req_for_audit` was snapshotted above, pre-substitution (issue
    // #147), so the tool row's `payload.req` carries the opaque refs,
    // not the redeemed plaintext.
    //
    // Sealed command: `WorkerCommand` is the only argument shape
    // `SupervisedWorker::call` accepts, and both its constructor and
    // `call` itself are module-private (see issue #16).
    let cmd = WorkerCommand::new(method, params);
    let call_result = tokio::task::block_in_place(|| worker.call(cmd));
    let elapsed_ms = started.elapsed().as_millis() as u64;

    // Prompt-injection screen on successful results. Errors are not
    // text-channel content (the planner sees them as failure codes,
    // not as text), so they can't carry injection — skip.
    let (final_result, blocked_meta) = match call_result {
        Ok(v) => {
            let (body, truncated) = crate::cassandra::injection_guard::extract_scannable_text(
                &v,
                crate::cassandra::injection_guard::SCAN_BYTE_CAP,
            );
            // Per-tool sensitivity (issue #142): doc-fetching net workers
            // use the Relaxed profile so quoted chat-template tokens in
            // fetched documentation do not auto-Block; every other worker
            // (incl. shell-exec and any unknown) stays Strict, fail-closed.
            let verdict = crate::cassandra::injection_guard::screen_with_profile(
                &body,
                crate::cassandra::injection_guard::GuardProfile::for_tool(tool),
            );
            match verdict.decision {
                crate::cassandra::injection_guard::InjectionDecision::Allow => {
                    (Ok(v), None)
                }
                crate::cassandra::injection_guard::InjectionDecision::Block => {
                    let placeholder = serde_json::json!({
                        "injection_blocked": true,
                        "score":             verdict.score,
                        "reason_codes":      verdict.reason_codes,
                    });
                    (Ok(placeholder), Some((verdict, body, truncated)))
                }
            }
        }
        Err(e) => (Err(e), None),
    };

    // Tool audit row (existing) — now carrying the placeholder on Block.
    let actor = format!("tool:{tool}");
    let audit_payload = match &final_result {
        Ok(v) => serde_json::json!({
            "req":    req_for_audit,
            "result": v,
            "ms":     elapsed_ms,
        }),
        Err(e) => serde_json::json!({
            "req": req_for_audit,
            "err": e.to_string(),
            "ms":  elapsed_ms,
        }),
    };
    // ── Emit `secret.redeemed` audit rows (one per substitution). ──
    //
    // Best-effort: a transient audit insert failure is logged but
    // does not propagate. The plaintext is already substituted into
    // params and the worker already ran; turning the dispatch into
    // an error because the audit log was unreachable would be worse
    // than missing rows. (Materialize-time audit IS hard-fail; see
    // Vault::materialize and spec §5.4 for the asymmetry rationale.)
    for event in &redemption_events {
        let payload = serde_json::json!({
            "tool":     tool,
            "method":   method,
            "ref_hash": event.ref_hash,
            "ms":       elapsed_ms,
        });
        if let Err(e) = sink.insert("policy", "secret.redeemed", payload).await {
            tracing::error!(
                tool = %tool,
                ref_hash = %event.ref_hash,
                error = %e,
                "secret.redeemed audit insert failed"
            );
        }
    }

    if let Err(audit_err) = sink.insert(&actor, method, audit_payload).await {
        tracing::error!(
            tool = %tool,
            method = %method,
            error = %audit_err,
            "audit_log INSERT failed; tool result still propagated"
        );
    }

    // Forensic policy row on Block. SHA-256 of the body that was
    // scanned (which may have been truncated at SCAN_BYTE_CAP).
    // The raw body is never written to any audit column — only the
    // hash, byte length, score, and class codes are stored.
    if let Some((verdict, body, truncated)) = blocked_meta {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let body_sha256 = format!("{:x}", hasher.finalize());
        let body_byte_len = body.len();
        let policy_payload = serde_json::json!({
            "tool":                    tool,
            "method":                  method,
            "score":                   verdict.score,
            "decision":                "block",
            "reason_codes":            verdict.reason_codes,
            "body_sha256":             body_sha256,
            "body_byte_len":           body_byte_len,
            "body_truncated_at_64kib": truncated,
        });
        if let Err(e) = sink.insert("policy", "injection.blocked", policy_payload).await {
            tracing::error!(
                tool = %tool,
                method = %method,
                error = %e,
                "policy audit insert failed"
            );
        }
    }

    Ok(final_result?)
}

/// What to launch and how to jail it.
pub struct WorkerSpec<'a> {
    pub policy: &'a SandboxPolicy,
    /// Absolute path of the worker binary, as visible *inside* the jail.
    /// Caller must add the binary's host path (or its parent dir) to
    /// `policy.fs_read` so bwrap can mount it.
    pub program: &'a str,
    pub args: &'a [&'a str],
    /// Optional wall-clock budget (milliseconds) for the *entire* worker
    /// process lifetime. If set, [`spawn_worker`] starts a watchdog thread
    /// that sends SIGKILL once the deadline passes — unless the worker
    /// already exited or the returned [`SupervisedWorker`] was dropped /
    /// closed first (which cancels the watchdog).
    ///
    /// `None` disables the watchdog entirely; the worker is bounded only
    /// by external means (caller closing it, sandbox CPU/mem caps, etc.).
    pub wall_clock_ms: Option<u64>,
}

/// Spawn the worker under `backend` and return a [`SupervisedWorker`].
///
/// Before spawning, [`derive_lockdown_env`] augments the policy with the
/// `KASTELLAN_LANDLOCK_RW` + `KASTELLAN_SECCOMP_PROFILE` env entries that
/// `kastellan-worker-prelude` reads at worker start-up. This is the
/// chokepoint for the worker-side defence-in-depth layer: callers cannot
/// accidentally skip it because tool_host always derives the env, and
/// the worker installs the filters from inside its own process.
///
/// If `spec.wall_clock_ms` is `Some`, a watchdog thread is started that
/// SIGKILLs the worker once the budget elapses. The watchdog is cancelled
/// when the returned [`SupervisedWorker`] is dropped (or closed), so
/// well-behaved callers never see spurious kills.
pub fn spawn_worker<B>(
    backend: &B,
    spec: &WorkerSpec<'_>,
) -> Result<SupervisedWorker, ToolHostError>
where
    B: SandboxBackend + ?Sized,
{
    let derived = derive_lockdown_env(spec.policy);
    let mut child = backend.spawn_under_policy(&derived, spec.program, spec.args)?;
    let pid = child.id();
    // Drain the worker's piped stderr in a detached background thread. The
    // sandbox backends pipe stderr (`Stdio::piped()`), but the JSON-RPC client
    // only reads stdout — so a worker that writes more than the ~64 KiB pipe
    // buffer to stderr would **block on write and deadlock** (then get
    // wall-clock-killed). Most workers are quiet; a headless Chromium
    // (browser-driver) is not. Reading to EOF keeps the pipe empty; the thread
    // self-terminates when the worker exits (stderr closes). Lines surface at
    // debug level so they're available when troubleshooting without being noisy.
    if let Some(stderr) = child.stderr.take() {
        drain_worker_stderr(pid, stderr);
    }
    let client = Client::from_child(child)?;
    let watchdog = spec.wall_clock_ms.map(|ms| watchdog::spawn_watchdog(pid, ms));
    Ok(SupervisedWorker {
        client,
        _watchdog: watchdog,
        egress: None,
    })
}

/// Spawn a detached thread that reads `stderr` to EOF, emitting each chunk at
/// `debug`. Its only hard job is to keep the pipe drained so the worker can't
/// deadlock writing to a full stderr buffer (see [`spawn_worker`]). The thread
/// ends when the worker's stderr closes (process exit), so it needs no join
/// handle and leaks nothing.
///
/// Reads **raw bytes**, not lines: a `BufRead::lines()` loop yields an `Err`
/// on the first invalid-UTF-8 byte and would stop draining — re-opening the
/// very deadlock this guards against if the worker keeps writing (Chromium's
/// stderr is overwhelmingly UTF-8, but "overwhelmingly" is not "always"). Each
/// chunk is logged lossily so non-UTF-8 bytes are surfaced as `�` rather than
/// halting the drain.
fn drain_worker_stderr(pid: u32, stderr: std::process::ChildStderr) {
    use std::io::Read;
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf) {
                Ok(0) => break,                 // EOF — pipe closed (worker exited)
                Ok(n) => tracing::debug!(
                    worker_pid = pid,
                    "worker stderr: {}",
                    String::from_utf8_lossy(&buf[..n]).trim_end()
                ),
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break, // genuine read error — pipe gone, nothing left to drain
            }
        }
    });
}

/// Owning handle to a spawned worker. Wraps the JSON-RPC [`Client`] and a
/// [`watchdog::WatchdogGuard`] (when `wall_clock_ms` was set on the spec).
///
/// Field drop order matters: `client` is declared first so it drops first,
/// closing stdio pipes. `_watchdog` drops second, setting the watchdog's
/// cancel flag. The watchdog thread checks the flag at most every 50 ms
/// and exits without firing SIGKILL — so closing a worker normally never
/// produces a kill on a reused PID. `egress` drops last: for a force-routed
/// net worker (slice #2) it kills the egress-proxy sidecar *after* the
/// worker's pipes have closed, so the worker stops talking to the proxy
/// before the proxy dies. Plain (`Net::Deny` / legacy) workers leave it `None`.
pub struct SupervisedWorker {
    client: Client,
    _watchdog: Option<watchdog::WatchdogGuard>,
    /// `Some` only for a force-routed net worker; set by
    /// `crate::egress::net_worker::spawn_net_worker`. Additive — its `Drop`
    /// tears the coupled egress-proxy sidecar down 1:1 with this worker.
    pub(crate) egress: Option<crate::egress::net_worker::EgressSidecar>,
}

impl SupervisedWorker {
    /// Make one JSON-RPC call against the worker.
    ///
    /// **Module-private** (no visibility modifier — see issue #16 fix
    /// 2026-05-13). Takes a sealed [`WorkerCommand`] so only
    /// [`dispatch`] (the canonical caller, in the same module) can
    /// reach this path. Both out-of-crate code and sibling modules
    /// inside `kastellan_core` can hold a `&mut SupervisedWorker` (as
    /// `core/tests/audit_dispatch_e2e.rs` does, and as
    /// `core::scheduler::tool_dispatch` does), but neither can call
    /// this method directly — they must funnel through `dispatch`,
    /// which writes the audit row. The compile-time chokepoint is
    /// now structural on both sides of the crate boundary.
    fn call(
        &mut self,
        cmd: WorkerCommand,
    ) -> Result<serde_json::Value, ClientError> {
        self.client.call(&cmd.method, cmd.params)
    }

    /// Close stdin (signals EOF to the worker), wait for it to exit, and
    /// cancel the watchdog. Returns the worker's exit status.
    pub fn close(self) -> std::io::Result<std::process::ExitStatus> {
        // Destructure to move `client` out by value (consumed by `close`)
        // while leaving `_watchdog` to drop at end-of-scope, which sets
        // the cancel flag. Safe because [`SupervisedWorker`] has no
        // [`Drop`] impl, so partial moves are allowed.
        let SupervisedWorker {
            client,
            _watchdog: _drop_at_scope_end,
            egress: _drop_egress_at_scope_end,
        } = self;
        // `client.close()` runs first (waits for the worker to exit, closing
        // its pipes); `_drop_egress_at_scope_end` then drops at end of scope,
        // killing the sidecar *after* the worker has stopped.
        client.close()
    }

    /// Forcefully kill the worker without waiting for graceful shutdown.
    /// The watchdog is cancelled by the [`Drop`] of [`Self`] (or
    /// [`Self::close`]).
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.client.kill()
    }
}
