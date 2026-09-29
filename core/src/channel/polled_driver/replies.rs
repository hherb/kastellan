//! The driver's outbound replies, queued **per conversation** (#782).
//!
//! Before #782 every reply sat in one FIFO shared by all conversations, and a
//! refused send kept the front reply in place. So one reply the upstream would
//! never take (a Matrix room the bot was removed from, a malformed room id)
//! held every later reply, in every conversation, for ever. The queue grew
//! with each new inbound message, and the only signal was a WARN that did not
//! say where it was stuck.
//!
//! The delivery contract now:
//!
//! - **Order holds within a conversation, not across conversations.** Each
//!   conversation has its own queue. A refused reply holds only the replies
//!   behind it in the same conversation; other conversations keep flowing.
//! - **A reply the upstream keeps refusing is given up** once [`ReplyGiveUp`]
//!   is met: that reply refused at least `min_refusals` times, **and** its
//!   conversation refusing for at least `after`. The reply is dropped, logged
//!   with its conversation, and handed to the caller's audit hook, which
//!   writes `channel.reply_undelivered`. That is the row the bus's
//!   `channel.replied` promises for a reply that did not land.
//! - **A refused credential holds every conversation and never counts toward
//!   a give-up.** It is the channel's problem, not the reply's: every send
//!   would be refused the same way, and dropping replies because a token
//!   expired overnight would lose all of them.
//! - **Each conversation's queue is capped** at [`MAX_QUEUED_PER_CONVERSATION`].
//!   A reply past the cap is dropped and audited at once, so a stuck
//!   conversation cannot grow the queue without bound.
//!
//! [`ReplyQueues`] and the decision fns are pure; [`enqueue`] and [`flush`]
//! call the worker and emit the lines the pure parts chose.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::channel::{ConversationId, OutgoingMessage};
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

use super::outage::{note_answer, report_down, OutageLog};
use super::refusal::{
    accepted, is_refusal, is_upstream_auth_refusal, refusal_code, refused, RefusalRun,
};
use super::{EncodeSend, PolledWorkerSpec, ReplyUndeliveredAudit, WorkerCalls};

/// When the driver gives up on a reply its worker keeps refusing (#782).
///
/// Both halves must hold. The time half is the real bound. The count half
/// keeps a suspended host (a laptop lid closed for a night) from giving up on
/// a reply that was only tried once before the sleep and once after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyGiveUp {
    /// How long the reply's conversation must have been refusing.
    pub after: Duration,
    /// How many times this reply must have been refused. At least 1.
    pub min_refusals: u32,
}

/// The production give-up bound: an hour of refusals, and at least three of
/// this reply.
///
/// Why an hour: it outlasts every transient refusal seen so far (a homeserver
/// restart, an egress blip), so a reply is only given up when retrying has
/// stopped being plausible. At the 60 s refusal-backoff cap that is about 60
/// attempts. A second reply in the same stuck conversation inherits the
/// conversation's hour, so it is given up after three more refusals (about
/// three minutes), not after another hour.
pub const REPLY_GIVE_UP: ReplyGiveUp =
    ReplyGiveUp { after: Duration::from_secs(60 * 60), min_refusals: 3 };

/// Most replies one conversation may queue. Past it, a new reply is dropped
/// and audited at once. Far above anything a person typing produces while a
/// reply is refused; it exists so the queue has a bound at all.
pub(super) const MAX_QUEUED_PER_CONVERSATION: usize = 256;

// A cap of 0 or 1 would drop every reply queued behind a refused one. Outside
// `#[cfg(test)]` on purpose, so a release build refuses such a value too.
const _: () = assert!(MAX_QUEUED_PER_CONVERSATION >= 64);

/// Pure: reject a give-up bound that would drop a reply at its first refusal.
/// Checked once, by [`super::PolledWorkerDriver::spawn`].
pub(super) fn check_reply_give_up(g: &ReplyGiveUp) -> anyhow::Result<()> {
    anyhow::ensure!(g.min_refusals >= 1, "reply give-up: min_refusals must be at least 1");
    Ok(())
}

/// Pure: is it time to give up on the reply at the front of a conversation?
///
/// `refusals` counts this reply's refusals, **including** the one just
/// received. `refusing_since` is when the conversation's current refusal run
/// began (the first refusal of the run, so `now` for the first one).
pub(super) fn give_up_due(
    refusals: u32,
    refusing_since: Instant,
    now: Instant,
    g: &ReplyGiveUp,
) -> bool {
    refusals >= g.min_refusals && now.saturating_duration_since(refusing_since) >= g.after
}

/// One conversation's queued replies, oldest first, and its refusal state.
#[derive(Debug)]
pub(super) struct ConversationQueue {
    conversation: ConversationId,
    replies: VecDeque<OutgoingMessage>,
    /// Pacing and log cadence for this conversation's sends. Ends only when a
    /// send is accepted, so a give-up does not reset the backoff: the next
    /// reply in a stuck conversation waits at the cap, not from 1 s.
    run: RefusalRun,
    /// Refusals of the reply at the front. Reset when it leaves the queue.
    front_refusals: u32,
}

/// Every conversation's queue, in the order each conversation first queued a
/// reply, plus the channel-wide credential hold. Pure.
#[derive(Debug)]
pub(super) struct ReplyQueues {
    queues: Vec<ConversationQueue>,
    /// A refused **credential** (#674) holds every conversation; see the
    /// module docs. Paced and logged like any other refusal run.
    hold: RefusalRun,
    cap: usize,
}

impl ReplyQueues {
    /// Empty queues, each capped at `cap` replies.
    pub(super) fn new(cap: usize) -> Self {
        Self { queues: Vec::new(), hold: RefusalRun::default(), cap }
    }

    /// Queue `out` behind its conversation's earlier replies. `Err` hands it
    /// back when that conversation already holds `cap` replies.
    pub(super) fn push(&mut self, out: OutgoingMessage) -> Result<(), OutgoingMessage> {
        match self.queues.iter_mut().find(|q| q.conversation == out.conversation) {
            Some(q) if q.replies.len() >= self.cap => Err(out),
            Some(q) => {
                q.replies.push_back(out);
                Ok(())
            }
            None => {
                self.queues.push(ConversationQueue {
                    conversation: out.conversation.clone(),
                    replies: VecDeque::from([out]),
                    run: RefusalRun::default(),
                    front_refusals: 0,
                });
                Ok(())
            }
        }
    }

    /// How many replies are queued, over every conversation.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queues.iter().map(|q| q.replies.len()).sum()
    }

    /// How many conversations have a queued reply.
    #[cfg(test)]
    pub(super) fn conversations(&self) -> usize {
        self.queues.len()
    }

    /// Forget the conversations whose queue is empty. Their refusal state goes
    /// with them, so the next reply to such a conversation starts a fresh run.
    fn prune(&mut self) {
        self.queues.retain(|q| !q.replies.is_empty());
    }
}

/// Pure: the line for a reply given up after `refusals` refusals over
/// `refusing_for`.
pub(super) fn format_gave_up_report(
    method: &str,
    conversation: &str,
    refusals: u32,
    refusing_for: Duration,
    error: &str,
) -> String {
    format!(
        "gave up on a reply to conversation {conversation} after {refusals} refusals of \
         {method} (the conversation has been refusing for {} s); recorded as \
         channel.reply_undelivered. Last refusal: {error}",
        refusing_for.as_secs()
    )
}

/// Pure: the line for a reply dropped because its conversation's queue is full.
pub(super) fn format_overflow_report(conversation: &str, cap: usize) -> String {
    format!(
        "dropped a new reply to conversation {conversation}: {cap} replies are already queued \
         behind a refused one; recorded as channel.reply_undelivered"
    )
}

/// Queue `out`, or — when its conversation is full — drop it, say so, and
/// audit it.
pub(super) fn enqueue(
    queues: &mut ReplyQueues,
    out: OutgoingMessage,
    label: &str,
    audit: Option<&ReplyUndeliveredAudit>,
) {
    if let Err(dropped) = queues.push(out) {
        let report = format_overflow_report(&dropped.conversation.0, queues.cap);
        emit_worker_refusal_report(label, &report, RefusalSeverity::Warn);
        if let Some(audit) = audit {
            audit(&dropped);
        }
    }
}

/// Send what can be sent: each conversation front-first, stopping that
/// conversation at its first refusal. Returns `true` when the worker is
/// **down** (the caller skips the poll).
///
/// Skipped entirely while a credential refusal's backoff runs, and a
/// conversation is skipped while its own backoff runs.
pub(super) fn flush(
    queues: &mut ReplyQueues,
    calls: &dyn WorkerCalls,
    spec: &PolledWorkerSpec,
    encode_send: EncodeSend,
    outage: &mut OutageLog,
    audit: Option<&ReplyUndeliveredAudit>,
) -> bool {
    let method = spec.send_method;
    let mut down = false;
    if queues.hold.ready(Instant::now()) {
        'conversations: for q in queues.queues.iter_mut() {
            if !q.run.ready(Instant::now()) {
                continue;
            }
            while let Some(out) = q.replies.front() {
                let conversation = q.conversation.0.as_str();
                match calls.call(method, encode_send(out)) {
                    Ok(_) => {
                        accepted(&mut queues.hold, outage, spec.label, method, None);
                        accepted(&mut q.run, outage, spec.label, method, Some(conversation));
                        q.replies.pop_front();
                        q.front_refusals = 0;
                    }
                    Err(e) if is_upstream_auth_refusal(&e) => {
                        // The whole channel's problem: hold every conversation,
                        // and count nothing against this reply.
                        refused(&mut queues.hold, outage, spec, method, None, &e);
                        break 'conversations;
                    }
                    Err(e) if is_refusal(&e) => {
                        on_reply_refused(q, outage, spec, &e, audit);
                        break;
                    }
                    Err(e) => {
                        if outage.on_down() {
                            report_down(spec.label, &e, "send failed; retrying after respawn");
                        }
                        down = true;
                        break 'conversations;
                    }
                }
            }
        }
    }
    queues.prune();
    down
}

/// The reply at the front of `q` was refused: pace the conversation, then
/// either log the refusal (if due) or, past the give-up bound, drop the reply,
/// log that instead, and audit it.
fn on_reply_refused(
    q: &mut ConversationQueue,
    outage: &mut OutageLog,
    spec: &PolledWorkerSpec,
    e: &anyhow::Error,
    audit: Option<&ReplyUndeliveredAudit>,
) {
    let now = Instant::now();
    q.front_refusals = q.front_refusals.saturating_add(1);
    let since = q.run.since().unwrap_or(now);
    if !give_up_due(q.front_refusals, since, now, &spec.reply_give_up) {
        refused(&mut q.run, outage, spec, spec.send_method, Some(&q.conversation.0), e);
        return;
    }
    // Still a refusal: the worker is up, and the next reply in this
    // conversation waits out the (unreset) backoff. The give-up line below
    // replaces the refusal line, which would say "retrying" about a reply that
    // is not retried.
    note_answer(outage, spec.label);
    let _ = q.run.on_refusal(now, &spec.refusal_backoff, refusal_code(e));
    let refusals = std::mem::take(&mut q.front_refusals);
    let Some(dropped) = q.replies.pop_front() else { return };
    let report = format_gave_up_report(
        spec.send_method,
        &q.conversation.0,
        refusals,
        now.saturating_duration_since(since),
        &e.to_string(),
    );
    emit_worker_refusal_report(spec.label, &report, RefusalSeverity::Warn);
    if let Some(audit) = audit {
        audit(&dropped);
    }
}
