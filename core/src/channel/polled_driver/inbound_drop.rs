//! Inbound messages lost because the bus closed — a channel restart or a
//! shutdown. Two stages, told apart on the row by [`InboundDropStage`]:
//!
//! - **[`InboundDropStage::Polled`]** (#826): the driver held a batch it could
//!   not hand over — the bus closed while a poll was out, or mid-batch.
//! - **[`InboundDropStage::Buffered`]** (#846): the driver had handed the
//!   messages over, but they were still in the inbound buffer, unread by the
//!   bus, when it stopped. Counted by the bus-side receiver as it is dropped
//!   (`inbound_rx.rs`).
//!
//! ## The polled stage
//!
//! Whether that is a **loss** depends on the channel:
//!
//! - **With an ack method** (email): the driver acks a message only after the
//!   bus took it, so whatever it could not hand over stays behind the worker's
//!   cursor and is redelivered on the next start. Not a loss; an INFO line.
//! - **Without one** (Matrix): the worker's own cursor has already moved past
//!   the batch, so nothing redelivers it. Before #826 that was a WARN with no
//!   count (or, mid-batch, an INFO) and no row — a peer's message vanished
//!   with nothing in `audit_log`. Now the driver counts what it drops, says so
//!   on the `[worker-refusal]` emitter (which falls back to stderr when
//!   `tracing` would not record the line), and hands the count to the
//!   channel's [`InboundDroppedAudit`] hook for a `channel.inbound_dropped`
//!   row.
//!
//! The row and the line carry the **count only**: never a peer, an id or a
//! body. Every one of those is peer-supplied, and a batch can span peers.
//!
//! An empty batch — the usual result of a long-poll that timed out — drops
//! nothing and says nothing.
//!
//! ## The buffered stage
//!
//! Here **both** channels lose the messages. A channel without an ack method
//! never redelivers; an ack channel acks each message as soon as the bus's
//! buffer accepts it (`polled_driver::run`), not when the bus reads it — so a
//! buffered message has already moved the worker's cursor past itself.
//!
//! ⚠️ Not every one, though, and the count cannot tell. An ack that fails, or
//! that a refused ack earlier in the same batch held back (a refusal stops
//! acking the rest of its batch, which is still handed over), leaves the
//! cursor short, so those messages ARE redelivered. After a refusal that can
//! be a whole batch tail. The receiver cannot see which buffered messages
//! were acked, so it counts them all and the line says so: over-counting is
//! the honest direction, because an unsaid loss is the defect this module
//! exists for.
//!
//! ## What this does not count
//!
//! A quiet `audit_log` after a restart is still not proof that nothing was
//! lost (a message the bus *had* read and was still handling is said on
//! `[audit-lost]` since #832). Outside both stages:
//!
//! - messages the worker had received but not yet returned from a poll when
//!   the driver exited — the live Matrix worker queues them in memory, and
//!   they die with it (#835);
//! - a batch that did not decode: dropped on a channel that does not
//!   redeliver, with a WARN but no count, because there is none to give
//!   ([`on_undecodable_batch`], #836).

use std::num::NonZeroUsize;

use super::audit::{record_inbound_dropped, InboundDroppedAudit};
use super::{ParsePoll, PolledWorkerSpec};
use crate::channel::actions;
use crate::channel::undelivered::observed_at_json;
use crate::channel::ChannelId;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

/// Where the dropped messages were when the bus closed: the row's `stage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundDropStage {
    /// In the driver's hands: a polled batch it could not hand to the closed
    /// bus (#826). Reported on a channel that does not redeliver only.
    Polled,
    /// In the bus's inbound buffer: handed over by the driver, never read by
    /// the bus (#846). Reported on every channel — see the module docs.
    Buffered,
}

impl InboundDropStage {
    /// The row's spelling, pinned: operators query `audit_log` by it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Polled => "polled",
            Self::Buffered => "buffered",
        }
    }
}

/// What the [`InboundDroppedAudit`] hook is told about a dropped batch: whose
/// channel, where the messages were, how many, and when — nothing a peer
/// wrote or chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboundDropped<'a> {
    pub channel: &'a ChannelId,
    /// Where they were when the bus closed.
    pub stage: InboundDropStage,
    /// How many messages the bus never saw. Never 0 — an empty batch is not
    /// reported — and the type says so.
    pub dropped: NonZeroUsize,
    /// When they were dropped. The row is written after the fact (#789), so
    /// `audit_log.ts` is the insert's time, not this.
    pub observed_at: time::OffsetDateTime,
}

impl InboundDropped<'_> {
    /// Pure: the payload of an [`actions::INBOUND_DROPPED`] row — the channel,
    /// the stage, the count and when, nothing else.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "channel": self.channel.0,
            "stage": self.stage.as_str(),
            "dropped": self.dropped,
            "observed_at": observed_at_json(self.observed_at),
        })
    }
}

/// Pure: the end of a drop line — whether the channel records it as a row.
fn recorded(sink: bool) -> String {
    let action = actions::INBOUND_DROPPED;
    if sink {
        format!("recording it as {action}")
    } else {
        format!("NOT recorded as {action}: this channel has no audit sink")
    }
}

/// Pure: the line for `n` inbound messages a channel that does not redeliver
/// dropped at the [`InboundDropStage::Polled`] stage. `sink` says whether the
/// channel records them as a row.
pub(super) fn format_inbound_drop_report(n: NonZeroUsize, sink: bool) -> String {
    let recorded = recorded(sink);
    let one = n.get() == 1;
    format!(
        "dropped {n} inbound message{}: the bus closed (the channel was restarted or shut down) \
         after the worker had passed {}, and this channel does not redeliver; {recorded}",
        if one { "" } else { "s" },
        if one { "it" } else { "them" },
    )
}

/// Pure: the line for `n` inbound messages dropped at the
/// [`InboundDropStage::Buffered`] stage (#846). `acks` says whether the
/// channel acks what it hands over (then the ack is why they are not
/// redelivered); `sink` whether it records them as a row.
pub(super) fn format_unread_drop_report(n: NonZeroUsize, acks: bool, sink: bool) -> String {
    let recorded = recorded(sink);
    let one = n.get() == 1;
    let them = if one { "it" } else { "them" };
    let why = if acks {
        format!(
            "this channel acked {them} when {} handed to the bus, so {} not redelivered (any \
             whose ack failed, or was held back by a refused ack earlier in its batch, is \
             redelivered, and counted here anyway)",
            if one { "it was" } else { "they were" },
            if one { "it is" } else { "they are" },
        )
    } else {
        "this channel does not redeliver".to_string()
    };
    format!(
        "dropped {n} inbound message{}: the bus stopped (the channel was restarted or shut down) \
         before reading {them} from its inbound buffer, and {why}; {recorded}",
        if one { "" } else { "s" },
    )
}

/// Say that `n` inbound messages on `cid` were dropped at `stage`, and hand
/// the count to the audit hook. Nothing for `n == 0`. `acks` is the channel's
/// (it only changes the [`InboundDropStage::Buffered`] line: the polled stage
/// is never reported on an ack channel).
pub(super) fn report_inbound_dropped(
    label: &str,
    cid: &ChannelId,
    stage: InboundDropStage,
    n: usize,
    acks: bool,
    audit: Option<&InboundDroppedAudit>,
) {
    let Some(n) = NonZeroUsize::new(n) else {
        return;
    };
    // WARN on the `[worker-refusal]` emitter, like the driver's dropped-reply
    // line (`replies::discard_on_exit`), not ERROR: the loss is recorded as a
    // durable row where there is a sink, and the line says so when there is
    // not. The emitter falls back to stderr when `tracing` would not record
    // it.
    let sink = audit.is_some();
    let report = match stage {
        InboundDropStage::Polled => format_inbound_drop_report(n, sink),
        InboundDropStage::Buffered => format_unread_drop_report(n, acks, sink),
    };
    emit_worker_refusal_report(label, &report, RefusalSeverity::Warn);
    let dropped = InboundDropped {
        channel: cid,
        stage,
        dropped: n,
        observed_at: time::OffsetDateTime::now_utc(),
    };
    record_inbound_dropped(audit, label, dropped);
}

/// A poll result that did not decode: a worker bug, not a death, so the
/// driver logs it and keeps going — whether the bus is up or has just closed.
/// What happened to the batch depends on the channel: an ack channel did not
/// ack it, so it is redelivered; one that does not redeliver has lost it, and
/// cannot say how many messages it held (#836).
pub(super) fn on_undecodable_batch(spec: &PolledWorkerSpec, e: &anyhow::Error) {
    if spec.ack_method.is_some() {
        tracing::warn!(
            label = spec.label,
            error = %e,
            "poll result decode failed; batch skipped (not acked, so the worker redelivers it)"
        );
    } else {
        tracing::warn!(
            label = spec.label,
            error = %e,
            "poll result decode failed; batch dropped: this channel does not redeliver, and how \
             many messages it held is unknown"
        );
    }
}

/// The bus closed while a poll was out: the whole batch `v` never reached it.
/// Consumes `v`, because the driver exits right after.
pub(super) fn on_bus_closed_during_poll(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    parse_poll: ParsePoll,
    v: serde_json::Value,
    audit: Option<&InboundDroppedAudit>,
) {
    if spec.ack_method.is_some() {
        tracing::info!(
            label = spec.label,
            "inbound receiver closed during a poll; polled driver exiting without acking the \
             batch (it is redelivered)"
        );
        return;
    }
    tracing::info!(label = spec.label, "inbound receiver closed during a poll; polled driver exiting");
    // Decoded only to count it: nothing in it is used.
    match parse_poll(v) {
        Ok(events) => {
            report_inbound_dropped(spec.label, cid, InboundDropStage::Polled, events.len(), false, audit)
        }
        // Said exactly as while the bus is up: no count to report.
        Err(e) => on_undecodable_batch(spec, &e),
    }
}

/// The bus closed mid-batch: the message whose send failed and the `unsent`
/// rest never reached it (`unsent` counts both).
pub(super) fn on_bus_closed_mid_batch(
    spec: &PolledWorkerSpec,
    cid: &ChannelId,
    unsent: usize,
    audit: Option<&InboundDroppedAudit>,
) {
    if spec.ack_method.is_some() {
        tracing::info!(
            label = spec.label,
            "inbound receiver closed; polled driver exiting without acking the rest of the batch \
             (it is redelivered)"
        );
        return;
    }
    tracing::info!(label = spec.label, "inbound receiver closed; polled driver exiting");
    report_inbound_dropped(spec.label, cid, InboundDropStage::Polled, unsent, false, audit);
}
