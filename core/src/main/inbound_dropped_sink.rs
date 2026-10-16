//! The daemon's `channel.inbound_dropped` hook, shared by both polled channels
//! (#826, #846), and its Matrix-only sibling for inbound messages lost because
//! the worker's answer did not decode, `channel.inbound_undecodable` (#836,
//! #852, #853).
//!
//! The inbound-dropped hook: Matrix calls it from its driver for a polled
//! batch the closed bus never took and for what its worker still held at exit (#835), and from its
//! bus-side receiver for messages buffered unread; email
//! from its receiver only (its polled batches are redelivered). One builder,
//! so the row, the shutdown label and the lost-row line are spelled the same
//! for both: what differs is the [`SinkWriter`](crate::audit_sink::SinkWriter)
//! it writes through, whose channel the row is counted and reported under.

use std::num::NonZeroUsize;
use std::sync::Arc;

use kastellan_core::channel::polled_driver::{
    InboundDropStage, InboundDropped, InboundDroppedAudit, InboundUndecodable,
    InboundUndecodableAudit, UndecodablePart,
};

use crate::audit_sink::{SinkChannel, Unwritten};

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
    why: &Unwritten<'_>,
) -> String {
    format!(
        "{} row for {dropped} dropped inbound message{} ({}) {}. The drop's \
         [worker-refusal] line stands",
        kastellan_core::channel::actions::INBOUND_DROPPED,
        if dropped.get() == 1 { "" } else { "s" },
        stage.as_str(),
        why.said(),
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
        (SinkChannel::Matrix, InboundDropStage::Worker) => {
            "matrix inbound messages dropped in the worker"
        }
        // Email's spec names no drain method, so this is never written; it is
        // spelled anyway rather than mislabelled should that change.
        (SinkChannel::Email, InboundDropStage::Worker) => "email inbound messages dropped in the worker",
    }
}

/// Build the [`InboundDroppedAudit`] hook for a polled channel: it writes the
/// `channel.inbound_dropped` row ([`inbound_dropped_row`]) through `writer`.
///
/// It is called from the driver's own std thread, which every conversation
/// and the poll wait on, or from the bus's stopping pump, so the insert is
/// **spawned** through `writer`, not awaited: a slow or unreachable Postgres
/// must not stall either. The hook holds an `Arc` of `writer`, so the lease
/// lasts until the driver, the receiver and any sibling hook built on the same
/// writer have all dropped theirs. A lost row is reported on the `[audit-lost]` marker
/// ([`format_inbound_row_lost`]), under the writer's channel — past a flood,
/// counted instead (`audit_sink_thinning.rs`).
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

/// Pure: the `(actor, action, payload)` of the row
/// [`inbound_undecodable_audit_sink`] writes for `lost` — the bus's actor,
/// and [`InboundUndecodable::payload`]: channel, part, when, and for events
/// the count.
pub(crate) fn inbound_undecodable_row(
    lost: &InboundUndecodable<'_>,
) -> (&'static str, &'static str, serde_json::Value) {
    ("channel", kastellan_core::channel::actions::INBOUND_UNDECODABLE, lost.payload())
}

/// Pure: what a `channel.inbound_undecodable` row of `part` is for, in its
/// lost-row line.
fn undecodable_row_for(part: UndecodablePart) -> String {
    match part {
        UndecodablePart::Batch => "an inbound batch lost undecodable".to_string(),
        UndecodablePart::Events(n) => format!(
            "{n} inbound event{} lost undecodable",
            if n.get() == 1 { "" } else { "s" }
        ),
        UndecodablePart::Drain => "an undecodable drain answer".to_string(),
    }
}

/// Pure: the `[audit-lost]` report for a `channel.inbound_undecodable` row of
/// `part` a sink could not write.
pub(crate) fn format_undecodable_row_lost(part: UndecodablePart, why: &Unwritten<'_>) -> String {
    format!(
        "{} row for {} {}. Its [worker-refusal] line stands",
        kastellan_core::channel::actions::INBOUND_UNDECODABLE,
        undecodable_row_for(part),
        why.said(),
    )
}

/// Pure: the shutdown line's name for a pending `channel.inbound_undecodable`
/// row of `channel`'s, for `part`. `'static` because
/// [`crate::audit_sink::RowLabel`] takes only the sink's own words.
fn undecodable_row_what(channel: SinkChannel, part: UndecodablePart) -> &'static str {
    match (channel, part) {
        (SinkChannel::Matrix, UndecodablePart::Batch) => "matrix inbound batch lost undecodable",
        (SinkChannel::Matrix, UndecodablePart::Events(_)) => "matrix inbound events lost undecodable",
        (SinkChannel::Matrix, UndecodablePart::Drain) => "matrix undecodable drain answer",
        // Email acks, so its undecodable batches are redelivered and never
        // written, and it has no drain; spelled anyway rather than
        // mislabelled should that change.
        (SinkChannel::Email, UndecodablePart::Batch) => "email inbound batch lost undecodable",
        (SinkChannel::Email, UndecodablePart::Events(_)) => "email inbound events lost undecodable",
        (SinkChannel::Email, UndecodablePart::Drain) => "email undecodable drain answer",
    }
}

/// Pure: the shutdown line's label for a pending row of `part`: with its
/// count for events, named only for a batch or a drain answer (no count).
fn undecodable_row_label(channel: SinkChannel, part: UndecodablePart) -> crate::audit_sink::RowLabel {
    let what = undecodable_row_what(channel, part);
    match part {
        UndecodablePart::Events(n) => crate::audit_sink::RowLabel::with_count(what, n.get()),
        UndecodablePart::Batch | UndecodablePart::Drain => crate::audit_sink::RowLabel::named(what),
    }
}

/// Build the [`InboundUndecodableAudit`] hook for a polled channel that does
/// not redeliver (Matrix): it writes the `channel.inbound_undecodable` row
/// ([`inbound_undecodable_row`]) through `writer`, spawned rather than
/// awaited — it is called from the driver's own thread, like the hook above —
/// and reports a row it could not write on `[audit-lost]`
/// ([`format_undecodable_row_lost`]).
pub(crate) fn inbound_undecodable_audit_sink(
    writer: Arc<crate::audit_sink::SinkWriter>,
    report: crate::audit_sink::Reporter,
) -> InboundUndecodableAudit {
    Box::new(move |lost| {
        let (actor, action, payload) = inbound_undecodable_row(&lost);
        let part = lost.part;
        let label = undecodable_row_label(writer.channel(), part);
        let who = writer.channel().writer();
        // A refused row is reported through the closure, so its `Err` is not
        // needed here.
        let _ = writer.spawn(actor, action, payload, label, move |why| {
            report(who, &format_undecodable_row_lost(part, &why));
        });
    })
}

#[cfg(test)]
#[path = "inbound_dropped_sink_tests.rs"]
mod tests;
