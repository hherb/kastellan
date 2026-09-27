//! Channel-generic driver for a long-lived, pull-only worker supervised by
//! [`PersistentWorker`]: owns the autonomous long-poll loop, surfaces the
//! worker's login identity at startup, and retains queued outbound messages
//! across a worker respawn (no dropped replies). The supervisor underneath
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
//! leaving the cursor unadvanced (same redelivery outcome as a crash). The one
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

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc as tok_mpsc;

use crate::worker_lifecycle::persistent::PersistentHandle;

mod outage;
use outage::{is_upstream_auth_refusal, report_call_failure, OutageLog};

use super::{ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerEvidence, PeerId};

/// Bounded depth of the inbound buffer between the driver thread and the bus.
/// Matches the Matrix channel's historical value; a single-user channel never
/// reaches it (the driver `blocking_send`s past it — backpressure, not drop).
const INBOUND_BUFFER: usize = 256;

/// How long the driver sleeps between retries while the worker is down (the
/// supervisor is respawning it underneath). Short so recovery latency is low;
/// the shutdown check runs every slice so a dead channel's thread exits fast.
const RETRY_SLICE: Duration = Duration::from_millis(200);

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

/// Best-effort side channel for a caller to record "this id was discarded
/// without ever becoming a bus event" somewhere durable (e.g. an
/// `audit_log` row) — called once per skipped id actually acked, as
/// `audit(message_id, reason)`. The driver itself stays DB-free by design
/// (see the module docs); this is a boxed closure rather than a bare `fn`
/// pointer specifically so a caller CAN capture state (a `PgPool` +
/// `tokio::runtime::Handle`, following the exact pattern
/// `crate::egress::net_worker::pg_decision_sink` already uses to drive an
/// async DB insert from a synchronous background thread). `None` means no
/// audit call is ever made — the default, and Matrix's case (it never
/// supplies a `parse_ack_only` either, so this is moot for it).
pub type AckOnlyAudit = Box<dyn Fn(&str, &str) + Send + 'static>;

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
        audit_ack_only: Option<AckOnlyAudit>,
        cid: ChannelId,
    ) -> anyhow::Result<(Self, serde_json::Value)> {
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
                audit_ack_only,
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
/// 1. drain queued outbound messages into `pending` (non-blocking);
/// 2. flush `pending` front-first, stopping at the first error — unacked
///    messages STAY in `pending`, so a death mid-send loses nothing;
/// 3. long-poll for inbound events, forward them to the bus, then — for a
///    spec with `ack_method` set — ack the ones that carried an `ack_token`;
/// 4. on any call error, sleep one short slice (shutdown-responsive) and
///    retry — the supervisor is respawning the worker underneath.
#[allow(clippy::too_many_arguments)] // mirrors spawn's own descriptor args + the two channel endpoints
fn run(
    calls: Box<dyn WorkerCalls>,
    spec: PolledWorkerSpec,
    parse_poll: ParsePoll,
    encode_send: EncodeSend,
    encode_ack: Option<EncodeAck>,
    parse_ack_only: Option<ParseAckOnly>,
    audit_ack_only: Option<AckOnlyAudit>,
    inbound_tx: tok_mpsc::Sender<IncomingMessage>,
    outbound_rx: std_mpsc::Receiver<OutgoingMessage>,
    cid: ChannelId,
) {
    let mut pending: VecDeque<OutgoingMessage> = VecDeque::new();
    // Latches the down/up transitions so they log once per outage instead of
    // once per retry slice — except a credential refusal, which has its own
    // cadence (see `OutageLog`).
    let mut outage = OutageLog::default();
    loop {
        // 1) Pull newly-queued replies into the local buffer (non-blocking).
        loop {
            match outbound_rx.try_recv() {
                Ok(out) => pending.push_back(out),
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    tracing::info!(label = spec.label, "outbound sender dropped; polled driver exiting");
                    return;
                }
            }
        }

        // 2) Flush buffered replies (front-first); stop at the first error.
        let mut errored = false;
        while let Some(out) = pending.front() {
            match calls.call(spec.send_method, encode_send(out)) {
                Ok(_) => {
                    if outage.on_success() {
                        tracing::info!(label = spec.label, "worker back up; polled driver resumed");
                    }
                    pending.pop_front();
                }
                Err(e) => {
                    let line = outage.on_failure(is_upstream_auth_refusal(&e), Instant::now());
                    report_call_failure(spec.label, &e, "send failed; retrying after respawn", line);
                    errored = true;
                    break;
                }
            }
        }

        // 3) Long-poll for inbound events → push to the bus.
        if !errored {
            match calls.call(spec.poll_method, serde_json::json!({ "timeout_ms": spec.poll_timeout_ms })) {
                Ok(v) => {
                    if outage.on_success() {
                        tracing::info!(label = spec.label, "worker back up; polled driver resumed");
                    }
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
                                if let (Some(method), Some(enc), Some(tok)) =
                                    (spec.ack_method, encode_ack, ack_token.as_deref())
                                {
                                    if let Err(e) = calls.call(method, enc(tok)) {
                                        tracing::warn!(label = spec.label, error = %e, "ack failed; event will be redelivered");
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
                            if let (Some(method), Some(enc)) = (spec.ack_method, encode_ack) {
                                for (id, reason) in ack_only_ids {
                                    tracing::warn!(
                                        label = spec.label,
                                        message_id = %id,
                                        reason = %reason,
                                        "discarding a message that never became an event; acking it \
                                         so the worker's cursor advances"
                                    );
                                    // Best-effort audit trail: never FAILS the
                                    // ack itself — a real hook only logs on its
                                    // own insert error (see AckOnlyAudit's docs
                                    // — `None` when the caller has no durable
                                    // sink to write to). It CAN block this
                                    // thread for the duration of the write: a
                                    // production hook (e.g. the daemon's)
                                    // typically `Handle::block_on`s an async DB
                                    // insert, same as `pg_decision_sink`. That
                                    // is fine here — this is a dedicated
                                    // background thread (`thread::spawn` in
                                    // `PolledWorkerDriver::spawn`), never a
                                    // tokio worker thread.
                                    if let Some(audit) = &audit_ack_only {
                                        audit(&id, &reason);
                                    }
                                    if let Err(e) = calls.call(method, enc(&id)) {
                                        tracing::warn!(
                                            label = spec.label,
                                            error = %e,
                                            message_id = %id,
                                            "ack of skipped id failed; will retry next poll"
                                        );
                                    }
                                }
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
                Err(e) => {
                    let line = outage.on_failure(is_upstream_auth_refusal(&e), Instant::now());
                    report_call_failure(spec.label, &e, "poll failed (worker died or restarting)", line);
                    errored = true;
                }
            }
        }

        // 4) Worker down: the supervisor owns respawn/backoff/alarm; just wait
        //    a short, shutdown-responsive slice and retry.
        if errored {
            if inbound_tx.is_closed() {
                tracing::info!(label = spec.label, "inbound receiver closed during retry; polled driver exiting");
                return;
            }
            thread::sleep(RETRY_SLICE);
        }
    }
}

#[cfg(test)]
mod tests;
