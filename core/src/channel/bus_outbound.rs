//! The outbound half of the channel bus: completed task → route → the
//! owning channel's queue, and the transport attempt's failure row. Its own
//! module since #825 (`bus.rs` was at 481 lines, and the reply catch-up was
//! about to grow this half).

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
/// (pure), claim it, and queue it for the matching channel. The single path
/// for both the live NOTIFY and the catch-up sweep (`via` says which), so the
/// claim (#825) is what makes a reply go out once whichever path — and
/// whichever of the two buses (#497) — gets there first. `senders` maps
/// `ChannelId` → an outbound `send` handle. Returns the `OutgoingMessage`
/// queued (for tests).
///
/// Anything that stops a reply *before* the claim leaves it in the backlog
/// for the next sweep: a failed load, a failed claim, a closed queue. Only a
/// claimed reply can be lost, and that loss always has a row.
pub async fn handle_completed(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    senders: &HashMap<ChannelId, mpsc::Sender<OutgoingMessage>>,
    id: i64,
    via: Via,
) -> Option<OutgoingMessage> {
    let (payload, result) = match completed.load(id).await {
        Ok(Some(pr)) => pr,
        Ok(None) => return None, // rolled back between NOTIFY and SELECT — benign
        Err(e) => {
            // Not lost since #825: unclaimed, so the next sweep retries it.
            warn!(task_id = id, error = %e, "outbound load failed; reply left for catch-up");
            return None;
        }
    };
    let Some(mut out) = reply_for_completed_task(&payload, result.as_ref()) else {
        // `None` is the normal answer for a completion that is not a channel
        // task (an `ask`/`l3_run`). A channel task with no routing metadata
        // is a reply nobody can deliver: settle it, so the sweep stops
        // finding it, and let the claim's one winner say so.
        if payload.get("kind").and_then(Value::as_str) == Some("channel") {
            settle_unroutable(completed, events, id).await;
        }
        return None;
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
        // Unclaimed on purpose (#825): the bus that serves it claims it.
        debug!(channel = %out.channel.0, "reply is for a channel this bus does not serve; ignoring");
        return None;
    };
    if tx.is_closed() {
        // The queue's only receiver is this channel's pump, so it has ended
        // and the bus is about to restart. Until #825 this was a drop with a
        // `queue_closed` row; now the reply stays unclaimed and the next
        // bus's start sweep delivers it.
        info!(channel = %out.channel.0, task_id = id, "send queue closed; reply left for catch-up");
        return None;
    }
    let claimed = match completed.claim(id, ReplyDisposition::Routed).await {
        Ok(Some(c)) => c,
        Ok(None) => return None, // already routed: the other path or the other bus
        Err(e) => {
            warn!(task_id = id, error = %e, "reply claim failed; reply left for catch-up");
            return None;
        }
    };
    let now = time::OffsetDateTime::now_utc();
    let late_by = catch_up::lateness(claimed.created_at, claimed.finished_at, now);
    if let Some(note) = catch_up::delay_note(claimed.created_at, claimed.finished_at, now) {
        out.body = format!("{note}\n\n{}", out.body);
    }
    if let Err(mpsc::error::SendError(dropped)) = tx.send(out.clone()).await {
        // The queue closed between the check above and this send. The reply
        // is claimed, so no sweep will find it again: this one IS lost, and
        // says so (#815). No `channel.replied` — it was never routed.
        warn!(channel = %dropped.channel.0, "outbound send queue closed; reply dropped");
        let reply = super::UndeliveredReply::of(&dropped, super::UndeliveredReason::QueueClosed, now);
        events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
        return None;
    }
    let mut row = serde_json::json!({
        "task_id": id, "channel": out.channel.0, "peer": out.peer.0, "via": via.as_str(),
    });
    if let Some(late_by) = late_by {
        row["delayed_secs"] = late_by.whole_seconds().into();
    }
    events.audit(actions::REPLIED, row).await;
    Some(out)
}

/// Settle a channel task that has no routing metadata, and — for the claim's
/// one winner only — say so: a WARN and a `channel.reply_unroutable` row.
/// The row names the task, since there is no channel or peer to name. Before
/// #825 this was a WARN on every bus and no row.
async fn settle_unroutable(completed: &dyn CompletedTasks, events: &dyn ChannelEvents, id: i64) {
    match completed.claim(id, ReplyDisposition::Unroutable).await {
        Ok(Some(_)) => {
            warn!(task_id = id, "channel task has no routing metadata; reply dropped");
            let observed_at = super::undelivered::observed_at_json(time::OffsetDateTime::now_utc());
            let row = serde_json::json!({"task_id": id, "observed_at": observed_at});
            events.audit(actions::REPLY_UNROUTABLE, row).await;
        }
        Ok(None) => {}
        Err(e) => warn!(task_id = id, error = %e, "unroutable reply claim failed; left for catch-up"),
    }
}
