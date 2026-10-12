//! The outbound half of the channel bus: completed task → route → the owning
//! channel's queue, unclaimed (the channel's pump claims it at the send since
//! #832, `bus_deliver`); the catch-up sweep over the reply backlog; and the
//! transport attempt's failure row. Its own module since #825 (`bus.rs`
//! was at 481 lines, and the reply catch-up was about to grow this half).

use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use kastellan_db::tasks;

use super::bus::{ChannelEvents, ClaimedReply, CompletedTasks, ReplyDisposition};
use super::bus_deliver::{claim_outcome, Queued};
use super::bus_guards::{InFlight, InFlightGuard};
use super::catch_up::{self, Via};
use super::route::reply_for_completed_task;
use super::{actions, Channel, ChannelId, OutgoingMessage};

/// Real `CompletedTasks` over a `PgListener` on `tasks_completed` + `tasks::get`.
/// Construct via [`PgCompletedTasks::connect`].
pub struct PgCompletedTasks {
    /// Locked only by `next_completed`, i.e. only by the completed-task pump; a
    /// `select!` that drops that future drops the guard with it.
    listener: tokio::sync::Mutex<sqlx::postgres::PgListener>,
    pool: sqlx::PgPool,
}
impl PgCompletedTasks {
    pub async fn connect(pool: sqlx::PgPool) -> anyhow::Result<Self> {
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;
        listener.listen("tasks_completed").await?;
        Ok(Self { listener: tokio::sync::Mutex::new(listener), pool })
    }
}
#[async_trait::async_trait]
impl CompletedTasks for PgCompletedTasks {
    async fn next_completed(&self) -> Option<i64> {
        let mut listener = self.listener.lock().await;
        loop {
            match listener.recv().await {
                Ok(n) => {
                    if let Ok(id) = n.payload().parse::<i64>() {
                        return Some(id);
                    }
                }
                // `recv` reconnects by itself after a lost connection —
                // dropping the NOTIFYs sent meanwhile, which the periodic
                // sweep recovers — so an error here is one it could not.
                Err(e) => {
                    warn!(error = %e, "tasks_completed listener error; stopping outbound pump");
                    return None;
                }
            }
        }
    }
    async fn load(&self, id: i64) -> anyhow::Result<Option<(Value, Option<Value>)>> {
        Ok(tasks::get(&self.pool, id).await?.map(|t| (t.payload, t.result)))
    }
    async fn claim(&self, id: i64, d: ReplyDisposition) -> anyhow::Result<Option<ClaimedReply>> {
        Ok(tasks::reply_claim::claim_reply(&self.pool, id, d).await?)
    }
    async fn unsettled(&self, after_id: i64, limit: i64) -> anyhow::Result<Vec<i64>> {
        Ok(tasks::reply_claim::unsettled_channel_replies(&self.pool, after_id, limit).await?)
    }
    async fn settled(&self, id: i64) -> anyhow::Result<bool> {
        Ok(tasks::reply_claim::reply_settled(&self.pool, id).await?)
    }
}

/// `ch.send(out)` — and when the transport refuses it, a WARN and a
/// `channel.reply_undelivered` row (`send_failed`), so the audit trail does
/// not assert a delivery that never happened. Every message a per-channel pump
/// hands its transport goes through here: a reply, a raised ask, an inbound
/// ack. `what` is the WARN's message, so the log still says which.
///
/// The row is the same view a polled driver's sink is handed, so it has one
/// definition (`UndeliveredReply::payload`, #800); the error stays in the log,
/// since it is transport text, not a fixed label, and the body is never
/// persisted.
pub(super) async fn send_or_record(
    ch: &dyn Channel,
    events: &dyn ChannelEvents,
    id: &ChannelId,
    out: OutgoingMessage,
    what: &'static str,
) {
    let (peer, conversation) = (out.peer.clone(), out.conversation.clone());
    if let Err(e) = ch.send(out).await {
        warn!(channel = %id.0, error = %e, "{what}");
        let reply = super::UndeliveredReply {
            channel: id,
            peer: &peer,
            conversation: &conversation,
            reason: super::UndeliveredReason::SendFailed,
            observed_at: time::OffsetDateTime::now_utc(),
        };
        events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
    }
}

/// Handle one completed-task id on the outbound side: load it, route it
/// (pure), and queue it — **unclaimed** — for the matching channel's pump,
/// which claims it right before sending (#832, `bus_deliver`). The single
/// path for both the live NOTIFY and the catch-up sweep (`via` says which).
/// `senders` maps `ChannelId` → that channel's queue. Returns the item
/// queued (for tests).
///
/// Nothing here claims a routable reply, so anything that stops one — a
/// failed load, a closed queue, a bus stop aborting this while it waits for
/// queue space, or the queue's pump ending with the reply still in it —
/// leaves it in the backlog, and the next sweep delivers it. The one claim made here settles
/// a channel task with no routing metadata (`channel.reply_unroutable`).
/// What a stop can still lose is the reply a channel's pump is delivering at
/// that moment, which `bus_deliver`'s guard reports — and an unroutable
/// claim in flight, which `settle_unroutable`'s does.
pub async fn handle_completed(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<Queued>>,
    id: i64,
    via: Via,
) -> Option<Queued> {
    match route_one(completed, events, senders, id, via).await {
        Routed::Queued(item) => Some(item),
        Routed::Skipped | Routed::Unserved | Routed::LoadFailed | Routed::ClaimFailed => None,
    }
}

/// What one route of a completed task came to: [`handle_completed`]'s answer,
/// and the sweep's tally.
enum Routed {
    /// Queued, unclaimed, for its channel's pump.
    Queued(Queued),
    /// Nothing more for this route to do: not a channel task, unroutable
    /// (settled, or another router's), or a queue found closed — whose reply
    /// stays in the backlog for the next bus.
    Skipped,
    /// For a channel this bus does not serve; left for its own bus.
    Unserved,
    /// The load failed; the reply is left for catch-up.
    LoadFailed,
    /// An unroutable task's claim failed. Left for catch-up, or — when a
    /// re-read found the task settled anyway — recorded (`claim_uncertain`).
    ClaimFailed,
}

async fn route_one(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<Queued>>,
    id: i64,
    via: Via,
) -> Routed {
    let (payload, result) = match completed.load(id).await {
        Ok(Some(pr)) => pr,
        // The row is gone: there is no DELETE grant on `tasks`, so in
        // practice this does not happen — and there is nothing to reply to.
        Ok(None) => return Routed::Skipped,
        Err(e) => {
            // Not lost since #825: unclaimed, so the next sweep retries it.
            warn!(task_id = id, error = %e, "outbound load failed; reply left for catch-up");
            return Routed::LoadFailed;
        }
    };
    let Some(out) = reply_for_completed_task(&payload, result.as_ref()) else {
        // `None` is the normal answer for a completion that is not a channel
        // task (an `ask`/`l3_run`). A channel task with no routing metadata
        // is a reply nobody can deliver: settle it, so the sweep stops
        // finding it, and let the claim's one winner say so.
        if payload.get("kind").and_then(Value::as_str) == Some("channel") {
            return settle_unroutable(completed, events, id).await;
        }
        return Routed::Skipped;
    };
    let Some(tx) = senders.get(&out.channel) else {
        // NOT a warning: the daemon runs one `ChannelBus` per channel family
        // (`main.rs` spawns a Matrix bus and an email bus), and every bus's
        // completed-task pump sees EVERY completed channel task via
        // LISTEN/NOTIFY. So "this reply isn't for a channel I serve" is the
        // normal case on each reply — the other bus is handling it — and
        // logging it at `warn` fired a misleading "dropping" line for every
        // successfully delivered reply, which is exactly how an operator learns
        // to ignore warnings (review finding). Unifying the buses would remove
        // the ambiguity outright and let this go back to being a real `warn!`
        // (it would then mean "nothing serves this channel at all") — #497.
        // A sweep counts these, so a channel nothing serves shows in its summary.
        debug!(channel = %out.channel.0, "reply is for a channel this bus does not serve; ignoring");
        return Routed::Unserved;
    };
    // A full queue parks here. Nothing is claimed yet, so a bus stop that
    // aborts the pump while it is parked loses nothing: the sweep finds it.
    let item = Queued::Reply { task_id: id, via, out };
    match tx.send(item.clone()).await {
        Ok(()) => Routed::Queued(item),
        Err(_) => {
            // The queue's only receiver is this channel's pump, so it has
            // ended and the bus is about to restart; the next bus's start
            // sweep delivers the reply.
            info!(channel = %item.message().channel.0, task_id = id, "send queue closed; reply left for catch-up");
            Routed::Skipped
        }
    }
}

/// Settle a channel task that has no routing metadata, and — for the claim's
/// one winner only — say so: a WARN and a `channel.reply_unroutable` row.
/// The row names the task, since there is no channel or peer to name. Before
/// #825 this was a WARN on every bus and no row.
async fn settle_unroutable(completed: &dyn CompletedTasks, events: &dyn ChannelEvents, id: i64) -> Routed {
    let observed_at = || super::undelivered::observed_at_json(time::OffsetDateTime::now_utc());
    // A stop on the claim's round-trip can leave it committed with no row.
    let guard = InFlightGuard::arm(events, InFlight::Unroutable { task_id: id }, None, None);
    let routed = settle_unroutable_inner(completed, events, id, observed_at).await;
    guard.settle();
    routed
}

/// [`settle_unroutable`], inside its guard.
async fn settle_unroutable_inner(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    id: i64,
    observed_at: impl Fn() -> Value,
) -> Routed {
    match completed.claim(id, ReplyDisposition::Unroutable).await {
        Ok(Some(_)) => {
            warn!(task_id = id, "channel task has no routing metadata; reply dropped");
            let row = serde_json::json!({"task_id": id, "observed_at": observed_at()});
            events.audit(actions::REPLY_UNROUTABLE, row).await;
            Routed::Skipped
        }
        Ok(None) => Routed::Skipped,
        Err(e) => {
            if claim_outcome(completed, id, &e).await == Some(true) {
                // Possibly this claim, committed before the error; possibly
                // another router's, which wrote its own row. A second row
                // marked uncertain beats none.
                warn!(task_id = id, error = %e, "unroutable reply claim failed, but the task is settled");
                let row = serde_json::json!({
                    "task_id": id, "observed_at": observed_at(), "claim_uncertain": true,
                });
                events.audit(actions::REPLY_UNROUTABLE, row).await;
            }
            Routed::ClaimFailed
        }
    }
}

/// Walk the reply backlog — finished channel tasks whose reply was never
/// settled — and route each through [`handle_completed`]'s path (#825). What
/// it finds is what the live NOTIFY missed: a reply that finished while no
/// bus listened, whose load or claim failed, whose queue was closed, or whose
/// NOTIFY was lost to a listener reconnect.
///
/// Pages by ascending id and moves the cursor past every id, including the
/// ones it skips (another bus's channel, a failed load), so a stuck set
/// cannot starve the rest. The cursor only ever moves forward: a backlog read
/// that fails, or that would not move it, ends **this** sweep with a WARN;
/// the pump carries on and the next sweep retries. Returns what the sweep
/// came to, and logs it at INFO when any reply was unserved or failed.
pub async fn sweep(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<Queued>>,
) -> catch_up::SweepReport {
    let mut report = catch_up::SweepReport::default();
    let mut after = 0;
    loop {
        let page = match completed.unsettled(after, catch_up::SWEEP_PAGE).await {
            Ok(page) => page,
            Err(e) => {
                warn!(error = %e, "reply catch-up sweep could not read the backlog; retrying next sweep");
                report.read_failed = true;
                break;
            }
        };
        let Some(&max) = page.iter().max() else { break };
        for &id in &page {
            match route_one(completed, events, senders, id, Via::CatchUp).await {
                Routed::Queued(_) => report.queued += 1,
                Routed::Unserved => report.unserved += 1,
                Routed::LoadFailed => report.load_failed += 1,
                Routed::ClaimFailed => report.claim_failed += 1,
                Routed::Skipped => {}
            }
        }
        if i64::try_from(page.len()).unwrap_or(i64::MAX) < catch_up::SWEEP_PAGE {
            break;
        }
        if max <= after {
            // A full page that does not move the cursor would repeat forever
            // inside the pump, which then never ends and never rings its bell.
            warn!(after, "reply catch-up backlog read did not move past its cursor; retrying next sweep");
            report.read_failed = true;
            break;
        }
        after = max;
    }
    if report.unserved > 0 || report.load_failed > 0 || report.claim_failed > 0 {
        info!(
            queued = report.queued, unserved = report.unserved,
            load_failed = report.load_failed, claim_failed = report.claim_failed,
            "reply catch-up sweep could not route every reply"
        );
    }
    report
}
