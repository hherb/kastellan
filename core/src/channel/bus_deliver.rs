//! The per-channel pump's half of the outbound path: what a channel's queue
//! carries ([`Queued`]), and [`deliver`], which hands one item to the
//! transport — claiming a reply first.
//!
//! **Why the claim is here (#832).** Until #832 the outbound pump claimed a
//! reply (#825's D1) and only then pushed it into the per-channel queue. A
//! reply still in that queue when its pump ended — a bus stop, a flapping
//! transport, or a receiver dropped between two lines — was then *settled*
//! (`routed`), recorded as replied, and never sent: no sweep would find it
//! again. The #825 start sweep fills that queue on purpose, so a flapping
//! pump could lose up to a queue's worth per flap. Now the outbound pump
//! queues the reply unclaimed and the claim happens immediately before the
//! send, so a reply that never reaches [`deliver`] is still in the backlog
//! and the next sweep delivers it. Duplicates (a NOTIFY and a sweep tick both
//! queueing a task while its queue is backed up) are settled by the claim,
//! which only one copy wins.
//!
//! What is still in flight at a stop is the claim → row → send span of the
//! one reply being delivered; an [`InFlightGuard`] says so.

use tracing::warn;

use super::bus::{ChannelEvents, CompletedTasks, ReplyDisposition};
use super::bus_guards::{InFlight, InFlightGuard};
use super::bus_outbound::send_or_record;
use super::catch_up::{self, Via};
use super::{actions, Channel, ChannelId, OutgoingMessage};

/// One item in a channel's outbound queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Queued {
    /// A finished task's reply, **not yet claimed**: [`deliver`] claims it.
    Reply { task_id: i64, via: Via, out: OutgoingMessage },
    /// A core-initiated message — a raised ask, queued through the
    /// `ChannelOutbox`. Its row (`ask.delivered`) is already written, and
    /// there is no task reply to claim.
    Ask(OutgoingMessage),
}

impl Queued {
    /// The message, whichever kind of item it is.
    pub fn message(&self) -> &OutgoingMessage {
        match self {
            Self::Reply { out, .. } | Self::Ask(out) => out,
        }
    }
}

/// What [`deliver`] came to — for the pump's claim-failure streak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Handed to the transport. A refusal there is recorded by
    /// `send_or_record` (`send_failed`), not here.
    Sent,
    /// A reply this router did not win — already settled by a duplicate copy
    /// of it, the other bus, or a claim that committed before it errored
    /// (that last one recorded as `claim_unknown`). Nothing sent.
    NotClaimed,
    /// The claim failed and the task is not settled: left for catch-up.
    ClaimFailed,
}

/// Deliver one queued item on `ch`. An ask goes straight to the transport. A
/// reply is claimed, gets its delay note if late, is recorded as
/// `channel.replied` (meaning *routed*: the transport attempt follows), and
/// is sent — all under an [`InFlightGuard`], since a stop anywhere in that
/// span can leave a claimed reply unsent.
pub async fn deliver(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    ch: &dyn Channel,
    id: &ChannelId,
    item: Queued,
) -> Delivery {
    match item {
        Queued::Ask(out) => {
            send_or_record(ch, events, id, out, "channel send failed").await;
            Delivery::Sent
        }
        Queued::Reply { task_id, via, out } => {
            let guard = InFlightGuard::arm(events, InFlight::Reply { task_id }, Some(&out.channel), Some(&out.peer));
            let delivery = deliver_reply(completed, events, ch, id, task_id, via, out).await;
            guard.settle();
            delivery
        }
    }
}

/// [`deliver`]'s reply arm, inside its guard.
async fn deliver_reply(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    ch: &dyn Channel,
    id: &ChannelId,
    task_id: i64,
    via: Via,
    mut out: OutgoingMessage,
) -> Delivery {
    let claimed = match completed.claim(task_id, ReplyDisposition::Routed).await {
        Ok(Some(c)) => c,
        Ok(None) => return Delivery::NotClaimed,
        Err(e) => {
            if claim_outcome(completed, task_id, &e).await != Some(true) {
                return Delivery::ClaimFailed;
            }
            // Settled, and this router does not know it won: sending could
            // duplicate the other router's reply, so — at most once — it is
            // recorded instead.
            warn!(
                channel = %out.channel.0, task_id, error = %e,
                "reply claim failed, but the task is settled; reply dropped"
            );
            let now = time::OffsetDateTime::now_utc();
            let reply = super::UndeliveredReply::of(&out, super::UndeliveredReason::ClaimUnknown, now);
            events.audit(actions::REPLY_UNDELIVERED, reply.payload()).await;
            return Delivery::NotClaimed;
        }
    };
    let late = catch_up::late_reply(&claimed, time::OffsetDateTime::now_utc());
    if let Some((_, note)) = &late {
        out.body = format!("{note}\n\n{}", out.body);
    }
    let mut row = serde_json::json!({
        "task_id": task_id, "channel": out.channel.0, "peer": out.peer.0, "via": via.as_str(),
    });
    if let Some((late_by, _)) = late {
        row["delayed_secs"] = late_by.whole_seconds().into();
    }
    events.audit(actions::REPLIED, row).await;
    send_or_record(ch, events, id, out, "channel send failed").await;
    Delivery::Sent
}

/// After a claim returned `e`: is the task settled? The claim is one
/// autocommit `UPDATE`, so an I/O error can arrive after the server ran it —
/// and then "left for catch-up" would be false, with no sweep ever finding
/// the reply again. `Some(false)` is the WARN it always was; `Some(true)` is
/// the caller's to record; `None` (the re-read failed too) is said here,
/// since nothing more can be known.
pub(super) async fn claim_outcome(completed: &dyn CompletedTasks, id: i64, e: &anyhow::Error) -> Option<bool> {
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
