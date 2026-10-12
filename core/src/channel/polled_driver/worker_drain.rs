//! The last call a driver makes as it exits: ask the worker how many inbound
//! messages it still held (#835).
//!
//! A worker that receives on its own between polls — the live Matrix worker
//! runs a continuous sync and queues what arrives — holds messages the driver
//! has never seen. When the driver exits, it drops its [`WorkerCalls`], the
//! worker dies, and those messages die with it: the worker's upstream has
//! already moved past them (Matrix saved its sync token before queueing them),
//! so nothing redelivers them. Before #835 nothing counted them either.
//!
//! So a spec that names a drain method ([`WorkerDrainSpec`]) gets one call, on
//! every exit, after the driver's own reports. The worker stops receiving
//! first, so nothing arrives behind the count, and answers with how many it
//! discarded ([`WorkerHeld`]). That count is reported at the
//! [`InboundDropStage::Worker`] stage like any other inbound drop: one
//! `[worker-refusal]` WARN and one `channel.inbound_dropped` row.
//!
//! Two answers have no count, so they are said without a row:
//!
//! - the worker's stop **may have cut an upstream batch short**
//!   (`interrupted`) — an unknown number of messages, possibly none, lost on
//!   top of the count;
//! - the worker **could not be asked**: it was down (being respawned, so what
//!   it held died with the old process), or answered something that did not
//!   decode (say, a stale binary without the method).
//!
//! The call has no timeout of its own, like every other call the driver makes
//! (`PersistentHandle::call`); Matrix's worker bounds its side at 2 s.

use super::inbound_drop::report_inbound_dropped;
use super::{InboundDropStage, InboundDroppedAudit, PolledWorkerSpec, WorkerCalls, WorkerDrainSpec};
use crate::channel::actions;
use crate::channel::ChannelId;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

/// Pure: the line for a worker stop that may have cut an upstream batch short.
pub(super) fn format_interrupted_report() -> String {
    format!(
        "the worker was stopped while it was taking in a batch from its upstream: an unknown \
         number of inbound messages, possibly none, are lost and not counted; NOT recorded as \
         {}: there is no count to record",
        actions::INBOUND_DROPPED,
    )
}

/// Pure: the line for a drain that got no usable answer.
pub(super) fn format_drain_failed_report(method: &str, e: &anyhow::Error) -> String {
    format!(
        "could not ask the worker how many inbound messages it still held ({method}: {e:#}); \
         any it held, received but not yet polled, are lost uncounted; NOT recorded as {}: \
         there is no count to record",
        actions::INBOUND_DROPPED,
    )
}

/// Ask the worker what it still held and report it. Called once, as the
/// driver exits, for a spec with a drain method.
pub(super) fn drain_worker_on_exit(
    calls: &dyn WorkerCalls,
    spec: &PolledWorkerSpec,
    drain: WorkerDrainSpec,
    cid: &ChannelId,
    audit: Option<&InboundDroppedAudit>,
) {
    let held = match calls.call(drain.method, serde_json::json!({})).and_then(drain.parse) {
        Ok(held) => held,
        Err(e) => {
            let report = format_drain_failed_report(drain.method, &e);
            emit_worker_refusal_report(spec.label, &report, RefusalSeverity::Warn);
            return;
        }
    };
    // `acks: false` only changes the buffered stage's line.
    report_inbound_dropped(spec.label, cid, InboundDropStage::Worker, held.discarded, false, audit);
    if held.interrupted {
        emit_worker_refusal_report(spec.label, &format_interrupted_report(), RefusalSeverity::Warn);
    }
}
