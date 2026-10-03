//! Matrix channel bring-up for the `kastellan` binary entrypoint.
//!
//! Originally extracted verbatim from `async fn main`'s "Channel bus (comms
//! slice #2 — Matrix)" block (Item 9b, to keep `main.rs` under the 500-LOC
//! cap). Since #514 the module has one further job: it no longer decides what
//! to do about a failure, it only *classifies* it. [`attempt`] performs one
//! bring-up and returns a
//! [`BootOutcome`](kastellan_core::channel::boot_supervisor::BootOutcome);
//! [`supervise_matrix_channel`] hands that to a
//! [`ChannelSupervisor`](kastellan_core::channel::boot_supervisor::ChannelSupervisor),
//! which retries with capped backoff until the channel comes up.
//!
//! That inversion is the whole fix. This module used to log
//! `channel not started` and return `None` on any failure, so a blip in the
//! first seconds of daemon startup — a sidecar cgroup refused because the user
//! manager happened to be restarting, an initial sync whose CONNECT failed
//! before the proxy path was usable — left the bot deaf for the life of the
//! process, with every unit `active` and nothing further in the log. It cost
//! 12 hours of missed messages on 2026-08-03.
//!
//! What is deliberately unchanged: the env gate (unset ⇒ the daemon is
//! byte-identical to a Matrix-less build), the per-OS backend selection, the
//! `MatrixEgress` wiring, and the 60-second login timeout that bounds a single
//! attempt.

use std::sync::Arc;

use sqlx::PgPool;
use tracing::info;

use kastellan_core::worker_stderr::AuditLostWriter;
use kastellan_core::channel::boot_supervisor::pg_sink::pg_boot_audit_sink;
use kastellan_core::channel::boot_supervisor::{
    BootOutcome, ChannelSupervisor, ReportingPolicy, StartedChannel,
};
use kastellan_core::channel::ChannelBus;
use kastellan_core::worker_lifecycle::force_route::ForceRoutingConfig;
use kastellan_core::worker_lifecycle::RestartBackoff;
use kastellan_sandbox::{SandboxBackend, SandboxBackends};

/// Bound on a single login attempt. The worker's `matrix.init` blocks until
/// the SDK has logged in and completed a first sync, so without this an
/// unreachable homeserver would hold an attempt open indefinitely and the
/// supervisor would never get to retry.
const MATRIX_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Pure: refuse a homeserver that can never work, *before* spending an attempt
/// on it. `Some(Fatal)` ⇒ do not start and do not retry; `None` ⇒ proceed.
///
/// A `localhost`-NAME homeserver is statically dead once egress is
/// force-routed (#459): the proxy resolves the name to loopback and
/// range-denies every CONNECT, so no number of attempts can succeed. Returning
/// [`BootOutcome::Fatal`] rather than `Retry` is what stops #514's retry loop
/// from becoming the respawn loop that check was added to prevent.
///
/// Split out of [`attempt`] so the classification — the part the fix depends
/// on — is testable without a sandbox, a pool or a homeserver. The predicate
/// itself is `channel::matrix::policy`'s and is tested there.
fn classify_homeserver(homeserver_url: &str, forced: bool) -> Option<BootOutcome> {
    kastellan_core::channel::matrix::forced_localhost_homeserver(homeserver_url, forced)
        .map(|detail| BootOutcome::Fatal(anyhow::anyhow!("{detail}")))
}

/// What the Matrix driver does with its rows at exit: it audits every reply
/// still queued (`driver_exit`), so one stuck at shutdown is a possible loss.
/// A `const` so a test pins it (#802).
const SINK_KIND: crate::audit_sink::SinkKind = crate::audit_sink::SinkKind::AuditsOnExit;

/// Pure: the `(actor, action, payload)` of the row
/// [`reply_undelivered_audit_sink`] writes for `reply`. The actor is the bus's
/// own (`PgChannelEvents::audit`), so both writers' rows read alike.
fn reply_undelivered_row(
    reply: &kastellan_core::channel::UndeliveredReply<'_>,
) -> (&'static str, &'static str, serde_json::Value) {
    ("channel", kastellan_core::channel::actions::REPLY_UNDELIVERED, reply.payload())
}

/// Pure: the `[audit-lost]` report for a `channel.reply_undelivered` row the
/// Matrix sink could not write, naming the conversation (capped by `quoted_id`, neutralised
/// by `format_audit_lost_line`) and reason so it can be matched to the driver's `[worker-refusal]`
/// line for the drop.
fn format_reply_row_lost(conversation: &str, reason: &str, why: &dyn std::fmt::Display) -> String {
    let conversation = crate::audit_sink::quoted_id(conversation);
    format!(
        "channel.reply_undelivered row for a reply to conversation {conversation} \
         ({reason}) not written: {why}. The dropped reply's [worker-refusal] line stands"
    )
}

/// Build the [`ReplyUndeliveredAudit`] closure the Matrix channel's polled
/// driver calls for every reply it drops (#782): given up after its worker
/// kept refusing it, past a full conversation queue, or still queued when the
/// driver exits. Writes the `channel.reply_undelivered` row the bus's
/// `channel.replied` promises for a reply that did not land — channel, peer,
/// reason and the drop's time only ([`reply_undelivered_row`]), never the
/// body.
///
/// The driver calls it from its own std thread, which every conversation and
/// the poll wait on, so the insert is **spawned** through `writer`
/// ([`crate::audit_sink::SinkWriter`], whose module doc gives the cost): a
/// slow or unreachable Postgres must not stall the channel for a pool-acquire
/// timeout per dropped reply. The closure owns `writer`, so the driver holds
/// its lease until it exits, and — once the lease has started (see
/// [`login_outcome`]) — the daemon's shutdown drain waits for it. A
/// row that was not written is reported on the `[audit-lost]` marker
/// ([`format_reply_row_lost`]).
///
/// [`ReplyUndeliveredAudit`]: kastellan_core::channel::polled_driver::ReplyUndeliveredAudit
///
/// `report` is how a lost row is said: [`crate::audit_sink::emit_report`] in
/// the daemon, a recorder in a test (#799).
fn reply_undelivered_audit_sink(
    writer: crate::audit_sink::SinkWriter,
    report: crate::audit_sink::Reporter,
) -> kastellan_core::channel::polled_driver::ReplyUndeliveredAudit {
    Box::new(move |reply| {
        let (actor, action, payload) = reply_undelivered_row(&reply);
        let reason = reply.reason;
        let conversation = reply.conversation.0.clone();
        let label = crate::audit_sink::RowLabel::new(
            "matrix reply to conversation",
            &conversation,
            Some(reason.as_str()),
        );
        // A refused row is reported through the closure, so its `Err` is not
        // needed here.
        let _ = writer.spawn(actor, action, payload, label, move |why| {
            report(
                AuditLostWriter::Matrix,
                &format_reply_row_lost(&conversation, reason.as_str(), &why),
            );
        });
    })
}

/// The Matrix sink as `attempt` builds it, on `ledger`: the reply-undelivered
/// hook, and its lease's [`Starting`] token — a lease that counts only once
/// [`login_outcome`] says the worker is up (#802). A function, so a test can
/// see the lease `attempt` takes.
///
/// [`Starting`]: crate::audit_sink::Starting
fn reply_sink(
    ledger: &'static crate::audit_sink::Ledger,
    pool: PgPool,
    handle: tokio::runtime::Handle,
    report: crate::audit_sink::Reporter,
) -> (
    kastellan_core::channel::polled_driver::ReplyUndeliveredAudit,
    crate::audit_sink::Starting,
) {
    let (writer, starting) =
        crate::audit_sink::SinkWriter::starting_with_ledger(ledger, pool, handle, SINK_KIND);
    (reply_undelivered_audit_sink(writer, report), starting)
}

/// Classify the login's outcome — the worker, or the [`BootOutcome::Retry`]
/// for a failed, panicked or cancelled, or timed-out login — and **start the sink's lease
/// only when the worker is up** (#802). A lease started any earlier would make
/// a login abandoned at its timeout a driver the shutdown waits for and calls
/// a possible loss. Counting from here misses no reply: none can be queued
/// before `attempt` builds the bus.
fn login_outcome<T>(
    outcome: Result<
        Result<anyhow::Result<T>, tokio::task::JoinError>,
        tokio::time::error::Elapsed,
    >,
    sink_started: crate::audit_sink::Starting,
) -> Result<T, BootOutcome> {
    match outcome {
        Ok(Ok(Ok(worker))) => {
            sink_started.started();
            Ok(worker)
        }
        Ok(Ok(Err(e))) => Err(BootOutcome::Retry(e.context("matrix worker spawn/login failed"))),
        Ok(Err(join_err)) => Err(BootOutcome::Retry(anyhow::anyhow!(
            "matrix worker spawn task panicked: {join_err}"
        ))),
        Err(_elapsed) => Err(BootOutcome::Retry(anyhow::anyhow!(
            "matrix worker login timed out ({}s)",
            MATRIX_LOGIN_TIMEOUT.as_secs()
        ))),
    }
}

/// One Matrix bring-up attempt: open the LISTEN/NOTIFY connection, spawn the
/// sandboxed live worker (which restores its persisted session — the one-time
/// initial login is done separately with `kastellan-cli matrix probe`), then
/// run a [`ChannelBus`] over the DB-backed pairing/authorizer + the tasks-queue
/// event/completion seams. Authorization is fail-closed at the bus: only
/// DB-paired peers' messages are enqueued.
///
/// The **cheap, outage-sensitive step goes first** (#517) — see the comment at
/// that call for why the order is load-bearing now that a channel is restarted
/// whenever its pumps die.
///
/// Every failure is classified, never swallowed:
///
/// * unset homeserver env ⇒ [`BootOutcome::NotConfigured`] (silent);
/// * a statically-dead homeserver ⇒ [`BootOutcome::Fatal`];
/// * spawn/login failure, a panicked spawn task, the login timeout, and a
///   `PgCompletedTasks::connect` failure ⇒ [`BootOutcome::Retry`]. All four
///   are conditions a later attempt can plausibly find resolved — the observed
///   #514 trigger was one of them.
///
/// Takes owned parameters because the supervisor calls it once per attempt
/// from a `'static` task.
///
/// * `pool` — daemon-scoped runtime pool (cloned into the authorizer, pairing
///   service, events, and completion seams).
/// * `sandboxes` — the per-OS backend bundle; selects the worker backend
///   (Firecracker VM when `KASTELLAN_MATRIX_USE_MICROVM=1` on Linux, else the
///   host jail) and the sidecar backend (always the host bwrap/Seatbelt — the
///   5c invariant: the egress proxy needs a real network route).
/// * `force_routing` — the resolved egress force-routing config; `Some` ⇒ each
///   (re)spawn gets a 1:1 transparent-tunnel sidecar via `MatrixEgress`.
/// * `outbox` — the core-initiated-outbound registry (#564 slice 2 task 8);
///   this bus registers its outbound queue into it and hands it a
///   `PgAskResolver` so a raised ask can reach this channel and an answer on
///   it can resolve back to the task.
async fn attempt(
    pool: PgPool,
    sandboxes: SandboxBackends,
    force_routing: Option<Arc<ForceRoutingConfig>>,
    outbox: Arc<kastellan_core::channel::outbox::ChannelOutbox>,
) -> BootOutcome {
    let Some(spawn_cfg) = kastellan_core::channel::matrix::daemon_spawn_config_from_env(
        std::env::current_exe().ok().as_deref().and_then(|p| p.parent()),
    ) else {
        return BootOutcome::NotConfigured;
    };

    // VM mode counts as always-forced: the Firecracker plan refuses to boot a
    // Net::Allowlist worker without the egress proxy.
    #[cfg(target_os = "linux")]
    let vm_mode = spawn_cfg.use_microvm;
    #[cfg(not(target_os = "linux"))]
    let vm_mode = false;
    if let Some(fatal) =
        classify_homeserver(&spawn_cfg.homeserver_url, force_routing.is_some() || vm_mode)
    {
        return fatal;
    }

    // Worker backend: Firecracker VM when the operator opted in
    // (KASTELLAN_MATRIX_USE_MICROVM=1, Linux); else the host jail. The SIDECAR
    // backend always stays the host bwrap/Seatbelt (5c invariant — the egress
    // proxy needs a real network route; a VM here would boot a proxy with none).
    #[cfg(target_os = "linux")]
    let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.bwrap);
    #[cfg(target_os = "linux")]
    let backend: Arc<dyn SandboxBackend> = if spawn_cfg.use_microvm {
        Arc::clone(&sandboxes.firecracker)
    } else {
        Arc::clone(&sandboxes.bwrap)
    };
    #[cfg(target_os = "macos")]
    let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);
    #[cfg(target_os = "macos")]
    let backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);

    let egress = force_routing.as_ref().map(|fr| kastellan_core::channel::matrix::MatrixEgress {
        sidecar_backend: Arc::clone(&sidecar_backend),
        routing: Arc::clone(fr),
    });

    // LISTEN/NOTIFY first, BEFORE the worker (#517). Since a channel is now
    // restarted whenever its pumps die, and the reachable cause of that is a
    // sustained Postgres outage (sqlx reconnects transparently, so an `Err`
    // from the listener means the *reconnect* failed), this attempt runs
    // repeatedly during exactly the outage that makes this step fail. In the
    // other order every one of those retries would spawn a sandboxed worker,
    // sit through a login and an initial sync, and only then fail on the cheap
    // step and tear it all down again. Costs one pool connection held across
    // the login, which the 60 s timeout already bounds.
    let completed = match kastellan_core::channel::bus::PgCompletedTasks::connect(pool.clone()).await
    {
        Ok(completed) => completed,
        Err(e) => {
            return BootOutcome::Retry(
                e.context("matrix: PgCompletedTasks::connect (LISTEN/NOTIFY) failed"),
            )
        }
    };

    // The worker's login is blocking (matrix.init waits for the SDK's login +
    // first sync), so run it on a blocking thread under a bounded timeout: it
    // doesn't block an async worker thread, and an unreachable homeserver
    // yields a Retry instead of holding the attempt open. On timeout the
    // blocking task is left to drain against the SDK's own HTTP timeouts (a
    // blocking task cannot be force-cancelled).
    //
    // The audit sink's lease starts as *starting* (#802): a login abandoned at
    // the timeout keeps its blocking task, and the writer in it, running —
    // and a driver that never came up has no replies to audit, so the
    // shutdown drain must neither wait for it nor call it a loss. It counts
    // once `login_outcome` sees the worker up.
    let (audit_undelivered, sink_started) = reply_sink(
        crate::audit_sink::daemon_ledger(),
        pool.clone(),
        tokio::runtime::Handle::current(),
        crate::audit_sink::emit_report,
    );
    let audit_undelivered = Some(audit_undelivered);
    let spawn = tokio::task::spawn_blocking(move || {
        kastellan_core::channel::matrix::spawn_matrix_worker(
            backend,
            kastellan_core::channel::ChannelId("matrix".to_string()),
            &spawn_cfg,
            egress,
            audit_undelivered,
        )
    });
    let worker =
        match login_outcome(tokio::time::timeout(MATRIX_LOGIN_TIMEOUT, spawn).await, sink_started) {
            Ok(worker) => worker,
            Err(outcome) => return outcome,
        };

    info!(identity = %worker.identity, "matrix worker logged in; starting channel bus");
    let authorizer = Arc::new(kastellan_core::channel::auth::DbPeerAuthorizer::new(pool.clone()));
    let pairing = Arc::new(kastellan_core::channel::pairing::DbPairingService::new(pool.clone()));
    let events = Arc::new(kastellan_core::channel::bus::PgChannelEvents::new(pool.clone()));
    let asks = Arc::new(kastellan_core::channel::bus::AskWiring {
        outbox,
        resolver: Arc::new(kastellan_core::channel::bus::PgAskResolver::new(pool.clone())),
    });
    BootOutcome::Started(StartedChannel::from_bus(ChannelBus::spawn(
        vec![Box::new(worker.channel)],
        authorizer,
        Some(pairing),
        events,
        Box::new(completed),
        Some(asks),
    )))
}

/// Supervise the Matrix channel: retry [`attempt`] with capped backoff until
/// it comes up — forever, unless the channel is unconfigured or statically
/// dead.
///
/// Returns immediately. The first attempt runs inside the supervisor task, so
/// a hung homeserver no longer delays daemon startup at all; previously the
/// 60-second login timeout was spent inline before `main` reached the email
/// channel or the shutdown wait.
///
/// The returned handle must be `shutdown()`-ed by `main`: it stops the retry
/// loop and, if the channel did come up, the bus with it.
pub(crate) fn supervise_matrix_channel(
    pool: &PgPool,
    sandboxes: &SandboxBackends,
    force_routing: &Option<Arc<ForceRoutingConfig>>,
    outbox: Arc<kastellan_core::channel::outbox::ChannelOutbox>,
) -> ChannelSupervisor {
    let pool = pool.clone();
    let sandboxes = sandboxes.clone();
    let force_routing = force_routing.clone();
    let audit = pg_boot_audit_sink(pool.clone(), "matrix");
    ChannelSupervisor::spawn(
        "matrix",
        RestartBackoff::default(),
        ReportingPolicy::default(),
        Some(audit),
        move || attempt(pool.clone(), sandboxes.clone(), force_routing.clone(), outbox.clone()),
    )
}

#[cfg(test)]
#[path = "matrix_boot_tests.rs"]
mod tests;
