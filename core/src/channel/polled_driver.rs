//! Channel-generic driver for a long-lived, pull-only worker supervised by
//! [`PersistentWorker`]: owns the autonomous long-poll loop, surfaces the
//! worker's login identity at startup, and retains queued outbound messages
//! across a worker respawn (a respawn drops no reply). Replies are queued per
//! conversation, and one the upstream keeps refusing is given up and audited
//! (#782) — the delivery contract is in `replies.rs`. The supervisor underneath
//! owns spawn/respawn/backoff/alarm; this driver only *calls* the worker and
//! retries through the supervisor's `"is restarting"` window.
//!
//! Matrix is the first consumer (`channel/matrix.rs`); IMAP/Telegram channel
//! workers (Phase 2) instantiate the same driver with their own
//! [`PolledWorkerSpec`] + parse/encode fns. Design + trade-offs:
//! `docs/superpowers/specs/2026-07-02-firecracker-microvm-slice5b4-matrix-in-vm-design.md`.
//!
//! ## Optional ack support
//!
//! Some polled transports (the email fallback channel) keep their polling
//! cursor server-side: the mail service only stops re-sending a message once
//! the worker explicitly acks it. Matrix has no such cursor, so ack support is
//! *optional* — a [`PolledWorkerSpec`] with `ack_method: None` (Matrix's spec)
//! makes the driver skip the ack step entirely, with no extra RPC and no
//! change to control flow versus before this existed.
//!
//! Why the ack fires *after* the event is handed to the bus, not before: if
//! the worker died between receiving the poll result and the driver forwarding
//! it, an ack sent first would advance the cursor for a message the bus never
//! got — a silent drop. Acking after `inbound_tx.blocking_send` returns `Ok`
//! means the worst case on a crash is redelivery (the message is re-sent next
//! poll because the cursor didn't move), never loss. That is an intentional
//! at-least-once contract, not an oversight — do not "fix" it into
//! exactly-once without a receipt protocol on the bus side too.
//!
//! A failed ack call is itself non-fatal: it's logged and the loop continues,
//! leaving the cursor unadvanced (same redelivery outcome as a crash; a
//! *refused* ack also holds the next poll — see `run`). The one
//! residual gap is structural and shared with Matrix's existing behaviour: if
//! the bus *accepts* the send but a downstream consumer later fails to fully
//! process it, the message is still acked (Matrix already drops in the
//! equivalent case, logging "channel enqueue failed; message dropped") — this
//! driver does not invent a receipt protocol to close that gap for one
//! channel.
//!
//! ## Acking ids that never become an event
//!
//! Some polled workers report a second list alongside their events: messages
//! they could not turn into a [`PolledEvent`] at all (email-in's `skipped` —
//! no usable `From`, a failed per-message detail fetch). Those ids still sit
//! behind the worker's server-side cursor; if nothing ever acks them the
//! cursor wedges on the first one forever and the channel goes permanently
//! silent. Threading them through as bogus `PolledEvent`s would be worse (a
//! fabricated inbound message with no real content reaching the bus), so
//! [`ParseAckOnly`] is a second, optional extractor run against the *same*
//! raw poll [`serde_json::Value`] `parse_poll` sees, purely to list ids to
//! ack — `parse_poll` itself stays a pure, events-only decode. `None`
//! (Matrix's case) means this extraction step never runs at all: no extra
//! RPC, byte-identical to before this existed.
//!
//! **The ack calls themselves only fire once the same batch's `events`
//! decoded successfully** (i.e. inside `parse_poll`'s `Ok` arm), even though
//! the *extraction* runs unconditionally on the raw value beforehand. This is
//! not a stylistic choice: the worker's ack advances one MONOTONIC
//! high-water-mark cursor shared by every message in the poll result —
//! `events` and `skipped` are two views over positions on that same cursor,
//! not two independent counters. Acking a `skipped` id from a batch whose
//! `events` failed to decode would drag that shared cursor past whatever
//! those undecoded events were, and unlike a failed ack (which just leaves
//! the cursor short, redelivering everything behind it), an advanced cursor
//! can never be wound back — the messages are gone for good, not merely
//! delayed.

use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc as tok_mpsc;

use crate::worker_lifecycle::persistent::PersistentHandle;
use crate::worker_lifecycle::RestartBackoff;

mod ack;
use ack::{ack, ack_skipped};
mod outage;
use outage::{report_down, OutageLog};
mod audit;
pub use audit::{AckOnlyAudit, DriverAudit, ReplyUndeliveredAudit};
mod refusal;
use refusal::{accepted, check_refusal_backoff, is_refusal, refused, RefusalRun};
mod replies;
pub use replies::{ReplyGiveUp, REPLY_GIVE_UP};
use replies::{
    check_reply_give_up, discard_on_exit, enqueue, flush, ReplyQueues, MAX_QUEUED_PER_CONVERSATION,
};

use super::{ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerEvidence, PeerId};

/// Bounded depth of the inbound buffer between the driver thread and the bus.
/// Matches the Matrix channel's historical value; a single-user channel never
/// reaches it (the driver `blocking_send`s past it — backpressure, not drop).
const INBOUND_BUFFER: usize = 256;

/// How long the driver sleeps between iterations that completed no poll: the
/// worker is down (the supervisor is respawning it underneath), or a refused
/// poll or ack is waiting out its backoff — so it is also the resolution at
/// which such a backoff is honoured. Short so recovery latency is low; the
/// shutdown check runs every slice so a dead channel's thread exits fast.
const RETRY_SLICE: Duration = Duration::from_millis(200);

/// How a channel paces retries of a call its worker **refused** (#769): 1 s,
/// doubling, capped at 60 s. The channels' specs use it; see
/// [`PolledWorkerSpec::refusal_backoff`].
///
/// Why these numbers: the base matches the ~1 s the supervisor's respawn used
/// to add before each retry, so a transient refused poll (a network blip
/// through the egress tunnel) is retried about as soon as before. (A refused
/// *send* is retried at the first flush after its delay, and a flush follows
/// each long-poll, so it can wait up to one poll window longer.) The cap bounds
/// a lasting refusal — an expired credential, a room the bot was removed from —
/// to one request a minute against an upstream that keeps saying no.
pub const REFUSAL_BACKOFF: RestartBackoff = RestartBackoff {
    base: Duration::from_secs(1),
    factor_num: 2,
    factor_den: 1,
    cap: Duration::from_secs(60),
};

/// What a channel-shaped worker looks like to the driver: three JSON-RPC
/// methods plus the worker-side long-poll wait.
#[derive(Clone, Copy, Debug)]
pub struct PolledWorkerSpec {
    /// Log label (also a good supervisor label), e.g. `"matrix"`.
    pub label: &'static str,
    /// Identity/login-proof method, called once at spawn (e.g. `matrix.init`).
    pub init_method: &'static str,
    /// Long-poll method; params are `{"timeout_ms": <poll_timeout_ms>}`.
    pub poll_method: &'static str,
    /// Outbound-delivery method; params come from the `EncodeSend` fn.
    pub send_method: &'static str,
    /// Optional cursor-advance method, called once per inbound event right
    /// after the driver hands that event to the bus (see `run`, step 3).
    /// `None` for a worker with no server-side polling cursor to advance —
    /// Matrix sets this to `None`, so it never gets the extra RPC and its
    /// control flow is byte-identical to before this field existed.
    pub ack_method: Option<&'static str>,
    /// Worker-side long-poll wait. Outbound latency is bounded by this (the
    /// single JSON-RPC pipe serializes poll and send).
    pub poll_timeout_ms: u64,
    /// How long to wait before calling a method again after the worker
    /// refused it, per consecutive refusal of that method (#769) — for a send,
    /// per conversation (#782). Production specs use [`REFUSAL_BACKOFF`].
    pub refusal_backoff: RestartBackoff,
    /// When to give up on a reply the worker keeps refusing (#782).
    /// Production specs use [`REPLY_GIVE_UP`].
    pub reply_give_up: ReplyGiveUp,
}

/// One inbound event as the channel layer sees it, before the driver stamps
/// its [`ChannelId`] on. Produced by a [`ParsePoll`] fn from the poll result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolledEvent {
    pub peer: String,
    pub conversation: String,
    pub body: String,
    /// Transport-supplied authenticity evidence, carried straight through to
    /// the [`IncomingMessage`] the driver builds. `None` for transports that
    /// authenticate their own peers (Matrix — see `matrix::wire::parse_matrix_poll`).
    pub evidence: Option<PeerEvidence>,
    /// A per-message acknowledgement token some polled transports need echoed
    /// back on their next send (e.g. an email fallback worker's delivery ack).
    /// Unused by Matrix.
    pub ack_token: Option<String>,
}

/// Decode one poll RESULT into events. A decode error marks the batch as a
/// worker bug (logged + skipped), NOT a worker death.
pub type ParsePoll = fn(serde_json::Value) -> anyhow::Result<Vec<PolledEvent>>;

/// Encode one outbound message into the send method's params.
pub type EncodeSend = fn(&OutgoingMessage) -> serde_json::Value;

/// Encode one event's [`PolledEvent::ack_token`] into the ack method's params
/// (e.g. `{"cursor": tok}`). Only called when both `PolledWorkerSpec::ack_method`
/// and the event's own `ack_token` are present — see `run`.
pub type EncodeAck = fn(&str) -> serde_json::Value;

/// Extract `(id, reason)` pairs to acknowledge that never became a
/// [`PolledEvent`] at all — see the module docs' "Acking ids that never
/// become an event". Run against the raw poll [`serde_json::Value`], in
/// addition to (and before) `parse_poll` consumes it. Extraction itself is
/// unconditional, but `run` only actually *acks* the resulting ids when the
/// same batch's `parse_poll` call also succeeded — see `run` and the module
/// docs' monotonic-cursor note. Only ever invoked when
/// `PolledWorkerSpec::ack_method` and an `EncodeAck` are both present too.
/// `reason` is a short, static-ish diagnostic (never message content) — it is
/// only ever used for a log line and, when supplied, an [`AckOnlyAudit`] call.
pub type ParseAckOnly = fn(&serde_json::Value) -> Vec<(String, String)>;

/// Seam over "something that can call the worker" so the driver is unit-tested
/// without a supervisor or a process. Production is [`PersistentHandle`].
pub trait WorkerCalls: Send + 'static {
    fn call(&self, method: &str, params: serde_json::Value)
        -> anyhow::Result<serde_json::Value>;
}

impl WorkerCalls for PersistentHandle {
    fn call(&self, method: &str, params: serde_json::Value)
        -> anyhow::Result<serde_json::Value> {
        PersistentHandle::call(self, method, params)
    }
}

/// A running polled-worker driver: the endpoints a channel wraps. Dropping
/// both endpoints stops the driver thread, which drops its [`WorkerCalls`] —
/// for a [`PersistentHandle`] that is the supervisor shutdown (worker + any
/// sidecar torn down via RAII).
pub struct PolledWorkerDriver {
    pub(crate) inbound_rx: tok_mpsc::Receiver<IncomingMessage>,
    pub(crate) outbound_tx: std_mpsc::Sender<OutgoingMessage>,
    pub(crate) join: thread::JoinHandle<()>,
}

impl PolledWorkerDriver {
    /// Call `init_method` once (blocking — the synchronous login-proof
    /// contract; the returned JSON is the worker identity), then start the
    /// driver thread. Fails when init fails: the caller gets no half-alive
    /// channel. The worker process itself is parented to the SUPERVISOR's
    /// persistent thread (PDEATHSIG-safe, #348) — this call only issues RPCs.
    #[allow(clippy::too_many_arguments)] // one descriptor arg per wire concern; grouping would obscure call sites
    pub fn spawn(
        spec: PolledWorkerSpec,
        calls: Box<dyn WorkerCalls>,
        parse_poll: ParsePoll,
        encode_send: EncodeSend,
        encode_ack: Option<EncodeAck>,
        parse_ack_only: Option<ParseAckOnly>,
        audit: DriverAudit,
        cid: ChannelId,
    ) -> anyhow::Result<(Self, serde_json::Value)> {
        check_refusal_backoff(&spec.refusal_backoff)?;
        check_reply_give_up(&spec.reply_give_up)?;
        let identity = calls
            .call(spec.init_method, serde_json::json!({}))
            .map_err(|e| anyhow::anyhow!("{}: {e}", spec.init_method))?;
        let (inbound_tx, inbound_rx) = tok_mpsc::channel::<IncomingMessage>(INBOUND_BUFFER);
        let (outbound_tx, outbound_rx) = std_mpsc::channel::<OutgoingMessage>();
        let join = thread::spawn(move || {
            run(
                calls,
                spec,
                parse_poll,
                encode_send,
                encode_ack,
                parse_ack_only,
                audit,
                inbound_tx,
                outbound_rx,
                cid,
            )
        });
        Ok((Self { inbound_rx, outbound_tx, join }, identity))
    }
}

/// The driver loop. Direct port of the Matrix channel's historical `drive()`
/// semantics minus its respawn state machine (the supervisor owns that now):
/// 1. drain queued outbound messages into their conversation's queue
///    (non-blocking; see [`replies`]);
/// 2. flush each conversation front-first, stopping that conversation at its
///    first refusal and everything at the first death or channel-wide refusal
///    (a refused credential, an unavailable upstream: #782) — unsent messages
///    STAY queued, so a death mid-send loses nothing;
/// 3. long-poll for inbound events, forward them to the bus, then — for a
///    spec with `ack_method` set — ack the ones that carried an `ack_token`;
/// 4. unless a poll just completed (its long-poll paces the loop), sleep one
///    short slice (shutdown-responsive) and go round again.
///
/// The driver exits when either channel endpoint is dropped (the channel
/// restarting or shutting down). Replies still queued then are dropped, with a
/// line per conversation and an audit call per reply (`replies::discard_on_exit`).
///
/// A failed call is one of two things (#769), told apart by
/// [`refusal::is_refusal`]:
/// - **the worker is down** — the supervisor is respawning it. Stop, skip the
///   poll, retry after one slice; [`OutageLog`] logs the outage once.
/// - **the worker refused the call** — it is alive and was kept. That method
///   waits out its own backoff ([`RefusalRun`], `spec.refusal_backoff`) while
///   the others keep going: a refused reply stays at the front of its
///   conversation's queue, in order, while polling and every other
///   conversation continue, until it is accepted or given up (#782). A refused
///   **ack** stops acking the batch and holds
///   the next poll too: the cursor did not move, so polling would only hand the
///   same messages to the bus again, as fast as the upstream answers.
#[allow(clippy::too_many_arguments)] // mirrors spawn's own descriptor args + the two channel endpoints
fn run(
    calls: Box<dyn WorkerCalls>,
    spec: PolledWorkerSpec,
    parse_poll: ParsePoll,
    encode_send: EncodeSend,
    encode_ack: Option<EncodeAck>,
    parse_ack_only: Option<ParseAckOnly>,
    audit: DriverAudit,
    inbound_tx: tok_mpsc::Sender<IncomingMessage>,
    outbound_rx: std_mpsc::Receiver<OutgoingMessage>,
    cid: ChannelId,
) {
    let DriverAudit { ack_only: audit_ack_only, reply_undelivered } = audit;
    let mut replies = ReplyQueues::new(MAX_QUEUED_PER_CONVERSATION);
    // Latches the down/up transitions so they log once per outage instead of
    // once per retry slice (see `OutageLog`).
    let mut outage = OutageLog::default();
    // Each method's run of refusals from a live worker (#769) — separate,
    // because a homeserver can refuse a send while every poll succeeds. Sends
    // keep theirs per conversation, inside `replies` (#782).
    let mut poll_refusals = RefusalRun::default();
    let mut ack_refusals = RefusalRun::default();
    loop {
        // 1) Pull newly-queued replies into the local buffer (non-blocking).
        loop {
            match outbound_rx.try_recv() {
                Ok(out) => enqueue(&mut replies, out, spec.label, reply_undelivered.as_ref()),
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    tracing::info!(label = spec.label, "outbound sender dropped; polled driver exiting");
                    discard_on_exit(&mut replies, [], spec.label, reply_undelivered.as_ref());
                    return;
                }
            }
        }

        // 2) Flush buffered replies, each conversation front-first (#782).
        let down = flush(
            &mut replies,
            &*calls,
            &spec,
            encode_send,
            &mut outage,
            reply_undelivered.as_ref(),
        );

        // 3) Long-poll for inbound events → push to the bus. Skipped while the
        //    worker is down, or while a refused poll or ack waits out its
        //    backoff.
        let mut polled = false;
        let now = Instant::now();
        if !down && poll_refusals.ready(now) && ack_refusals.ready(now) {
            match calls.call(spec.poll_method, serde_json::json!({ "timeout_ms": spec.poll_timeout_ms })) {
                Ok(v) => {
                    // The bus went away while the poll was out (the channel
                    // was stopped mid long-poll). Nothing in this batch can
                    // reach it, so ack none of it: an unacked batch is
                    // redelivered on the next start. Checked HERE, not only at
                    // an event's `blocking_send`, because a batch of skipped
                    // ids alone has no event to notice the closed bus — and
                    // would ack those ids and write their audit rows into a
                    // daemon that is shutting down, losing both (#792).
                    if inbound_tx.is_closed() {
                        tracing::info!(
                            label = spec.label,
                            "inbound receiver closed during a poll; polled driver exiting \
                             without acking the batch (it is redelivered)"
                        );
                        discard_on_exit(&mut replies, outbound_rx.try_iter(), spec.label, reply_undelivered.as_ref());
                        return;
                    }
                    polled = true;
                    accepted(&mut poll_refusals, &mut outage, spec.label, spec.poll_method, None);
                    // Cleared by an ack refusal: the rest of the batch would be
                    // refused the same way.
                    let mut acking = true;
                    // Extracted BEFORE `parse_poll` consumes `v` by value —
                    // both extractors see the identical raw poll result.
                    let ack_only_ids = parse_ack_only.map(|f| f(&v)).unwrap_or_default();
                    match parse_poll(v) {
                        Ok(events) => {
                            for ev in events {
                                // Captured before `ev`'s other fields move into
                                // `msg` below — a partial move, not a clone.
                                let ack_token = ev.ack_token;
                                let msg = IncomingMessage {
                                    channel: cid.clone(),
                                    peer: PeerId(ev.peer),
                                    conversation: ConversationId(ev.conversation),
                                    body: ev.body,
                                    evidence: ev.evidence,
                                };
                                if inbound_tx.blocking_send(msg).is_err() {
                                    tracing::info!(label = spec.label, "inbound receiver closed; polled driver exiting");
                                    discard_on_exit(&mut replies, outbound_rx.try_iter(), spec.label, reply_undelivered.as_ref());
                                    return;
                                }

                                // Ack only after the bus has accepted the event
                                // (the blocking_send above returned Ok), so a
                                // worker death between poll and hand-off
                                // redelivers the message on the next poll
                                // rather than silently dropping it.
                                //
                                // Known residual (matches Matrix's existing
                                // "channel enqueue failed; message dropped"
                                // semantics rather than inventing a receipt
                                // protocol): if the bus later fails downstream
                                // of this accept, the message is acked but
                                // lost. A failed ack call itself is also
                                // non-fatal — it just leaves the worker's
                                // cursor unadvanced, so the message is
                                // redelivered. At-least-once, by design.
                                if let (true, Some(method), Some(enc), Some(tok)) =
                                    (acking, spec.ack_method, encode_ack, ack_token.as_deref())
                                {
                                    match ack(&*calls, &spec, method, enc(tok), &mut outage, &mut ack_refusals) {
                                        Ok(keep) => acking = keep,
                                        Err(e) => tracing::warn!(label = spec.label, error = %e, "ack failed; event will be redelivered"),
                                    }
                                }
                            }

                            // Ack ids that never became a `PolledEvent` at all
                            // (e.g. email's `skipped` list) — see the module
                            // docs' "Acking ids that never become an event".
                            // Deliberately INSIDE the `Ok(events)` arm, not
                            // after the whole `match`: the worker's polling
                            // cursor is a single MONOTONIC high-water mark
                            // shared by both lists (localmail's `GREATEST`
                            // cursor), not two independent counters. Acking a
                            // skipped id after a batch whose `events` FAILED
                            // to decode would advance that shared cursor past
                            // messages the bus never saw, permanently losing
                            // them (they can never be redelivered once the
                            // cursor has passed them) — so this only ever
                            // runs once the events in the SAME batch are
                            // confirmed handed to the bus. Only when the spec
                            // actually supports acking at all; a `None`
                            // `parse_ack_only` (Matrix) means `ack_only_ids`
                            // is always empty, so this loop never runs for
                            // Matrix — byte-identical.
                            if let (true, Some(enc)) = (acking, encode_ack) {
                                ack_skipped(&*calls, &spec, enc, ack_only_ids, audit_ack_only.as_ref(), &cid, &mut outage, &mut ack_refusals);
                            }
                        }
                        Err(e) => {
                            // A malformed poll result is a worker bug, not a
                            // death — log + skip the batch, keep polling. The
                            // skipped ids from THIS batch are deliberately NOT
                            // acked here (see the comment above the ack-only
                            // loop): they share the worker's one monotonic
                            // cursor with the events that just failed to
                            // decode, and acking them would silently drag
                            // that cursor past messages nobody ever saw.
                            tracing::warn!(label = spec.label, error = %e, "poll result decode failed; batch skipped");
                        }
                    }
                }
                // A failed poll is a failure of the whole channel: no
                // conversation's give-up clock runs through it (#782).
                Err(e) if is_refusal(&e) => {
                    refused(&mut poll_refusals, &mut outage, &spec, spec.poll_method, None, &e);
                    replies.restart_give_up_clocks();
                }
                Err(e) => {
                    if outage.on_down() {
                        report_down(spec.label, &e, "poll failed (worker died or restarting)");
                    }
                    replies.restart_give_up_clocks();
                }
            }
        }

        // 4) No poll completed — the worker is down (the supervisor owns
        //    respawn/backoff/alarm), or the poll was refused or is held by a
        //    poll or ack refusal backoff: wait a short, shutdown-responsive
        //    slice and go round again. A completed poll needs no wait; its
        //    long-poll already paced this iteration.
        if !polled {
            if inbound_tx.is_closed() {
                tracing::info!(label = spec.label, "inbound receiver closed during retry; polled driver exiting");
                discard_on_exit(&mut replies, outbound_rx.try_iter(), spec.label, reply_undelivered.as_ref());
                return;
            }
            thread::sleep(RETRY_SLICE);
        }
    }
}

#[cfg(test)]
mod tests;
