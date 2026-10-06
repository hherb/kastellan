//! Inbound messages a polled driver could not hand to the bus because the bus
//! had closed — a channel restart or a shutdown that landed while a poll was
//! out, or mid-batch (#826).
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

use super::audit::{record_inbound_dropped, InboundDroppedAudit};
use super::{ParsePoll, PolledWorkerSpec};
#[allow(unused_imports)] // referenced by the doc comments' intra-doc links
use crate::channel::actions;
use crate::channel::undelivered::observed_at_json;
use crate::channel::ChannelId;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

/// What the [`InboundDroppedAudit`] hook is told about a dropped batch: whose
/// channel, how many messages, and when — nothing a peer wrote or chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InboundDropped<'a> {
    pub channel: &'a ChannelId,
    /// How many messages the bus never saw. Never 0: an empty batch is not
    /// reported.
    pub dropped: usize,
    /// When they were dropped. The row is written after the fact (#789), so
    /// `audit_log.ts` is the insert's time, not this.
    pub observed_at: time::OffsetDateTime,
}

impl InboundDropped<'_> {
    /// Pure: the payload of an [`actions::INBOUND_DROPPED`] row — the channel,
    /// the count and when, nothing else.
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "channel": self.channel.0,
            "dropped": self.dropped,
            "observed_at": observed_at_json(self.observed_at),
        })
    }
}

/// Pure: the line for `n` inbound messages dropped by a channel that does not
/// redeliver. `sink` says whether the channel records them as a row.
pub(super) fn format_inbound_drop_report(n: usize, sink: bool) -> String {
    let recorded = if sink {
        "recording it as channel.inbound_dropped"
    } else {
        "NOT recorded as channel.inbound_dropped: this channel has no audit sink"
    };
    format!(
        "dropped {n} inbound message{}: the bus closed (the channel was restarted or shut down) \
         after the worker had passed {}, and this channel does not redeliver; {recorded}",
        if n == 1 { "" } else { "s" },
        if n == 1 { "it" } else { "them" },
    )
}

/// Say that `n` inbound messages on `cid` were dropped, and hand the count to
/// the audit hook. Nothing for `n == 0`.
fn report_inbound_dropped(
    label: &str,
    cid: &ChannelId,
    n: usize,
    audit: Option<&InboundDroppedAudit>,
) {
    if n == 0 {
        return;
    }
    // WARN, like the driver's other drop lines (`replies::discard_on_exit`):
    // the emitter falls back to stderr when `tracing` would not record it.
    let report = format_inbound_drop_report(n, audit.is_some());
    emit_worker_refusal_report(label, &report, RefusalSeverity::Warn);
    let dropped =
        InboundDropped { channel: cid, dropped: n, observed_at: time::OffsetDateTime::now_utc() };
    record_inbound_dropped(audit, label, dropped);
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
        Ok(events) => report_inbound_dropped(spec.label, cid, events.len(), audit),
        // A batch that does not decode is skipped while the bus is up too.
        Err(e) => {
            tracing::warn!(label = spec.label, error = %e, "poll result decode failed; batch skipped")
        }
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
    report_inbound_dropped(spec.label, cid, unsent, audit);
}
