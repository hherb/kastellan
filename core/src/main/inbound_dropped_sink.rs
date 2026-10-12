//! The daemon's `channel.inbound_dropped` hook, shared by both polled channels
//! (#826, #846).
//!
//! Matrix calls it from its driver for a polled batch the closed bus never
//! took, and from its bus-side receiver for messages buffered unread; email
//! from its receiver only (its polled batches are redelivered). One builder,
//! so the row, the shutdown label and the lost-row line are spelled the same
//! for both: what differs is the [`SinkWriter`](crate::audit_sink::SinkWriter)
//! it writes through, whose channel the row is counted and reported under.

use std::num::NonZeroUsize;
use std::sync::Arc;

use kastellan_core::channel::polled_driver::{InboundDropStage, InboundDropped, InboundDroppedAudit};

use crate::audit_sink::SinkChannel;

/// Pure: the `(actor, action, payload)` of the row [`inbound_dropped_audit_sink`]
/// writes for `dropped` — the bus's actor, and [`InboundDropped::payload`]:
/// channel, stage, count and when.
pub(crate) fn inbound_dropped_row(
    dropped: &InboundDropped<'_>,
) -> (&'static str, &'static str, serde_json::Value) {
    ("channel", kastellan_core::channel::actions::INBOUND_DROPPED, dropped.payload())
}

/// Pure: the `[audit-lost]` report for a `channel.inbound_dropped` row a sink
/// could not write, naming the stage and the count so it can be matched to
/// the drop's own `[worker-refusal]` line.
pub(crate) fn format_inbound_row_lost(
    stage: InboundDropStage,
    dropped: NonZeroUsize,
    why: &dyn std::fmt::Display,
) -> String {
    format!(
        "{} row for {dropped} dropped inbound message{} ({}) not written: {why}. The drop's \
         [worker-refusal] line stands",
        kastellan_core::channel::actions::INBOUND_DROPPED,
        if dropped.get() == 1 { "" } else { "s" },
        stage.as_str(),
    )
}

/// Pure: the shutdown line's name for a pending row of `channel`'s at
/// `stage`. `'static` because [`crate::audit_sink::RowLabel::with_count`]
/// takes only the sink's own words, never a worker's.
fn row_what(channel: SinkChannel, stage: InboundDropStage) -> &'static str {
    match (channel, stage) {
        (SinkChannel::Matrix, InboundDropStage::Polled) => "matrix inbound messages dropped",
        (SinkChannel::Matrix, InboundDropStage::Buffered) => {
            "matrix inbound messages dropped unread"
        }
        (SinkChannel::Email, InboundDropStage::Polled) => "email inbound messages dropped",
        (SinkChannel::Email, InboundDropStage::Buffered) => "email inbound messages dropped unread",
    }
}

/// Build the [`InboundDroppedAudit`] hook for a polled channel: it writes the
/// `channel.inbound_dropped` row ([`inbound_dropped_row`]) through `writer`.
///
/// It is called from the driver's own std thread, which every conversation
/// and the poll wait on, or from the bus's stopping pump, so the insert is
/// **spawned** through `writer`, not awaited: a slow or unreachable Postgres
/// must not stall either. The hook holds an `Arc` of `writer`, so its lease
/// lasts until both the driver and the receiver have dropped their clones of
/// it. A row that was not written is reported on the `[audit-lost]` marker
/// ([`format_inbound_row_lost`]), under the writer's channel.
///
/// `report` is how a lost row is said: [`crate::audit_sink::emit_report`] in
/// the daemon, a recorder in a test (#799).
pub(crate) fn inbound_dropped_audit_sink(
    writer: Arc<crate::audit_sink::SinkWriter>,
    report: crate::audit_sink::Reporter,
) -> InboundDroppedAudit {
    Arc::new(move |dropped| {
        let (actor, action, payload) = inbound_dropped_row(&dropped);
        let (stage, n) = (dropped.stage, dropped.dropped);
        let label =
            crate::audit_sink::RowLabel::with_count(row_what(writer.channel(), stage), n.get());
        // Under the channel the row is counted under: the writer's own.
        let who = writer.channel().writer();
        // A refused row is reported through the closure, so its `Err` is not
        // needed here.
        let _ = writer.spawn(actor, action, payload, label, move |why| {
            report(who, &format_inbound_row_lost(stage, n, &why));
        });
    })
}

#[cfg(test)]
#[path = "inbound_dropped_sink_tests.rs"]
mod tests;
