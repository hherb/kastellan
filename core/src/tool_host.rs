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

use kastellan_protocol::client::{Client, ClientError};
use kastellan_sandbox::{SandboxBackend, SandboxError, SandboxPolicy};

mod audit_sink;
pub use audit_sink::{AuditSink, PgAuditSink};

mod egress_provision;

mod injection_placeholder;
pub use injection_placeholder::{injection_blocked_placeholder, WITHHELD_NOTE};

mod post_process;

mod secret_scrub;

mod lockdown_env;
pub use lockdown_env::{derive_lockdown_env, ENV_CPU_MS, ENV_LANDLOCK_PROFILE, ENV_LANDLOCK_RO, ENV_LANDLOCK_RW, ENV_SECCOMP_PROFILE};

mod scratch;
pub use scratch::{prepare_ephemeral_scratch, EphemeralScratch, ENV_WORKER_SCRATCH};

mod spawn_invocation;
pub use spawn_invocation::build_program_and_args;

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

    /// Egress slice #3b (#268). Dispatch-time leak-scanner provisioning failed
    /// for a secret-bearing force-routed net worker. Fail-CLOSED: the worker is
    /// never called, so a secret can never reach a net worker the scanner
    /// cannot watch. The fail-closed audit row was already emitted before this
    /// error. Scheduler treats it like POLICY_DENIED (fail fast, no retry).
    #[error("tool_host: egress leak-scanner provisioning failed: {0}")]
    EgressProvisionFailed(String),
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

    // ── Egress slice #3b (#268): dispatch-time leak-scanner provisioning. ──
    //
    // If this worker has an egress sidecar (a force-routed net worker) and the
    // call carries scannable secret refs, write each secret's value-fingerprint
    // into the sidecar's `secret_hashes.json` BEFORE `worker.call` triggers any
    // egress, so the proxy's per-connection scanner can catch exfiltration.
    // `compute_provision` runs synchronously, releasing the `worker.egress`
    // borrow before `worker.call`; `emit_provision` writes the audit rows and,
    // on a write failure, returns Err — fail CLOSED (D1): a secret never reaches
    // a net worker the scanner cannot watch. No-op for non-net workers
    // (`egress == None`) and for calls with no scannable secrets.
    let provision =
        egress_provision::compute_provision(worker.egress.as_ref(), &req_for_audit, vault);
    egress_provision::emit_provision(sink, tool, provision).await?;

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

    // ── Post-`worker.call` half (scrub + injection screen + audit-emission
    //    arms) lives in `post_process` (Item 9b prod-split). `req_for_audit` is
    //    the pre-substitution snapshot (issue #147); `elapsed_ms` is measured
    //    here, right after the call, so the audit rows carry the true latency.
    post_process::finalize(
        sink,
        vault,
        tool,
        method,
        &req_for_audit,
        &redemption_events,
        call_result,
        elapsed_ms,
    )
    .await
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
    // (browser-driver) is not. The shared drainer reads to EOF (keeping the pipe
    // empty) and surfaces each chunk at `debug`; the thread self-terminates when
    // the worker exits (stderr closes). See `worker_stderr` (the Matrix channel
    // worker uses the tail-retaining variant for death reports — #348).
    if let Some(stderr) = child.stderr.take() {
        crate::worker_stderr::spawn_drain(pid, stderr);
    }
    let client = Client::from_child(child)?;
    // Build the re-armable watchdog in the DISARMED state. It is armed only for
    // the duration of each `SupervisedWorker::call` (see that method), so a warm
    // worker sitting idle in the IdleTimeout slot is never under a kill timer.
    let watchdog = spec.wall_clock_ms.map(|ms| watchdog::Watchdog::new(pid, ms));
    Ok(SupervisedWorker {
        client,
        watchdog,
        egress: None,
        broker: None,
        scratch: None,
    })
}

/// Owning handle to a spawned worker. Wraps the JSON-RPC [`Client`] and a
/// [`watchdog::Watchdog`] (when `wall_clock_ms` was set on the spec).
///
/// Field drop order matters: `client` is declared first so it drops first,
/// closing stdio pipes. `watchdog` drops second, shutting down the watchdog
/// thread (it never fires on drop). `egress` drops third: for a force-routed
/// net worker (slice #2) it kills the egress-proxy sidecar *after* the
/// worker's pipes have closed, so the worker stops talking to the proxy
/// before the proxy dies. Plain (`Net::Deny` / legacy) workers leave it `None`.
/// `broker` drops fourth: same reasoning as `egress` — the worker stops
/// talking to its broker sidecar (over the bound UDS) before the broker
/// dies. `None` for every worker except web-research in broker mode.
/// `scratch` drops last: for a macOS per-spawn scratch worker its RAII guard
/// removes the host dir after both the worker's pipes and the egress sidecar
/// are gone. `None` on Linux and for any non-scratch worker.
pub struct SupervisedWorker {
    client: Client,
    /// Re-armable wall-clock watchdog (when `wall_clock_ms` was set on the
    /// spec). Armed for the span of each [`Self::call`] and disarmed in
    /// between, so a reused (warm) worker is never killed while idle. Dropping
    /// it shuts the watchdog thread down (no kill).
    watchdog: Option<watchdog::Watchdog>,
    /// `Some` only for a force-routed net worker; set by
    /// `crate::egress::net_worker::spawn_net_worker`. Additive — its `Drop`
    /// tears the coupled egress-proxy sidecar down 1:1 with this worker.
    pub(crate) egress: Option<crate::egress::net_worker::EgressSidecar>,
    /// `Some` only for a worker spawned in broker mode; set by
    /// `crate::worker_lifecycle::force_route::spawn_worker_with_optional_broker`.
    /// Additive — its `Drop` kills the coupled broker sidecar (and removes its
    /// scratch) 1:1 with this worker. Independent of `egress` (a worker may carry
    /// both).
    pub(crate) broker: Option<crate::broker::BrokerSidecar>,
    /// `Some` only for a worker that requested per-spawn scratch
    /// (`ToolEntry.ephemeral_scratch`, macOS). Set post-spawn via
    /// [`SupervisedWorker::with_scratch`], mirroring how `egress` is attached.
    /// Its `Drop` removes the host scratch dir after the worker's pipes close.
    /// `None` for every worker on Linux and every non-scratch worker.
    pub(crate) scratch: Option<scratch::EphemeralScratch>,
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
        // Arm the wall-clock watchdog for exactly this in-flight call. The RAII
        // `ArmGuard` disarms it synchronously when `call` returns (success or
        // error), before the worker can be returned to the warm-cache slot — so
        // there is no deadline ticking during idle gaps and no Drop-ordering
        // race against the slot handoff. The watchdog bounds the JSON-RPC call
        // window, not the spawn/boot window (boot is bounded by the spawn path).
        let _arm = self.watchdog.as_ref().map(watchdog::Watchdog::arm_scope);
        self.client.call(&cmd.method, cmd.params)
    }

    /// Close stdin (signals EOF to the worker), wait for it to exit, and
    /// shut down the watchdog thread. Returns the worker's exit status.
    pub fn close(self) -> std::io::Result<std::process::ExitStatus> {
        // Destructure to move `client` out by value (consumed by `close`)
        // while binding the remaining guards so we can drop them in a
        // controlled order below. Safe because [`SupervisedWorker`] has no
        // [`Drop`] impl, so partial moves are allowed.
        let SupervisedWorker {
            client,
            watchdog,
            egress,
            broker,
            scratch,
        } = self;
        // `client.close()` runs first (waits for the worker to exit, closing
        // its pipes). The remaining guards are then dropped *explicitly* in the
        // same order as the struct's field-drop order — watchdog, egress,
        // scratch — so `close()` matches the implicit `Drop` path exactly: the
        // egress sidecar is killed after the worker has stopped, and the host
        // scratch dir is removed last. (Pattern bindings would otherwise drop
        // in reverse-declaration order, putting scratch before egress; harmless
        // here since the worker is already gone, but we make it explicit so the
        // documented ordering can't silently drift.)
        let status = client.close();
        drop(watchdog);
        drop(egress);
        drop(broker);
        drop(scratch);
        status
    }

    /// Forcefully kill the worker without waiting for graceful shutdown.
    /// The watchdog is shut down by the [`Drop`] of [`Self`] (or
    /// [`Self::close`]).
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.client.kill()
    }

    /// Attach an optional per-spawn scratch guard, returning `self` for
    /// chaining. The guard's `Drop` cleans the host dir when this worker is
    /// dropped. `None` is a no-op (Linux / non-scratch workers).
    pub fn with_scratch(mut self, scratch: Option<scratch::EphemeralScratch>) -> Self {
        self.scratch = scratch;
        self
    }
}
