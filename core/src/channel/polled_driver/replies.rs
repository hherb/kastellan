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
//!   with its conversation, and handed to the caller's audit hook when there
//!   is one (the daemon's Matrix channel has one), which writes
//!   `channel.reply_undelivered`. That is the row the bus's `channel.replied`
//!   promises for a reply that did not land.
//! - **A failure of the whole channel holds every conversation and charges
//!   none of them.** A refused credential (`UPSTREAM_AUTH_FAILED`), an
//!   unreachable upstream (`UPSTREAM_UNAVAILABLE`), a dead worker, a failed
//!   poll: every send would fail the same way, so none of it is evidence
//!   against a conversation. It adds nothing to any reply's refusal count,
//!   and it **restarts** every conversation's give-up clock, so an outage —
//!   a homeserver down overnight, a token revoked — can never age a reply
//!   out. A conversation is given up only after `after` of refusals *with the
//!   channel otherwise answering*. The cost: a channel that fails more often
//!   than `after` never gives up on a dead room; its queue is still capped,
//!   and its refusal line still names it every 15 min.
//! - **Each conversation's queue is capped** at [`MAX_QUEUED_PER_CONVERSATION`].
//!   A reply past the cap is dropped and audited at once, so a stuck
//!   conversation cannot grow the queue without bound.
//! - **A driver that exits drops what is queued**, and says so: one line per
//!   conversation, and an audit call per reply ([`discard_on_exit`]).
//!
//! [`ReplyQueues`], [`ConversationQueue`]'s transitions and the decision fns
//! are pure (the clock is passed in); [`enqueue`], [`flush`] and
//! [`discard_on_exit`] call the worker or the audit hook and emit the lines
//! the pure parts chose.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::channel::{ConversationId, OutgoingMessage, UndeliveredReason};
use crate::worker_lifecycle::persistent::{classify_call_error, CallFailure};
use crate::worker_lifecycle::RestartBackoff;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

use super::outage::{note_answer, report_down, OutageLog};
use super::refusal::{accepted, refusal_code, refused, report_refusal, RefusalRun, Refused};
use super::{EncodeSend, PolledWorkerSpec, ReplyUndeliveredAudit, WorkerCalls};

/// When the driver gives up on a reply its worker keeps refusing (#782).
///
/// Both halves must hold. The time half is the real bound: it is measured
/// from the start of the conversation's current refusal run, which a
/// channel-wide failure restarts (see the module docs). The count half gives
/// every reply attempts of its own: without it, a reply queued behind one
/// just given up in a conversation refusing for an hour would be dropped at
/// its very first refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyGiveUp {
    /// How long the reply's conversation must have been refusing.
    pub after: Duration,
    /// How many times this reply must have been refused, the refusal that
    /// decides included. At least 2 ([`check_reply_give_up`]): a reply is
    /// never dropped at its first refusal.
    pub min_refusals: u32,
}

/// The production give-up bound: an hour of refusals, and at least three of
/// this reply.
///
/// Why an hour: it outlasts every transient refusal seen so far (a homeserver
/// restart, an egress blip), so a reply is only given up when retrying has
/// stopped being plausible. At the 60 s refusal-backoff cap that is about 60
/// attempts. A reply **already queued** behind a given-up one inherits the
/// conversation's hour, so it goes after its own three refusals (about three
/// minutes at the cap), not after another hour. A reply that arrives after the
/// conversation's queue emptied starts a fresh run: [`ReplyQueues`] forgets an
/// empty conversation, refusal state and all.
pub const REPLY_GIVE_UP: ReplyGiveUp =
    ReplyGiveUp { after: Duration::from_secs(60 * 60), min_refusals: 3 };

/// Most replies one conversation may queue. Past it, a new reply is dropped
/// and audited at once. Far above anything a person typing produces while a
/// reply is refused; it exists so the queue has a bound at all.
pub(super) const MAX_QUEUED_PER_CONVERSATION: usize = 256;

// A cap of 0 drops every reply; a small one drops the replies a person types
// while one reply is briefly refused (a homeserver restart takes minutes).
// 64 is a floor well above that, not a tuned value. Outside `#[cfg(test)]` on
// purpose, so a release build refuses such a value too.
const _: () = assert!(MAX_QUEUED_PER_CONVERSATION >= 64);

/// Pure: reject a give-up bound that would drop a reply at its first refusal
/// (`min_refusals` below 2 — the count includes the deciding refusal, so 0
/// and 1 both mean "no retry of its own"). Checked once, by
/// [`super::PolledWorkerDriver::spawn`].
pub(super) fn check_reply_give_up(g: &ReplyGiveUp) -> anyhow::Result<()> {
    anyhow::ensure!(
        g.min_refusals >= 2,
        "reply give-up: min_refusals must be at least 2, so a reply is never dropped at its \
         first refusal (got {})",
        g.min_refusals
    );
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

/// What a refusal of a conversation's front reply comes to.
#[derive(Debug)]
pub(super) enum OnRefused {
    /// Keep it; retry after the backoff. Carries whether to log it.
    Retry(Refused),
    /// Past the bound: the reply left the queue and is handed back.
    GaveUp {
        reply: OutgoingMessage,
        /// Its refusals, the deciding one included.
        refusals: u32,
        /// How long the conversation had been refusing.
        refusing_for: Duration,
    },
}

/// One conversation's queued replies, oldest first, and its refusal state.
#[derive(Debug)]
pub(super) struct ConversationQueue {
    conversation: ConversationId,
    replies: VecDeque<OutgoingMessage>,
    /// Pacing, log cadence and give-up clock for this conversation's sends.
    /// Ends when a send is accepted; its clock alone restarts on a
    /// channel-wide failure. A give-up does not end it, so a reply already
    /// queued behind the given-up one waits at the backoff cap, not from 1 s.
    /// It goes when the queue empties ([`ReplyQueues`] forgets the
    /// conversation).
    run: RefusalRun,
    /// Refusals of the reply at the front. Reset when it leaves the queue.
    front_refusals: u32,
}

impl ConversationQueue {
    fn new(out: OutgoingMessage) -> Self {
        Self {
            conversation: out.conversation.clone(),
            replies: VecDeque::from([out]),
            run: RefusalRun::default(),
            front_refusals: 0,
        }
    }

    /// The reply to send next.
    pub(super) fn front(&self) -> Option<&OutgoingMessage> {
        self.replies.front()
    }

    /// Pure: the front reply was accepted. It leaves the queue and its count
    /// goes with it; the caller ends the run ([`accepted`], which logs).
    fn on_front_accepted(&mut self) {
        self.replies.pop_front();
        self.front_refusals = 0;
    }

    /// Pure: the front reply was refused with `code` at `now`. Counts the
    /// refusal against the reply and the conversation's run, then keeps it
    /// ([`OnRefused::Retry`]) or, past `give_up`, drops it
    /// ([`OnRefused::GaveUp`]). The run is not reset by a give-up: see
    /// the `run` field's doc. A queue with no front reply has nothing to
    /// give up, and only paces.
    pub(super) fn on_front_refused(
        &mut self,
        now: Instant,
        give_up: &ReplyGiveUp,
        backoff: &RestartBackoff,
        code: i32,
    ) -> OnRefused {
        let r = self.run.on_refusal(now, backoff, code);
        self.front_refusals = self.front_refusals.saturating_add(1);
        let since = self.run.since().unwrap_or(now);
        if !give_up_due(self.front_refusals, since, now, give_up) {
            return OnRefused::Retry(r);
        }
        let Some(reply) = self.replies.pop_front() else {
            return OnRefused::Retry(r);
        };
        OnRefused::GaveUp {
            reply,
            refusals: std::mem::take(&mut self.front_refusals),
            refusing_for: now.saturating_duration_since(since),
        }
    }
}

/// Every conversation's queue, in the order each conversation's current queue
/// began, plus the channel-wide send hold. Pure.
#[derive(Debug)]
pub(super) struct ReplyQueues {
    queues: Vec<ConversationQueue>,
    /// A channel-wide send failure — a refused credential (#674), an
    /// unavailable upstream — holds every conversation; see the module docs.
    /// Paced and logged like any other refusal run.
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
                self.queues.push(ConversationQueue::new(out));
                Ok(())
            }
        }
    }

    /// A failure of the whole channel: restart every conversation's give-up
    /// clock. Backoffs and counts are kept; only the time half of
    /// [`ReplyGiveUp`] starts again at the next refusal. See the module docs.
    pub(super) fn restart_give_up_clocks(&mut self) {
        for q in &mut self.queues {
            q.run.restart_clock();
        }
    }

    /// Take every queued reply, grouped by conversation (in queue order,
    /// oldest first), leaving the queues empty. For a driver that exits.
    pub(super) fn drain(&mut self) -> Vec<(ConversationId, Vec<OutgoingMessage>)> {
        std::mem::take(&mut self.queues)
            .into_iter()
            .map(|q| (q.conversation, q.replies.into()))
            .collect()
    }

    /// How many replies are queued, over every conversation.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.queues.iter().map(|q| q.replies.len()).sum()
    }

    /// How many conversations have a queue.
    #[cfg(test)]
    pub(super) fn conversations(&self) -> usize {
        self.queues.len()
    }

    /// The queue of `conversation`, if it has one.
    #[cfg(test)]
    pub(super) fn queue_mut(&mut self, conversation: &str) -> Option<&mut ConversationQueue> {
        self.queues.iter_mut().find(|q| q.conversation.0 == conversation)
    }

    /// Forget the conversations whose queue is empty. Their refusal state goes
    /// with them, so the next reply to such a conversation starts a fresh run.
    fn prune(&mut self) {
        self.queues.retain(|q| !q.replies.is_empty());
    }
}

/// Pure: how a drop line ends, from whether the channel has an audit sink.
/// The sink is asynchronous and logs its own failure, so the line says the
/// row is being written, not that it was.
fn audit_clause(sink: bool) -> &'static str {
    if sink {
        "recording it as channel.reply_undelivered"
    } else {
        "NOT recorded as channel.reply_undelivered: this channel has no audit sink"
    }
}

/// Pure: the line for a reply given up after `refusals` refusals over
/// `refusing_for`. `sink`: whether the channel records it.
pub(super) fn format_gave_up_report(
    method: &str,
    conversation: &str,
    refusals: u32,
    refusing_for: Duration,
    error: &str,
    sink: bool,
) -> String {
    format!(
        "gave up on a reply to conversation {conversation} after {refusals} refusals of \
         {method} (the conversation has been refusing for {} s); {}. Last refusal: {error}",
        refusing_for.as_secs(),
        audit_clause(sink)
    )
}

/// Pure: the line for a reply dropped because its conversation's queue is full.
pub(super) fn format_overflow_report(conversation: &str, cap: usize, sink: bool) -> String {
    format!(
        "dropped a new reply to conversation {conversation}: {cap} replies are already queued \
         behind a refused one; {}",
        audit_clause(sink)
    )
}

/// Pure: the line for `n` replies to `conversation` dropped by a driver that
/// is exiting.
pub(super) fn format_exit_report(conversation: &str, n: usize, sink: bool) -> String {
    let each = if sink {
        "recording each as channel.reply_undelivered"
    } else {
        "NOT recorded as channel.reply_undelivered: this channel has no audit sink"
    };
    format!(
        "discarded {n} queued repl{} to conversation {conversation}: the driver is exiting (the \
         channel was restarted or shut down); {each}",
        if n == 1 { "y" } else { "ies" }
    )
}

/// Hand a dropped reply to the audit hook, if there is one.
fn record_undelivered(
    audit: Option<&ReplyUndeliveredAudit>,
    out: &OutgoingMessage,
    reason: UndeliveredReason,
) {
    if let Some(audit) = audit {
        audit(out, reason);
    }
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
        let report = format_overflow_report(&dropped.conversation.0, queues.cap, audit.is_some());
        emit_worker_refusal_report(label, &report, RefusalSeverity::Warn);
        record_undelivered(audit, &dropped, UndeliveredReason::QueueFull);
    }
}

/// The driver is exiting: every reply still queued, and every reply in `late`
/// (still in the outbound channel), will never be sent. Say so, one line per
/// conversation, and audit each reply.
pub(super) fn discard_on_exit(
    queues: &mut ReplyQueues,
    late: impl IntoIterator<Item = OutgoingMessage>,
    label: &str,
    audit: Option<&ReplyUndeliveredAudit>,
) {
    let mut groups = queues.drain();
    for out in late {
        match groups.iter_mut().find(|(c, _)| *c == out.conversation) {
            Some((_, replies)) => replies.push(out),
            None => groups.push((out.conversation.clone(), vec![out])),
        }
    }
    for (conversation, replies) in groups {
        let report = format_exit_report(&conversation.0, replies.len(), audit.is_some());
        emit_worker_refusal_report(label, &report, RefusalSeverity::Warn);
        for out in &replies {
            record_undelivered(audit, out, UndeliveredReason::DriverExit);
        }
    }
}

/// Send what can be sent: each conversation front-first, stopping that
/// conversation at its first refusal and every conversation at a
/// channel-wide failure. Returns `true` when the worker is **down** (the
/// caller skips the poll).
///
/// Skipped entirely while the channel-wide hold's backoff runs, and a
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
    let mut channel_wide = false;
    if queues.hold.ready(Instant::now()) {
        'conversations: for q in queues.queues.iter_mut() {
            if !q.run.ready(Instant::now()) {
                continue;
            }
            while let Some(out) = q.front() {
                let e = match calls.call(method, encode_send(out)) {
                    Ok(_) => {
                        accepted(&mut queues.hold, outage, spec.label, method, None);
                        accepted(&mut q.run, outage, spec.label, method, Some(&q.conversation.0));
                        q.on_front_accepted();
                        continue;
                    }
                    Err(e) => e,
                };
                match classify_call_error(&e) {
                    CallFailure::Refused => {
                        on_reply_refused(q, outage, spec, &e, audit);
                        break;
                    }
                    CallFailure::CredentialRefused | CallFailure::Unavailable => {
                        // The whole channel's problem: hold every
                        // conversation, and charge none of them.
                        refused(&mut queues.hold, outage, spec, method, None, &e);
                        channel_wide = true;
                        break 'conversations;
                    }
                    CallFailure::Gone => {
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
    if down || channel_wide {
        queues.restart_give_up_clocks();
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
    if note_answer(outage, spec.label) {
        q.run.rearm_report();
    }
    let outcome =
        q.on_front_refused(Instant::now(), &spec.reply_give_up, &spec.refusal_backoff, refusal_code(e));
    match outcome {
        OnRefused::Retry(r) => {
            report_refusal(spec.label, spec.send_method, Some(&q.conversation.0), e, &r);
        }
        OnRefused::GaveUp { reply, refusals, refusing_for } => {
            // Replaces the refusal line, which would say "retrying" about a
            // reply that is not retried.
            let report = format_gave_up_report(
                spec.send_method,
                &q.conversation.0,
                refusals,
                refusing_for,
                &e.to_string(),
                audit.is_some(),
            );
            emit_worker_refusal_report(spec.label, &report, RefusalSeverity::Warn);
            record_undelivered(audit, &reply, UndeliveredReason::GaveUp);
        }
    }
}
