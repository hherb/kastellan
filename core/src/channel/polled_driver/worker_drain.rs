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
//! every exit, after the driver's own polled-stage report. The worker stops
//! receiving first, so nothing arrives behind the count (unless it answers
//! `interrupted`, which covers a stop it could not confirm), and answers with
//! how many it discarded ([`WorkerHeld`]). That count is reported at the
//! [`InboundDropStage::Worker`] stage like any other inbound drop: one
//! `[worker-refusal]` WARN and one `channel.inbound_dropped` row.
//!
//! Three outcomes have no count, so they are said without a row:
//!
//! - the worker's stop **may have cut an upstream batch short, or could not
//!   be confirmed** (`interrupted`) — an unknown number of messages, possibly
//!   none, lost on top of the count;
//! - the worker **was down** at the driver's last call. It is **not asked**:
//!   what the dead process held died with it, and the supervisor's respawn
//!   loop answers a call that arrives while it waits out a backoff with
//!   "restarting" and then starts a fresh worker anyway — so asking would hold
//!   this exit for up to a backoff, or a fresh Matrix login, past the daemon's
//!   shutdown bound, only to be answered by a process that never held
//!   anything the driver missed;
//! - the worker **could not be asked**: the call failed (it died on the call,
//!   or a stale binary without the method answers METHOD_NOT_FOUND), or it
//!   answered something that did not decode.
//!
//! The call has no timeout of its own, like every other call the driver makes
//! (`PersistentHandle::call`); Matrix's worker bounds its side at
//! `kastellan_matrix_wire::DRAIN_STOP_BOUND_MS`, which the daemon's shutdown
//! drain is asserted to outlast together with one long-poll. That bound holds
//! for a live worker only, which is why a down one is not asked.

use super::inbound_drop::report_inbound_dropped;
use super::{InboundDropStage, InboundDroppedAudit, PolledWorkerSpec, WorkerCalls, WorkerDrainSpec};
use crate::channel::actions;
use crate::channel::ChannelId;
use crate::worker_stderr::{emit_worker_refusal_report, RefusalSeverity};

/// Pure: the line for a worker stop that may have cut an upstream batch
/// short — or that the worker could not confirm, which reads the same way.
pub(super) fn format_interrupted_report() -> String {
    format!(
        "the worker's stop may have cut short a batch it was taking in from its upstream, or \
         could not be confirmed: an unknown number of inbound messages, possibly none, are lost \
         and not counted; NOT recorded as {}: there is no count to record",
        actions::INBOUND_DROPPED,
    )
}

/// Pure: the line for a worker that was down at the driver's last call, so it
/// was not asked.
pub(super) fn format_drain_skipped_down_report(method: &str) -> String {
    format!(
        "did not ask the worker how many inbound messages it still held ({method}): it was down \
         at the driver's last call, so any it held died with it, and a respawned one is not \
         asked; any it held, received but not yet polled, are lost uncounted; NOT recorded as \
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

/// Refuse a spec that names a drain method on a channel with an ack method:
/// the worker stage's line and row both say the channel does not redeliver
/// (see [`PolledWorkerSpec::drain`]).
pub(super) fn check_drain(spec: &PolledWorkerSpec) -> anyhow::Result<()> {
    if let (Some(drain), Some(ack)) = (spec.drain, spec.ack_method) {
        anyhow::bail!(
            "{}: drain method {} on a channel with ack method {ack}: the worker stage reports a \
             channel that does not redeliver (#835)",
            spec.label,
            drain.method,
        );
    }
    Ok(())
}

/// Ask the worker what it still held and report it. Called once, as the
/// driver exits, for a spec with a drain method. `worker_down` is whether the
/// driver's last call found the worker down; then it is not asked (see the
/// module docs).
pub(super) fn drain_worker_on_exit(
    calls: &dyn WorkerCalls,
    spec: &PolledWorkerSpec,
    drain: WorkerDrainSpec,
    worker_down: bool,
    cid: &ChannelId,
    audit: Option<&InboundDroppedAudit>,
) {
    if worker_down {
        let report = format_drain_skipped_down_report(drain.method);
        emit_worker_refusal_report(spec.label, &report, RefusalSeverity::Warn);
        return;
    }
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
