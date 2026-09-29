//! Email channel bring-up for the `kastellan` binary entrypoint.
//!
//! Sibling of `matrix_boot.rs`, deliberately mirroring its structure —
//! backend selection, egress wiring, `ChannelBus::spawn` shape — with two
//! differences forced by the email channel's own design
//! (`docs/superpowers/specs/2026-07-28-email-fallback-channel-design.md`):
//!
//! Since #514 both modules also share their *shape*: [`attempt`] performs one
//! bring-up and classifies the result as a
//! [`BootOutcome`](kastellan_core::channel::boot_supervisor::BootOutcome),
//! and [`supervise_email_channel`] hands that to a
//! [`ChannelSupervisor`](kastellan_core::channel::boot_supervisor::ChannelSupervisor)
//! that retries with capped backoff. What differs is the *classification* —
//! see point 1.
//!
//! 1. **The email CHANNEL refuses to start; the DAEMON does not.**
//!    [`kastellan_core::channel::email::config::EmailConfig::from_env`]
//!    refuses a PARTIAL config (`Err`, not `Ok(None)`) precisely so a missing
//!    `KASTELLAN_EMAIL_AUTHSERV_ID` etc. is loud instead of quietly
//!    rejecting every message and looking like a delivery bug — see that
//!    function's module docs. That `Err` becomes
//!    [`BootOutcome::Fatal`](kastellan_core::channel::boot_supervisor::BootOutcome::Fatal):
//!    the supervisor prints the loud "fix it, then restart the daemon" line
//!    and stops. It does **not** retry, because the process environment
//!    cannot change under a running daemon, so a retry loop there would spin
//!    forever while telling the operator to restart.
//!
//!    A worker *spawn* failure is classified the other way (`Retry`) — it is
//!    a sandbox/egress condition, and the failure that prompted #514 was
//!    exactly that shape.
//!
//!    That is a deliberate correction (final whole-branch review, Important
//!    5). An earlier version propagated both through `?`, aborting daemon
//!    startup. Three reasons that was wrong: (a) **availability inversion** —
//!    this channel exists *because* Matrix has no homeserver failover, so a
//!    typo in the fallback's config must not take the primary channel and the
//!    scheduler down with it; (b) **spec deviation** — design §6 says the
//!    daemon refuses to start *the email channel*, not the daemon; (c) the
//!    fail-closed argument did not actually apply — a half-configured channel
//!    already fails **closed** (a blank authserv-id makes
//!    `gate::trusted_dmarc_pass` reject every message), so there was no
//!    fail-open case the abort protected against. What the abort really
//!    bought was *loudness*, which an `error!` delivers without the
//!    collateral — and it additionally removed the last startup path that
//!    skipped `main.rs`'s graceful shutdown sequence (bus shutdowns →
//!    scheduler → audit mirror → `pool.close()`).
//!
//!    The posture DIFFERENCE from Matrix is real and kept, and #514 sharpened
//!    rather than erased it: `email.init` does no network I/O at all (see
//!    `workers/email-in/src/handler.rs`), so a *config* failure here can only
//!    be a deployment fact and is fatal, where Matrix's equivalent failure
//!    (an unreachable homeserver) is transient and retried. The two modules
//!    now share one retry mechanism and disagree only about which errors feed
//!    it — which is where the disagreement belongs.
//! 2. **No microVM branch.** [`kastellan_core::channel::email::config::EmailConfig`]
//!    has no `use_microvm` field in this slice — the worker always runs on
//!    the host jail backend (bwrap on Linux, Seatbelt on macOS), which is
//!    also the sidecar backend (the same 5c invariant Matrix documents: the
//!    egress proxy needs a real network route; a VM sidecar would have none).
//!
//! The final `PgCompletedTasks::connect` step is classified the same way
//! Matrix classifies it — `Retry`, because a LISTEN/NOTIFY bring-up hiccup on
//! an already-open pool is a generic DB-listener concern, not an
//! email-channel-specific misconfiguration. An **unset**
//! `KASTELLAN_EMAIL_ENDPOINT` stays silent by design: the channel is simply
//! not configured, which is the default and not a problem to report.

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{error, info};

use kastellan_core::channel::boot_supervisor::pg_sink::pg_boot_audit_sink;
use kastellan_core::channel::boot_supervisor::{
    BootOutcome, ChannelSupervisor, ReportingPolicy, StartedChannel,
};
use kastellan_core::channel::polled_driver::AckOnlyAudit;
use kastellan_core::channel::{ChannelBus, ChannelId};
use kastellan_core::worker_lifecycle::force_route::ForceRoutingConfig;
use kastellan_core::worker_lifecycle::RestartBackoff;
use kastellan_sandbox::{SandboxBackend, SandboxBackends};

/// Cap on `reason`'s length before it becomes a durable `audit_log` payload
/// value. `reason` originates from the worker's `skipped[].reason`
/// (`workers/email-in/src/handler.rs::describe_email_error`), which already
/// truncates an upstream (localmail) HTTP error body to 200 chars — but
/// `polled_driver::run`, which hands it to this sink, is DB-free by design
/// and applies no cap of its own. Defence in depth: this sink must not trust
/// a future worker change (or a compromised worker) to keep bounding it
/// before it lands permanently in `audit_log`. Comfortably above the
/// worker's own 200-char cap so today's values pass through untouched.
///
/// Aliased to the supervisor's cap rather than a second `256` typed here: both
/// bound an unbounded, externally-originated string on its way into the same
/// column, so one number and one set of edge cases is the honest arrangement.
const AUDIT_REASON_CAP_CHARS: usize =
    kastellan_core::channel::boot_supervisor::AUDIT_CAUSE_CAP_CHARS;

/// Truncate `reason` to [`AUDIT_REASON_CAP_CHARS`] on a `char` boundary
/// (never mid-UTF-8-codepoint — `reason` may echo arbitrary upstream text).
/// A no-op for anything at or under the cap, which covers every value the
/// worker emits today.
fn cap_reason(reason: &str) -> String {
    kastellan_core::channel::audit_text::cap_chars(reason, AUDIT_REASON_CAP_CHARS)
}

/// Build the [`AckOnlyAudit`] closure `spawn_email_worker` invokes for every
/// message id it acks without that id ever becoming a bus event (localmail's
/// `skipped` list — an unattributable `From`, an unfetchable detail fetch,
/// etc.). Those ids are messages the agent silently never saw, so they must
/// stay traceable in `audit_log` even though the polled driver that acks
/// them is DB-free by design.
///
/// Payload carries the message id and the reason ONLY — never a body, never
/// headers — and `reason` is capped via [`cap_reason`] before it is written
/// (see that function's docs for why this sink applies its own bound rather
/// than trusting the worker's).
///
/// The driver calls it from its own std thread, which every conversation, the
/// poll and the ack wait on, so the insert is **spawned**
/// ([`crate::audit_sink::spawn_audit_insert`], whose doc gives the cost), not
/// `block_on`'d (#789): a slow or unreachable Postgres stalled the email
/// channel for a pool-acquire timeout per skipped id. A failed insert is
/// logged with the message id, so it can be matched to the driver's own line
/// for the skip.
fn email_skipped_audit_sink(pool: PgPool, handle: tokio::runtime::Handle) -> AckOnlyAudit {
    Box::new(move |message_id: &str, reason: &str| {
        let payload = serde_json::json!({
            "channel": "email",
            "message_id": message_id,
            "reason": cap_reason(reason),
        });
        let message_id = kastellan_core::untrusted_text::neutralise_controls(message_id);
        crate::audit_sink::spawn_audit_insert(
            &handle,
            &pool,
            "channel",
            kastellan_core::channel::actions::SKIPPED_ACK_ONLY,
            payload,
            move |e| {
                error!(
                    error = %e,
                    %message_id,
                    "email: skipped-id audit insert failed (non-fatal); the driver's line for \
                     the skip stands, but it has no audit row"
                );
            },
        );
    })
}

/// Pure: a configuration error can never be fixed without an operator edit
/// **plus a restart**, because the process environment is immutable for this
/// daemon's lifetime. Fatal, therefore — and the message the supervisor prints
/// for a fatal outcome says exactly that, which is what
/// `log_channel_disabled` used to say here.
///
/// This is the one classification that differs from Matrix's, and it is the
/// reason the email channel keeps its distinct posture after #514: Matrix
/// fails soft over an unreachable homeserver because that is a transient
/// outage, whereas a half-set `KASTELLAN_EMAIL_*` block is a deployment fact
/// that no amount of retrying will change.
fn classify_config_error(e: anyhow::Error) -> BootOutcome {
    BootOutcome::Fatal(e.context("email channel configuration is incomplete or invalid"))
}

/// Pure: a worker spawn failure is a sandbox/egress condition, not a
/// configuration one, so it is retryable.
///
/// This is not a theoretical distinction — the failure that prompted #514 was
/// exactly this shape: `systemd-run --scope` refused to create the sidecar's
/// cgroup because the user manager was itself shutting down. The next attempt
/// absorbs it.
fn classify_spawn_error(e: anyhow::Error) -> BootOutcome {
    BootOutcome::Retry(e.context("the email worker failed to start"))
}

/// One email bring-up attempt: read the config, open the LISTEN/NOTIFY
/// connection (cheap and outage-sensitive, so it goes first — #517), spawn the
/// sandboxed worker
/// (force-routed through a real 1:1 **intercepting** sidecar whenever
/// `force_routing` is `Some` — the `Some`/`None` branching mirrors
/// `matrix_boot::attempt`'s `MatrixEgress` wiring exactly, but the TLS posture
/// does NOT: Matrix's sidecar stays a transparent tunnel, while this one
/// always intercepts (`Mitm::Intercept`) so an operator's upstream-extra-CA
/// anchor can reach a self-signed localmail; passing `None` here would
/// silently degrade the worker onto the HOST network namespace, see
/// [`kastellan_core::channel::email::EmailEgress`]'s docs), wire the skipped-id
/// audit closure, and run a [`ChannelBus`] over it.
///
/// Classification, which is the whole of this function's policy:
///
/// * unset `KASTELLAN_EMAIL_ENDPOINT` ⇒ [`BootOutcome::NotConfigured`],
///   silently — the default for every deployment without the email fallback;
/// * a set-but-PARTIAL config ⇒ [`classify_config_error`] ⇒ fatal;
/// * a worker spawn failure ⇒ [`classify_spawn_error`] ⇒ retry;
/// * a `PgCompletedTasks::connect` failure ⇒ retry (a LISTEN/NOTIFY bring-up
///   hiccup on an already-open pool is a generic DB-listener condition, not an
///   email misconfiguration — the same reading Matrix gives it).
///
/// `outbox` is the shared core-initiated-outbound registry (#564 slice 2).
/// The bus registers its own reply queue into it on spawn and deregisters on
/// shutdown, which is what lets a raised ask reach the same pump replies go
/// through. It is created in `main` before both the scheduler and this
/// supervisor, because each supervisor restarts its bus underneath and a
/// held `Sender` would be stale after the first restart. See the
/// registration note at the call site for what the audit trail does — and
/// does not — say about email delivery today.
///
/// There is still no `Err` variant anywhere on this path, so no future `?` can
/// reintroduce the daemon-aborting behaviour this module's docs argue against.
async fn attempt(
    pool: PgPool,
    sandboxes: SandboxBackends,
    force_routing: Option<Arc<ForceRoutingConfig>>,
    outbox: Arc<kastellan_core::channel::outbox::ChannelOutbox>,
) -> BootOutcome {
    let cfg = match kastellan_core::channel::email::config::EmailConfig::from_env() {
        // Unset ⇒ channel absent. Silent on purpose: this is the default for
        // every deployment that doesn't use the email fallback.
        Ok(None) => return BootOutcome::NotConfigured,
        Ok(Some(cfg)) => cfg,
        Err(e) => return classify_config_error(e),
    };

    // Worker backend: always the host jail — no microVM option for this
    // worker in this slice. SIDECAR backend always stays the host
    // bwrap/Seatbelt too (5c invariant — the egress proxy needs a real
    // network route; a VM here would boot a proxy with none), same as Matrix.
    #[cfg(target_os = "linux")]
    let backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.bwrap);
    #[cfg(target_os = "linux")]
    let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.bwrap);
    #[cfg(target_os = "macos")]
    let backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);
    #[cfg(target_os = "macos")]
    let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);

    let egress = force_routing.as_ref().map(|fr| kastellan_core::channel::email::EmailEgress {
        sidecar_backend: Arc::clone(&sidecar_backend),
        routing: Arc::clone(fr),
    });

    // LISTEN/NOTIFY first, BEFORE the worker (#517) — same reasoning as
    // `matrix_boot::attempt`: a channel is now restarted when its pumps die,
    // the reachable cause of that is a sustained Postgres outage, and this is
    // the step such an outage fails. Spawning a sandboxed worker (and its
    // sidecar) first would mean building and tearing down the expensive half on
    // every retry of an outage the cheap half could have detected immediately.
    let completed = match kastellan_core::channel::bus::PgCompletedTasks::connect(pool.clone()).await
    {
        Ok(completed) => completed,
        Err(e) => {
            return BootOutcome::Retry(
                e.context("email: PgCompletedTasks::connect (LISTEN/NOTIFY) failed"),
            )
        }
    };

    let audit_ack_only =
        Some(email_skipped_audit_sink(pool.clone(), tokio::runtime::Handle::current()));

    let spawned = match kastellan_core::channel::email::spawn_email_worker(
        backend,
        ChannelId("email".to_string()),
        &cfg,
        egress,
        audit_ack_only,
    ) {
        Ok(s) => s,
        Err(e) => return classify_spawn_error(e),
    };

    info!(identity = %spawned.identity, "email worker started; starting channel bus");
    let authorizer = Arc::new(kastellan_core::channel::auth::DbPeerAuthorizer::new(pool.clone()));
    let pairing = Arc::new(kastellan_core::channel::pairing::DbPairingService::new(pool.clone()));
    let events = Arc::new(kastellan_core::channel::bus::PgChannelEvents::new(pool.clone()));
    // Wired so an email peer can still ANSWER an ask (the inbound half
    // works today); outbound SMTP is a later slice.
    //
    // Be precise about what the audit trail says for the OUTBOUND half,
    // because an earlier version of this comment claimed the opposite and
    // the spec repeated it. `ChannelOutbox::try_deliver` fails only on
    // `NoSuchChannel`/`QueueFull`/`QueueClosed`; this bus registers a live
    // sender and its pump drains it, so a raised ask is accepted and
    // audited **`ask.delivered`** — never `ask.delivery_failed`.
    // `EmailChannel::send`'s unconditional refusal happens afterwards, in
    // the pump, which writes `channel.reply_undelivered {channel, peer}` —
    // a row naming neither the ask nor the task.
    //
    // So for email, `ask.delivered` currently means "queued", and it is
    // always false in the sense an operator cares about. That is the
    // general caveat on `ACTION_ASK_DELIVERED` ("queued, not delivered to
    // the human"), except that here it is guaranteed rather than a race.
    // Making the failure correlatable needs the outbound message to carry
    // its `ask_id` into the pump — tracked as its own issue rather than
    // widened into this slice.
    let asks = Arc::new(kastellan_core::channel::bus::AskWiring {
        outbox,
        resolver: Arc::new(kastellan_core::channel::bus::PgAskResolver::new(pool.clone())),
    });
    BootOutcome::Started(StartedChannel::from_bus(ChannelBus::spawn(
        vec![Box::new(spawned.channel)],
        authorizer,
        Some(pairing),
        events,
        Box::new(completed),
        Some(asks),
    )))
}

/// Supervise the email channel: retry [`attempt`] with capped backoff until it
/// comes up, unless it is unconfigured or its configuration is unusable.
///
/// Returns immediately; the returned handle must be `shutdown()`-ed by `main`,
/// which stops the retry loop and, if the channel came up, the bus with it.
///
/// The daemon is never aborted by anything on this path — see the module docs'
/// point 1 for why that is the correct posture for a *fallback* channel.
pub(crate) fn supervise_email_channel(
    pool: &PgPool,
    sandboxes: &SandboxBackends,
    force_routing: &Option<Arc<ForceRoutingConfig>>,
    outbox: Arc<kastellan_core::channel::outbox::ChannelOutbox>,
) -> ChannelSupervisor {
    let pool = pool.clone();
    let sandboxes = sandboxes.clone();
    let force_routing = force_routing.clone();
    let audit = pg_boot_audit_sink(pool.clone(), "email");
    ChannelSupervisor::spawn(
        "email",
        RestartBackoff::default(),
        ReportingPolicy::default(),
        Some(audit),
        move || attempt(pool.clone(), sandboxes.clone(), force_routing.clone(), outbox.clone()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PARTIAL config must be FATAL: the process environment is fixed for
    /// this daemon's lifetime, so no number of retries can complete it. The
    /// operator-facing message already says "fix it, then restart" — retrying
    /// instead would make that message a lie and spin forever.
    #[test]
    fn a_partial_config_is_fatal_not_retryable() {
        let err = anyhow::anyhow!("KASTELLAN_EMAIL_AUTHSERV_ID is not set");
        let outcome = classify_config_error(err);
        assert!(matches!(outcome, BootOutcome::Fatal(_)), "{outcome:?}");
    }

    /// The fatal cause keeps the underlying detail, which for a config problem
    /// names every missing variable — that is the whole value of the loud line.
    #[test]
    fn the_fatal_cause_still_names_the_missing_variable() {
        let err = anyhow::anyhow!("KASTELLAN_EMAIL_AUTHSERV_ID is not set");
        match classify_config_error(err) {
            BootOutcome::Fatal(e) => {
                let rendered = format!("{e:#}");
                assert!(rendered.contains("KASTELLAN_EMAIL_AUTHSERV_ID"), "{rendered}");
                assert!(rendered.contains("incomplete or invalid"), "{rendered}");
            }
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    /// #789: the skipped-id sink returns at once against a Postgres that never
    /// answers. It used to `block_on` its insert on the driver's thread, which
    /// every conversation, poll and ack wait on.
    #[test]
    fn the_skipped_id_sink_does_not_hold_the_driver_thread() {
        use crate::audit_sink::test_support::{assert_returns_at_once, stalled_pool};
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let (pool, _listener) = rt.block_on(async { stalled_pool() });
        let sink = email_skipped_audit_sink(pool, rt.handle().clone());
        assert_returns_at_once("the email skipped-id sink", || sink("<id@host>", "unattributable"));
    }

    /// A worker spawn failure is RETRYABLE — it is the observed #514 trigger
    /// (`systemd-run --scope` refusing to create the sandbox cgroup while the
    /// user manager restarts), which the next attempt absorbs.
    #[test]
    fn a_worker_spawn_failure_is_retryable() {
        let err = anyhow::anyhow!("egress-proxy sidecar exited before becoming ready");
        let outcome = classify_spawn_error(err);
        assert!(matches!(outcome, BootOutcome::Retry(_)), "{outcome:?}");
    }
}
