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

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use tokio::sync::mpsc as tok_mpsc;

use crate::worker_lifecycle::persistent::PersistentHandle;

use super::{ChannelId, ConversationId, IncomingMessage, OutgoingMessage, PeerId};

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
}

/// Decode one poll RESULT into events. A decode error marks the batch as a
/// worker bug (logged + skipped), NOT a worker death.
pub type ParsePoll = fn(serde_json::Value) -> anyhow::Result<Vec<PolledEvent>>;

/// Encode one outbound message into the send method's params.
pub type EncodeSend = fn(&OutgoingMessage) -> serde_json::Value;

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
// The endpoints are read by this module's tests today; the Matrix channel
// adoption (next task in slice 5b-4a) is the first in-crate consumer, so the
// non-test lib compile sees them as unread until then.
#[allow(dead_code)]
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
    pub fn spawn(
        spec: PolledWorkerSpec,
        calls: Box<dyn WorkerCalls>,
        parse_poll: ParsePoll,
        encode_send: EncodeSend,
        cid: ChannelId,
    ) -> anyhow::Result<(Self, serde_json::Value)> {
        let identity = calls
            .call(spec.init_method, serde_json::json!({}))
            .map_err(|e| anyhow::anyhow!("{}: {e}", spec.init_method))?;
        let (inbound_tx, inbound_rx) = tok_mpsc::channel::<IncomingMessage>(INBOUND_BUFFER);
        let (outbound_tx, outbound_rx) = std_mpsc::channel::<OutgoingMessage>();
        let join =
            thread::spawn(move || run(calls, spec, parse_poll, encode_send, inbound_tx, outbound_rx, cid));
        Ok((Self { inbound_rx, outbound_tx, join }, identity))
    }
}

/// The driver loop. Direct port of the Matrix channel's historical `drive()`
/// semantics minus its respawn state machine (the supervisor owns that now):
/// 1. drain queued outbound messages into `pending` (non-blocking);
/// 2. flush `pending` front-first, stopping at the first error — unacked
///    messages STAY in `pending`, so a death mid-send loses nothing;
/// 3. long-poll for inbound events and forward them to the bus;
/// 4. on any call error, sleep one short slice (shutdown-responsive) and
///    retry — the supervisor is respawning the worker underneath.
fn run(
    calls: Box<dyn WorkerCalls>,
    spec: PolledWorkerSpec,
    parse_poll: ParsePoll,
    encode_send: EncodeSend,
    inbound_tx: tok_mpsc::Sender<IncomingMessage>,
    outbound_rx: std_mpsc::Receiver<OutgoingMessage>,
    cid: ChannelId,
) {
    let mut pending: VecDeque<OutgoingMessage> = VecDeque::new();
    // True while the last worker call failed — logs the down/up transitions
    // once instead of once per retry slice.
    let mut down = false;
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
                    if down {
                        tracing::info!(label = spec.label, "worker back up; polled driver resumed");
                        down = false;
                    }
                    pending.pop_front();
                }
                Err(e) => {
                    if !down {
                        tracing::warn!(label = spec.label, error = %e, "send failed; retrying after respawn");
                    }
                    errored = true;
                    break;
                }
            }
        }

        // 3) Long-poll for inbound events → push to the bus.
        if !errored {
            match calls.call(spec.poll_method, serde_json::json!({ "timeout_ms": spec.poll_timeout_ms })) {
                Ok(v) => {
                    if down {
                        tracing::info!(label = spec.label, "worker back up; polled driver resumed");
                        down = false;
                    }
                    match parse_poll(v) {
                        Ok(events) => {
                            for ev in events {
                                let msg = IncomingMessage {
                                    channel: cid.clone(),
                                    peer: PeerId(ev.peer),
                                    conversation: ConversationId(ev.conversation),
                                    body: ev.body,
                                };
                                if inbound_tx.blocking_send(msg).is_err() {
                                    tracing::info!(label = spec.label, "inbound receiver closed; polled driver exiting");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            // A malformed poll result is a worker bug, not a
                            // death — log + skip the batch, keep polling.
                            tracing::warn!(label = spec.label, error = %e, "poll result decode failed; batch skipped");
                        }
                    }
                }
                Err(e) => {
                    if !down {
                        tracing::warn!(label = spec.label, error = %e, "poll failed (worker died or restarting)");
                    }
                    errored = true;
                }
            }
        }

        // 4) Worker down: the supervisor owns respawn/backoff/alarm; just wait
        //    a short, shutdown-responsive slice and retry.
        if errored {
            down = true;
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
