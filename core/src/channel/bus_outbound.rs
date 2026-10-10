//! The outbound half of the channel bus: completed task → route → claim →
//! the owning channel's queue; the catch-up sweep over the reply backlog; and
//! the transport attempt's failure row. Its own module since #825 (`bus.rs`
//! was at 481 lines, and the reply catch-up was about to grow this half).

use std::collections::HashMap;

use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use kastellan_db::tasks;

use super::bus::{ChannelEvents, ClaimedReply, CompletedTasks, ReplyDisposition};
use super::catch_up::{self, Via};
use super::route::reply_for_completed_task;
use super::{actions, Channel, ChannelId, OutgoingMessage};

/// Real `CompletedTasks` over a `PgListener` on `tasks_completed` + `tasks::get`.
/// Construct via [`PgCompletedTasks::connect`].
pub struct PgCompletedTasks {
    listener: sqlx::postgres::PgListener,
    pool: sqlx::PgPool,
}
impl PgCompletedTasks {
    pub async fn connect(pool: sqlx::PgPool) -> anyhow::Result<Self> {
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool).await?;
        listener.listen("tasks_completed").await?;
        Ok(Self { listener, pool })
    }
}
#[async_trait::async_trait]
impl CompletedTasks for PgCompletedTasks {
    async fn next_completed(&mut self) -> Option<i64> {
        loop {
            match self.listener.recv().await {
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
/// (pure), reserve queue space, claim it, and queue it for the matching
/// channel. The single path for both the live NOTIFY and the catch-up sweep
/// (`via` says which), so the claim (#825) is what makes a reply go out once
/// whichever path — and whichever of the two buses (#497) — gets there first.
/// `senders` maps `ChannelId` → an outbound `send` handle. Returns the
/// `OutgoingMessage` queued (for tests).
///
/// Anything that stops a reply *before* the claim leaves it in the backlog
/// for the next sweep: a failed load, a failed claim, a closed queue — and a
/// bus stop that aborts it while it waits for queue space, which is why the
/// slot is reserved before the claim, not after. A claim that fails *after*
/// committing is caught by a re-read and recorded (`claim_unknown`). A
/// claimed reply is otherwise lost only with a row (`queue_closed` here,
/// `send_failed` in the per-channel pump), with three exceptions, all #832's
/// family: an abort landing on the claim's own round-trip (the task is
/// `routed`, with no row and nothing sent); a reply still in the per-channel
/// queue when that pump ends or is aborted (it has a `channel.replied` row
/// and was never sent); and a receiver dropped between the post-claim
/// `is_closed` check and `permit.send` (the same, by a hair).
pub async fn handle_completed(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
    id: i64,
    via: Via,
) -> Option<OutgoingMessage> {
    match route_one(completed, events, senders, id, via).await {
        Routed::Queued(out) => Some(out),
        Routed::Skipped | Routed::Unserved | Routed::LoadFailed | Routed::ClaimFailed => None,
    }
}

/// What one route of a completed task came to: [`handle_completed`]'s answer,
/// and the sweep's tally.
enum Routed {
    /// Claimed and queued.
    Queued(OutgoingMessage),
    /// Nothing more for this route to do: not a channel task, already
    /// settled, unroutable, recorded as undelivered — or a queue found closed
    /// at `reserve()`, whose reply stays in the backlog for the next bus.
    Skipped,
    /// For a channel this bus does not serve; left unclaimed for its own bus.
    Unserved,
    /// The load failed; the reply is left for catch-up.
    LoadFailed,
    /// The claim failed. The reply is left for catch-up, or — when a re-read
    /// found the task settled anyway — recorded (`claim_unknown`); when the
    /// re-read failed too, neither is known.
    ClaimFailed,
}

async fn route_one(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
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
    let Some(mut out) = reply_for_completed_task(&payload, result.as_ref()) else {
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
        // Unclaimed on purpose (#825): the bus that serves it claims it. A
        // sweep counts these, so a channel nothing serves shows in its summary.
        debug!(channel = %out.channel.0, "reply is for a channel this bus does not serve; ignoring");
        return Routed::Unserved;
    };
    // Reserve the queue slot BEFORE claiming. A full queue parks here, and a
    // bus stop that aborts the pump while it is parked must find the reply
    // still unclaimed — after the claim, nothing would ever send it.
    let Ok(permit) = tx.reserve().await else {
        // The queue's only receiver is this channel's pump, so it has ended
        // and the bus is about to restart. Until #825 this was a drop with a
        // `queue_closed` row; now the reply stays unclaimed and the next
        // bus's start sweep delivers it.
        info!(channel = %out.channel.0, task_id = id, "send queue closed; reply left for catch-up");
        return Routed::Skipped;
    };
    let claimed = match completed.claim(id, ReplyDisposition::Routed).await {
        Ok(Some(c)) => c,
        Ok(None) => return Routed::Skipped, // already routed: the other path or the other bus
        Err(e) => {
            if claim_outcome(completed, id, &e).await == Some(true) {
                // Settled, and this router does not know it won: sending
                // could duplicate the other router's reply, so — at most
                // once — it is recorded instead.
                warn!(
                    channel = %out.channel.0, task_id = id, error = %e,
                    "reply claim failed, but the task is settled; reply dropped"
                );
                let now = time::OffsetDateTime::now_utc();
                let reply = super::UndeliveredReply::of(&out, super::UndeliveredReason::ClaimUnknown, now);
                events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
            }
            return Routed::ClaimFailed;
        }
    };
    let now = time::OffsetDateTime::now_utc();
    let late = catch_up::late_reply(&claimed, now);
    if let Some((_, note)) = &late {
        out.body = format!("{note}\n\n{}", out.body);
    }
    if tx.is_closed() {
        // The pump ended while the claim was out. A permit's `send` would
        // drop the reply without a word once the receiver is gone, so check
        // first — this narrows that window to the two lines below, it does
        // not close it. The reply is claimed, no sweep will find it again,
        // and this one IS lost — said with a row (#815). No
        // `channel.replied`: it was never routed.
        warn!(channel = %out.channel.0, "outbound send queue closed; reply dropped");
        let reply = super::UndeliveredReply::of(&out, super::UndeliveredReason::QueueClosed, now);
        events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
        return Routed::Skipped;
    }
    permit.send(out.clone());
    let mut row = serde_json::json!({
        "task_id": id, "channel": out.channel.0, "peer": out.peer.0, "via": via.as_str(),
    });
    if let Some((late_by, _)) = late {
        row["delayed_secs"] = late_by.whole_seconds().into();
    }
    events.audit(actions::REPLIED, row).await;
    Routed::Queued(out)
}

/// After a claim returned `e`: is the task settled? The claim is one
/// autocommit `UPDATE`, so an I/O error can arrive after the server ran it —
/// and then "left for catch-up" would be false, with no sweep ever finding
/// the reply again. `Some(false)` is the WARN it always was; `Some(true)` is
/// the caller's to record; `None` (the re-read failed too) is said here,
/// since nothing more can be known.
async fn claim_outcome(completed: &dyn CompletedTasks, id: i64, e: &anyhow::Error) -> Option<bool> {
    match completed.settled(id).await {
        Ok(false) => {
            warn!(task_id = id, error = %e, "reply claim failed; reply left for catch-up");
            Some(false)
        }
        Ok(true) => Some(true),
        Err(reread) => {
            warn!(
                task_id = id, error = %e, reread_error = %reread,
                "reply claim failed and its outcome is unknown; the reply is left for catch-up or lost"
            );
            None
        }
    }
}

/// Settle a channel task that has no routing metadata, and — for the claim's
/// one winner only — say so: a WARN and a `channel.reply_unroutable` row.
/// The row names the task, since there is no channel or peer to name. Before
/// #825 this was a WARN on every bus and no row.
async fn settle_unroutable(completed: &dyn CompletedTasks, events: &dyn ChannelEvents, id: i64) -> Routed {
    let observed_at = || super::undelivered::observed_at_json(time::OffsetDateTime::now_utc());
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
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
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
