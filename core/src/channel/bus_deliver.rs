//! The per-channel pump's half of the outbound path: what a channel's queue
//! carries ([`Queued`]), and [`deliver`], which hands one item to the
//! transport — claiming a reply first.
//!
//! **Why the claim is here (#832).** Until #832 the completed-task pump
//! claimed a reply (#825's D1) and only then pushed it into the per-channel
//! queue. A reply still in that queue when its pump ended — a bus stop, a
//! flapping transport, or a receiver dropped between two lines — was then
//! *settled* (`routed`), recorded as replied, and never sent: no sweep would
//! find it again. The #825 start sweep fills that queue on purpose, so a
//! flapping pump could lose up to a queue's worth per flap. Now the
//! completed-task pump queues the reply unclaimed and the claim happens
//! immediately before the send, so a reply that never reaches [`deliver`] is
//! still in the backlog and the next sweep delivers it. Duplicates (a NOTIFY
//! and a sweep tick both queueing a task while its queue is backed up) are
//! settled by the claim, which only one copy wins.
//!
//! What is still in flight at a stop is the claim → row → send span of the
//! one reply being delivered, or the send of one raised ask; an
//! [`InFlightGuard`] says so.

use tracing::warn;

use super::bus::{ChannelEvents, CompletedTasks, ReplyDisposition};
use super::bus_guards::{format_claim_unknowable, InFlight, InFlightGuard};
use super::bus_outbound::send_or_record;
use super::catch_up::{self, Via};
use super::{actions, Channel, ChannelId, OutgoingMessage};

/// One item in a channel's outbound queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Queued {
    /// A finished task's reply, **not yet claimed**: [`deliver`] claims it.
    Reply { task_id: i64, via: Via, out: OutgoingMessage },
    /// A core-initiated message — a raised ask for task `task_id`, queued
    /// through the `ChannelOutbox`. Its row (`ask.delivered`) is already
    /// written, and there is no task reply to claim; the id is only for the
    /// line said if a stop loses it (the body carries the approval token, so
    /// that line cannot quote it).
    Ask { task_id: i64, out: OutgoingMessage },
}

impl Queued {
    /// The message, whichever kind of item it is.
    pub fn message(&self) -> &OutgoingMessage {
        match self {
            Self::Reply { out, .. } | Self::Ask { out, .. } => out,
        }
    }
}

/// What [`deliver`] came to — for the pump's claim-failure streak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// A reply, claimed and handed to the transport. A refusal there is
    /// recorded by `send_or_record` (`send_failed`), not here.
    Sent,
    /// A raised ask, handed to the transport. No claim was made, so it says
    /// nothing about the claim path either way.
    AskSent,
    /// A reply this router did not win — already settled by a duplicate copy
    /// of it, or by the other bus. Nothing sent, and nothing wrong.
    NotClaimed,
    /// The claim returned an error. Usually the task is not settled and the
    /// reply is left for catch-up; when a re-read finds it settled anyway (a
    /// claim that committed before the error reached the bus) it is recorded
    /// as `claim_unknown` and not sent; when the re-read fails too, nothing
    /// can be known and it is said on `[audit-lost]`. Every case is a failed
    /// claim, for the pump's streak: a link that commits and then drops the
    /// connection turns every reply into "claimed, not sent".
    ClaimFailed,
}

impl Delivery {
    /// What this outcome says about the claim path: `Some(true)` a failed
    /// claim, `Some(false)` one that worked (won or lost), `None` no claim
    /// at all — an ask, which must neither count toward the pump's run of
    /// failures nor reset it.
    pub fn claim_failed(self) -> Option<bool> {
        match self {
            Self::ClaimFailed => Some(true),
            Self::Sent | Self::NotClaimed => Some(false),
            Self::AskSent => None,
        }
    }
}

/// The pump's step after each [`deliver`]: feed `d` to its claim streak, and
/// return `Some(n)` when the "reply claims keep failing" ERROR is due.
pub fn note_claim(streak: &mut catch_up::ClaimStreak, d: Delivery, now: std::time::Instant) -> Option<u32> {
    streak.record(d.claim_failed()?, now)
}

/// Deliver one queued item on `ch`. An ask goes straight to the transport,
/// under an [`InFlightGuard`] of its own. A reply is claimed, gets its delay
/// note if late, is recorded as `channel.replied` (meaning *routed*: the
/// transport attempt follows), and is sent — all under an [`InFlightGuard`],
/// since a stop anywhere in that span can leave a claimed reply unsent.
pub async fn deliver(
    completed: &dyn CompletedTasks,
    events: &dyn ChannelEvents,
    ch: &dyn Channel,
    id: &ChannelId,
    item: Queued,
) -> Delivery {
    match item {
        Queued::Ask { task_id, out } => {
            // Nothing will re-send an ask (`ask.delivered` is written), so a
            // stop mid-send is said, like a reply's.
            let guard = InFlightGuard::arm(events, InFlight::Ask { task_id }, Some(&out.channel), Some(&out.peer));
            send_or_record(ch, events, id, out, "channel send failed").await;
            guard.settle();
            Delivery::AskSent
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
            match claim_outcome(completed, task_id, &e).await {
                Some(true) => {}
                Some(false) => return Delivery::ClaimFailed,
                None => {
                    // The claim may have committed, and if it did no sweep
                    // will find the reply again — a possible loss with no
                    // row, so said whatever the operator's `RUST_LOG`.
                    events.report_lost(&format_claim_unknowable(task_id, &out.channel, &out.peer));
                    return Delivery::ClaimFailed;
                }
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
            return Delivery::ClaimFailed;
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
/// the reply again. `Some(false)`: WARNed here, left for catch-up (best
/// knowledge: a statement the server is still running when the re-read lands
/// can commit after it); `Some(true)` is the caller's to record; `None` (the
/// re-read failed too) is WARNed here, and the caller decides whether it is a
/// loss worth saying louder.
///
/// A `Some(true)` is not always a lost reply: since #832 duplicates are
/// routine, and a copy whose claim errors after another copy already won and
/// sent reads as settled too — a `claim_unknown` row for a reply that went
/// out. The row errs towards "maybe lost", never towards a second send.
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
